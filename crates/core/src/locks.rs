//! Who holds a file or folder, for when replacing, moving or deleting it fails ("resource busy
//! or locked", "being used by another process"): processes that run an executable from it,
//! work in it, or (Windows) have a file or folder in it open.

use std::path::{Path, PathBuf};

use crate::model::Model;
use crate::sys;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// Its executable is inside.
    Runs,
    /// Its working directory is inside.
    WorksIn,
    /// It has a file or folder inside open.
    Opens,
}

#[derive(Debug, Clone)]
pub struct Holder {
    pub pid: u32,
    pub name: String,
    pub holds: Vec<Hold>,
    /// For `Opens`: the first file or folder it has open.
    pub open: Option<String>,
    /// The process is in the model; false for processes that started after it.
    pub listed: bool,
}

#[derive(Debug, Clone)]
pub struct Holders {
    /// The path asked about, absolute.
    pub path: String,
    /// Ordered by PID.
    pub holders: Vec<Holder>,
    /// Open files were checked (Windows only).
    pub open_checked: bool,
}

pub fn find(model: &Model, path: &Path) -> Result<Holders, String> {
    let root = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let root = PathBuf::from(plain(&root.to_string_lossy()));
    let root_text = root.to_string_lossy().into_owned();
    let mut holders: Vec<Holder> = Vec::new();
    for (pid, p) in &model.procs {
        if p.exe.as_deref().is_some_and(|e| within(e, &root_text)) {
            add(&mut holders, *pid, &p.name, Hold::Runs, true);
        }
        if p.cwd.as_deref().is_some_and(|c| within(c, &root_text)) {
            add(&mut holders, *pid, &p.name, Hold::WorksIn, true);
        }
    }
    let open = sys::open_files();
    for (pid, path) in open.iter().flatten() {
        let path = plain(path);
        if !within(&path, &root_text) {
            continue;
        }
        let h = match model.procs.get(pid) {
            // Every process holds its working directory open; `WorksIn` already says so.
            Some(p) if p.cwd.as_deref().is_some_and(|c| norm(c) == norm(&path)) => continue,
            Some(p) => add(&mut holders, *pid, &p.name, Hold::Opens, true),
            None => {
                let exe = sys::details(*pid).exe.unwrap_or_default();
                let name = exe.rsplit(['/', '\\']).next().unwrap_or_default();
                add(&mut holders, *pid, name, Hold::Opens, false)
            }
        };
        h.open.get_or_insert(path);
    }
    holders.sort_by_key(|h| h.pid);
    Ok(Holders {
        path: root_text,
        holders,
        open_checked: open.is_some(),
    })
}

/// The holder row for `pid`, created when missing, with `hold` added.
fn add<'a>(holders: &'a mut Vec<Holder>, pid: u32, name: &str, hold: Hold, listed: bool) -> &'a mut Holder {
    let i = match holders.iter().position(|h| h.pid == pid) {
        Some(i) => i,
        None => {
            holders.push(Holder {
                pid,
                name: name.to_string(),
                holds: Vec::new(),
                open: None,
                listed,
            });
            holders.len() - 1
        }
    };
    let h = &mut holders[i];
    if !h.holds.contains(&hold) {
        h.holds.push(hold);
    }
    h
}

/// `path` relative to `root` when it lies inside ("." for `root` itself), else as it is.
pub fn relative<'a>(path: &'a str, root: &str) -> &'a str {
    if !within(path, root) {
        return path;
    }
    match path.get(root.len()..).map(|rest| rest.trim_matches(['/', '\\'])) {
        Some("") => ".",
        Some(rest) => rest,
        None => path,
    }
}

/// Without the `\\?\` prefix `canonicalize` adds on Windows, which process paths never have.
fn plain(path: &str) -> String {
    match path.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => rest.to_string(),
        },
        None => path.to_string(),
    }
}

