//! Whether anyone can still talk to a stdio helper (MCP server, language server, REPL): such a
//! process reads requests from a pipe whose writer is the agent that started it. Once every
//! writer is gone, nothing can ever reach it again, whatever its name or whoever started it.
//!
//! Checking blocks while a live reader waits on the pipe, so each check runs on its own small
//! thread and reports when it finishes. A pipe that loses its writer wakes the waiting check at
//! once, so a blocked check is also how a closing pipe is noticed.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::model::{Model, Role};

/// How often a pipe found open is checked again.
const RECHECK_SECS: u64 = 5;

type ProcKey = (u32, u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Checking,
    Open {
        at: u64,
    },
    Closed,
    /// Its input is not a pipe, or cannot be read.
    Unknown,
}

#[derive(Debug, Default)]
pub struct StdioProbe {
    states: Arc<Mutex<HashMap<ProcKey, State>>>,
}

impl StdioProbe {
    /// Checks processes whose parent is gone, the only ones whose pipe can have lost its
    /// writer without them being part of a running tree. Returns those found closed.
    pub fn update(&mut self, model: &Model) -> HashSet<ProcKey> {
        let now = model.taken_at;
        let targets: HashSet<ProcKey> = model
            .procs
            .values()
            .filter(|p| p.ppid.is_some() && !model.parent.contains_key(&p.pid))
            .filter(|p| {
                !matches!(
                    model.roles.get(&p.pid),
                    Some(Role::Agent(_) | Role::Host | Role::System)
                ) && !model.pty_glue.contains(&p.pid)
            })
            .map(|p| (p.pid, p.start_time))
            .collect();
        let mut states = self.states.lock().unwrap();
        // Checks still running for processes that left keep their thread until it returns;
        // their answer is then dropped.
        states.retain(|k, _| targets.contains(k));
        for key in targets {
            let due = match states.get(&key) {
                None => true,
                Some(State::Open { at }) => now.saturating_sub(*at) >= RECHECK_SECS,
                Some(_) => false,
            };
            if due && cfg!(windows) {
                states.insert(key, State::Checking);
                self.spawn(key, now);
            }
        }
        states
            .iter()
            .filter(|(_, s)| **s == State::Closed)
            .map(|(k, _)| *k)
            .collect()
    }

    fn spawn(&self, key: ProcKey, now: u64) {
        let states = Arc::clone(&self.states);
        let check = move || {
            let state = match crate::snapshot::stdin_closed(key.0) {
                Some(true) => State::Closed,
                Some(false) => State::Open { at: now },
                None => State::Unknown,
            };
            if let Some(slot) = states.lock().unwrap().get_mut(&key) {
                *slot = state;
            }
        };
        // A failed spawn leaves the process marked as checking; it is not tried again.
        let _ = std::thread::Builder::new()
            .name("agentlens-stdio".into())
            .stack_size(64 * 1024)
            .spawn(check);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::snapshot::{ProcInfo, Snapshot};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn reports_a_parentless_process_whose_input_lost_its_writer() {
        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        // As the model sees it once its parent is gone.
        let ping = ProcInfo {
            pid: child.id(),
            ppid: Some(4_000_000),
            name: "PING.EXE".into(),
            exe: None,
            cmd: vec!["ping".into()],
            cwd: None,
            env: Vec::new(),
            start_time: 1,
            cpu: 0.0,
            private_bytes: 0,
            working_set: 0,
        };
        let model = |at| {
            Model::build(
                Snapshot {
                    taken_at: at,
                    processes: vec![ping.clone()],
                    tcp: Vec::new(),
                    listening: Vec::new(),
                    cores: 1,
                },
                &Catalog::builtin(),
            )
        };
        let mut probe = StdioProbe::default();
        let wait = |probe: &mut StdioProbe, at: u64| {
            let start = Instant::now();
            loop {
                let closed = probe.update(&model(at));
                if !closed.is_empty() || start.elapsed() > Duration::from_secs(2) {
                    return closed;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let open = wait(&mut probe, 10);
        drop(child.stdin.take());
        let closed = wait(&mut probe, 10 + RECHECK_SECS);
        let _ = child.kill();
        let _ = child.wait();
        assert!(open.is_empty());
        assert_eq!(closed, HashSet::from([(child.id(), 1)]));
    }
}
