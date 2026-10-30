//! Agent hooks. agentlens adds itself to an agent's own hook settings (`agentlens hook <agent>`),
//! and each event the agent reports leaves the session's latest state in a small file that the
//! dashboard reads: working, waiting for input, or needing the user.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::catalog::{Catalog, HookDef};
use crate::model::Model;
use crate::snapshot::ProcInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookState {
    Working,
    Waiting,
    /// Asking the user something, such as a permission.
    Attention,
}

/// The state file of one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    agent: String,
    pid: u32,
    start_time: u64,
    state: Option<HookState>,
    /// Unix milliseconds of the event, so an older event finishing late does not win.
    at: u64,
}

/// `%LOCALAPPDATA%\agentlens` on Windows, `$XDG_STATE_HOME` (or `~/.local/state`)`/agentlens`
/// elsewhere.
pub fn data_dir() -> Option<PathBuf> {
    let dir = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else {
        match std::env::var_os("XDG_STATE_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".local").join("state"),
        }
    };
    Some(dir.join("agentlens"))
}

/// Where session state files are kept: `sessions` in [`data_dir`].
pub fn state_dir() -> Option<PathBuf> {
    Some(data_dir()?.join("sessions"))
}

/// `~/` at the start of a settings path is the user's home.
pub(crate) fn expand(path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
            Some(rest.split('/').fold(PathBuf::from(home), |p, part| p.join(part)))
        }
        None => Some(PathBuf::from(path)),
    }
}

/// The state an event means: `<event>:<notification_type>` first, then the event alone.
fn state_of(def: &HookDef, input: &Value) -> Option<String> {
    let event = input.get("hook_event_name")?.as_str()?;
    let detail = input.get("notification_type").and_then(Value::as_str);
    detail
        .and_then(|d| def.events.get(&format!("{event}:{d}")))
        .or_else(|| def.events.get(event))
        .cloned()
}

