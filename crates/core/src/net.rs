//! SSH sessions and per-process connections.

use crate::catalog::Catalog;
use crate::model::{Model, stem};
use crate::snapshot::TcpConn;

/// ssh options that take a value (`-p 22` or `-p22`).
const SSH_VALUE_OPTS: &str = "BbcDEeFIiJLlmOoPpQRSWw";

#[derive(Debug, Clone)]
pub struct SshConn {
    pub pid: u32,
    pub process: String,
    pub inbound: bool,
    /// The name typed on the command line when it is a `~/.ssh/config` alias.
    pub alias: Option<String>,
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
    pub remote: String,
    pub local: String,
    pub since: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshHost {
    pub patterns: Vec<String>,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
}

pub fn parse_ssh_config(text: &str) -> Vec<SshHost> {
    let mut hosts: Vec<SshHost> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c == '=' || c.is_whitespace()) {
            Some((k, v)) => (
                k.to_ascii_lowercase(),
                v.trim().trim_start_matches('=').trim().trim_matches('"').to_string(),
            ),
            None => continue,
        };
        match key.as_str() {
            "host" => hosts.push(SshHost {
                patterns: value.split_whitespace().map(str::to_string).collect(),
                ..Default::default()
            }),
            "match" => hosts.push(SshHost::default()),
            _ => {
                let Some(host) = hosts.last_mut() else { continue };
                match key.as_str() {
                    "hostname" => host.hostname = Some(value),
                    "user" => host.user = Some(value),
                    "port" => host.port = value.parse().ok(),
                    _ => {}
                }
            }
        }
    }
    hosts
}

pub fn load_ssh_config() -> Vec<SshHost> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(|home| std::path::PathBuf::from(home).join(".ssh").join("config"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| parse_ssh_config(&text))
        .unwrap_or_default()
}

fn glob(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// Resolves an alias like ssh does: for each field, the first matching block wins.
pub fn resolve(config: &[SshHost], alias: &str) -> SshHost {
    let mut out = SshHost::default();
    for host in config {
        let negated = host
            .patterns
            .iter()
            .any(|p| p.strip_prefix('!').is_some_and(|p| glob(p, alias)));
        if negated || !host.patterns.iter().any(|p| !p.starts_with('!') && glob(p, alias)) {
            continue;
        }
        out.hostname = out.hostname.or(host.hostname.clone());
        out.user = out.user.or(host.user.clone());
        out.port = out.port.or(host.port);
    }
    out
}

/// (user, destination, port) from an ssh-style command line.
pub fn parse_ssh_args(args: &[String]) -> Option<(Option<String>, String, Option<u16>)> {
    let mut user = None;
    let mut port = None;
    let mut it = args.iter().skip(1);
    while let Some(arg) = it.next() {
        if let Some(opt) = arg.strip_prefix('-') {
            // Flags can be grouped (`-NL 8080:db:5432`): the first one that takes a value
            // uses the rest of the group, or the next argument when nothing is left.
            if let Some((i, flag)) = opt.char_indices().find(|(_, f)| SSH_VALUE_OPTS.contains(*f)) {
                let rest = &opt[i + flag.len_utf8()..];
                let value = if rest.is_empty() {
                    it.next().cloned()
                } else {
                    Some(rest.to_string())
                };
                match flag {
                    'p' | 'P' => port = value.and_then(|v| v.parse().ok()),
                    'l' => user = value,
                    _ => {}
                }
            }
            continue;
        }
        let dest = arg.strip_prefix("ssh://").unwrap_or(arg);
        let (u, host) = match dest.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h),
            None => (user, dest),
        };
        let (host, p) = match host.rsplit_once(':') {
            Some((h, p)) if arg.starts_with("ssh://") => (h, p.parse().ok()),
            _ => (host, None),
        };
        return Some((u, host.to_string(), p.or(port)));
    }
    None
}

