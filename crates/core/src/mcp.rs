//! Registering agentlens as an MCP server in an agent's own settings, so the agent can ask the
//! dashboard about leftovers. The server (`agentlens mcp`) and the dashboard talk through
//! [`inbox`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::catalog::McpDef;
use crate::hooks::{data_dir, expand, write_checked};

/// The server's name in agents' settings.
pub const NAME: &str = "agentlens";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpState {
    Off,
    On,
    /// Registered with a different agentlens path, such as before the binary moved.
    Outdated,
}

fn config_path(def: &McpDef) -> Result<PathBuf, String> {
    expand(&def.config).ok_or_else(|| "no home directory".into())
}

/// The settings file as JSON, whatever its format.
fn parse(def: &McpDef, text: &str) -> Result<Value, String> {
    match def.format.as_str() {
        "json" => serde_json::from_str(text).map_err(|e| e.to_string()),
        "toml" => {
            let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
            serde_json::to_value(table).map_err(|e| e.to_string())
        }
        other => Err(format!("unknown settings format {other}")),
    }
}

/// The entry agentlens at `exe` would have.
fn entry(def: &McpDef, exe: &Path) -> Value {
    fn fill(v: &mut Value, exe: &str) {
        match v {
            Value::String(s) => *s = s.replace("{exe}", exe),
            Value::Array(items) => items.iter_mut().for_each(|v| fill(v, exe)),
            Value::Object(map) => map.values_mut().for_each(|v| fill(v, exe)),
            _ => {}
        }
    }
    let mut v = serde_json::to_value(&def.entry).unwrap_or(Value::Null);
    fill(&mut v, &exe.display().to_string());
    v
}

/// Whether `found` has everything `want` has; agents may add fields of their own (`env`).
fn covers(found: &Value, want: &Value) -> bool {
    match (found, want) {
        (Value::Object(f), Value::Object(w)) => w.iter().all(|(k, v)| f.get(k).is_some_and(|fv| covers(fv, v))),
        _ => found == want,
    }
}

fn state_in(def: &McpDef, root: &Value, exe: &Path) -> McpState {
    let found = def
        .key
        .iter()
        .try_fold(root, |v, k| v.get(k))
        .and_then(|servers| servers.get(NAME));
    match found {
        None => McpState::Off,
        Some(found) if covers(found, &entry(def, exe)) => McpState::On,
        Some(_) => McpState::Outdated,
    }
}

fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Whether the agent's settings register agentlens at `exe`.
pub fn state(def: &McpDef, exe: &Path) -> Result<McpState, String> {
    let path = config_path(def)?;
    let Some(text) = read(&path)? else {
        return Ok(McpState::Off);
    };
    let root = parse(def, &text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(state_in(def, &root, exe))
}

/// Whether the agent's settings file exists, a sign the agent is used here.
pub fn config_exists(def: &McpDef) -> bool {
    config_path(def).is_ok_and(|p| p.is_file())
}

/// Adds (`on`) agentlens at `exe` to the agent's MCP servers, replacing an older entry, or
/// removes it. Everything else in the file is kept, and the file is backed up first.
pub fn set(def: &McpDef, exe: &Path, on: bool) -> Result<PathBuf, String> {
    let path = config_path(def)?;
    if !def.add.is_empty() {
        let exe = exe.display().to_string();
        // Adding refuses a name that exists, so an older entry goes first.
        let removed = run(&def.remove, &exe);
        if on {
            run(&def.add, &exe)?;
        } else {
            removed?;
        }
        return Ok(path);
    }
    let existing = read(&path)?;
    let text = match def.format.as_str() {
        "json" => set_json(def, existing.as_deref(), exe, on),
        "toml" => set_toml(def, existing.as_deref(), exe, on),
        other => Err(format!("unknown settings format {other}")),
    }
    .map_err(|e| format!("{}: {e}", path.display()))?;
    let want = if on { McpState::On } else { McpState::Off };
    write_checked(&path, existing.as_deref(), &text, |t| {
        parse(def, t).is_ok_and(|root| state_in(def, &root, exe) == want)
    })?;
    Ok(path)
}

fn set_json(def: &McpDef, existing: Option<&str>, exe: &Path, on: bool) -> Result<String, String> {
    let mut root: Value = match existing {
        Some(text) => serde_json::from_str(text).map_err(|e| e.to_string())?,
        None => json!({}),
    };
    let mut servers = &mut root;
    for key in &def.key {
        let map = servers.as_object_mut().ok_or("settings are not a JSON object")?;
        servers = map.entry(key.clone()).or_insert_with(|| Value::Object(Map::new()));
    }
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| format!("`{}` is not an object", def.key.join(".")))?;
    if on {
        servers.insert(NAME.into(), entry(def, exe));
    } else {
        servers.remove(NAME);
    }
    Ok(serde_json::to_string_pretty(&root).map_err(|e| e.to_string())? + "\n")
}