/// Records one hook event of `agent`, run by `owner` (pid, start time). An `end` event removes
/// the session's file.
pub fn record(
    dir: &Path,
    catalog: &Catalog,
    agent: &str,
    owner: Option<(u32, u64)>,
    input: &str,
) -> Result<(), String> {
    let def = catalog
        .agents
        .iter()
        .find(|a| a.id == agent)
        .and_then(|a| a.hooks.as_ref())
        .ok_or_else(|| format!("no hooks for agent {agent}"))?;
    let input: Value = serde_json::from_str(input).map_err(|e| e.to_string())?;
    let session = input
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .ok_or("no usable session_id")?;
    let path = dir.join(format!("{session}.json"));
    let state = state_of(def, &input).ok_or("event not mapped")?;
    if state == "end" {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let old: Option<Record> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    if old.as_ref().is_some_and(|o| o.at > at) {
        return Ok(());
    }
    let (pid, start_time) = owner
        .or(old.as_ref().map(|o| (o.pid, o.start_time)))
        .ok_or("owner not found")?;
    let state = match state.as_str() {
        "working" => Some(HookState::Working),
        "waiting" => Some(HookState::Waiting),
        "attention" => Some(HookState::Attention),
        // `start`: the session and its owner, no state yet.
        _ => old.and_then(|o| o.state),
    };
    let record = Record {
        agent: agent.to_string(),
        pid,
        start_time,
        state,
        at,
    };
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // Written whole, then renamed, so the dashboard never reads half a file.
    let tmp = dir.join(format!("{session}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(&record).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// The agent process this hook runs under: the nearest ancestor the catalog calls an agent.
pub fn owner(catalog: &Catalog) -> Option<(u32, u64)> {
    let procs: HashMap<u32, crate::sys::RawProc> = crate::sys::Lister::new()
        .list()
        .into_iter()
        .map(|p| (p.pid, p))
        .collect();
    let mut pid = procs.get(&std::process::id())?.ppid?;
    for _ in 0..32 {
        let raw = procs.get(&pid)?;
        let d = crate::sys::details(pid);
        let info = ProcInfo {
            pid,
            ppid: raw.ppid,
            name: raw.name.clone(),
            exe: d.exe,
            cmd: d.cmd,
            cwd: None,
            env: Vec::new(),
            start_time: raw.start_time,
            cpu: 0.0,
            private_bytes: 0,
            working_set: 0,
        };
        if crate::model::match_agent(catalog, &info).is_some() {
            return Some((pid, raw.start_time));
        }
        pid = raw.ppid?;
    }
    None
}

/// The latest hook state of each running session, by its agent process (pid, start time).
/// Files of processes no longer running are removed once a day old.
pub fn read_states(dir: &Path, model: &Model) -> HashMap<(u32, u64), HookState> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let record: Option<Record> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
        let running = record
            .as_ref()
            .filter(|r| model.procs.get(&r.pid).is_some_and(|p| p.start_time == r.start_time));
        match running {
            Some(r) => {
                if let Some(state) = r.state {
                    out.insert((r.pid, r.start_time), state);
                }
            }
            None => {
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age.as_secs() > 86_400);
                if old {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    out
}

/// One of ours: `<…>/agentlens[.exe] hook …`.
fn is_ours(entry: &Value) -> bool {
    let program = entry.get("command").and_then(Value::as_str).unwrap_or_default();
    let file = program.rsplit(['/', '\\']).next().unwrap_or_default().to_lowercase();
    let first_arg = entry.get("args").and_then(|a| a.get(0)).and_then(Value::as_str);
    file.trim_end_matches(".exe") == "agentlens" && first_arg == Some("hook")
}

fn settings_path(def: &HookDef) -> Result<PathBuf, String> {
    expand(&def.settings).ok_or_else(|| "no home directory".into())
}

/// The agent events agentlens registers for (`Notification:idle_prompt` registers `Notification`).
fn events(def: &HookDef) -> Vec<&str> {
    let mut out: Vec<&str> = def.events.keys().map(|k| k.split(':').next().unwrap_or(k)).collect();
    out.dedup();
    out
}

/// Whether agentlens is registered in the agent's settings for every event it needs.
pub fn registered(def: &HookDef) -> Result<bool, String> {
    let path = settings_path(def)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let root: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(events(def).iter().all(|event| {
        root.pointer(&format!("/hooks/{event}"))
            .and_then(Value::as_array)
            .is_some_and(|groups| {
                groups
                    .iter()
                    .filter_map(|g| g.get("hooks").and_then(Value::as_array))
                    .flatten()
                    .any(is_ours)
            })
    }))
}

/// Adds (`on`) or removes agentlens in the agent's hook settings, leaving everything else as it
/// was. The file is backed up first (`<file>.agentlens-bak`) and checked after writing.
pub fn set_registered(def: &HookDef, agent: &str, exe: &Path, on: bool) -> Result<PathBuf, String> {
    let path = settings_path(def)?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut root: Value = match &existing {
        Some(text) => serde_json::from_str(text).map_err(|e| format!("{}: {e}", path.display()))?,
        None => json!({}),
    };
    let root_obj = root.as_object_mut().ok_or("settings are not a JSON object")?;
    let hooks = root_obj
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("`hooks` is not an object")?;
    for event in events(def) {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("`hooks.{event}` is not a list"))?;
        // Drop our entries, then groups left empty by that.
        for group in groups.iter_mut() {
            if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                list.retain(|h| !is_ours(h));
            }
        }
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|l| !l.is_empty()));
        if on {
            groups.push(json!({
                "hooks": [{
                    "type": "command",
                    "command": exe.display().to_string(),
                    "args": ["hook", agent],
                    // Never makes the agent wait for agentlens.
                    "async": true,
                    "timeout": 10,
                }]
            }));
        }
        if groups.is_empty() {
            hooks.remove(event);
        }
    }
    let text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())? + "\n";
    write_checked(&path, existing.as_deref(), &text, |t| {
        serde_json::from_str::<Value>(t).is_ok()
    })?;
    Ok(path)
}

