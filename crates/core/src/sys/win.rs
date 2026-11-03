//! Windows: one `NtQuerySystemInformation` call lists every process with its parent, start
//! time, CPU time and memory; per-PID calls read the executable path and command line without
//! enumerating all processes again. Working directory and environment live in the target's
//! PEB and are read in batches through sysinfo.

use std::ptr::null_mut;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, HLOCAL, HWND, LPARAM, LocalFree, UNICODE_STRING,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess,
};
use windows::Win32::UI::Shell::CommandLineToArgvW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;
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
/// Whether the pipe on the process's standard input has lost its writer: `Some(true)` when
/// every write end is closed (nobody can send it anything again), `Some(false)` when one is
/// open, `None` when its input is not a pipe or cannot be read.
///
/// Blocks while the target has a synchronous read pending on the pipe, which is how a live
/// stdio server waits; it returns once data arrives or the writer closes. Call it off the UI
/// thread (`stdio::StdioProbe`).
pub fn stdin_closed(pid: u32) -> Option<bool> {
    use windows::Wdk::System::Threading::ProcessBasicInformation;
    use windows::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE};
    use windows::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Pipes::PeekNamedPipe;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_BASIC_INFORMATION, PROCESS_DUP_HANDLE, PROCESS_VM_READ,
    };

    // SAFETY: reads plain values from the target's memory into locals; every handle opened here
    // is closed here or by the worker below.
    let dup = unsafe {
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ | PROCESS_DUP_HANDLE;
        let process = OpenProcess(access, false, pid).ok()?;
        let read = |addr: usize| {
            let mut value = 0usize;
            ReadProcessMemory(
                process,
                addr as *const _,
                &mut value as *mut usize as *mut _,
                size_of::<usize>(),
                None,
            )
            .ok()
            .map(|_| value)
        };
        let mut info = PROCESS_BASIC_INFORMATION::default();
        let mut len = 0;
        let ok = NtQueryInformationProcess(
            process,
            ProcessBasicInformation,
            &mut info as *mut _ as *mut _,
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            &mut len,
        )
        .is_ok();
        // PEB.ProcessParameters, then RTL_USER_PROCESS_PARAMETERS.StandardInput (64-bit layout).
        let stdin = ok
            .then(|| read(info.PebBaseAddress as usize + 0x20))
            .flatten()
            .and_then(|params| read(params + 0x20))
            .filter(|h| *h != 0);
        let mut dup = HANDLE::default();
        let copied = stdin.is_some_and(|h| {
            DuplicateHandle(
                process,
                HANDLE(h as *mut _),
                GetCurrentProcess(),
                &mut dup,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
            .is_ok()
        });
        let _ = CloseHandle(process);
        copied.then_some(dup)?
    };
    // SAFETY: the duplicated handle is ours and closed here.
    unsafe {
        let state = (GetFileType(dup) == FILE_TYPE_PIPE).then(|| match PeekNamedPipe(dup, None, 0, None, None, None) {
            Ok(()) => Some(false),
            Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => Some(true),
            Err(_) => None,
        });
        let _ = CloseHandle(dup);
        state.flatten()
    }
}

/// `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX`.
#[repr(C)]
#[derive(Clone, Copy)]
struct HandleEntry {
    object: usize,
    pid: usize,
    handle: usize,
    granted_access: u32,
    creator_back_trace_index: u16,
    object_type_index: u16,
    attributes: u32,
    reserved: u32,
}

/// How long one handle may take before it is left to its worker: asking about a handle whose
/// owner waits in a synchronous read on it blocks until that read returns.
const HANDLE_STALL: std::time::Duration = std::time::Duration::from_millis(250);

/// Every file and folder other processes have open, as (pid, path), from one listing of all
/// handles in the system: each file handle is copied and asked for its path. Processes that
/// cannot be opened (other users, protected) are left out. `None` when the listing failed.
pub fn open_files() -> Option<Vec<(u32, String)>> {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    let handles = file_handles()?;
    let me = std::process::id();
    let handles: Arc<Vec<(u32, usize)>> = Arc::new(handles.into_iter().filter(|(pid, _)| *pid != me).collect());
    let state: Arc<HandleProgress> = Arc::new(Mutex::new((0, 0, Vec::new())));
    let spawn = || {
        let (handles, state) = (Arc::clone(&handles), Arc::clone(&state));
        std::thread::Builder::new()
            .name("agentlens-handles".into())
            .stack_size(64 * 1024)
            .spawn(move || resolve_handles(&handles, &state))
            .ok()
    };
    spawn()?;
    let (mut seen, mut since, mut stalled) = (0, Instant::now(), 0);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(5));
        let (taken, finished) = {
            let s = state.lock().unwrap();
            (s.0, s.1)
        };
        if finished + stalled >= handles.len() {
            break;
        }
        if finished != seen {
            (seen, since) = (finished, Instant::now());
        } else if since.elapsed() > HANDLE_STALL && taken > finished + stalled {
            // The worker is stuck on one handle: leave it there and go on with a new one.
            stalled += 1;
            since = Instant::now();
            spawn()?;
        }
    }
    let mut s = state.lock().unwrap();
    Some(std::mem::take(&mut s.2))
}

