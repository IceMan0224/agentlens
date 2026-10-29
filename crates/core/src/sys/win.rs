//! Windows: one `NtQuerySystemInformation` call lists every process with its parent, start
//! time, CPU time and memory; per-PID calls read the executable path and command line without
//! enumerating all processes again. Working directory and environment live in the target's
//! PEB and are read in batches through sysinfo.

use std::ptr::null_mut;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, HLOCAL, LocalFree, UNICODE_STRING,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess,
};
use windows::Win32::UI::Shell::CommandLineToArgvW;
use windows::core::{PCWSTR, PWSTR};

use super::{Details, RawProc};
use crate::actions::Outcome;

/// `SYSTEM_PROCESS_INFORMATION` with the fields `winternl.h` hides as reserved.
#[repr(C)]
struct SystemProcessInfo {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name: UNICODE_STRING,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    /// Commit charge; the same value as `PROCESS_MEMORY_COUNTERS_EX::PrivateUsage`.
    pagefile_usage: usize,
}

#[derive(Default)]
pub struct Lister {
    /// Buffer size the last listing needed, so the next one usually takes one attempt. The
    /// buffer itself (about 1 MB, mostly thread records) is freed after each listing.
    size_hint: usize,
}

impl Lister {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&mut self) -> Vec<RawProc> {
        list_processes(&mut self.size_hint)
    }
}

fn list_processes(size_hint: &mut usize) -> Vec<RawProc> {
    // u64 elements keep the records 8-byte aligned.
    let mut buf: Vec<u64> = vec![0; (*size_hint).max(256 * 1024) / 8];
    for _ in 0..8 {
        let mut needed = 0u32;
        // SAFETY: the buffer is writable for the length passed.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut needed,
            )
        };
        if status.is_ok() {
            *size_hint = needed as usize + 64 * 1024;
            return unsafe { parse(&buf) };
        }
        // The process list grew between the size query and the copy: retry with headroom.
        let grow = (needed as usize).max(buf.len() * 8 * 2) + 64 * 1024;
        buf = vec![0; grow / 8];
    }
    Vec::new()
}

/// SAFETY: `buf` must hold a successful `SystemProcessInformation` result.
unsafe fn parse(buf: &[u64]) -> Vec<RawProc> {
    let base = buf.as_ptr().cast::<u8>();
    let mut out = Vec::with_capacity(512);
    let mut offset = 0usize;
    loop {
        let e = unsafe { &*base.add(offset).cast::<SystemProcessInfo>() };
        let pid = e.unique_process_id as u32;
        let name = if e.image_name.Buffer.is_null() {
            if pid == 0 {
                "System Idle Process".into()
            } else {
                String::new()
            }
        } else {
            String::from_utf16_lossy(unsafe { unicode_chars(&e.image_name) })
        };
        let ppid = e.inherited_from_unique_process_id as u32;
        out.push(RawProc {
            pid,
            ppid: (ppid != 0).then_some(ppid),
            name,
            start_time: (e.create_time.max(0) as u64 / 10_000_000).saturating_sub(11_644_473_600),
            // 100 ns units.
            cpu_ns: (e.user_time + e.kernel_time).max(0) as u64 * 100,
            private_bytes: e.pagefile_usage as u64,
            working_set: e.working_set_size as u64,
        });
        if e.next_entry_offset == 0 {
            return out;
        }
        offset += e.next_entry_offset as usize;
    }
}

/// Executable path and command line, or nothing when access is denied. Working directory and
/// environment are left to `read_cwd_env`.
pub fn details(pid: u32) -> Details {
    // SAFETY: plain Win32 calls on a handle opened and closed here.
    let (exe, cmd) = unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => {
                let exe = image_path(handle);
                let cmd = command_line(handle);
                let _ = CloseHandle(handle);
                (exe, cmd)
            }
            Err(_) => (None, Vec::new()),
        }
    };
    Details {
        exe,
        cmd,
        cwd: None,
        env: None,
    }
}

/// Working directory and raw environment for each (pid, start time), in order; `None` for
/// processes that exited or were replaced. A throwaway sysinfo `System` reads them so the full
/// environments it collects are dropped right away. Each call enumerates every process again.
pub fn read_cwd_env(pending: &[(u32, u64)]) -> Vec<Option<(Option<String>, Vec<String>)>> {
    let Ok(mut reader) = System::new() else {
        return vec![None; pending.len()];
    };
    let pids: Vec<Pid> = pending.iter().map(|(pid, _)| Pid::from_u32(*pid)).collect();
    reader.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        false,
        ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_environ(UpdateKind::Always),
    );
    pending
        .iter()
        .map(|(pid, start)| {
            let p = reader
                .process(Pid::from_u32(*pid))
                .filter(|p| p.start_time() == *start)?;
            let cwd = p.cwd().map(|path| path.to_string_lossy().into_owned());
            Some((
                cwd,
                p.environ().iter().map(|kv| kv.to_string_lossy().into_owned()).collect(),
            ))
        })
        .collect()
}