/// Connections of SSH clients and servers, and any connection to an SSH port (the catalog's
/// plus every `Port` in `~/.ssh/config`).
pub fn ssh_connections(model: &Model, catalog: &Catalog, config: &[SshHost]) -> Vec<SshConn> {
    let ssh = &catalog.ssh;
    let ports: Vec<u16> = ssh
        .ports
        .iter()
        .copied()
        .chain(config.iter().filter_map(|h| h.port))
        .collect();
    let mut out = Vec::new();
    for conn in &model.tcp {
        let Some(p) = model.procs.get(&conn.pid) else { continue };
        let s = stem(p);
        let client = ssh.clients.contains(&s);
        let inbound = ssh.servers.contains(&s) && ports.contains(&conn.local_port);
        if !(client || inbound || ports.contains(&conn.remote_port)) {
            continue;
        }
        let (mut alias, mut user, mut host, mut port) = (None, None, conn.remote_addr.clone(), conn.remote_port);
        if client && let Some((u, dest, pt)) = parse_ssh_args(&p.cmd) {
            let resolved = resolve(config, &dest);
            if resolved.hostname.is_some() {
                alias = Some(dest.clone());
            }
            user = u.or(resolved.user);
            host = resolved.hostname.unwrap_or(dest);
            port = pt.or(resolved.port).unwrap_or(conn.remote_port);
        }
        out.push(SshConn {
            pid: conn.pid,
            process: p.name.clone(),
            inbound,
            alias,
            user,
            host,
            port,
            remote: endpoint(&conn.remote_addr, conn.remote_port),
            local: endpoint(&conn.local_addr, conn.local_port),
            since: p.start_time,
        });
    }
    out.sort_by_key(|c| (c.inbound, c.pid));
    // A socket can be listed once per address family; keep one row per connection.
    out.dedup_by(|a, b| a.pid == b.pid && a.remote == b.remote && a.local == b.local);
    out
}

pub fn connections_of<'a>(model: &'a Model, pids: &'a [u32]) -> impl Iterator<Item = &'a TcpConn> + 'a {
    model.tcp.iter().filter(move |c| pids.contains(&c.pid))
}

pub fn endpoint(addr: &str, port: u16) -> String {
    if addr.contains(':') {
        format!("[{addr}]:{port}")
    } else {
        format!("{addr}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_destinations() {
        assert_eq!(
            parse_ssh_args(&args("ssh -p 2222 me@box")),
            Some((Some("me".into()), "box".into(), Some(2222)))
        );
        assert_eq!(
            parse_ssh_args(&args("ssh -i key -lroot -o X=1 box")),
            Some((Some("root".into()), "box".into(), None))
        );
        assert_eq!(
            parse_ssh_args(&args("ssh ssh://u@h:2200")),
            Some((Some("u".into()), "h".into(), Some(2200)))
        );
        assert_eq!(parse_ssh_args(&args("ssh -N")), None);
        // Grouped flags ending in an option that takes a value.
        assert_eq!(
            parse_ssh_args(&args("ssh -NL 8080:db:5432 bastion")),
            Some((None, "bastion".into(), None))
        );
        assert_eq!(
            parse_ssh_args(&args("ssh -4p 2222 box")),
            Some((None, "box".into(), Some(2222)))
        );
        assert_eq!(
            parse_ssh_args(&args("ssh -vp2200 box")),
            Some((None, "box".into(), Some(2200)))
        );
    }

    #[test]
    fn resolves_aliases_first_value_wins() {
        let config = parse_ssh_config(
            "Host dev\n  HostName 10.0.0.5\n  User alice\nHost *\n  User nobody\n  Port 2022\nHost !dev other\n  Port 1\n",
        );
        let r = resolve(&config, "dev");
        assert_eq!(r.hostname.as_deref(), Some("10.0.0.5"));
        assert_eq!(r.user.as_deref(), Some("alice"));
        assert_eq!(r.port, Some(2022));
        assert_eq!(resolve(&config, "other").port, Some(2022));
        assert_eq!(resolve(&config, "unknown").hostname, None);
    }
}
