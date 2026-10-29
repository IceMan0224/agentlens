//! Linux: procfs. `/proc/<pid>/stat` and `statm` every tick; `exe`, `cmdline`, `cwd` and
//! `environ` once per process.

use std::fs;
use std::io::Read;
use std::sync::OnceLock;

use super::{Details, RawProc};

struct Clock {
    ticks_per_sec: u64,
    /// Unix seconds at boot; process start times are offsets from it.
    boot_time: u64,
    page_size: u64,
}

fn clock() -> &'static Clock {
    static CLOCK: OnceLock<Clock> = OnceLock::new();
    CLOCK.get_or_init(|| {
        // SAFETY: sysconf has no preconditions.
        let (ticks, page) = unsafe { (libc::sysconf(libc::_SC_CLK_TCK), libc::sysconf(libc::_SC_PAGESIZE)) };
        let boot_time = fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok()))
            .unwrap_or(0);
        Clock {
            ticks_per_sec: ticks.max(1) as u64,
            boot_time,
            page_size: page.max(1) as u64,
        }
    })
}

#[derive(Default)]
pub struct Lister {
    /// Reused for every `/proc` file read in a listing: hundreds per second otherwise.
    buf: String,
}

impl Lister {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&mut self) -> Vec<RawProc> {
        let Ok(dir) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let c = clock();
        let mut out = Vec::with_capacity(512);
        for entry in dir.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            // Gone between listing and reading, or a zombie: skip.
            let Some(stat) = read_into(&mut self.buf, pid, "stat")
                .and_then(parse_stat)
                .filter(|s| !s.zombie)
            else {
                continue;
            };
            let (resident, shared) = read_into(&mut self.buf, pid, "statm")
                .and_then(parse_statm)
                .unwrap_or((0, 0));
            out.push(RawProc {
                pid,
                ppid: (stat.ppid != 0).then_some(stat.ppid),
                name: stat.comm,
                start_time: c.boot_time + stat.start_ticks / c.ticks_per_sec,
                cpu_ns: (stat.utime + stat.stime) * 1_000_000_000 / c.ticks_per_sec,
                private_bytes: resident.saturating_sub(shared) * c.page_size,
                working_set: resident * c.page_size,
            });
        }
        out
    }
}

struct Stat {
    /// Truncated to 15 bytes by the kernel; the collector prefers the exe name.
    comm: String,
    zombie: bool,
    ppid: u32,
    utime: u64,
    stime: u64,
    start_ticks: u64,
}

/// Parses `/proc/<pid>/stat`. The command name is in parentheses and may itself contain
/// spaces or parentheses, so fields are counted from the last `)`.
fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // rest[0] is field 3 (state); utime, stime and starttime are fields 14, 15 and 22.
    Some(Stat {
        comm: text.get(open + 1..close)?.to_string(),
        zombie: matches!(rest.first(), Some(&"Z") | Some(&"X")),
        ppid: rest.get(1)?.parse().ok()?,
        utime: rest.get(11)?.parse().ok()?,
        stime: rest.get(12)?.parse().ok()?,
        start_ticks: rest.get(19)?.parse().ok()?,
    })
}

fn read_into<'a>(buf: &'a mut String, pid: u32, file: &str) -> Option<&'a str> {
    buf.clear();
    fs::File::open(format!("/proc/{pid}/{file}"))
        .ok()?
        .read_to_string(buf)
        .ok()?;
    Some(buf.as_str())
}

fn read_stat(pid: u32) -> Option<Stat> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// (resident, shared) in pages, from `/proc/<pid>/statm`.
fn parse_statm(text: &str) -> Option<(u64, u64)> {
    let mut fields = text.split_whitespace().skip(1);
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

/// Start time in Unix seconds, or `None` once the process exited or became a zombie.
pub fn start_time(pid: u32) -> Option<u64> {
    let stat = read_stat(pid).filter(|s| !s.zombie)?;
    let c = clock();
    Some(c.boot_time + stat.start_ticks / c.ticks_per_sec)
}

/// Everything is readable for the user's own processes; other users' come back empty.
pub fn details(pid: u32) -> Details {
    let link = |name: &str| {
        fs::read_link(format!("/proc/{pid}/{name}"))
            .ok()
            .map(|p| p.to_string_lossy().trim_end_matches(" (deleted)").to_string())
    };
    let nul_list = |name: &str| -> Vec<String> {
        fs::read(format!("/proc/{pid}/{name}"))
            .map(|bytes| {
                bytes
                    .split(|b| *b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    Details {
        exe: link("exe"),
        cmd: nul_list("cmdline"),
        cwd: link("cwd"),
        env: Some(nul_list("environ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_awkward_command_names() {
        let line = "4242 (tmux: server (1)) S 1 4242 4242 0 -1 4194560 100 0 0 0 7 3 0 0 20 0 1 0 12345 1000 200";
        let s = parse_stat(line).unwrap();
        assert_eq!(s.comm, "tmux: server (1)");
        assert!(!s.zombie);
        assert_eq!((s.ppid, s.utime, s.stime, s.start_ticks), (1, 7, 3, 12345));
        assert!(
            parse_stat("1 (z) Z 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 9")
                .unwrap()
                .zombie
        );
    }

    #[test]
    fn reads_this_process() {
        let me = std::process::id();
        let all = Lister::default().list();
        let p = all.iter().find(|p| p.pid == me).unwrap();
        assert!(p.ppid.is_some() && p.working_set > 0 && p.start_time > 1_700_000_000);
        assert_eq!(start_time(me), Some(p.start_time));
        let d = details(me);
        assert!(d.exe.is_some() && !d.cmd.is_empty() && d.cwd.is_some());
        assert!(d.env.unwrap().iter().any(|e| e.starts_with("PATH=")));
    }
}
