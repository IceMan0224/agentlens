//! Leftovers of ended agent sessions: found from what the dashboard saw (lineage) and from what
//! agents write into the environment of everything they start (birth record).

use std::collections::HashSet;

use agentlens_core::catalog::Catalog;
use agentlens_core::health::{Candidate, CandidateKind, History, Policy, candidates};
use agentlens_core::model::Model;
use agentlens_core::origin::{OWNER_GONE_GRACE_SECS, Origins};
use agentlens_core::{ProcInfo, Snapshot, TcpConn};

const T0: u64 = 1_800_000_000;

fn proc(pid: u32, ppid: u32, exe: &str, cmd: &[&str], env: &[(&str, &str)]) -> ProcInfo {
    ProcInfo {
        pid,
        ppid: Some(ppid).filter(|p| *p != 0),
        name: exe.rsplit('\\').next().unwrap().to_string(),
        exe: Some(exe.to_string()),
        cmd: cmd.iter().map(|s| s.to_string()).collect(),
        cwd: Some("C:\\work\\app".into()),
        env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        start_time: T0 - 1000 + pid as u64,
        cpu: 0.0,
        private_bytes: 10 << 20,
        working_set: 20 << 20,
    }
}

fn model(at: u64, processes: Vec<ProcInfo>, tcp: Vec<TcpConn>) -> Model {
    Model::build(
        Snapshot {
            taken_at: at,
            processes,
            tcp,
            listening: Vec::new(),
        },
        &Catalog::builtin(),
    )
}

fn policy() -> Policy {
    Policy {
        idle_minutes: 30,
        owner_gone_grace: OWNER_GONE_GRACE_SECS,
        end_windowed: false,
        end_shared: false,
    }
}

fn orphans(m: &Model, origins: &Origins, policy: &Policy, windowed: HashSet<u32>) -> Vec<Candidate> {
    candidates(m, &History::default(), origins, policy, || windowed)
        .into_iter()
        .filter(|c| c.kind == CandidateKind::Orphan)
        .collect()
}

fn find(found: &[Candidate], pid: u32) -> Option<&Candidate> {
    found.iter().find(|c| c.root_pid == pid)
}

const TERMINAL: &str = "C:\\Program Files\\WindowsApps\\WindowsTerminal.exe";
const SHELL: &str = "C:\\Program Files\\PowerShell\\7\\pwsh.exe";
const CLAUDE: &str = "C:\\Users\\me\\.local\\bin\\claude.exe";
const BASH: &str = "C:\\Program Files\\Git\\bin\\bash.exe";
const NODE: &str = "C:\\Program Files\\nodejs\\node.exe";

/// A terminal tab running Claude Code, which runs a native MCP server and a dev server.
fn session(env_mcp: &[(&str, &str)], env_cmd: &[(&str, &str)]) -> Vec<ProcInfo> {
    vec![
        proc(10, 0, TERMINAL, &["WindowsTerminal.exe"], &[]),
        proc(11, 10, SHELL, &["pwsh"], &[]),
        proc(12, 11, CLAUDE, &["claude"], &[]),
        // Not a runtime and no `mcp` runtime: the snapshot alone never lists it.
        proc(
            13,
            12,
            "C:\\tools\\github-server.exe",
            &["github-server", "stdio"],
            env_mcp,
        ),
        proc(14, 12, BASH, &["bash", "-c", "npm run dev"], env_cmd),
        proc(15, 14, NODE, &["node", "C:\\work\\app\\dev-server.mjs"], env_cmd),
    ]
}

/// The same tab after Claude Code was killed: its children stay.
fn without(processes: &[ProcInfo], gone: &[u32]) -> Vec<ProcInfo> {
    processes.iter().filter(|p| !gone.contains(&p.pid)).cloned().collect()
}

