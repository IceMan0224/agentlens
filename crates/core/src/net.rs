//! SSH sessions, listening ports and per-process connections.

use std::net::IpAddr;

use crate::catalog::Catalog;
use crate::model::{Model, Role, stem};
use crate::snapshot::TcpConn;

/// ssh options that take a value (`-p 22` or `-p22`).
const SSH_VALUE_OPTS: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// PuTTY (plink) options that take a value. Its options are whole words, case-sensitive.
const PUTTY_VALUE_OPTS: &[&str] = &[
    "P",
    "l",
    "pw",
    "pwfile",
    "i",
    "load",
    "hostkey",
    "m",
    "L",
    "R",
    "D",
    "nc",
    "proxycmd",
    "sercfg",
    "sshlog",
    "sshrawlog",
];

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
    pub identity_files: Vec<String>,
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
                    "identityfile" => host.identity_files.push(value),
                    _ => {}
                }
            }
        }
    }
    hosts
}

pub fn load_ssh_config() -> Vec<SshHost> {
    crate::keys::ssh_dir()
        .map(|dir| dir.join("config"))
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

/// (user, destination, port) from a PuTTY-style command line (`plink -P 22 -pw x me@box`).
pub fn parse_putty_args(args: &[String]) -> Option<(Option<String>, String, Option<u16>)> {
    let mut user = None;
    let mut port = None;
    let mut it = args.iter().skip(1);
    while let Some(arg) = it.next() {
        if let Some(opt) = arg.strip_prefix('-') {
            if PUTTY_VALUE_OPTS.contains(&opt) {
                let value = it.next().cloned();
                match opt {
                    "P" => port = value.and_then(|v| v.parse().ok()),
                    "l" => user = value,
                    _ => {}
                }
            }
            continue;
        }
        let (u, host) = match arg.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h),
            None => (user, arg.as_str()),
        };
        return Some((u, host.to_string(), port));
    }
    None
}