/// TOML is edited as text so the user's comments and layout stay: our table is cut out and, when
/// adding, appended again.
fn set_toml(def: &McpDef, existing: Option<&str>, exe: &Path, on: bool) -> Result<String, String> {
    let existing = existing.unwrap_or_default();
    toml::from_str::<toml::Table>(existing).map_err(|e| e.to_string())?;
    let ours: Vec<&str> = def.key.iter().map(String::as_str).chain([NAME]).collect();
    let mut out = String::new();
    let mut skipping = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            skipping = !trimmed.starts_with("[[") && table_path(trimmed).starts_with(&ours);
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    let mut out = out.trim_end().to_string();
    if on {
        let mut table = json!({ NAME: entry(def, exe) });
        for key in def.key.iter().rev() {
            table = json!({ key: table });
        }
        let text = toml::to_string(&table).map_err(|e| e.to_string())?;
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(text.trim_end());
    }
    out.push('\n');
    Ok(out)
}

/// The keys of a `[a."b".c]` table header.
fn table_path(header: &str) -> Vec<&str> {
    let inner = header.trim_start_matches('[').split(']').next().unwrap_or_default();
    inner.split('.').map(|k| k.trim().trim_matches(['"', '\''])).collect()
}

/// Runs one of the agent's own commands, with `{exe}` filled in.
fn run(command: &[String], exe: &str) -> Result<(), String> {
    let (program, args) = command.split_first().ok_or("no command")?;
    let program = which(program).ok_or_else(|| format!("{program} not found on PATH"))?;
    let output = std::process::Command::new(&program)
        .args(args.iter().map(|a| a.replace("{exe}", exe)))
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stderr);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        text
    };
    Err(format!("{} failed: {}", command.join(" "), text.trim()))
}

/// A command on PATH; on Windows also its `.cmd` and `.bat` launchers, as npm installs them.
fn which(name: &str) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        ["exe", "cmd", "bat"].iter().map(|e| format!("{name}.{e}")).collect()
    } else {
        vec![name.to_string()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|p| p.is_file())
}

/// Questions from `agentlens mcp` servers to the dashboard, passed as small files in the
/// agentlens state directory: there is no socket or port, and only the user can reach them.
pub mod inbox {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Request {
        pub tool: String,
        #[serde(default)]
        pub args: Value,
        /// PID of the asking server, which runs under the asking agent session.
        pub caller: u32,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Reply {
        pub text: String,
        pub error: bool,
    }

    /// Replies nobody collected (the server was ended while waiting) are removed after this.
    const STALE_SECS: u64 = 60;

    pub fn dir() -> Option<PathBuf> {
        Some(data_dir()?.join("requests"))
    }

    fn write_whole(path: &Path, value: &impl Serialize) -> Result<(), String> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(value).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    /// Asks the dashboard and waits for its reply; an error when none comes within `timeout`.
    pub fn ask(dir: &Path, request: &Request, timeout: Duration) -> Result<Reply, String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = format!("{}-{nanos}", std::process::id());
        let req = dir.join(format!("{id}.req"));
        let reply = dir.join(format!("{id}.reply"));
        write_whole(&req, request)?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(text) = std::fs::read_to_string(&reply) {
                let _ = std::fs::remove_file(&reply);
                return serde_json::from_str(&text).map_err(|e| e.to_string());
            }
            if Instant::now() >= deadline {
                // Withdrawn unless a dashboard already took it.
                let _ = std::fs::remove_file(&req);
                return Err("no dashboard answered".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Takes the waiting requests; with two dashboards open, each request goes to one of them.
    pub fn take(dir: &Path) -> Vec<(String, Request)> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".req")
                && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                let taken = dir.join(format!("{id}.{}.taken", std::process::id()));
                if std::fs::rename(&path, &taken).is_err() {
                    continue;
                }
                let request = std::fs::read_to_string(&taken)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok());
                let _ = std::fs::remove_file(&taken);
                if let Some(request) = request {
                    out.push((id.to_string(), request));
                }
            } else {
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age.as_secs() > STALE_SECS);
                if stale {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        out
    }

