use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use netstat2::{AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState, get_sockets_info};
use serde::{Deserialize, Serialize};

use crate::sys;

/// Variables whose names look like credentials are dropped even when asked for, and
/// credentials in command lines and URLs are masked (see `redact_cmd`), so snapshots do not
/// carry secrets in the usual places. Detection is by name and shape, not exhaustive.
const SECRET_MARKERS: &[&str] = &["TOKEN", "KEY", "SECRET", "PASSWORD", "AUTH"];

const REDACTED: &str = "***";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Unix seconds.
    pub taken_at: u64,
    pub processes: Vec<ProcInfo>,
    pub tcp: Vec<TcpConn>,
    /// Absent from snapshots saved before ports were collected.
    #[serde(default)]
    pub listening: Vec<Listener>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    pub exe: Option<String>,
    pub cmd: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// Unix seconds. Used with `ppid` to detect PID reuse.
    pub start_time: u64,
    pub cpu: f32,
    pub private_bytes: u64,
    pub working_set: u64,
}

/// A TCP socket waiting for connections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listener {
    pub pid: u32,
    pub addr: String,
    pub port: u16,
}

/// An established TCP connection; closing sockets are not collected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TcpConn {
    pub pid: u32,
    pub local_addr: String,
    pub local_port: u16,
    pub remote_addr: String,
    pub remote_port: u16,
}

/// How often working directories and environments are read on platforms where that is a
/// separate, expensive step (Windows: the PEB read enumerates every process again, ~13 ms,
/// and new processes appear almost every second). Elsewhere `details` already has them.
const CWD_ENV_READ_INTERVAL: Duration = Duration::from_secs(5);

/// Connections change less often than CPU and memory, and listing them is the most expensive
/// part of a tick on macOS (~11 ms: every file descriptor of every process). The previous list
/// is reused in between; `refresh_network` forces a new one, e.g. after a disconnect.
const TCP_INTERVAL: Duration = Duration::from_secs(3);

pub struct Collector {
    lister: sys::Lister,
    /// Environment variables worth keeping (`Catalog::env_keys`), uppercase.
    env_keys: Vec<String>,
    /// Per (pid, start time): details read once, plus the last CPU time for usage deltas.
    known: HashMap<(u32, u64), Known>,
    last_tick: Option<Instant>,
    last_cwd_env_read: Option<Instant>,
    tcp: Vec<TcpConn>,
    listening: Vec<Listener>,
    last_tcp: Option<Instant>,
}

struct Known {
    exe: Option<String>,
    cmd: Vec<String>,
    cwd: Option<String>,
    /// Filtered: only identifying keys, never secrets.
    env: Vec<(String, String)>,
    cwd_env_pending: bool,
    cpu_ns: u64,
}

impl Collector {
    /// Keeps only `env_keys` of each process's environment (see `Catalog::env_keys`).
    pub fn new(env_keys: Vec<String>) -> Self {
        Self {
            lister: sys::Lister::new(),
            env_keys: env_keys.iter().map(|k| k.to_ascii_uppercase()).collect(),
            known: HashMap::new(),
            last_tick: None,
            last_cwd_env_read: None,
            tcp: Vec::new(),
            listening: Vec::new(),
            last_tcp: None,
        }
    }

    /// Switches to a new set of variables; details are read again so every process has them.
    pub fn set_env_keys(&mut self, env_keys: Vec<String>) {
        self.env_keys = env_keys.iter().map(|k| k.to_ascii_uppercase()).collect();
        self.known.clear();
        self.last_cwd_env_read = None;
    }

    /// Makes the next snapshot list connections and listening sockets again instead of
    /// reusing the last lists.
    pub fn refresh_network(&mut self) {
        self.last_tcp = None;
    }

    /// CPU usage is a delta between two snapshots, so the first one reports 0%.
    pub fn snapshot(&mut self) -> Snapshot {
        let now = Instant::now();
        let elapsed_ns = self.last_tick.map(|t| (now - t).as_nanos() as f64);
        self.last_tick = Some(now);

        let raw = self.lister.list();
        let alive: std::collections::HashSet<(u32, u64)> = raw.iter().map(|p| (p.pid, p.start_time)).collect();
        self.known.retain(|key, _| alive.contains(key));

        let mut cpu = Vec::with_capacity(raw.len());
        for p in &raw {
            let known = self.known.entry((p.pid, p.start_time)).or_insert_with(|| {
                // Access denied (protected or other users' processes) leaves these empty; they
                // are not retried.
                let d = sys::details(p.pid);
                Known {
                    exe: d.exe,
                    cmd: redact_cmd(d.cmd),
                    cwd: d.cwd,
                    cwd_env_pending: d.env.is_none(),
                    env: d
                        .env
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|kv| keep_env(kv, &self.env_keys))
                        .collect(),
                    cpu_ns: p.cpu_ns,
                }
            });
            let delta = p.cpu_ns.saturating_sub(known.cpu_ns);
            known.cpu_ns = p.cpu_ns;
            cpu.push(
                elapsed_ns
                    .filter(|e| *e > 0.0)
                    .map_or(0.0, |e| (delta as f64 / e * 100.0) as f32),
            );
        }
        self.read_pending_cwd_env();