/// Next handle to take, handles finished, and the (pid, path) found so far.
type HandleProgress = std::sync::Mutex<(usize, usize, Vec<(u32, String)>)>;

fn resolve_handles(handles: &[(u32, usize)], state: &HandleProgress) {
    use windows::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows::Win32::Storage::FileSystem::{
        FILE_TYPE_DISK, GetFileType, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, PROCESS_DUP_HANDLE};

    let mut processes: std::collections::HashMap<u32, Option<HANDLE>> = std::collections::HashMap::new();
    let mut buf = vec![0u16; 1024];
    loop {
        let i = {
            let mut s = state.lock().unwrap();
            if s.0 >= handles.len() {
                break;
            }
            s.0 += 1;
            s.0 - 1
        };
        let (pid, value) = handles[i];
        // SAFETY: plain Win32 calls; the copied handle is closed here, the process handles at
        // the end.
        let path = unsafe {
            let process = *processes
                .entry(pid)
                .or_insert_with(|| OpenProcess(PROCESS_DUP_HANDLE, false, pid).ok());
            let mut copy = HANDLE::default();
            match process {
                Some(process)
                    if DuplicateHandle(
                        process,
                        HANDLE(value as *mut _),
                        GetCurrentProcess(),
                        &mut copy,
                        0,
                        false,
                        DUPLICATE_SAME_ACCESS,
                    )
                    .is_ok() =>
                {
                    // Pipes and devices have no path; only files and folders on disk are asked.
                    let path = (GetFileType(copy) == FILE_TYPE_DISK)
                        .then(|| {
                            let len = GetFinalPathNameByHandleW(copy, &mut buf, VOLUME_NAME_DOS) as usize;
                            (len > 0 && len < buf.len()).then(|| String::from_utf16_lossy(&buf[..len]))
                        })
                        .flatten();
                    let _ = CloseHandle(copy);
                    path
                }
                _ => None,
            }
        };
        let mut s = state.lock().unwrap();
        s.1 += 1;
        if let Some(path) = path {
            s.2.push((pid, path));
        }
    }
    for process in processes.into_values().flatten() {
        // SAFETY: opened above and not used after this.
        let _ = unsafe { CloseHandle(process) };
    }
}

/// (pid, handle value) of every file handle in the system. The kernel numbers object types
/// differently between versions, so the file type's number is read off a handle opened here.
fn file_handles() -> Option<Vec<(u32, usize)>> {
    use windows::Wdk::System::SystemInformation::SYSTEM_INFORMATION_CLASS;
    use windows::Win32::Foundation::STATUS_INFO_LENGTH_MISMATCH;

    let probe = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
    let probe_value = std::os::windows::io::AsRawHandle::as_raw_handle(&probe) as usize;
    let me = std::process::id() as usize;
    // SYSTEM_HANDLE_INFORMATION_EX: a count and a reserved word, then the entries.
    let mut buf: Vec<u64> = vec![0; 4 << 20 >> 3];
    for _ in 0..6 {
        let mut needed = 0u32;
        // SAFETY: the buffer is writable for the length passed.
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_INFORMATION_CLASS(64), // SystemExtendedHandleInformation
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut needed,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            buf = vec![0; (needed as usize).max(buf.len() * 16) / 8 + 8192];
            continue;
        }
        if status.is_err() {
            return None;
        }
        let count = buf[0] as usize;
        let max = (buf.len() * 8 - 16) / size_of::<HandleEntry>();
        // SAFETY: the kernel wrote `count` entries after the 16-byte header; capped to the buffer.
        let entries = unsafe { std::slice::from_raw_parts(buf.as_ptr().add(2).cast::<HandleEntry>(), count.min(max)) };
        let file_type = entries
            .iter()
            .find(|e| e.pid == me && e.handle == probe_value)?
            .object_type_index;
        drop(probe);
        return Some(
            entries
                .iter()
                .filter(|e| e.object_type_index == file_type)
                .map(|e| (e.pid as u32, e.handle))
                .collect(),
        );
    }
    None
}

/// Processes owning a visible, titled top-level window: programs the user sees and works with.
pub fn windowed_pids() -> std::collections::HashSet<u32> {
    unsafe extern "system" fn add(hwnd: HWND, found: LPARAM) -> BOOL {
        unsafe {
            if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 {
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                (*(found.0 as *mut std::collections::HashSet<u32>)).insert(pid);
            }
        }
        BOOL(1)
    }
    let mut found = std::collections::HashSet::new();
    // Fails only when the callback stops early, which it never does.
    let _ = unsafe { EnumWindows(Some(add), LPARAM(&mut found as *mut _ as isize)) };
    found
}

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

    #[test]
    fn stdin_closed_tells_an_open_pipe_from_one_without_writer() {
        use std::process::{Command, Stdio};
        // ping never reads its input, so checking never waits behind a read.
        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let open = stdin_closed(child.id());
        drop(child.stdin.take());
        let closed = stdin_closed(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!((open, closed), (Some(false), Some(true)));
        assert_eq!(
            stdin_closed(std::process::id()),
            stdin_closed(std::process::id()),
            "stable"
        );
    }
}