pub fn terminate_all(targets: &[(u32, u64)]) -> Vec<Outcome> {
    targets.iter().map(|(pid, start)| terminate(*pid, *start)).collect()
}

fn terminate(pid: u32, start_time: u64) -> Outcome {
    // SAFETY: plain Win32 calls on a handle opened and closed here.
    unsafe {
        let handle = match OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Outcome::AlreadyGone,
            Err(e) => return Outcome::Failed(e.message()),
        };
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let outcome = match GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) {
            Err(e) => Outcome::Failed(e.message()),
            Ok(()) if unix_secs(created).abs_diff(start_time) > 1 => Outcome::Replaced,
            Ok(()) => match TerminateProcess(handle, 1) {
                Ok(()) => Outcome::Killed,
                // Exited between the checks.
                Err(_) if exited.dwLowDateTime != 0 || exited.dwHighDateTime != 0 => Outcome::AlreadyGone,
                Err(e) => Outcome::Failed(e.message()),
            },
        };
        let _ = CloseHandle(handle);
        outcome
    }
}

/// Same conversion as the process list's start time.
fn unix_secs(ft: FILETIME) -> u64 {
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    (ticks / 10_000_000).saturating_sub(11_644_473_600)
}

unsafe fn image_path(handle: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

unsafe fn command_line(handle: HANDLE) -> Vec<String> {
    let mut needed = 0u32;
    let _ = unsafe { NtQueryInformationProcess(handle, ProcessCommandLineInformation, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Vec::new();
    }
    let mut buf: Vec<u64> = vec![0; (needed as usize).div_ceil(8)];
    let status = unsafe {
        NtQueryInformationProcess(
            handle,
            ProcessCommandLineInformation,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if status.is_err() {
        return Vec::new();
    }
    let text = unsafe { &*buf.as_ptr().cast::<UNICODE_STRING>() };
    if text.Buffer.is_null() || text.Length == 0 {
        return Vec::new();
    }
    let mut wide: Vec<u16> = unsafe { unicode_chars(text) }.to_vec();
    wide.push(0);
    split_args(&wide)
}

/// The characters of a `UNICODE_STRING`, which carries a byte length and is not necessarily
/// NUL-terminated.
///
/// SAFETY: `s.Buffer` must point to at least `s.Length` readable bytes.
unsafe fn unicode_chars(s: &UNICODE_STRING) -> &[u16] {
    unsafe { std::slice::from_raw_parts(s.Buffer.0, s.Length as usize / 2) }
}

/// Splits a NUL-terminated command line the way the C runtime does.
fn split_args(wide: &[u16]) -> Vec<String> {
    let mut count = 0i32;
    // SAFETY: `wide` is NUL-terminated; the returned block is freed with LocalFree.
    unsafe {
        let argv = CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut count);
        if argv.is_null() {
            return Vec::new();
        }
        let args = (0..count as usize)
            .map(|i| String::from_utf16_lossy((*argv.add(i)).as_wide()))
            .collect();
        let _ = LocalFree(Some(HLOCAL(argv.cast())));
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_this_process_with_parent_and_command_line() {
        let mut lister = Lister::default();
        let all = lister.list();
        assert!(lister.size_hint > 0);
        let me = all
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("own process listed");
        assert!(me.name.to_lowercase().ends_with(".exe"));
        assert!(me.ppid.is_some_and(|pp| all.iter().any(|p| p.pid == pp)));
        assert!(me.private_bytes > 0 && me.working_set > 0 && me.start_time > 1_700_000_000);

        let Details { exe, cmd, .. } = details(me.pid);
        let exe = exe.unwrap();
        assert!(exe.to_lowercase().ends_with(&me.name.to_lowercase()), "{exe}");
        assert_eq!(cmd.first().map(|a| a.to_lowercase()), Some(exe.to_lowercase()));
    }

    #[test]
    fn splits_quoted_arguments() {
        let wide: Vec<u16> = "\"C:\\Program Files\\x.exe\" -p 22 \"a b\"\0".encode_utf16().collect();
        assert_eq!(split_args(&wide), ["C:\\Program Files\\x.exe", "-p", "22", "a b"]);
    }
}