        let processes = raw
            .into_iter()
            .zip(cpu)
            .map(|(p, cpu)| {
                let known = &self.known[&(p.pid, p.start_time)];
                ProcInfo {
                    pid: p.pid,
                    ppid: p.ppid,
                    name: display_name(p.name, known.exe.as_deref()),
                    exe: known.exe.clone(),
                    cmd: known.cmd.clone(),
                    cwd: known.cwd.clone(),
                    env: known.env.clone(),
                    start_time: p.start_time,
                    cpu,
                    private_bytes: p.private_bytes,
                    working_set: p.working_set,
                }
            })
            .collect();

        self.read_sockets();
        Snapshot {
            taken_at: unix_now(),
            processes,
            tcp: self.tcp.clone(),
            listening: self.listening.clone(),
        }
    }

    fn read_sockets(&mut self) {
        if self.last_tcp.is_none_or(|t| t.elapsed() >= TCP_INTERVAL) {
            (self.tcp, self.listening) = tcp_sockets();
            self.last_tcp = Some(Instant::now());
        }
    }

    fn read_pending_cwd_env(&mut self) {
        if self
            .last_cwd_env_read
            .is_some_and(|t| t.elapsed() < CWD_ENV_READ_INTERVAL)
        {
            return;
        }
        let pending: Vec<(u32, u64)> = self
            .known
            .iter()
            .filter(|(_, k)| k.cwd_env_pending)
            .map(|(key, _)| *key)
            .collect();
        if pending.is_empty() {
            return;
        }
        self.last_cwd_env_read = Some(Instant::now());
        for (key, read) in pending.iter().zip(sys::read_cwd_env(&pending)) {
            let Some(known) = self.known.get_mut(key) else { continue };
            known.cwd_env_pending = false;
            if let Some((cwd, env)) = read {
                known.cwd = cwd;
                known.env = env.iter().filter_map(|kv| keep_env(kv, &self.env_keys)).collect();
            }
        }
    }
}

/// The kernel's process name, completed from the executable's file name when the kernel
/// truncated it (15 bytes on Linux, 16 on macOS). Otherwise the kernel's name stands: host and
/// shell matching rely on it, and argv[0] can be rewritten by the process itself.
fn display_name(kernel: String, exe: Option<&str>) -> String {
    let file_name = |path: &str| path.rsplit(['/', '\\']).next().unwrap_or(path).to_string();
    match exe.map(file_name) {
        Some(file) if kernel.len() >= 15 && file.len() > kernel.len() && file.starts_with(kernel.as_str()) => file,
        _ => kernel,
    }
}

fn keep_env(entry: &str, keys: &[String]) -> Option<(String, String)> {
    let (key, value) = entry.split_once('=')?;
    let upper = key.to_ascii_uppercase();
    (keys.contains(&upper) && !secret_var(&upper)).then(|| (key.to_string(), redact_urls(value)))
}

fn secret_var(upper: &str) -> bool {
    SECRET_MARKERS.iter().any(|m| upper.contains(m))
}

/// A flag whose value is a credential: `--api-key`, `--token`, `--password`, plink's `-pw`.
fn secret_flag(flag: &str) -> bool {
    let name: String = flag
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(name.as_str(), "pw" | "pass" | "key" | "auth")
        || [
            "token",
            "secret",
            "password",
            "passwd",
            "apikey",
            "credential",
            "authorization",
        ]
        .iter()
        .any(|m| name.contains(m))
}

/// Masks the value of secret-looking flags (`--api-key x`, `--token=x`), secret-looking
/// `NAME=value` assignments, and passwords in URLs. Flags and everything else stay, so
/// processes are still recognized by their command lines.
fn redact_cmd(cmd: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(cmd.len());
    let mut hide_next = false;
    for arg in cmd {
        if std::mem::take(&mut hide_next) {
            out.push(REDACTED.to_string());
            continue;
        }
        if let Some(flag) = arg.strip_prefix('-') {
            match flag.split_once('=') {
                Some((name, _)) if secret_flag(name) => {
                    out.push(format!("-{name}={REDACTED}"));
                    continue;
                }
                None if secret_flag(flag) => {
                    hide_next = true;
                    out.push(arg);
                    continue;
                }
                _ => {}
            }
        } else if let Some((name, _)) = arg.split_once('=')
            && !name.is_empty()
            && !name.contains(char::is_whitespace)
            && secret_var(&name.to_ascii_uppercase())
        {
            out.push(format!("{name}={REDACTED}"));
            continue;
        }
        out.push(redact_urls(&arg));
    }
    out
}