/// Connections of SSH clients and servers, and any connection to one of the catalog's SSH
/// ports. Ports from `~/.ssh/config` count for SSH servers only: clients are matched by name,
/// and a `Port 443` there must not make every process's HTTPS connection an SSH one.
pub fn ssh_connections(model: &Model, catalog: &Catalog, config: &[SshHost]) -> Vec<SshConn> {
    let ssh = &catalog.ssh;
    let server_ports: Vec<u16> = ssh
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
        let inbound = ssh.servers.contains(&s) && server_ports.contains(&conn.local_port);
        if !(client || inbound || ssh.ports.contains(&conn.remote_port)) {
            continue;
        }
        let (mut alias, mut user, mut host, mut port) = (None, None, conn.remote_addr.clone(), conn.remote_port);
        let parsed = if ssh.putty.contains(&s) {
            parse_putty_args(&p.cmd)
        } else {
            parse_ssh_args(&p.cmd)
        };
        if client && let Some((u, dest, pt)) = parsed {
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

/// Who a listening port belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortOwner {
    /// Inside an agent session: (group, tab, session) indices into `Model::groups`.
    Session(usize, usize, usize),
    /// Inside an orphan tree, by its root.
    Orphan(u32),
    Other,
}

#[derive(Debug, Clone)]
pub struct Port {
    pub port: u16,
    pub pid: u32,
    pub process: String,
    /// One per address family or interface it is bound to.
    pub addrs: Vec<String>,
    /// Reachable from other machines: bound to every interface or to a non-loopback address.
    pub exposed: bool,
    /// Connections accepted on it right now.
    pub clients: usize,
    pub owner: PortOwner,
    /// The agent itself listens (its own local server), not a program it started.
    pub agent_internal: bool,
}

impl Port {
    /// Belongs to an agent session or an orphan, rather than to the rest of the system.
    pub fn agent_related(&self) -> bool {
        self.owner != PortOwner::Other
    }
}

pub fn is_exposed(addr: &str) -> bool {
    addr.parse::<IpAddr>().is_ok_and(|ip| !ip.is_loopback())
}

/// Listening ports, one row per (process, port), orphans first, then exposed ones.
pub fn ports(model: &Model) -> Vec<Port> {
    let mut out: Vec<Port> = Vec::new();
    for l in &model.listening {
        let Some(p) = model.procs.get(&l.pid) else { continue };
        if let Some(row) = out.iter_mut().find(|r| r.pid == l.pid && r.port == l.port) {
            if !row.addrs.contains(&l.addr) {
                row.addrs.push(l.addr.clone());
            }
            row.exposed |= is_exposed(&l.addr);
            continue;
        }
        let owner = match model.locate(l.pid) {
            Some((g, t, s)) => PortOwner::Session(g, t, s),
            None => model
                .orphans
                .iter()
                .find(|o| model.is_ancestor_or_self(o.pid, l.pid))
                .map_or(PortOwner::Other, |o| PortOwner::Orphan(o.pid)),
        };
        // Clients from inside the same tree are its own workers talking to each other.
        let tree = model.subtree(match owner {
            PortOwner::Session(g, t, s) => model.groups[g].tabs[t].sessions[s].root_pid,
            PortOwner::Orphan(root) => root,
            PortOwner::Other => l.pid,
        });
        let internal = |c: &TcpConn| {
            model.tcp.iter().any(|peer| {
                peer.local_port == c.remote_port && peer.local_addr == c.remote_addr && tree.contains(&peer.pid)
            })
        };
        out.push(Port {
            port: l.port,
            pid: l.pid,
            process: p.name.clone(),
            addrs: vec![l.addr.clone()],
            exposed: is_exposed(&l.addr),
            clients: model
                .tcp
                .iter()
                .filter(|c| c.pid == l.pid && c.local_port == l.port && !internal(c))
                .count(),
            owner,
            agent_internal: matches!(model.roles.get(&l.pid), Some(Role::Agent(_))),
        });
    }
    for row in &mut out {
        row.agent_internal &= !row.exposed;
    }
    out.sort_by_key(|r| {
        (
            !matches!(r.owner, PortOwner::Orphan(_)),
            !r.exposed,
            r.agent_internal,
            r.port,
        )
    });
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
    fn parses_putty_word_options() {
        // `-pw` takes the next argument: the password must not become the destination.
        assert_eq!(
            parse_putty_args(&args("plink -ssh -P 2222 -pw s3cret me@box")),
            Some((Some("me".into()), "box".into(), Some(2222)))
        );
        assert_eq!(
            parse_putty_args(&args("plink -load prod -l root box")),
            Some((Some("root".into()), "box".into(), None))
        );
        assert_eq!(parse_putty_args(&args("putty -load prod")), None);
    }

    #[test]
    fn config_ports_do_not_make_https_ssh() {
        use crate::snapshot::{ProcInfo, Snapshot};
        let proc = |pid: u32, name: &str, cmd: &str| ProcInfo {
            pid,
            ppid: None,
            name: name.into(),
            exe: None,
            cmd: args(cmd),
            cwd: None,
            env: Vec::new(),
            start_time: 1,
            cpu: 0.0,
            private_bytes: 0,
            working_set: 0,
        };
        let conn = |pid: u32| TcpConn {
            pid,
            local_addr: "10.0.0.2".into(),
            local_port: 50000 + pid as u16,
            remote_addr: "140.82.112.35".into(),
            remote_port: 443,
        };
        let catalog = Catalog::builtin();
        let model = Model::build(
            Snapshot {
                taken_at: 2,
                processes: vec![proc(10, "node", "node cli.js"), proc(11, "ssh", "ssh gh")],
                tcp: vec![conn(10), conn(11)],
                listening: Vec::new(),
            },
            &catalog,
        );
        // GitHub's documented SSH-over-HTTPS setup.
        let config = parse_ssh_config("Host gh\n  Hostname ssh.github.com\n  Port 443\n");
        let rows = ssh_connections(&model, &catalog, &config);
        assert_eq!(rows.iter().map(|c| c.pid).collect::<Vec<_>>(), [11]);
        assert_eq!((rows[0].host.as_str(), rows[0].port), ("ssh.github.com", 443));
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