#[test]
fn lineage_proposes_what_an_ended_session_left_after_the_grace() {
    let live = session(&[], &[]);
    let mut origins = Origins::default();
    origins.update(&model(T0, live.clone(), Vec::new()), &Catalog::builtin());

    let left = without(&live, &[12]);
    let early = model(T0 + 10, left.clone(), Vec::new());
    origins.update(&early, &Catalog::builtin());
    assert!(
        find(&orphans(&early, &origins, &policy(), HashSet::new()), 13).is_none(),
        "grace"
    );

    let later = model(T0 + 10 + OWNER_GONE_GRACE_SECS, left, Vec::new());
    origins.update(&later, &Catalog::builtin());
    let found = orphans(&later, &origins, &policy(), HashSet::new());
    let mcp = find(&found, 13).expect("native MCP server");
    assert!(mcp.eligible, "{:?}", mcp.reasons);
    assert!(mcp.reasons.iter().any(|r| r == "owner-gone"));
    assert!(mcp.reasons.iter().any(|r| r.starts_with("from session Claude Code @")));
    // The dev server's shell is the root of its tree; both are proposed (decision 1).
    let dev = find(&found, 14).expect("dev server tree");
    assert!(dev.eligible && find(&found, 15).is_none());
}

#[test]
fn children_that_exit_with_their_session_are_never_listed() {
    let live = session(&[], &[]);
    let mut origins = Origins::default();
    origins.update(&model(T0, live.clone(), Vec::new()), &Catalog::builtin());
    let after = model(T0 + 60, without(&live, &[12, 13, 14, 15]), Vec::new());
    origins.update(&after, &Catalog::builtin());
    assert!(orphans(&after, &origins, &policy(), HashSet::new()).is_empty());
}

#[test]
fn a_reused_pid_does_not_inherit_the_old_lineage() {
    let live = session(&[], &[]);
    let mut origins = Origins::default();
    origins.update(&model(T0, live.clone(), Vec::new()), &Catalog::builtin());
    // PID 13 now belongs to an unrelated program started later.
    let mut left = without(&live, &[12, 13]);
    let mut other = proc(13, 0, "C:\\tools\\github-server.exe", &["github-server"], &[]);
    other.start_time = T0 + 5;
    left.push(other);
    for at in [T0 + 10, T0 + 40] {
        let m = model(at, left.clone(), Vec::new());
        origins.update(&m, &Catalog::builtin());
        assert!(find(&orphans(&m, &origins, &policy(), HashSet::new()), 13).is_none());
    }
}

#[test]
fn a_process_moved_out_of_a_live_session_is_not_a_leftover() {
    // `nohup server &` from a session that keeps running.
    let live = session(&[], &[]);
    let mut origins = Origins::default();
    origins.update(&model(T0, live.clone(), Vec::new()), &Catalog::builtin());
    let mut moved = without(&live, &[14]);
    moved.iter_mut().find(|p| p.pid == 15).unwrap().ppid = Some(1);
    let m = model(T0 + 60, moved, Vec::new());
    origins.update(&m, &Catalog::builtin());
    assert!(origins.owner_gone(15, m.taken_at, 0).is_none());
}

const SESSION: &str = "0164dc07-376e-47e0-af74-26534eb48fa6";

/// What Claude Code sets: its own machinery gets the session, commands also get its PID.
type Env = Vec<(&'static str, &'static str)>;