    pub fn answer(dir: &Path, id: &str, reply: &Reply) -> Result<(), String> {
        write_whole(&dir.join(format!("{id}.reply")), reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agentlens-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn def(config: &Path, format: &str, key: &[&str], entry: &str) -> McpDef {
        McpDef {
            config: config.display().to_string(),
            format: format.into(),
            key: key.iter().map(|k| k.to_string()).collect(),
            entry: toml::from_str(entry).unwrap(),
            add: Vec::new(),
            remove: Vec::new(),
        }
    }

    #[test]
    fn json_registration_keeps_other_settings() {
        let dir = temp("json");
        let path = dir.join("settings.json");
        let other = r#"{
  "$schema": "https://example.invalid/schema.json",
  "mcp": {
    "other": { "type": "local", "command": ["npx", "other"] }
  },
  "theme": "dark"
}
"#;
        std::fs::write(&path, other).unwrap();
        let d = def(
            &path,
            "json",
            &["mcp"],
            r#"type = "local"
command = ["{exe}", "mcp"]
enabled = true"#,
        );
        let exe = Path::new(r"C:\tools\agentlens.exe");
        assert_eq!(state(&d, exe).unwrap(), McpState::Off);
        set(&d, exe, true).unwrap();
        assert_eq!(state(&d, exe).unwrap(), McpState::On);
        let on: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on["mcp"]["agentlens"]["command"],
            json!([r"C:\tools\agentlens.exe", "mcp"])
        );
        let keys: Vec<&String> = on.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["$schema", "mcp", "theme"], "order kept");
        // The binary moved: the old entry is noticed and replaced.
        let moved = Path::new(r"D:\bin\agentlens.exe");
        assert_eq!(state(&d, moved).unwrap(), McpState::Outdated);
        set(&d, moved, true).unwrap();
        assert_eq!(state(&d, moved).unwrap(), McpState::On);
        set(&d, moved, false).unwrap();
        let off: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(off, serde_json::from_str::<Value>(other).unwrap());
    }

    #[test]
    fn toml_registration_keeps_comments_and_other_tables() {
        let dir = temp("toml");
        let path = dir.join("config.toml");
        let other = "# my settings\nmodel = \"o3\"\n\n[mcp_servers.docs]\ncommand = \"npx\" # docs\nargs = [\"docs\"]\n\n[profiles.fast]\nmodel = \"mini\"\n";
        std::fs::write(&path, other).unwrap();
        let d = def(&path, "toml", &["mcp_servers"], "command = \"{exe}\"\nargs = [\"mcp\"]");
        let exe = Path::new("/usr/local/bin/agentlens");
        set(&d, exe, true).unwrap();
        // A second time replaces, never duplicates, the table.
        set(&d, exe, true).unwrap();
        let on = std::fs::read_to_string(&path).unwrap();
        assert_eq!(state(&d, exe).unwrap(), McpState::On);
        assert!(on.starts_with(other.trim_end()), "{on}");
        assert_eq!(on.matches("[mcp_servers.agentlens]").count(), 1, "{on}");
        set(&d, exe, false).unwrap();
        let off = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(off, other);
    }

    #[test]
    fn settings_that_do_not_parse_are_left_alone() {
        let dir = temp("bad");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{ // comments\n}").unwrap();
        let d = def(&path, "json", &["mcpServers"], "command = \"{exe}\"");
        let result = set(&d, Path::new("agentlens"), true);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(result.is_err());
        assert_eq!(text, "{ // comments\n}");
    }

    #[test]
    fn every_catalog_entry_is_complete() {
        for a in &crate::catalog::Catalog::builtin().agents {
            if let Some(m) = &a.mcp {
                assert!(m.config.starts_with("~/"), "{}", a.id);
                assert!(["json", "toml"].contains(&m.format.as_str()), "{}", a.id);
                assert!(!m.key.is_empty() && !m.entry.is_empty(), "{}", a.id);
                assert!(entry(m, Path::new("X")).to_string().contains("\"X\""), "{}", a.id);
                assert_eq!(m.add.is_empty(), m.remove.is_empty(), "{}", a.id);
            }
        }
    }

    #[test]
    fn a_request_reaches_one_dashboard_and_its_reply_comes_back() {
        let dir = temp("inbox");
        let request = inbox::Request {
            tool: "list_leftovers".into(),
            args: json!({}),
            caller: 7,
        };
        let dashboard = {
            let dir = dir.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    let taken = inbox::take(&dir);
                    if let Some((id, req)) = taken.first() {
                        let reply = inbox::Reply {
                            text: format!("{} from {}", req.tool, req.caller),
                            error: false,
                        };
                        inbox::answer(&dir, id, &reply).unwrap();
                        return taken.len();
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                0
            })
        };
        let reply = inbox::ask(&dir, &request, Duration::from_secs(5)).unwrap();
        assert_eq!(dashboard.join().unwrap(), 1);
        assert_eq!(reply.text, "list_leftovers from 7");
        // Nobody listening: the request is withdrawn.
        assert!(inbox::ask(&dir, &request, Duration::from_millis(200)).is_err());
        let left = std::fs::read_dir(&dir).unwrap().count();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(left, 0);
    }
}
