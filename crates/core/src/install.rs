//! Finds where catalog agents are installed so `node <script>` launches can be matched.

use std::path::Path;

use crate::catalog::{Catalog, normalize};

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