/// Masks the credentials in every `scheme://user:password@host` of `text`. A user name
/// alone is kept for ssh (`ssh://me@box`) and masked elsewhere, where it usually is a token
/// (`https://ghp_x@github.com`).
fn redact_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| matches!(c, '/' | '?' | '#' | '"' | '\'') || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rsplit_once('@') {
            Some((userinfo, host)) => {
                let scheme = head[..i]
                    .rsplit(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                match userinfo.split_once(':') {
                    Some((user, _)) => out.push_str(&format!("{user}:{REDACTED}")),
                    None if matches!(scheme.as_str(), "ssh" | "sftp" | "scp") => out.push_str(userinfo),
                    None => out.push_str(REDACTED),
                }
                out.push('@');
                out.push_str(host);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Established connections and listening sockets, from one listing.
fn tcp_sockets() -> (Vec<TcpConn>, Vec<Listener>) {
    let Ok(sockets) = get_sockets_info(AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6, ProtocolFlags::TCP) else {
        return (Vec::new(), Vec::new());
    };
    let (mut conns, mut listening) = (Vec::new(), Vec::new());
    for s in sockets {
        let ProtocolSocketInfo::Tcp(tcp) = s.protocol_socket_info else {
            continue;
        };
        for pid in s.associated_pids {
            match tcp.state {
                TcpState::Established => conns.push(TcpConn {
                    pid,
                    local_addr: tcp.local_addr.to_string(),
                    local_port: tcp.local_port,
                    remote_addr: tcp.remote_addr.to_string(),
                    remote_port: tcp.remote_port,
                }),
                TcpState::Listen => listening.push(Listener {
                    pid,
                    addr: tcp.local_addr.to_string(),
                    port: tcp.local_port,
                }),
                _ => {}
            }
        }
    }
    (conns, listening)
}

/// See `sys::stdin_closed`: blocks while a live reader waits on the pipe.
pub(crate) fn stdin_closed(pid: u32) -> Option<bool> {
    sys::stdin_closed(pid)
}

/// Processes owning a visible window (Windows; empty elsewhere).
pub fn windowed_pids() -> std::collections::HashSet<u32> {
    sys::windowed_pids()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_only_completes_truncated_names() {
        let name = |k: &str, exe| display_name(k.to_string(), exe);
        // 15-byte kernel name continued by the executable's name.
        assert_eq!(
            name("codex-code-mode", Some("/x/codex-code-mode-host")),
            "codex-code-mode-host"
        );
        // Short names stand, even when the executable is named differently.
        assert_eq!(name("zsh", Some("/bin/zsh-5.9")), "zsh");
        assert_eq!(name("node.exe", Some("C:\\nodejs\\node.exe")), "node.exe");
    }

    #[test]
    fn keep_env_keeps_only_requested_vars_and_never_secrets() {
        let keys = [
            "ORCA_TAB_ID".to_string(),
            "ANTHROPIC_BASE_URL".into(),
            "ORCA_API_TOKEN".into(),
        ];
        assert_eq!(
            keep_env("orca_tab_id=t1", &keys),
            Some(("orca_tab_id".into(), "t1".into()))
        );
        assert!(keep_env("ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic", &keys).is_some());
        assert_eq!(keep_env("ORCA_API_TOKEN=abc", &keys), None, "asked for, but a secret");
        assert_eq!(keep_env("PATH=C:\\", &keys), None);
        assert_eq!(
            keep_env("ANTHROPIC_BASE_URL=https://me:pw@proxy.local/v1", &keys),
            Some(("ANTHROPIC_BASE_URL".into(), "https://me:***@proxy.local/v1".into()))
        );
    }

    #[test]
    fn redact_cmd_masks_credentials_only() {
        let cmd = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        let r = |s: &str| redact_cmd(cmd(s)).join(" ");
        assert_eq!(
            r("npx -y @upstash/context7-mcp --api-key sk-1 --resume abc"),
            "npx -y @upstash/context7-mcp --api-key *** --resume abc"
        );
        assert_eq!(r("plink -pw s3cret me@box"), "plink -pw *** me@box");
        assert_eq!(r("app --token=abc --no-auth host"), "app --token=*** --no-auth host");
        assert_eq!(r("env GITHUB_TOKEN=abc node x.js"), "env GITHUB_TOKEN=*** node x.js");
        assert_eq!(
            r("git clone https://x-access-token:abc@github.com/o/r https://ghp_1@github.com/o/r"),
            "git clone https://x-access-token:***@github.com/o/r https://***@github.com/o/r"
        );
        assert_eq!(r("ssh ssh://me@box:2200 -p 22"), "ssh ssh://me@box:2200 -p 22");
    }
}
