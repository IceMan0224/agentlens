//! User rules in `rules.toml` (see `rules_path`): the catalog's tables, plus settings.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::{Catalog, HostDef};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// A session whose processes stay below the activity threshold this long is idle.
    pub idle_minutes: u64,
    /// Substrings of a process name or command line that must never be killed.
    pub protect: Vec<String>,
    /// Dashboard language.
    pub language: Language,
    /// Ends the proposed cleanup candidates this often while the dashboard runs; 0 is off.
    pub auto_cleanup_minutes: u64,
    /// Automatic cleanup also ends idle sessions, not only proposed candidates.
    pub auto_cleanup_idle: bool,
    /// Also propose leftovers of ended sessions that show a window (a browser an agent opened).
    pub end_windowed: bool,
    /// Also propose leftovers a live session is still connected to (a shared build daemon).
    pub end_shared: bool,
    /// Extends the built-in catalog: same tables as `catalog.toml`.
    #[serde(flatten)]
    pub catalog: Catalog,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Ko,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            idle_minutes: 30,
            protect: Vec::new(),
            language: Language::default(),
            auto_cleanup_minutes: 0,
            auto_cleanup_idle: false,
            end_windowed: false,
            end_shared: false,
            catalog: Catalog::default(),
        }
    }
}

/// `%APPDATA%\agentlens\rules.toml` on Windows, `$XDG_CONFIG_HOME` (or `~/.config`)
/// `/agentlens/rules.toml` elsewhere.
pub fn rules_path() -> Option<PathBuf> {
    let dir = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        }
    };
    Some(dir.join("agentlens").join("rules.toml"))
}

impl Rules {
    /// A missing file means defaults; a malformed one is an error so edits are not silently lost.
    pub fn load() -> Result<Self, String> {
        let Some(path) = rules_path() else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let path = rules_path().ok_or("no config directory (APPDATA/HOME not set)")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// Sets one top-level setting in the rules file (see `set_value`).
    pub fn save_setting(key: &str, value: &str) -> Result<PathBuf, String> {
        let path = rules_path().ok_or("no config directory (APPDATA/HOME not set)")?;
        set_value(&path, key, value)?;
        Ok(path)
    }

    /// What decides which cleanup candidates are proposed.
    pub fn policy(&self) -> crate::health::Policy {
        crate::health::Policy {
            idle_minutes: self.idle_minutes,
            owner_gone_grace: crate::origin::OWNER_GONE_GRACE_SECS,
            end_windowed: self.end_windowed,
            end_shared: self.end_shared,
        }
    }

    /// Registers an executable as a host in the rules file (see `append_host`).
    pub fn register_host(exe_stem: &str) -> Result<PathBuf, String> {
        let path = rules_path().ok_or("no config directory (APPDATA/HOME not set)")?;
        append_host(&path, exe_stem)?;
        Ok(path)
    }
}

fn read_existing(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, text).map_err(|e| e.to_string())
}

