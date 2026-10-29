//! Unix process trees: orphans are adopted by launchd/systemd instead of losing their parent,
//! terminals are `.app` bundles, and SSH logins and tmux panes act as tabs.

use agentlens_core::catalog::Catalog;
use agentlens_core::model::{GroupKind, HelperKind, Model, OrphanReason, Role};
use agentlens_core::{ProcInfo, Snapshot};

fn proc(pid: u32, ppid: u32, exe: &str, cmd: &[&str]) -> ProcInfo {
    ProcInfo {
        pid,
        ppid: Some(ppid).filter(|p| *p != 0),
        name: exe.rsplit('/').next().unwrap().to_string(),
        exe: Some(exe.to_string()),
        cmd: cmd.iter().map(|s| s.to_string()).collect(),
        cwd: Some("/Users/me/work/app".into()),
        env: Vec::new(),
        start_time: 1_800_000_000 + pid as u64,
        cpu: 0.0,
        private_bytes: 10 << 20,
        working_set: 20 << 20,
    }
}

fn mac() -> Model {
    let node = "/opt/homebrew/bin/node";
    let processes = vec![
        proc(0, 0, "/kernel_task", &[]),
        proc(1, 0, "/sbin/launchd", &["/sbin/launchd"]),
        // Terminal.app -> login -> zsh -> codex (npm script under node)
        proc(
            100,
            1,
            "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
            &[],
        ),
        proc(110, 100, "/usr/bin/login", &["login", "-pf", "me"]),
        proc(120, 110, "/bin/zsh", &["-zsh"]),
        proc(
            130,
            120,
            node,
            &["node", "/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js"],
        ),
        proc(
            131,
            130,
            "/opt/homebrew/lib/node_modules/@openai/codex/vendor/codex",
            &["codex"],
        ),
        // sshd -> sshd-session -> zsh -> claude
        proc(200, 1, "/usr/sbin/sshd", &["sshd"]),
        proc(210, 200, "/usr/libexec/sshd-session", &["sshd-session: me@ttys001"]),
        proc(220, 210, "/bin/zsh", &["-zsh"]),
        // The native installer runs a file named after its version; macOS names the process
        // after that file, so only argv[0] says "claude".
        proc(230, 220, "/Users/me/.local/share/claude/versions/2.1.276", &["claude"]),
        // An MCP server whose agent exited: adopted by launchd.
        proc(
            300,
            1,
            node,
            &[
                "node",
                "/Users/me/.npm/_npx/x/node_modules/@modelcontextprotocol/server-fs/dist/index.js",
            ],
        ),
        // A user's own server started from a since-closed terminal: shown, not proposed.
        proc(400, 1, node, &["node", "server.js"]),
        // An ordinary launchd agent, not a runtime: ignored.
        proc(500, 1, "/Applications/Slack.app/Contents/MacOS/Slack", &[]),
    ];
    Model::build(
        Snapshot {
            taken_at: 1_800_000_999,
            processes,
            tcp: Vec::new(),
        },
        &Catalog::builtin(),
    )
}

fn group<'a>(m: &'a Model, id: &str) -> &'a agentlens_core::model::Group {
    m.groups
        .iter()
        .find(|g| g.kind == GroupKind::Known(id.into()))
        .unwrap_or_else(|| panic!("no {id} group"))
}

#[test]
fn terminal_app_and_ssh_logins_are_hosts() {
    let m = mac();
    let terminal = group(&m, "macos-terminal");
    assert_eq!(terminal.tabs.len(), 1);
    assert_eq!(terminal.tabs[0].sessions[0].agent_id, "codex");
    assert_eq!(
        terminal.tabs[0].sessions[0].root_pid, 130,
        "the node launcher roots the session"
    );
    assert!(matches!(m.roles[&131], Role::Agent(_)));

    let ssh = group(&m, "ssh");
    assert_eq!(ssh.tabs[0].sessions[0].agent_id, "claude");
    assert!(
        !m.orphans.iter().any(|o| o.pid == 200 || o.pid == 230),
        "an SSH login is not an orphan"
    );
}

