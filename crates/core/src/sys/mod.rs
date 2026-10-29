//! Process access through each OS's own kernel interface: `NtQuerySystemInformation` on
//! Windows, procfs on Linux, libproc and `sysctl` on macOS.
//!
//! Every platform module provides the same functions:
//! - `Lister::list` — all processes with parent, start time, CPU time and memory (every tick)
//! - `details(pid)` — exe, command line and, where cheap, cwd and environment (once per process)
//! - `read_cwd_env` — cwd and environment for processes whose `details` left them out
//! - `terminate_all` — end processes after re-checking each one's start time
//! - `windowed_pids` — processes with a visible window (Windows only; empty elsewhere)
//! - `stdin_closed` — whether the pipe on a process's input lost its writer (Windows only)

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{read_cwd_env, stdin_closed, terminate_all, windowed_pids};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{Lister, details, start_time};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{Lister, details, start_time};

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{Lister, details, read_cwd_env, stdin_closed, terminate_all, windowed_pids};

pub struct RawProc {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    /// Unix seconds.
    pub start_time: u64,
    /// User + kernel CPU time in nanoseconds.
    pub cpu_ns: u64,
    /// Memory only this process holds: commit charge on Windows, physical footprint on macOS,
    /// resident minus shared pages on Linux.
    pub private_bytes: u64,
    pub working_set: u64,
}

pub struct Details {
    pub exe: Option<String>,
    pub cmd: Vec<String>,
    pub cwd: Option<String>,
    /// Raw `KEY=value` entries; `None` when this platform reads them later in a batch.
    pub env: Option<Vec<String>>,
}
