//! Against real processes: tree kills with re-verification, refusals, and SSH detection.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agentlens_core::Collector;
use agentlens_core::actions::{Outcome, Target, execute, plan, refusals};
use agentlens_core::catalog::Catalog;
use agentlens_core::model::Model;
use agentlens_core::net::ssh_connections;

/// A shell with one long-lived child: (program, args, child name prefix).
#[cfg(windows)]
const TREE: (&str, &[&str], &str) = ("cmd.exe", &["/d", "/c", "ping -n 60 127.0.0.1 >NUL"], "ping");
#[cfg(unix)]
const TREE: (&str, &[&str], &str) = ("sh", &["-c", "sleep 60; true"], "sleep");

#[cfg(windows)]
const SSH: &str = r"C:\Windows\System32\OpenSSH\ssh.exe";
#[cfg(unix)]
const SSH: &str = "/usr/bin/ssh";

fn build() -> Model {
    let mut collector = Collector::new(Catalog::builtin().env_keys());
    Model::build(collector.snapshot(), &Catalog::builtin())
}

/// Polls until `check` holds or the deadline passes.
fn wait_until(mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn collector_reads_details_once_and_picks_up_new_processes() {
    let mut collector = Collector::new(Catalog::builtin().env_keys());
    let me = std::process::id();
    let first = collector.snapshot();
    let mine = |s: &agentlens_core::Snapshot| s.processes.iter().find(|p| p.pid == me).unwrap().clone();
    assert!(!mine(&first).cmd.is_empty(), "own command line is readable");

    let (mut child, _) = spawn_tree();
    let second = collector.snapshot();
    assert_eq!(mine(&second).cmd, mine(&first).cmd, "cached details are kept");
    let cmd = second
        .processes
        .iter()
        .find(|p| p.pid == child.id())
        .expect("new process appears");
    assert!(cmd.cmd.iter().any(|a| a.contains(TREE.2)), "{:?}", cmd.cmd);

    let m = Model::build(second, &Catalog::builtin());
    execute(&plan(&m, &[child.id()]));
    assert!(wait_until(|| child.try_wait().unwrap().is_some()));
}

/// The shell and its child's PID, alive for about a minute.
fn spawn_tree() -> (Child, u32) {
    let (program, args, child_name) = TREE;
    let child = Command::new(program).args(args).stdout(Stdio::null()).spawn().unwrap();
    let parent = child.id();
    let mut grandchild = 0;
    assert!(wait_until(|| {
        let m = build();
        grandchild = m
            .children
            .get(&parent)
            .into_iter()
            .flatten()
            .copied()
            .find(|c| m.procs[c].name.to_lowercase().starts_with(child_name))
            .unwrap_or(0);
        grandchild != 0
    }));
    (child, grandchild)
}

#[test]
fn a_shell_cannot_run_its_next_command_while_its_tree_is_ended() {
    // `long-child; next-command`: ending the child first would let the shell go on (or a
    // supervising loop start a replacement) before the shell itself is ended.
    // A race: seen with bash on macOS about once in five runs, so try several times.
    #[cfg(windows)]
    let program = "cmd.exe";
    #[cfg(unix)]
    let program = if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "sh"
    };
    let (_, flags, child_name) = TREE;
    let mut ran = 0;
    for attempt in 0..5 {
        let marker = std::env::temp_dir().join(format!("agentlens-next-{}-{attempt}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        #[cfg(windows)]
        let script = format!("ping -n 60 127.0.0.1 >NUL & echo next> \"{}\"", marker.display());
        #[cfg(unix)]
        let script = format!("sleep 60; echo next > '{}'", marker.display());
        let mut shell = Command::new(program)
            .args(&flags[..flags.len() - 1])
            .arg(&script)
            .spawn()
            .unwrap();
        assert!(wait_until(|| {
            let m = build();
            m.children
                .get(&shell.id())
                .is_some_and(|c| c.iter().any(|p| m.procs[p].name.to_lowercase().starts_with(child_name)))
        }));

        execute(&plan(&build(), &[shell.id()]));
        assert!(wait_until(|| shell.try_wait().unwrap().is_some()));
        std::thread::sleep(Duration::from_millis(300));
        ran += usize::from(marker.exists());
        let _ = std::fs::remove_file(&marker);
    }
    assert_eq!(ran, 0, "the shell ran its next command in {ran} of 5 attempts");
}

#[test]
fn kills_whole_tree_parents_first() {
    let (mut child, ping) = spawn_tree();
    let m = build();
    let plan = plan(&m, &[child.id()]);
    assert_eq!(
        plan.targets.first().map(|t| t.pid),
        Some(child.id()),
        "parents go first"
    );
    assert_eq!(plan.targets.last().map(|t| t.pid), Some(ping));
    assert!(refusals(&plan, &m, &Catalog::builtin(), std::process::id()).is_empty());

    let results = execute(&plan);
    assert!(results.iter().all(|(_, o)| *o == Outcome::Killed), "{results:?}");
    assert!(wait_until(|| child.try_wait().unwrap().is_some()));
    assert!(wait_until(|| !build().procs.contains_key(&ping)));

    // Running the same plan again finds nothing to kill.
    let again = execute(&plan);
    assert!(again.iter().all(|(_, o)| *o == Outcome::AlreadyGone), "{again:?}");
}

#[test]
fn leaves_a_process_whose_start_time_does_not_match() {
    let (mut child, _ping) = spawn_tree();
    let m = build();
    let mut stale = plan(&m, &[child.id()]);
    let real = stale.targets.clone();
    stale.targets = vec![Target {
        start_time: real.first().unwrap().start_time + 100,
        ..real.first().unwrap().clone()
    }];
    assert_eq!(execute(&stale)[0].1, Outcome::Replaced);
    assert!(child.try_wait().unwrap().is_none(), "still running");

    stale.targets = real;
    execute(&stale);
    assert!(wait_until(|| child.try_wait().unwrap().is_some()));
}

#[test]
fn refuses_to_kill_itself_or_its_ancestors() {
    let m = build();
    let me = std::process::id();
    let catalog = Catalog::builtin();
    assert!(!refusals(&plan(&m, &[me]), &m, &catalog, me).is_empty());
    if let Some(&parent) = m.parent.get(&me) {
        let reasons = refusals(&plan(&m, &[parent]), &m, &catalog, me);
        assert!(reasons.iter().any(|r| r.contains("agentlens itself")), "{reasons:?}");
    }
}

#[test]
fn detects_and_disconnects_an_ssh_session() {
    let ssh = SSH;
    if !std::path::Path::new(ssh).exists() {
        eprintln!("skipped: OpenSSH client not installed");
        return;
    }
    // A listener that accepts and never sends a banner keeps ssh connected and waiting.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accept = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));

    let mut child = Command::new(ssh)
        .args([
            "-p",
            &port.to_string(),
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "tester@127.0.0.1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _server_side = accept.join().unwrap().unwrap();

    let mut found = None;
    assert!(wait_until(|| {
        let m = build();
        found = ssh_connections(&m, &Catalog::builtin(), &[])
            .into_iter()
            .find(|c| c.pid == child.id());
        found.is_some()
    }));
    let conn = found.unwrap();
    assert_eq!(conn.user.as_deref(), Some("tester"));
    assert_eq!(conn.host, "127.0.0.1");
    assert_eq!(conn.port, port);
    assert!(!conn.inbound);

    let m = build();
    let results = execute(&plan(&m, &[conn.pid]));
    assert!(results.iter().all(|(_, o)| *o == Outcome::Killed), "{results:?}");
    assert!(wait_until(|| child.try_wait().unwrap().is_some()));
    assert!(wait_until(|| !ssh_connections(&build(), &Catalog::builtin(), &[])
        .iter()
        .any(|c| c.pid == conn.pid)));
}
