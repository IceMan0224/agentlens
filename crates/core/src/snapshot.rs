use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use netstat2::{AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState, get_sockets_info};
use serde::{Deserialize, Serialize};

use crate::sys;

/// Variables whose names look like credentials are dropped even when asked for, so snapshots
/// never carry secrets.
const SECRET_MARKERS: &[&str] = &["TOKEN", "KEY", "SECRET", "PASSWORD", "AUTH"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Unix seconds.
    pub taken_at: u64,
    pub processes: Vec<ProcInfo>,
    pub tcp: Vec<TcpConn>,
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

/// An established TCP connection; listening and closing sockets are not collected.
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
            last_tcp: None,
        }
    }

    /// Switches to a new set of variables; details are read again so every process has them.
    pub fn set_env_keys(&mut self, env_keys: Vec<String>) {
        self.env_keys = env_keys.iter().map(|k| k.to_ascii_uppercase()).collect();
        self.known.clear();
        self.last_cwd_env_read = None;
    }

    /// Makes the next snapshot list connections again instead of reusing the last list.
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
                    cmd: d.cmd,
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

        Snapshot {
            taken_at: unix_now(),
            processes,
            tcp: self.connections(),
        }
    }

    fn connections(&mut self) -> Vec<TcpConn> {
        if self.last_tcp.is_none_or(|t| t.elapsed() >= TCP_INTERVAL) {
            self.tcp = tcp_connections();
            self.last_tcp = Some(Instant::now());
        }
        self.tcp.clone()
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
    let secret = SECRET_MARKERS.iter().any(|m| upper.contains(m));
    (keys.contains(&upper) && !secret).then(|| (key.to_string(), value.to_string()))
}

fn tcp_connections() -> Vec<TcpConn> {
    let Ok(sockets) = get_sockets_info(AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6, ProtocolFlags::TCP) else {
        return Vec::new();
    };
    sockets
        .into_iter()
        .filter_map(|s| match s.protocol_socket_info {
            ProtocolSocketInfo::Tcp(tcp) if tcp.state == TcpState::Established => Some((tcp, s.associated_pids)),
            _ => None,
        })
        .flat_map(|(tcp, pids)| {
            pids.into_iter().map(move |pid| TcpConn {
                pid,
                local_addr: tcp.local_addr.to_string(),
                local_port: tcp.local_port,
                remote_addr: tcp.remote_addr.to_string(),
                remote_port: tcp.remote_port,
            })
        })
        .collect()
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
    }
}
