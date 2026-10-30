//! Finds where catalog agents are installed so `node <script>` launches can be matched, and
//! which catalog hosts this machine has.

use std::path::{Path, PathBuf};

use crate::catalog::{Catalog, HostDef, normalize};

const SHIM_EXTS: &[&str] = &["cmd", "ps1", "bat"];

/// Returns (agent id, fragment) pairs: the resolved exe path for native installs, or the
/// `node_modules/...` target of npm-style launchers. On Windows those are `.cmd`/`.ps1` shims;
/// on macOS and Linux, extensionless symlinks into `node_modules` or shell scripts.
pub fn scan(catalog: &Catalog) -> Vec<(String, String)> {
    let Some(path_var) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    // List each PATH directory once: probing every command and extension separately is
    // thousands of file system calls on a long PATH.
    for dir in std::env::split_paths(&path_var) {
        if is_system_dir(catalog, &dir) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            let (command, ext) = name.rsplit_once('.').unwrap_or((name.as_str(), ""));
            let Some(agent) = catalog.agents.iter().find(|a| a.exe.iter().any(|e| e == command)) else {
                continue;
            };
            if ext == "exe" {
                push_unique(&mut found, &agent.id, normalize(&entry.path().to_string_lossy()));
            } else if SHIM_EXTS.contains(&ext)
                && let Ok(text) = std::fs::read_to_string(entry.path())
            {
                for target in shim_targets(&text) {
                    push_unique(&mut found, &agent.id, target);
                }
            } else if ext.is_empty() {
                for target in extensionless_targets(&entry.path()) {
                    push_unique(&mut found, &agent.id, target);
                }
            }
        }
    }
    found
}

/// Ids of the hosts with a command on `PATH` or an entry in an app launcher. Each system keeps
/// only some of these directories; the others are simply not there.
pub fn scan_hosts(catalog: &Catalog) -> Vec<String> {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let commands: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    let mut launchers: Vec<PathBuf> = [
        "/Applications",
        "/System/Applications/Utilities",
        "/usr/share/applications",
        "/usr/local/share/applications",
        "/var/lib/flatpak/exports/share/applications",
        "/var/lib/snapd/desktop/applications",
    ]
    .map(PathBuf::from)
    .into();
    let start_menu = Path::new("Microsoft/Windows/Start Menu/Programs");
    launchers.extend(var("APPDATA").map(|d| d.join(start_menu)));
    launchers.extend(var("ProgramData").map(|d| d.join(start_menu)));
    if let Some(home) = var("HOME") {
        launchers.push(home.join("Applications"));
        let data = var("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        launchers.push(data.join("applications"));
        launchers.push(data.join("flatpak/exports/share/applications"));
    }
    hosts_in(catalog, &commands, &launchers)
}

fn hosts_in(catalog: &Catalog, commands: &[PathBuf], launchers: &[PathBuf]) -> Vec<String> {
    let mut found = Vec::new();
    let mut add = |matches: &dyn Fn(&HostDef) -> bool| {
        for host in catalog.hosts.iter().filter(|h| matches(h)) {
            if !found.contains(&host.id) {
                found.push(host.id.clone());
            }
        }
    };
    for entry in commands
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|list| list.flatten())
    {
        // App execution aliases on Windows are neither files nor folders here.
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let (command, ext) = name.rsplit_once('.').unwrap_or((name.as_str(), ""));
        if ext.is_empty() || ext == "exe" || SHIM_EXTS.contains(&ext) {
            add(&|h| h.exe.iter().chain(&h.command).any(|e| e == command));
        }
    }
    let mut names = Vec::new();
    for dir in launchers {
        launcher_names(dir, LAUNCHER_DEPTH, &mut names);
    }
    for name in &names {
        let Some((stem, ext)) = name.rsplit_once('.') else {
            continue;
        };
        if !LAUNCHER_EXTS.contains(&ext) {
            continue;
        }
        // `.desktop` files are often named by reverse domain: `dev.zed.Zed`.
        let last = stem.rsplit('.').next().unwrap_or(stem);
        let bundle = format!("/{name}/");
        add(&|h| {
            h.path.contains(&bundle)
                || h.exe.iter().chain(&h.command).any(|e| e == stem || e == last)
                || h.app.iter().any(|a| {
                    a == stem || a == last || stem.strip_prefix(a.as_str()).is_some_and(|r| r.starts_with(' '))
                })
        });
    }
    found
}

const LAUNCHER_EXTS: &[&str] = &["app", "desktop", "lnk"];

/// Start Menu entries sit in a folder per vendor.
const LAUNCHER_DEPTH: u32 = 2;

/// Lowercase file names under `dir`. A macOS bundle is a folder, and counts as one entry.
fn launcher_names(dir: &Path, depth: u32, names: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if entry.file_type().is_ok_and(|t| t.is_dir()) && !name.ends_with(".app") {
            if depth > 0 {
                launcher_names(&entry.path(), depth - 1, names);
            }
        } else {
            names.push(name);
        }
    }
}

fn is_system_dir(catalog: &Catalog, dir: &Path) -> bool {
    let dir = format!("{}/", normalize(&dir.to_string_lossy()).trim_end_matches('/'));
    catalog
        .processes
        .system_dirs
        .iter()
        .any(|d| dir.starts_with(d.as_str()))
}

