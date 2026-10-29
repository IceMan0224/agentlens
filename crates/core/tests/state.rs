//! A session's state from the CPU use of its processes.

use agentlens_core::catalog::Catalog;
use agentlens_core::health::{History, SessionState, WORKING_SECS, session_state};
use agentlens_core::model::Model;
use agentlens_core::{ProcInfo, Snapshot};

const T0: u64 = 1_800_000_000;

fn model(at: u64, cpu: f32) -> Model {
    let proc = |pid, ppid, name: &str, cpu| ProcInfo {
        pid,
        ppid: Some(ppid).filter(|p| *p != 0),
        name: name.into(),
        exe: None,
        cmd: vec![name.into()],
        cwd: None,
        env: Vec::new(),
        start_time: T0 - 100,
        cpu,
        private_bytes: 0,
        working_set: 0,
    };
    Model::build(
        Snapshot {
            taken_at: at,
            processes: vec![proc(10, 0, "pwsh.exe", 0.0), proc(11, 10, "claude.exe", cpu)],
            tcp: Vec::new(),
            listening: Vec::new(),
        },
        &Catalog::builtin(),
    )
}

#[test]
fn working_then_quiet_then_idle() {
    let mut history = History::default();
    let state = |history: &mut History, at, cpu| {
        let m = model(at, cpu);
        history.update(&m);
        session_state(&m, history, &[11], 30)
    };
    // Just started watching a session that does nothing: not working.
    assert_eq!(state(&mut history, T0, 0.0), SessionState::Quiet);
    assert_eq!(state(&mut history, T0 + 1, 12.0), SessionState::Working);
    assert_eq!(state(&mut history, T0 + WORKING_SECS, 0.0), SessionState::Working);
    assert_eq!(state(&mut history, T0 + 1 + WORKING_SECS, 0.0), SessionState::Quiet);
    assert_eq!(state(&mut history, T0 + 1 + 30 * 60, 0.0), SessionState::Idle);
}