#[test]
fn processes_adopted_by_launchd_are_orphans() {
    let m = mac();
    let mcp = m.orphans.iter().find(|o| o.pid == 300).expect("adopted MCP server");
    assert_eq!(mcp.reason, OrphanReason::Reparented);
    assert!(mcp.agent_related);
    assert_eq!(m.roles[&300], Role::Other);
    assert!(
        !matches!(m.roles[&300], Role::Helper(HelperKind::Mcp)),
        "no longer under a session"
    );

    let own = m.orphans.iter().find(|o| o.pid == 400).expect("runtime is shown");
    assert!(!own.agent_related);
    assert!(
        !m.orphans.iter().any(|o| o.pid == 500 || o.pid == 100),
        "apps and hosts are not orphans"
    );
}

// ---- Regressions from code review -------------------------------------------------------

fn named(pid: u32, ppid: u32, name: &str, exe: &str, cmd: &[&str]) -> ProcInfo {
    ProcInfo {
        name: name.to_string(),
        ..proc(pid, ppid, exe, cmd)
    }
}

fn build(processes: Vec<ProcInfo>) -> Model {
    Model::build(
        Snapshot {
            taken_at: 1_800_000_999,
            processes,
            tcp: Vec::new(),
        },
        &Catalog::builtin(),
    )
}

#[test]
fn unknown_gui_terminal_is_an_unregistered_host_not_an_orphan() {
    // Every GUI app is a child of launchd: that alone must not make it an orphan.
    let m = build(vec![
        proc(1, 0, "/sbin/launchd", &[]),
        proc(100, 1, "/Applications/Hyper.app/Contents/MacOS/Hyper", &[]),
        proc(110, 100, "/bin/zsh", &["-zsh"]),
        proc(120, 110, "/Users/me/.local/bin/claude", &["claude"]),
    ]);
    assert!(m.orphans.is_empty(), "{:?}", m.orphans);
    let g = m
        .groups
        .iter()
        .find(|g| g.kind == GroupKind::Unregistered("hyper".into()))
        .expect("Hyper group");
    assert_eq!(g.tabs[0].sessions[0].root_pid, 120);
}

#[test]
fn tmux_server_is_a_host_despite_its_process_title() {
    // tmux renames itself (`tmux: server`); the executable still says tmux.
    let m = build(vec![
        proc(1, 0, "/usr/lib/systemd/systemd", &["/sbin/init"]),
        named(200, 1, "tmux: server", "/usr/bin/tmux", &["tmux"]),
        proc(210, 200, "/usr/bin/bash", &["-bash"]),
        proc(220, 210, "/usr/local/bin/codex", &["codex"]),
    ]);
    assert!(m.orphans.is_empty(), "{:?}", m.orphans);
    assert_eq!(group(&m, "tmux").tabs[0].sessions[0].root_pid, 220);
}

#[test]
fn parent_cycles_from_pid_reuse_do_not_hang() {
    // 100 started 200 and exited; within the same second a child of 200 reused PID 100.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut a = proc(100, 200, "/usr/bin/bash", &["bash"]);
        let mut b = proc(200, 100, "/usr/bin/bash", &["bash"]);
        a.start_time = 1_800_000_500;
        b.start_time = 1_800_000_500;
        let m = build(vec![proc(1, 0, "/sbin/init", &[]), a, b]);
        let _ = tx.send((m.ancestors(100).len(), m.subtree(100).len()));
    });
    let (ancestors, subtree) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("build finished");
    assert!(ancestors <= 1 && subtree <= 2);
}

#[test]
fn inbound_ssh_session_can_be_disconnected() {
    let m = build(vec![
        proc(1, 0, "/sbin/launchd", &[]),
        proc(300, 1, "/usr/sbin/sshd", &["sshd"]),
        proc(310, 300, "/usr/libexec/sshd-session", &["sshd-session: me@ttys001"]),
        proc(320, 310, "/bin/zsh", &["-zsh"]),
    ]);
    use agentlens_core::actions::{disconnect_plan, plan, refusals};
    let catalog = Catalog::builtin();
    // Disconnecting ends the login's own sshd-session...
    let refused = refusals(&disconnect_plan(&m, 310), &m, &catalog, 999);
    assert!(refused.is_empty(), "{refused:?}");
    // ...but host processes stay protected from ordinary tree kills.
    assert!(!refusals(&plan(&m, &[310]), &m, &catalog, 999).is_empty());
}
