//! What the dashboard answers agents that ask through `agentlens mcp`. Answers are in English,
//! for the agent, whatever language the dashboard shows.

use agentlens_core::actions;
use agentlens_core::catalog::Catalog;
use agentlens_core::health::{Candidate, CandidateKind, short_cmd};
use agentlens_core::model::Model;

use crate::report::mb;

/// The session the asking server runs under: its root and a label.
pub fn session_of(m: &Model, caller: u32) -> Option<(u32, String)> {
    let (gi, ti, si) = m.locate(caller)?;
    let (g, t) = (&m.groups[gi], &m.groups[gi].tabs[ti]);
    let s = &t.sessions[si];
    Some((s.root_pid, format!("{} @ {} / {}", s.agent_name, g.name, t.label)))
}

fn kind(c: &Candidate) -> &'static str {
    match c.kind {
        CandidateKind::Orphan => "leftover",
        CandidateKind::IdleSession => "idle session",
        CandidateKind::DuplicateMcp => "duplicate MCP server",
    }
}

pub fn leftovers(m: &Model, candidates: &[Candidate]) -> String {
    if candidates.is_empty() {
        return "No cleanup candidates: nothing that ended sessions left behind was found.".into();
    }
    let proposed = candidates.iter().filter(|c| c.eligible).count();
    let mut out = format!(
        "{} cleanup candidate(s), {proposed} proposed. end_leftovers without pids ends the \
         proposed ones; the others are ended only when named by pid.\n",
        candidates.len()
    );
    for c in candidates {
        let procs = m.subtree(c.root_pid).len();
        let default = if c.eligible { "proposed" } else { "kept by default" };
        out.push_str(&format!(
            "- pid {} {} ({procs} process(es), {}): {}, {default}; {}\n",
            c.root_pid,
            c.label,
            mb(c.private_bytes),
            kind(c),
            c.reasons.join(", ")
        ));
    }
    out
}

pub fn session_processes(m: &Model, caller: u32) -> Result<String, String> {
    let (root, label) = session_of(m, caller).ok_or_else(|| {
        format!(
            "The session of this MCP server (pid {caller}) is not in the dashboard; its agent may not be recognized."
        )
    })?;
    let pids = m.subtree(root);
    let mut out = format!("{label}: {} process(es), {}\n", pids.len(), mb(m.private_bytes(&pids)));
    for pid in pids {
        let Some(p) = m.procs.get(&pid) else { continue };
        let depth = match pid == root {
            true => 0,
            false => m.ancestors(pid).iter().take_while(|a| **a != root).count() + 1,
        };
        let ports: Vec<String> = m
            .listening
            .iter()
            .filter(|l| l.pid == pid)
            .map(|l| format!("{}:{}", l.addr, l.port))
            .collect();
        let mut line = format!("{}- {pid} {} {}", "  ".repeat(depth), p.name, mb(p.private_bytes));
        let cmd = short_cmd(p);
        if !cmd.is_empty() {
            line.push_str(&format!(" `{cmd}`"));
        }
        if !ports.is_empty() {
            line.push_str(&format!(" listening {}", ports.join(" ")));
        }
        if pid == caller {
            line.push_str(" (this MCP server)");
        }
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

/// Roots of the candidate trees to end for `pids` (none: the proposed ones), and notes on what
/// is left alone. Only current candidates qualify, never the session that asks, and the
/// dashboard's own refusals (hosts, system, protect rules, itself) apply.
pub fn end_roots(
    m: &Model,
    catalog: &Catalog,
    candidates: &[Candidate],
    self_pid: u32,
    caller: u32,
    pids: Option<&[u32]>,
) -> (Vec<u32>, Vec<String>) {
    let mut notes = Vec::new();
    let chosen: Vec<&Candidate> = match pids {
        None => candidates.iter().filter(|c| c.eligible).collect(),
        Some(pids) => pids
            .iter()
            .filter_map(|pid| {
                let found = candidates.iter().find(|c| c.root_pid == *pid);
                if found.is_none() {
                    notes.push(format!(
                        "pid {pid} is not a cleanup candidate (list_leftovers shows them)"
                    ));
                }
                found
            })
            .collect(),
    };
    let mut roots = Vec::new();
    for c in chosen {
        let plan = actions::plan(m, &[c.root_pid]);
        if plan.targets.iter().any(|t| m.is_ancestor_or_self(t.pid, caller)) {
            notes.push(format!("pid {} runs the session that asked; left alone", c.root_pid));
            continue;
        }
        let refusals = actions::refusals(&plan, m, catalog, self_pid);
        if refusals.is_empty() {
            roots.push(c.root_pid);
        } else {
            notes.push(format!("pid {} refused: {}", c.root_pid, refusals.join("; ")));
        }
    }
    (roots, notes)
}

#[cfg(test)]
mod tests {
    use agentlens_core::Snapshot;
    use agentlens_core::health::{History, Policy, candidates};
    use agentlens_core::origin::Origins;

    use super::*;

    fn fixture() -> (Model, Catalog, Vec<Candidate>) {
        let snapshot: Snapshot = serde_json::from_str(include_str!("../../core/tests/fixtures/sample.json")).unwrap();
        let catalog = Catalog::builtin();
        let m = Model::build(snapshot, &catalog);
        let mut history = History::default();
        history.update(&m);
        let mut origins = Origins::default();
        origins.update(&m, &catalog);
        let policy = Policy {
            idle_minutes: 30,
            owner_gone_grace: 0,
            end_windowed: false,
            end_shared: false,
        };
        let found = candidates(&m, &history, &origins, &policy, Default::default);
        (m, catalog, found)
    }

    /// A process of the claude session at 33472, as an MCP server under it would be.
    fn caller(m: &Model) -> u32 {
        m.subtree(33472)[1]
    }

    #[test]
    fn leftovers_show_evidence_and_which_are_proposed() {
        let (m, _, found) = fixture();
        let text = leftovers(&m, &found);
        assert!(text.contains("pid 25480"), "{text}");
        let line = text.lines().find(|l| l.starts_with("- pid 25480")).unwrap();
        assert!(line.contains("proposed;") && line.contains("owner-gone"), "{text}");
        assert!(text.contains("kept by default"), "{text}");
    }

    #[test]
    fn the_asking_session_is_found_from_its_server() {
        let (m, _, _) = fixture();
        let caller = caller(&m);
        let text = session_processes(&m, caller).unwrap();
        assert!(text.starts_with("Claude Code @ "), "{text}");
        assert!(text.contains("(this MCP server)"), "{text}");
        assert!(
            session_processes(&m, 25480).is_err(),
            "a leftover belongs to no session"
        );
    }

    #[test]
    fn ending_takes_candidates_only_and_never_the_asking_session() {
        let (m, catalog, found) = fixture();
        let caller = caller(&m);
        let (roots, notes) = end_roots(&m, &catalog, &found, 1, caller, None);
        assert_eq!(roots, [25480], "the proposed ones: {notes:?}");
        // A live session's process is not a candidate, whatever the agent asks.
        let (roots, notes) = end_roots(&m, &catalog, &found, 1, caller, Some(&[33472]));
        assert!(roots.is_empty());
        assert!(notes[0].contains("not a cleanup candidate"), "{notes:?}");
        // A candidate holding the asking server is refused even when named.
        let (roots, notes) = end_roots(&m, &catalog, &found, 1, 25480, Some(&[25480]));
        assert!(roots.is_empty());
        assert!(notes[0].contains("runs the session that asked"), "{notes:?}");
    }
}
