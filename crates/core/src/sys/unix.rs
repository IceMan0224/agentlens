//! Shared by Linux and macOS: signals with start-time re-verification.

use std::io;
use std::time::{Duration, Instant};

use super::{details, start_time};
use crate::actions::Outcome;

/// How long processes get to exit after SIGTERM before SIGKILL, and after SIGKILL.
const GRACE: Duration = Duration::from_secs(1);

/// `details` already reads cwd and environment here; this exists for the shared collector.
pub fn read_cwd_env(pending: &[(u32, u64)]) -> Vec<Option<(Option<String>, Vec<String>)>> {
    pending
        .iter()
        .map(|(pid, start)| {
            (start_time(*pid) == Some(*start)).then(|| {
                let d = details(*pid);
                (d.cwd, d.env.unwrap_or_default())
            })
        })
        .collect()
}

/// SIGTERM to every target (parents first, as given), one shared grace period, then SIGKILL
/// for whatever is still the same process.
pub fn terminate_all(targets: &[(u32, u64)]) -> Vec<Outcome> {
    let mut outcomes: Vec<Option<Outcome>> = targets
        .iter()
        .map(|&(pid, start)| match start_time(pid) {
            None => Some(Outcome::AlreadyGone),
            Some(t) if t.abs_diff(start) > 1 => Some(Outcome::Replaced),
            Some(_) => signal(pid, libc::SIGTERM).err(),
        })
        .collect();

    for sig in [None, Some(libc::SIGKILL)] {
        if let Some(sig) = sig {
            for (i, &(pid, start)) in targets.iter().enumerate() {
                if outcomes[i].is_none() && alive(pid, start) {
                    outcomes[i] = signal(pid, sig).err();
                }
            }
        }
        let deadline = Instant::now() + GRACE;
        while Instant::now() < deadline
            && targets
                .iter()
                .enumerate()
                .any(|(i, &(pid, start))| outcomes[i].is_none() && alive(pid, start))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    targets
        .iter()
        .zip(outcomes)
        .map(|(&(pid, start), outcome)| match outcome {
            Some(o) => o,
            None if alive(pid, start) => Outcome::Failed("still running after SIGKILL".into()),
            None => Outcome::Killed,
        })
        .collect()
}

/// Running and still the same process; exited, zombie or replaced count as not alive.
fn alive(pid: u32, start: u64) -> bool {
    start_time(pid).is_some_and(|t| t.abs_diff(start) <= 1)
}

fn signal(pid: u32, sig: libc::c_int) -> Result<(), Outcome> {
    // SAFETY: kill(2) has no memory-safety preconditions.
    if unsafe { libc::kill(pid as libc::pid_t, sig) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    Err(match err.raw_os_error() {
        Some(libc::ESRCH) => Outcome::AlreadyGone,
        _ => Outcome::Failed(err.to_string()),
    })
}
