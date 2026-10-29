//! macOS: one `sysctl(KERN_PROC_ALL)` lists every process of every user (as `ps` does);
//! one `proc_pid_rusage` per process adds CPU time and memory for the user's own processes.
//! `proc_pidpath`, `sysctl(KERN_PROCARGS2)` and the vnode path info are read once per process.

use std::ffi::c_void;
use std::mem::{MaybeUninit, size_of, size_of_val};
use std::ptr::null_mut;
use std::sync::OnceLock;

use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_time::{mach_timebase_info, mach_timebase_info_data_t};

use super::{Details, RawProc};

/// `p_stat` of a process that exited but was not reaped yet.
const SZOMB: u8 = 5;

/// `struct kinfo_proc` (sys/sysctl.h) as laid out by the 64-bit macOS SDK. libc does not
/// define it, so fields are read at these offsets, taken from the SDK with `offsetof`.
mod kinfo {
    pub const SIZE: usize = 648;
    /// `kp_proc.p_un.__p_starttime.tv_sec`, i64.
    pub const START_SEC: usize = 0;
    /// `kp_proc.p_stat`, u8.
    pub const STAT: usize = 36;
    /// `kp_proc.p_pid`, i32.
    pub const PID: usize = 40;
    /// `kp_proc.p_comm`, 17 bytes; truncated to 16 characters.
    pub const COMM: usize = 243;
    pub const COMM_LEN: usize = 17;
    /// `kp_eproc.e_ppid`, i32.
    pub const PPID: usize = 560;
}

struct KinfoProc<'a>(&'a [u8]);

impl KinfoProc<'_> {
    fn i32_at(&self, at: usize) -> i32 {
        i32::from_ne_bytes(self.0[at..at + 4].try_into().unwrap())
    }
    fn pid(&self) -> i32 {
        self.i32_at(kinfo::PID)
    }
    fn ppid(&self) -> i32 {
        self.i32_at(kinfo::PPID)
    }
    fn zombie(&self) -> bool {
        self.0[kinfo::STAT] == SZOMB
    }
    fn start_time(&self) -> u64 {
        i64::from_ne_bytes(self.0[kinfo::START_SEC..kinfo::START_SEC + 8].try_into().unwrap()).max(0) as u64
    }
    fn comm(&self) -> String {
        c_string(&self.0[kinfo::COMM..kinfo::COMM + kinfo::COMM_LEN])
    }
}

/// Raw `kinfo_proc` records for `mib`, growing the buffer if processes appear meanwhile.
fn kinfo_records(mib: &mut [libc::c_int]) -> Vec<u8> {
    for _ in 0..4 {
        let mut size = 0usize;
        // SAFETY: a null buffer asks for the size; then the buffer holds `size` bytes.
        unsafe {
            if libc::sysctl(mib.as_mut_ptr(), mib.len() as u32, null_mut(), &mut size, null_mut(), 0) != 0 {
                return Vec::new();
            }
            size += size / 8 + kinfo::SIZE * 16;
            let mut buf = vec![0u8; size];
            if libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buf.as_mut_ptr().cast(),
                &mut size,
                null_mut(),
                0,
            ) == 0
            {
                buf.truncate(size - size % kinfo::SIZE);
                return buf;
            }
        }
    }
    Vec::new()
}

/// Mach absolute time to nanoseconds (1:1 on Intel, 125:3 on Apple silicon).
fn timebase() -> (u64, u64) {
    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = mach_timebase_info_data_t::default();
        // SAFETY: writes into the struct passed.
        let ok = unsafe { mach_timebase_info(&mut info) } == KERN_SUCCESS;
        if ok && info.denom != 0 {
            (info.numer as u64, info.denom as u64)
        } else {
            (1, 1)
        }
    })
}

/// Fills a C struct through a libproc call that returns the number of bytes written.
unsafe fn pidinfo<T>(pid: i32, flavor: i32) -> Option<T> {
    let mut value = MaybeUninit::<T>::zeroed();
    let size = size_of::<T>() as i32;
    let n = unsafe { libc::proc_pidinfo(pid, flavor, 0, value.as_mut_ptr().cast(), size) };
    // SAFETY: the kernel filled the whole struct.
    (n == size).then(|| unsafe { value.assume_init() })
}

fn c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub struct Lister;

impl Lister {
    pub fn new() -> Self {
        Self
    }

    pub fn list(&mut self) -> Vec<RawProc> {
        let buf = kinfo_records(&mut [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL]);
        let (numer, denom) = timebase();
        buf.chunks_exact(kinfo::SIZE)
            .map(KinfoProc)
            .filter(|k| !k.zombie())
            .map(|k| {
                let pid = k.pid();
                // Needs the same user (or root): other users' processes keep their place in the
                // tree but report no CPU or memory.
                let usage = rusage(pid);
                RawProc {
                    pid: pid as u32,
                    ppid: (k.ppid() > 0).then_some(k.ppid() as u32),
                    name: k.comm(),
                    start_time: k.start_time(),
                    // Mach absolute time units, like the task info times.
                    cpu_ns: usage.map_or(0, |u| (u.ri_user_time + u.ri_system_time) * numer / denom),
                    // Physical footprint is what Activity Monitor reports as a process's memory.
                    private_bytes: usage.map_or(0, |u| u.ri_phys_footprint),
                    working_set: usage.map_or(0, |u| u.ri_resident_size),
                }
            })
            .collect()
    }
}