/// `path` is `root` or inside it. Windows paths compare without case and with either slash.
fn within(path: &str, root: &str) -> bool {
    let (path, root) = (norm(path), norm(root));
    let sep = if cfg!(windows) { '\\' } else { '/' };
    path == root || path.strip_prefix(&root).is_some_and(|rest| rest.starts_with(sep))
}

fn norm(path: &str) -> String {
    let path = path.trim_end_matches(['/', '\\']);
    match cfg!(windows) {
        true => path.replace('/', "\\").to_lowercase(),
        false => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::snapshot::{ProcInfo, Snapshot};

    fn proc(pid: u32, exe: &str, cwd: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid: None,
            name: format!("p{pid}"),
            exe: Some(exe.into()),
            cmd: Vec::new(),
            cwd: Some(cwd.into()),
            env: Vec::new(),
            start_time: 1,
            cpu: 0.0,
            private_bytes: 0,
            working_set: 0,
        }
    }

    fn model(processes: Vec<ProcInfo>) -> Model {
        let snapshot = Snapshot {
            taken_at: 2,
            processes,
            tcp: Vec::new(),
            listening: Vec::new(),
            cores: 1,
            containers: None,
        };
        Model::build(snapshot, &Catalog::builtin())
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agentlens-locks-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        dir
    }

    #[test]
    fn finds_executables_and_working_directories_inside() {
        let dir = temp_dir("exe");
        let root = plain(&std::fs::canonicalize(&dir).unwrap().to_string_lossy());
        let sep = std::path::MAIN_SEPARATOR;
        let m = model(vec![
            proc(10, &format!("{root}{sep}bin{sep}tool.exe"), "/elsewhere"),
            proc(11, "/usr/bin/sh", &format!("{root}{sep}bin{sep}")),
            // Shares the name's prefix but is a sibling folder.
            proc(12, &format!("{root}-old{sep}tool.exe"), &format!("{root}-old")),
        ]);
        let found = find(&m, &dir).unwrap();
        let holds: Vec<(u32, Vec<Hold>)> = found
            .holders
            .iter()
            .filter(|h| h.listed)
            .map(|h| (h.pid, h.holds.clone()))
            .collect();
        assert_eq!(holds, [(10, vec![Hold::Runs]), (11, vec![Hold::WorksIn])]);
        assert_eq!(
            relative(&format!("{root}{sep}bin{sep}tool.exe"), &found.path),
            format!("bin{sep}tool.exe")
        );
        assert_eq!(relative(&format!("{root}{sep}"), &found.path), ".");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_paths_are_an_error() {
        assert!(find(&model(Vec::new()), Path::new("/no/such/agentlens/path")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn finds_who_has_a_file_open() {
        let dir = temp_dir("open");
        let file = dir.join("bin").join("held.txt");
        // ping writes its output into the file, so it holds it open while it runs.
        let mut child = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::fs::File::create(&file).unwrap())
            .spawn()
            .unwrap();
        // The child is not in the model: it is reported, but as one the model does not have.
        let found = find(&model(Vec::new()), &dir).unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let held = found.holders.iter().find(|h| h.pid == child.id());
        assert!(found.open_checked);
        let opened = held.and_then(|h| h.open.as_deref()).map(|o| relative(o, &found.path));
        assert_eq!(opened, Some(r"bin\held.txt"), "{found:?}");
        assert!(held.is_some_and(|h| h.holds == [Hold::Opens] && !h.listed && h.name.eq_ignore_ascii_case("ping.exe")));
        let found = find(&model(Vec::new()), &file).unwrap();
        assert!(found.holders.iter().all(|h| h.pid != child.id()), "{found:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn compares_paths_by_component() {
        assert!(within("/a/b/c", "/a/b"));
        assert!(within("/a/b/", "/a/b"));
        assert!(!within("/a/bc", "/a/b"));
        if cfg!(windows) {
            assert!(within(r"C:\Users\Me\X\a.exe", "c:/users/me/x/"));
            assert_eq!(plain(r"\\?\C:\x"), r"C:\x");
            assert_eq!(plain(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        }
    }
}
