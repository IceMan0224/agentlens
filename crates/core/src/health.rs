//! Activity history and cleanup candidates, each with the evidence behind it.

use std::collections::HashMap;

use crate::model::{HelperKind, Model, OrphanReason, Role, short_path};

/// CPU% (summed over cores) above which a process counts as active in a sample.
const ACTIVE_CPU: f32 = 1.0;

/// Processes are keyed by (pid, start time) so a reused PID starts a fresh history.
#[derive(Debug, Default)]
pub struct History {
    first_seen: HashMap<(u32, u64), u64>,
    last_active: HashMap<(u32, u64), u64>,
}

impl History {
    pub fn update(&mut self, model: &Model) {
        let now = model.taken_at;
        for p in model.procs.values() {
            let key = (p.pid, p.start_time);
            self.first_seen.entry(key).or_insert(now);
            if p.cpu >= ACTIVE_CPU {
                self.last_active.insert(key, now);
            }
        }
        self.first_seen
            .retain(|(pid, start), _| model.procs.get(pid).is_some_and(|p| p.start_time == *start));
        self.last_active.retain(|key, _| self.first_seen.contains_key(key));
    }

    /// Seconds since any of `pids` was last active. Processes never seen active count from
    /// when monitoring first saw them, so this is a lower bound right after startup.
    pub fn idle_secs(&self, model: &Model, pids: &[u32]) -> u64 {
        let latest = pids
            .iter()
            .filter_map(|pid| model.procs.get(pid))
            .filter_map(|p| {
                let key = (p.pid, p.start_time);
                self.last_active
                    .get(&key)
                    .or_else(|| self.first_seen.get(&key))
                    .copied()
            })
            .max()
            .unwrap_or(model.taken_at);
        model.taken_at.saturating_sub(latest)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Orphan,
    IdleSession,
    DuplicateMcp,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub kind: CandidateKind,
    /// Root of the process tree a cleanup would end.
    pub root_pid: u32,
    pub label: String,
    pub reasons: Vec<String>,
    pub private_bytes: u64,
    /// Proposed for cleanup by default; others are informational and need a deliberate choice.
    pub eligible: bool,
}

pub fn fmt_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s => format!("{}d{}h", s / 86_400, s % 86_400 / 3600),
    }
}

pub fn candidates(model: &Model, history: &History, idle_minutes: u64) -> Vec<Candidate> {
    let mut out = Vec::new();

    for orphan in &model.orphans {
        let p = &model.procs[&orphan.pid];
        let tree = model.subtree(orphan.pid);
        let mut reasons = vec![match orphan.reason {
            OrphanReason::ParentGone => "parent-gone".to_string(),
            OrphanReason::PidReused => "parent-pid-reused".to_string(),
            OrphanReason::Reparented => "adopted-by-init".to_string(),
        }];
        reasons.push(format!(
            "age {}",
            fmt_duration(model.taken_at.saturating_sub(p.start_time))
        ));
        let idle = history.idle_secs(model, &tree);
        if idle >= 60 {
            reasons.push(format!("idle {}", fmt_duration(idle)));
        }
        if !orphan.agent_related {
            reasons.push(
                if orphan.launched_by_agent {
                    "started-by-agent"
                } else {
                    "not-agent"
                }
                .into(),
            );
        }
        if let Some(tab) = &orphan.origin {
            reasons.push(format!("from tab {tab}"));
        }
        out.push(Candidate {
            kind: CandidateKind::Orphan,
            root_pid: orphan.pid,
            label: format!("{} {}", p.name, short_cmd(p)),
            reasons,
            private_bytes: model.private_bytes(&tree),
            eligible: orphan.agent_related,
        });
    }

    for group in &model.groups {
        for tab in &group.tabs {
            for session in &tab.sessions {
                let tree = model.subtree(session.root_pid);
                let idle = history.idle_secs(model, &tree);
                if idle >= idle_minutes * 60 {
                    out.push(Candidate {
                        kind: CandidateKind::IdleSession,
                        root_pid: session.root_pid,
                        label: format!("{} @ {} / {}", session.agent_name, group.name, tab.label),
                        reasons: vec![format!("idle {}", fmt_duration(idle))],
                        private_bytes: model.private_bytes(&tree),
                        eligible: false,
                    });
                }
            }
        }
    }

    // Same MCP server command running under several sessions.
    let mut mcp: HashMap<String, Vec<u32>> = HashMap::new();
    for (pid, role) in &model.roles {
        if *role == Role::Helper(HelperKind::Mcp) {
            let p = &model.procs[pid];
            let args: Vec<String> = p.cmd.iter().skip(1).map(|a| a.to_lowercase()).collect();
            mcp.entry(format!("{} {}", crate::model::stem(p), args.join(" ")))
                .or_default()
                .push(*pid);
        }
    }
    // Sorted inside and out, so the rows keep their order from one refresh to the next.
    let mut dups: Vec<_> = mcp
        .into_values()
        .filter(|pids| pids.len() > 1)
        .map(|mut pids| {
            pids.sort_unstable();
            pids
        })
        .collect();
    dups.sort();
    for pids in dups {
        let sessions: std::collections::HashSet<_> = pids.iter().filter_map(|p| model.locate(*p)).collect();
        let p = &model.procs[&pids[0]];
        out.push(Candidate {
            kind: CandidateKind::DuplicateMcp,
            root_pid: pids[0],
            label: format!("{}x {}", pids.len(), short_cmd(p)),
            reasons: vec![format!("{} sessions", sessions.len()), format!("pids {pids:?}")],
            private_bytes: model.private_bytes(&pids),
            eligible: false,
        });
    }

    out
}

/// The script being run plus a few of its arguments, else the first arguments.
pub fn short_cmd(p: &crate::snapshot::ProcInfo) -> String {
    let args: Vec<&String> = p.cmd.iter().skip(1).collect();
    let script = args
        .iter()
        .position(|a| !a.starts_with('-') && (a.contains('/') || a.contains('\\')));
    let text = match script {
        Some(i) => std::iter::once(short_path(args[i], 2))
            .chain(args[i + 1..].iter().take(3).map(|a| a.to_string()))
            .collect::<Vec<_>>()
            .join(" "),
        None => args.iter().take(2).map(|a| a.as_str()).collect::<Vec<_>>().join(" "),
    };
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(60) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}