/// CPU times, resident size and physical footprint in one call.
fn rusage(pid: i32) -> Option<libc::rusage_info_v2> {
    let mut info = MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: the buffer is a rusage_info_v2, matching the flavor.
    let rc = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, info.as_mut_ptr().cast()) };
    // SAFETY: filled by the kernel on success.
    (rc == 0).then(|| unsafe { info.assume_init() })
}

/// Start time in Unix seconds, or `None` once the process exited or became a zombie.
pub fn start_time(pid: u32) -> Option<u64> {
    let buf = kinfo_records(&mut [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid as libc::c_int]);
    let k = KinfoProc(buf.get(..kinfo::SIZE)?);
    (k.pid() == pid as i32 && !k.zombie()).then(|| k.start_time())
}

/// Everything is readable for the user's own processes; other users' come back empty.
pub fn details(pid: u32) -> Details {
    let pid = pid as i32;
    let (cmd, env) = procargs(pid).unwrap_or_default();
    Details {
        exe: exe_path(pid),
        cmd,
        cwd: cwd(pid),
        env: Some(env),
    }
}

fn exe_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer holds the size passed.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast::<c_void>(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

fn cwd(pid: i32) -> Option<String> {
    // SAFETY: plain struct-filling call.
    let info = unsafe { pidinfo::<libc::proc_vnodepathinfo>(pid, libc::PROC_PIDVNODEPATHINFO)? };
    let path = &info.pvi_cdir.vip_path;
    // SAFETY: the nested char array is one contiguous MAXPATHLEN buffer.
    let bytes = unsafe { std::slice::from_raw_parts(path as *const _ as *const u8, size_of_val(path)) };
    Some(c_string(bytes)).filter(|p| !p.is_empty())
}

/// Arguments and environment from `KERN_PROCARGS2`: argc, the exec path, NUL padding, argv
/// strings, then environment strings.
fn procargs(pid: i32) -> Option<(Vec<String>, Vec<String>)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0usize;
    // SAFETY: a null buffer asks for the size; then the buffer holds `size` bytes.
    unsafe {
        if libc::sysctl(mib.as_mut_ptr(), 3, null_mut(), &mut size, null_mut(), 0) != 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        if libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, null_mut(), 0) != 0 {
            return None;
        }
        buf.truncate(size);
        Some(parse_procargs(&buf))
    }
}

fn parse_procargs(buf: &[u8]) -> (Vec<String>, Vec<String>) {
    let Some(argc) = buf
        .get(..4)
        .map(|b| i32::from_ne_bytes([b[0], b[1], b[2], b[3]]).max(0) as usize)
    else {
        return (Vec::new(), Vec::new());
    };
    let rest = &buf[4..];
    // Skip the exec path and the NUL padding after it.
    let path_end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    let start = rest[path_end..]
        .iter()
        .position(|b| *b != 0)
        .map_or(rest.len(), |i| path_end + i);
    let mut strings = rest[start..]
        .split(|b| *b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned());
    let args: Vec<String> = strings.by_ref().take(argc).collect();
    let env = strings.take_while(|s| !s.is_empty()).collect();
    (args, env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_procargs_layout() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/sleep\0\0\0\0sleep\x0060\0HOME=/Users/x\0TERM=xterm\0\0\0");
        let (args, env) = parse_procargs(&buf);
        assert_eq!(args, ["sleep", "60"]);
        assert_eq!(env, ["HOME=/Users/x", "TERM=xterm"]);
    }

    #[test]
    fn reads_this_process() {
        let me = std::process::id();
        let all = Lister.list();
        let p = all.iter().find(|p| p.pid == me).unwrap();
        assert_eq!(p.ppid, Some(std::os::unix::process::parent_id()), "kinfo_proc offsets");
        assert!(p.working_set > 0 && p.private_bytes > 0 && p.start_time > 1_700_000_000);
        // launchd belongs to root yet is listed, so adopted processes keep a parent.
        assert!(all.iter().any(|p| p.pid == 1));

        // CPU time agrees with what the kernel reports to the process itself.
        let mut own = MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: getrusage fills the struct.
        let own = unsafe {
            libc::getrusage(libc::RUSAGE_SELF, own.as_mut_ptr());
            own.assume_init()
        };
        let tv_ns = |tv: libc::timeval| tv.tv_sec as u64 * 1_000_000_000 + tv.tv_usec as u64 * 1000;
        let own_ns = tv_ns(own.ru_utime) + tv_ns(own.ru_stime);
        let listed = Lister.list().into_iter().find(|p| p.pid == me).unwrap().cpu_ns;
        assert!(
            listed >= own_ns / 2 && listed <= own_ns * 2 + 50_000_000,
            "listed {listed} ns vs getrusage {own_ns} ns"
        );
        assert_eq!(start_time(me), Some(p.start_time));
        let d = details(me);
        assert!(d.exe.is_some() && !d.cmd.is_empty() && d.cwd.is_some());
        assert!(d.env.unwrap().iter().any(|e| e.starts_with("PATH=")));
    }
}