/// An extensionless command: a symlink (npm links straight into `node_modules`), a script
/// naming its target, or on Unix a native binary identified by its resolved path.
fn extensionless_targets(path: &Path) -> Vec<String> {
    let resolved = std::fs::canonicalize(path)
        .map(|p| normalize(&p.to_string_lossy()))
        .unwrap_or_default();
    if let Some(i) = resolved.find("node_modules/") {
        return vec![resolved[i..].to_string()];
    }
    match script_text(path) {
        Some(text) => shim_targets(&text),
        None if cfg!(unix) && !resolved.is_empty() => vec![resolved],
        None => Vec::new(),
    }
}

/// Launcher scripts are short; native binaries (a native Claude Code is over 100 MB) are not
/// read past their first two bytes.
const SCRIPT_LIMIT: u64 = 64 * 1024;

/// The text of a `#!` script, or `None` for anything else.
fn script_text(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic).ok()?;
    if &magic != b"#!" {
        return None;
    }
    let mut rest = Vec::new();
    file.take(SCRIPT_LIMIT).read_to_end(&mut rest).ok()?;
    Some(String::from_utf8_lossy(&rest).into_owned())
}

fn push_unique(found: &mut Vec<(String, String)>, id: &str, fragment: String) {
    if !found.iter().any(|(i, f)| i == id && *f == fragment) {
        found.push((id.to_string(), fragment));
    }
}

/// Extracts `node_modules/...` paths from a shim script, e.g.
/// `node "%~dp0\node_modules\@zcode\cli\dist\zcode.cjs" %*` -> `node_modules/@zcode/cli/dist/zcode.cjs`.
pub fn shim_targets(text: &str) -> Vec<String> {
    let text = normalize(text);
    let mut targets = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("node_modules/") {
        let tail = &rest[start..];
        let end = tail
            .find(['"', '\'', ' ', '%', '$', '\r', '\n', '`'])
            .unwrap_or(tail.len());
        let target = &tail[..end];
        // Bare package dirs (e.g. a `node_modules/` prefix check) are too broad to identify an agent.
        if target.matches('/').count() >= 2 && !targets.iter().any(|t| t == target) {
            targets.push(target.to_string());
        }
        rest = &tail[end..];
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_hosts_on_path_and_in_each_systems_launchers() {
        let root = std::env::temp_dir().join(format!("agentlens-hosts-{}", std::process::id()));
        let (bin, apps) = (root.join("bin"), root.join("apps"));
        std::fs::create_dir_all(bin.join("zed")).unwrap(); // A folder, not the command.
        std::fs::create_dir_all(apps.join("Ghostty.app")).unwrap();
        std::fs::create_dir_all(apps.join("JetBrains")).unwrap();
        for file in [
            "bin/wt.exe",
            "bin/tmux",
            "bin/orca.txt",
            "apps/Visual Studio Code.lnk",
            "apps/JetBrains/IntelliJ IDEA 2026.1.lnk",
            "apps/org.wezfurlong.wezterm.desktop",
            "apps/org.gnome.Terminal.desktop",
            "apps/Wave Link.lnk",
        ] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let mut found = hosts_in(&Catalog::builtin(), &[bin], &[apps]);
        std::fs::remove_dir_all(&root).unwrap();
        found.sort();
        assert_eq!(
            found,
            [
                "ghostty",
                "jetbrains",
                "linux-terminal",
                "tmux",
                "vscode",
                "wezterm",
                "windows-terminal"
            ]
        );
    }

    #[test]
    fn reads_scripts_but_not_binaries() {
        let dir = std::env::temp_dir().join(format!("agentlens-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("zcode");
        std::fs::write(
            &script,
            "#!/bin/sh\nexec node \"$basedir/node_modules/@zcode/cli/dist/zcode.cjs\" \"$@\"\n",
        )
        .unwrap();
        let binary = dir.join("agent");
        std::fs::write(&binary, [0x7f, b'E', b'L', b'F', 0, 0]).unwrap();
        assert_eq!(
            script_text(&script).map(|t| shim_targets(&t)),
            Some(vec!["node_modules/@zcode/cli/dist/zcode.cjs".to_string()])
        );
        assert_eq!(script_text(&binary), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extracts_targets_from_npm_shims() {
        let cmd = "@ECHO off\r\nnode \"%~dp0\\node_modules\\@zcode\\cli\\dist\\zcode.cjs\" %*\r\n";
        assert_eq!(shim_targets(cmd), ["node_modules/@zcode/cli/dist/zcode.cjs"]);

        let exe_shim = "\"%dp0%\\node_modules\\@opencode\\cli\\bin\\opencode.exe\"   %*";
        assert_eq!(shim_targets(exe_shim), ["node_modules/@opencode/cli/bin/opencode.exe"]);

        let ps1 = "& \"$basedir/node$exe\"  \"$basedir/node_modules/@openai/codex/bin/codex.js\" $args\n& \"node$exe\"  \"$basedir/node_modules/@openai/codex/bin/codex.js\" $args";
        assert_eq!(shim_targets(ps1), ["node_modules/@openai/codex/bin/codex.js"]);
    }
}
