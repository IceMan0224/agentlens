//! Classification of a made-up snapshot shaped like a real one: 11 Orca tabs (claude, codex,
//! opencode), a claude run by the ego browser, a dev server an agent left running, and an MCP
//! server tree whose session was closed.

use agentlens_core::Snapshot;
use agentlens_core::catalog::Catalog;
use agentlens_core::health::{Candidate, CandidateKind, History, Policy, candidates};
use agentlens_core::model::{GroupKind, HelperKind, Model, OrphanReason, Role};
use agentlens_core::origin::Origins;
use agentlens_core::rules::Rules;

fn model() -> Model {
    let text = include_str!("fixtures/sample.json");
    let snapshot: Snapshot = serde_json::from_str(text).unwrap();
    Model::build(snapshot, &Catalog::builtin())
}

fn session_count(model: &Model, host: &str, agent: &str) -> usize {
    model
        .groups
        .iter()
        .filter(|g| g.kind == GroupKind::Known(host.into()))
        .flat_map(|g| &g.tabs)
        .flat_map(|t| &t.sessions)
        .filter(|s| s.agent_id == agent)
        .count()
}

#[test]
fn groups_sessions_by_host_and_orca_tab() {
    let m = model();
    let orca = m
        .groups
        .iter()
        .find(|g| g.kind == GroupKind::Known("orca".into()))
        .unwrap();
    assert_eq!(orca.tabs.len(), 11);
    assert_eq!(orca.host_pids.len(), 8);
    assert_eq!(session_count(&m, "orca", "claude"), 5);
    assert_eq!(session_count(&m, "orca", "codex"), 4);
    assert_eq!(session_count(&m, "orca", "opencode"), 2);
    assert_eq!(session_count(&m, "ego", "claude"), 1);
    assert!(orca.tabs.iter().all(|t| t.key.starts_with("orca:")));

    let tab = orca.tabs.iter().find(|t| t.label == "acme/web-app").unwrap();
    assert_eq!(tab.sessions[0].root_pid, 33472);
    assert_eq!(
        tab.sessions[0].session_id.as_deref(),
        Some("849478ea-950d-559b-8007-9eea0990a450")
    );
}

#[test]
fn launcher_and_nested_agents_stay_in_one_session() {
    let m = model();
    // node codex.js (28624) -> codex.exe (12040) -> ... -> codex.exe app-server (41248)
    let (g, t, s) = m.locate(41248).unwrap();
    assert_eq!(m.groups[g].tabs[t].sessions[s].root_pid, 28624);
    assert_eq!(m.locate(12040), Some((g, t, s)));
}

#[test]
fn helpers_are_classified() {
    let m = model();
    assert_eq!(m.roles[&26032], Role::Helper(HelperKind::Repl));
    assert_eq!(m.roles[&43044], Role::Helper(HelperKind::Mcp)); // rea.mjs mcp under opencode
    // Inside codex's package but neither a REPL nor named as a sandbox: a plain tool.
    assert_eq!(m.roles[&22488], Role::Helper(HelperKind::Tool)); // codex-code-mode-host.exe
    assert!(matches!(m.roles[&12040], Role::Agent(_)));
}

#[test]
fn programs_started_by_agents_are_shown_and_closed_sessions_leftovers_proposed() {
    let m = model();
    let dev = m
        .orphans
        .iter()
        .find(|o| o.pid == 35728)
        .expect("dev-server.mjs is reported");
    assert_eq!(dev.reason, OrphanReason::ParentGone);
    assert!(!dev.agent_related);
    assert!(dev.launched_by_agent);
    assert_eq!(dev.origin.as_deref(), Some("demo/game"));

    let mut history = History::default();
    history.update(&m);
    let found = found(&m, &history);
    let orphans: Vec<_> = found.iter().filter(|c| c.kind == CandidateKind::Orphan).collect();
    let dev = orphans.iter().find(|c| c.root_pid == 35728).unwrap();
    assert!(!dev.eligible, "an agent started it, but nothing says its session ended");
    // Its environment names a Claude Code session that is no longer running. A report sees one
    // moment, so it does not wait for the owner to stay gone.
    let mut origins = Origins::default();
    origins.update(&m, &Catalog::builtin());
    let policy = Policy {
        owner_gone_grace: 0,
        ..Rules::default().policy()
    };
    let report = candidates(&m, &history, &origins, &policy, Default::default);
    let mcp = report.iter().find(|c| c.root_pid == 25480).unwrap();
    assert!(
        mcp.eligible && mcp.reasons.iter().any(|r| r == "owner-gone"),
        "{:?}",
        mcp.reasons
    );
    // Rancher Desktop also inherited Orca's environment but is not a runtime: not reported.
    assert!(!m.orphans.iter().any(|o| m.procs[&o.pid].name.starts_with("Rancher")));
}

#[test]
fn duplicate_mcp_servers_are_reported() {
    let m = model();
    let mut history = History::default();
    history.update(&m);
    let dup = found(&m, &history)
        .into_iter()
        .find(|c| c.kind == CandidateKind::DuplicateMcp)
        .unwrap();
    assert!(dup.label.contains("rea.mjs"));
    assert!(!dup.eligible);
}

#[test]
fn script_agents_and_providers_are_detected() {
    let mut snapshot: Snapshot = serde_json::from_str(include_str!("fixtures/sample.json")).unwrap();
    // Reuse a codex launcher's place in the tree for a zcode and a GLM-backed claude.
    let launcher = snapshot.processes.iter_mut().find(|p| p.pid == 42436).unwrap();
    launcher.cmd = vec![
        "C:\\Program Files\\nodejs\\node.exe".into(),
        "C:\\Users\\u\\AppData\\Roaming\\npm\\node_modules\\@zcode\\cli\\dist\\zcode.cjs".into(),
    ];
    let claude = snapshot.processes.iter_mut().find(|p| p.pid == 33868).unwrap();
    claude
        .env
        .push(("ANTHROPIC_BASE_URL".into(), "https://api.z.ai/api/anthropic".into()));

    let m = Model::build(snapshot, &Catalog::builtin());
    let find = |pid| {
        m.groups
            .iter()
            .flat_map(|g| &g.tabs)
            .flat_map(|t| &t.sessions)
            .find(|s| s.root_pid == pid)
            .unwrap()
    };
    assert_eq!(find(42436).agent_id, "zcode");
    assert_eq!(find(42436).provider.as_deref(), Some("GLM"));
    assert_eq!(find(33868).agent_id, "claude");
    assert_eq!(find(33868).provider.as_deref(), Some("GLM"));
}

/// Candidates with default rules and no lineage: what a fresh dashboard shows.
fn found(m: &Model, history: &History) -> Vec<Candidate> {
    let mut origins = Origins::default();
    origins.update(m, &Catalog::builtin());
    candidates(m, history, &origins, &Rules::default().policy(), Default::default)
}
