//! Activity history and cleanup candidates, each with the evidence behind it.

use std::collections::{HashMap, HashSet};

use crate::model::{HelperKind, Model, OrphanReason, Role, short_path};
use crate::origin::{Origins, OwnerGone, Starter};

/// CPU% (summed over cores) above which a process counts as active in a sample.
const ACTIVE_CPU: f32 = 1.0;

/// Start of the IANA dynamic port range.
const DYNAMIC_PORTS: u16 = 49152;

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

/// What decides which candidates are proposed.
#[derive(Debug, Clone)]
pub struct Policy {
    /// A session idle this long is listed.
    pub idle_minutes: u64,
    /// How long an owner must have been gone (`origin::OWNER_GONE_GRACE_SECS`; 0 for one-shot
    /// reports, which see a single moment).
    pub owner_gone_grace: u64,
    /// Propose leftovers that show a window, such as a browser an agent opened.
    pub end_windowed: bool,
    /// Propose leftovers that a live session is still connected to, such as a shared daemon.
    pub end_shared: bool,
}

/// `windowed` lists processes with a visible window; it is called only when a proposed orphan
/// could be one of them.
pub fn candidates(
    model: &Model,
    history: &History,
    origins: &Origins,
    policy: &Policy,
    windowed: impl FnOnce() -> HashSet<u32>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    let gone = |pid: u32| origins.owner_gone(pid, model.taken_at, policy.owner_gone_grace);

    for orphan in &model.orphans {
        let mut reasons = vec![match orphan.reason {
            OrphanReason::ParentGone => "parent-gone".to_string(),
            OrphanReason::PidReused => "parent-pid-reused".to_string(),
            OrphanReason::Reparented => "adopted-by-init".to_string(),
        }];
        let owner = gone(orphan.pid);
        let stdio = owner.is_none() && origins.stdio_closed(model, orphan.pid, policy.owner_gone_grace);
        if let Some(owner) = owner {
            owner_reasons(owner, &mut reasons);
        } else if stdio {
            reasons.push("stdio-closed".into());
        } else if !orphan.agent_related {
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
        let eligible = orphan.agent_related || owner.is_some() || stdio;
        out.push(orphan_candidate(model, history, orphan.pid, reasons, eligible));
    }

    // Leftovers of ended sessions that the snapshot alone does not show as orphans: their
    // parent is still there (a wrapper, a host), or they are not runtimes.
    let mut covered: HashSet<u32> = model.orphans.iter().flat_map(|o| model.subtree(o.pid)).collect();
    for pid in origins.gone_roots(model, policy.owner_gone_grace) {
        if let (false, Some(owner)) = (covered.contains(&pid), gone(pid)) {
            let mut reasons = Vec::new();
            owner_reasons(owner, &mut reasons);
            out.push(orphan_candidate(model, history, pid, reasons, true));
            covered.extend(model.subtree(pid));
        }
    }
    // Parentless programs nobody can reach any more, with nothing else saying an agent started
    // them: shown, not proposed.
    for pid in origins.stdio_closed_pids(model, policy.owner_gone_grace) {
        if !covered.contains(&pid) {
            let reasons = vec!["stdio-closed".to_string(), "not-agent".to_string()];
            out.push(orphan_candidate(model, history, pid, reasons, false));
        }
    }

    keep_in_use(model, policy, windowed, &mut out);

    for group in &model.groups {
        for tab in &group.tabs {
            for session in &tab.sessions {
                let tree = model.subtree(session.root_pid);
                let idle = history.idle_secs(model, &tree);
                if idle >= policy.idle_minutes * 60 {
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

fn owner_reasons(owner: &OwnerGone, reasons: &mut Vec<String>) {
    reasons.push("owner-gone".into());
    reasons.push(format!("from session {}", owner.origin));
    match owner.starter {
        Some(Starter::Harness) => reasons.push("agent-infra".into()),
        Some(Starter::Command) => reasons.push("agent-command".into()),
        None => {}
    }
}

/// An orphan row: `reasons` first, then age, idleness and the ports its tree listens on.
fn orphan_candidate(model: &Model, history: &History, pid: u32, mut reasons: Vec<String>, eligible: bool) -> Candidate {
    let p = &model.procs[&pid];
    let tree = model.subtree(pid);
    reasons.push(format!(
        "age {}",
        fmt_duration(model.taken_at.saturating_sub(p.start_time))
    ));
    let idle = history.idle_secs(model, &tree);
    if idle >= 60 {
        reasons.push(format!("idle {}", fmt_duration(idle)));
    }
    let mut ports: Vec<&crate::snapshot::Listener> = model.listening.iter().filter(|l| tree.contains(&l.pid)).collect();
    ports.sort_by_key(|l| l.port);
    if !ports.is_empty() {
        // Ports in the dynamic range are usually picked by the OS for a program's own
        // workers; name them only when there is nothing else.
        let named: Vec<&&crate::snapshot::Listener> = ports.iter().filter(|l| l.port < DYNAMIC_PORTS).collect();
        let shown = if named.is_empty() {
            ports.iter().collect()
        } else {
            named
        };
        let mut list: Vec<String> = shown.iter().map(|l| format!(":{}", l.port)).collect();
        list.dedup();
        reasons.push(format!("listening {}", list.join(" ")));
        if ports.iter().any(|l| crate::net::is_exposed(&l.addr)) {
            reasons.push("exposed".into());
        }
    }
    Candidate {
        kind: CandidateKind::Orphan,
        root_pid: pid,
        label: format!("{} {}", p.name, short_cmd(p)),
        reasons,
        private_bytes: model.private_bytes(&tree),
        eligible,
    }
}

/// Withdraws the proposal for orphans the user may still be using, unless the policy says to
/// end them anyway: trees with a visible window, and trees a live session is connected to.
fn keep_in_use(model: &Model, policy: &Policy, windowed: impl FnOnce() -> HashSet<u32>, out: &mut [Candidate]) {
    let proposed = |c: &Candidate| c.kind == CandidateKind::Orphan && c.eligible;
    if !out.iter().any(proposed) {
        return;
    }
    let windowed = if policy.end_windowed {
        HashSet::new()
    } else {
        windowed()
    };
    // Each connection end held by a live session's processes -> that session's label.
    let mut sessions: HashMap<(&str, u16, u16), String> = HashMap::new();
    if !policy.end_shared {
        for g in &model.groups {
            for t in &g.tabs {
                for s in &t.sessions {
                    let label = format!("{} @ {} / {}", s.agent_name, g.name, t.label);
                    let tree = model.subtree(s.root_pid);
                    for c in model.tcp.iter().filter(|c| tree.contains(&c.pid)) {
                        sessions.insert((c.local_addr.as_str(), c.local_port, c.remote_port), label.clone());
                    }
                }
            }
        }
    }
    for c in out.iter_mut().filter(|c| proposed(c)) {
        let tree = model.subtree(c.root_pid);
        if tree.iter().any(|p| windowed.contains(p)) {
            c.eligible = false;
            c.reasons.push("has-window".into());
        }
        // The other end of one of the tree's connections is held by a live session.
        let user = model
            .tcp
            .iter()
            .filter(|t| tree.contains(&t.pid))
            .find_map(|t| sessions.get(&(t.remote_addr.as_str(), t.remote_port, t.local_port)));
        if let Some(label) = user {
            c.eligible = false;
            c.reasons.push(format!("in-use by {label}"));
        }
    }
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