/// Sets a top-level `key = value` (`value` is TOML, e.g. `"ko"` or `15`) by rewriting only its
/// line, or adding one at the top, so comments and edits not loaded yet are kept. Refuses to
/// touch a file that does not parse, and to write one that would not.
pub fn set_value(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let existing = read_existing(path)?;
    toml::from_str::<Rules>(&existing).map_err(|e| format!("{}: {e}", path.display()))?;
    let entry = format!("{key} = {value}");
    let mut lines: Vec<&str> = existing.lines().collect();
    // Top-level keys come before the first table header.
    let top = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    let found = lines[..top].iter().position(|l| {
        l.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    });
    match found {
        Some(i) => lines[i] = &entry,
        None => lines.insert(0, &entry),
    }
    let text = lines.join(
        "
",
    ) + "
";
    toml::from_str::<Rules>(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    write_file(path, &text)
}

/// Appends a `[[hosts]]` entry to the file instead of rewriting it, so the user's comments
/// and any edits not loaded yet are kept. Does nothing when the executable is already a host
/// there, and refuses to touch a file that does not parse, or to write one that would not.
pub fn append_host(path: &Path, exe_stem: &str) -> Result<(), String> {
    let existing = read_existing(path)?;
    let current: Rules = toml::from_str(&existing).map_err(|e| format!("{}: {e}", path.display()))?;
    let stem = exe_stem.to_lowercase();
    if current
        .catalog
        .hosts
        .iter()
        .any(|h| h.exe.iter().any(|e| e.to_lowercase() == stem))
    {
        return Ok(());
    }
    #[derive(Serialize)]
    struct Hosts<'a> {
        hosts: [&'a HostDef; 1],
    }
    let host = HostDef {
        id: stem.clone(),
        name: exe_stem.to_string(),
        exe: vec![stem],
        ..HostDef::default()
    };
    let entry = toml::to_string(&Hosts { hosts: [&host] }).map_err(|e| e.to_string())?;
    let separator = if existing.is_empty() || existing.ends_with("\n\n") {
        ""
    } else if existing.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let text = format!("{existing}{separator}{entry}");
    // `hosts = [...]` written as an array cannot be extended by a `[[hosts]]` table.
    toml::from_str::<Rules>(&text).map_err(|e| {
        format!(
            "{}: a [[hosts]] entry cannot be appended here; add it by hand ({e})",
            path.display()
        )
    })?;
    write_file(path, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_rules_with_defaults() {
        let rules: Rules = toml::from_str(
            r#"
            protect = ["dev-server"]
            [[hosts]]
            id = "myterm"
            exe = ["myterm"]
            "#,
        )
        .unwrap();
        assert_eq!(rules.idle_minutes, 30);
        assert_eq!(rules.catalog.hosts[0].exe, ["myterm"]);
        assert_eq!(rules.protect, ["dev-server"]);
    }

    #[test]
    fn append_host_keeps_comments_and_unloaded_edits() {
        let dir = std::env::temp_dir().join(format!("agentlens-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.toml");
        // Edited in the editor and saved, never reloaded into the dashboard.
        std::fs::write(&path, "# my notes\nprotect = [\"prod-server\"]\n").unwrap();
        append_host(&path, "Foo").unwrap();
        append_host(&path, "foo").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(text.starts_with("# my notes\nprotect = [\"prod-server\"]\n"), "{text}");
        let rules: Rules = toml::from_str(&text).unwrap();
        assert_eq!(rules.protect, ["prod-server"]);
        assert_eq!(rules.catalog.hosts.len(), 1, "registered once:\n{text}");
        assert_eq!(rules.catalog.hosts[0].exe, ["foo"]);
    }

    #[test]
    fn set_value_rewrites_only_its_line() {
        let dir = std::env::temp_dir().join(format!("agentlens-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.toml");
        std::fs::write(
            &path,
            "# notes
idle_minutes = 30 # half an hour

[[hosts]]
id = \"t\"
",
        )
        .unwrap();
        set_value(&path, "idle_minutes", "15").unwrap();
        set_value(&path, "language", "\"ko\"").unwrap();
        set_value(&path, "language", "\"en\"").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // A file that does not parse is left alone.
        std::fs::write(&path, "idle_minutes = [").unwrap();
        assert!(set_value(&path, "language", "\"ko\"").is_err());
        let broken = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            text,
            "language = \"en\"
# notes
idle_minutes = 15

[[hosts]]
id = \"t\"
"
        );
        assert_eq!(broken, "idle_minutes = [");
        let rules: Rules = toml::from_str(&text).unwrap();
        assert_eq!((rules.idle_minutes, rules.language), (15, Language::En));
    }

    #[test]
    fn append_host_leaves_inline_host_arrays_alone() {
        let dir = std::env::temp_dir().join(format!("agentlens-inline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.toml");
        let inline = "hosts = [{ id = \"x\", exe = [\"x\"] }]\n";
        std::fs::write(&path, inline).unwrap();
        let result = append_host(&path, "foo");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(result.is_err());
        assert_eq!(text, inline, "the file still parses");
    }

    #[test]
    fn saved_rules_omit_empty_tables() {
        let text = toml::to_string_pretty(&Rules::default()).unwrap();
        assert!(!text.contains("processes") && !text.contains("hosts"), "{text}");
    }
}