fn claude_env() -> (Env, Env) {
    (
        vec![
            ("AI_AGENT", "claude-code_2-1-312_harness"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_SESSION_ID", SESSION),
        ],
        vec![
            ("AI_AGENT", "claude-code_2-1-312_agent"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_SESSION_ID", SESSION),
            ("CLAUDE_PID", "12"),
        ],
    )
}

#[test]
fn birth_record_finds_leftovers_from_before_the_dashboard_started() {
    let (mcp_env, cmd_env) = claude_env();
    let left = without(&session(&mcp_env, &cmd_env), &[12]);
    // A fresh dashboard: nothing was seen under the session.
    let mut origins = Origins::default();
    let first = model(T0, left.clone(), Vec::new());
    origins.update(&first, &Catalog::builtin());
    assert!(
        orphans(&first, &origins, &policy(), HashSet::new())
            .iter()
            .all(|c| !c.eligible || c.root_pid != 13)
    );

    let m = model(T0 + OWNER_GONE_GRACE_SECS, left, Vec::new());
    origins.update(&m, &Catalog::builtin());
    let found = orphans(&m, &origins, &policy(), HashSet::new());
    let mcp = find(&found, 13).expect("MCP server named by its session");
    assert!(mcp.eligible);
    assert!(mcp.reasons.iter().any(|r| r == "agent-infra"), "{:?}", mcp.reasons);
    assert!(
        mcp.reasons
            .iter()
            .any(|r| r == "from session Claude Code session 0164dc07")
    );
    let dev = find(&found, 14).expect("dev server named by its owner's PID");
    assert!(dev.reasons.iter().any(|r| r == "agent-command"), "{:?}", dev.reasons);
    assert!(dev.reasons.iter().any(|r| r == "from session Claude Code pid 12"));
}

#[test]
fn birth_record_of_a_live_owner_is_not_a_leftover() {
    let (mcp_env, cmd_env) = claude_env();
    let mut live = session(&mcp_env, &cmd_env);
    // Both moved out of the session's tree, but the session is still running.
    live.iter_mut().find(|p| p.pid == 13).unwrap().ppid = Some(1);
    live.iter_mut().find(|p| p.pid == 14).unwrap().ppid = Some(1);
    // A sibling still under the session carries its id.
    live.push(proc(16, 12, NODE, &["node", "server.js"], &mcp_env));
    let mut origins = Origins::default();
    let m = model(T0 + 60, live, Vec::new());
    origins.update(&m, &Catalog::builtin());
    for pid in [13, 14] {
        assert!(origins.owner_gone(pid, m.taken_at, 0).is_none(), "{pid}");
    }
}

#[test]
fn birth_record_pid_reused_by_another_program_means_the_owner_is_gone() {
    let (_, cmd_env) = claude_env();
    let mut left = without(&session(&[], &cmd_env), &[12]);
    let mut imposter = proc(12, 0, "C:\\Windows\\notepad.exe", &["notepad"], &[]);
    imposter.start_time = T0;
    left.push(imposter);
    let mut origins = Origins::default();
    let m = model(T0 + 60, left, Vec::new());
    origins.update(&m, &Catalog::builtin());
    assert!(origins.owner_gone(14, m.taken_at, 0).is_some());
}

#[test]
fn a_session_started_inside_an_ended_one_is_kept_alive() {
    // Codex was started from a command of a Claude Code session that has since ended: it and
    // its children inherited Claude's variables, but Codex itself is running.
    let (_, cmd_env) = claude_env();
    let processes = vec![
        proc(14, 0, BASH, &["bash"], &cmd_env),
        proc(
            20,
            14,
            "C:\\Users\\me\\AppData\\Roaming\\npm\\codex.exe",
            &["codex"],
            &cmd_env,
        ),
        proc(21, 20, NODE, &["node", "repl.js"], &cmd_env),
    ];
    let mut origins = Origins::default();
    let m = model(T0 + 60, processes, Vec::new());
    origins.update(&m, &Catalog::builtin());
    assert!(origins.owner_gone(21, m.taken_at, 0).is_none(), "under a live agent");
    assert!(origins.gone_roots(&m, 0).is_empty(), "ending the shell would end Codex");
}

#[test]
fn leftovers_with_a_window_or_in_use_are_kept_unless_asked() {
    let (mcp_env, cmd_env) = claude_env();
    let mut processes = without(&session(&mcp_env, &cmd_env), &[12]);
    // Another live session connected to the dev server.
    processes.push(proc(30, 11, CLAUDE, &["claude"], &[]));
    processes.push(proc(31, 30, NODE, &["node", "client.js"], &[]));
    let conn = |pid, local, remote| TcpConn {
        pid,
        local_addr: "127.0.0.1".into(),
        local_port: local,
        remote_addr: "127.0.0.1".into(),
        remote_port: remote,
    };
    let tcp = vec![conn(15, 5173, 50000), conn(31, 50000, 5173)];
    let mut origins = Origins::default();
    let m = model(T0 + 60, processes, tcp);
    origins.update(&m, &Catalog::builtin());
    let quick = Policy {
        owner_gone_grace: 0,
        ..policy()
    };

    let found = orphans(&m, &origins, &quick, HashSet::from([13]));
    let mcp = find(&found, 13).unwrap();
    assert!(!mcp.eligible && mcp.reasons.iter().any(|r| r == "has-window"));
    let dev = find(&found, 14).unwrap();
    assert!(!dev.eligible, "{:?}", dev.reasons);
    assert!(dev.reasons.iter().any(|r| r.starts_with("in-use by Claude Code @")));

    let end_all = Policy {
        end_windowed: true,
        end_shared: true,
        ..quick
    };
    let found = orphans(&m, &origins, &end_all, HashSet::from([13]));
    assert!(find(&found, 13).unwrap().eligible && find(&found, 14).unwrap().eligible);
}