/// Replaces an agent's settings file with `text`, backing up the old one first
/// (`<file>.agentlens-bak`). The file is read back and `check`ed, and restored if that fails.
pub(crate) fn write_checked(
    path: &Path,
    existing: Option<&str>,
    text: &str,
    check: impl Fn(&str) -> bool,
) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let backup = path.with_file_name(format!(
        "{}.agentlens-bak",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("settings")
    ));
    if let Some(old) = existing {
        std::fs::write(&backup, old).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    let written = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if !check(&written) {
        match existing {
            Some(old) => {
                let _ = std::fs::write(path, old);
            }
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
        return Err(format!("{}: could not be written correctly; restored", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(settings: &Path) -> HookDef {
        HookDef {
            settings: settings.display().to_string(),
            events: [
                ("SessionStart", "start"),
                ("UserPromptSubmit", "working"),
                ("Stop", "waiting"),
                ("Notification", "attention"),
                ("Notification:idle_prompt", "waiting"),
                ("SessionEnd", "end"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agentlens-hooks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn registering_keeps_other_hooks_and_settings() {
        let dir = temp("reg");
        let path = dir.join("settings.json");
        let other = r#"{
  "model": "opus",
  "hooks": {
    "Stop": [ { "hooks": [ { "type": "command", "command": "C:/orca/claude-hook.cmd", "timeout": 10 } ] } ]
  },
  "permissions": { "allow": ["Bash(ls)"] }
}"#;
        std::fs::write(&path, other).unwrap();
        let exe = Path::new(r"C:\tools\agentlens.exe");
        let d = def(&path);
        assert!(!registered(&d).unwrap());
        set_registered(&d, "claude", exe, true).unwrap();
        // The file as it was before each write.
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.json.agentlens-bak")).unwrap(),
            other
        );
        set_registered(&d, "claude", exe, true).unwrap();
        assert!(registered(&d).unwrap());
        let on: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let keys: Vec<&String> = on.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "hooks", "permissions"], "order kept");
        let stop = on["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "Orca's hook and one of ours: {stop:?}");
        assert_eq!(
            on["hooks"]["Notification"].as_array().unwrap().len(),
            1,
            "registered once"
        );
        let ours = &on["hooks"]["Stop"][1]["hooks"][0];
        assert_eq!(
            (ours["async"].as_bool(), ours["args"][1].as_str()),
            (Some(true), Some("claude"))
        );

        set_registered(&d, "claude", exe, false).unwrap();
        let off: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let before: Value = serde_json::from_str(other).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(off, before, "back to the user's own settings");
    }

    #[test]
    fn settings_that_do_not_parse_are_left_alone() {
        let dir = temp("bad");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let result = set_registered(&def(&path), "claude", Path::new("agentlens"), true);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(result.is_err());
        assert_eq!(text, "{ not json");
    }

    #[test]
    fn events_leave_the_latest_state_until_the_session_ends() {
        let dir = temp("rec");
        let mut catalog = Catalog::builtin();
        let claude = catalog.agents.iter_mut().find(|a| a.id == "claude").unwrap();
        claude.hooks = Some(def(Path::new("unused")));
        let event = |name: &str, extra: &str| {
            format!(r#"{{"session_id":"s-1","hook_event_name":"{name}","cwd":"C:\\w"{extra}}}"#)
        };
        let read = |dir: &Path| -> Option<Record> {
            serde_json::from_str(&std::fs::read_to_string(dir.join("s-1.json")).ok()?).ok()
        };
        let owner = Some((4242, 77));
        record(&dir, &catalog, "claude", owner, &event("SessionStart", "")).unwrap();
        assert_eq!(read(&dir).unwrap().state, None);
        // The owner may not be found again later: the first one is kept.
        record(&dir, &catalog, "claude", None, &event("UserPromptSubmit", "")).unwrap();
        let r = read(&dir).unwrap();
        assert_eq!((r.pid, r.start_time, r.state), (4242, 77, Some(HookState::Working)));
        record(
            &dir,
            &catalog,
            "claude",
            owner,
            &event("Notification", r#","notification_type":"permission_prompt""#),
        )
        .unwrap();
        assert_eq!(read(&dir).unwrap().state, Some(HookState::Attention));
        record(
            &dir,
            &catalog,
            "claude",
            owner,
            &event("Notification", r#","notification_type":"idle_prompt""#),
        )
        .unwrap();
        assert_eq!(read(&dir).unwrap().state, Some(HookState::Waiting));
        assert!(
            record(
                &dir,
                &catalog,
                "claude",
                owner,
                r#"{"session_id":"../x","hook_event_name":"Stop"}"#
            )
            .is_err()
        );
        record(&dir, &catalog, "claude", owner, &event("SessionEnd", "")).unwrap();
        let gone = read(&dir).is_none();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(gone);
    }
}
