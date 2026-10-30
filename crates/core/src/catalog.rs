//! What agentlens knows about processes: the embedded `catalog.toml`, extended by the user's
//! rules (same tables) and by what the install scan found on this machine.

use serde::{Deserialize, Serialize};

use crate::rules::Rules;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Catalog {
    #[serde(skip_serializing_if = "is_default")]
    pub processes: Processes,
    #[serde(skip_serializing_if = "is_default")]
    pub ssh: Ssh,
    #[serde(skip_serializing_if = "is_default")]
    pub providers: Providers,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentDef>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<HostDef>,
    /// From the user's rules only: substrings of a process name or command line that must
    /// never be killed.
    #[serde(skip)]
    pub protect: Vec<String>,
}

/// Process names by role; see `catalog.toml` for what each list means.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Processes {
    pub shells: Vec<String>,
    pub runtimes: Vec<String>,
    pub pty_glue: Vec<String>,
    pub system: Vec<String>,
    pub reapers: Vec<String>,
    pub system_dirs: Vec<String>,
    pub browsers: Vec<String>,
    pub repl: Vec<String>,
    pub sandbox: Vec<String>,
    pub mcp: Vec<String>,
    pub session_flags: Vec<String>,
    /// Variables agents set on the processes they start, valued `<agent>_<version>_<kind>`.
    pub agent_env: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ssh {
    pub clients: Vec<String>,
    pub putty: Vec<String>,
    pub servers: Vec<String>,
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Providers {
    pub env: Vec<String>,
    pub first_party: Vec<String>,
    pub known: Vec<ProviderDef>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderDef {
    pub name: String,
    /// Substrings of an endpoint URL.
    pub url: Vec<String>,
    /// Substrings of a command line (model names).
    pub cmd: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentDef {
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Command names (`codex` matches `codex.exe`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exe: Vec<String>,
    /// Package paths matched against the command line of a runtime running the agent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    /// Paths the agent leaves in the command lines of processes it starts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub traces: Vec<String>,
    /// Model provider shown when the environment names no other.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Names this agent goes by in `processes.agent_env` values (`claude-code`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ai_agent: Vec<String>,
    /// Variable the agent sets on the processes it starts, naming its session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_env: Option<String>,
    /// Variable the agent sets on the processes it starts, holding its own PID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid_env: Option<String>,
    /// The agent's hook settings, where agentlens can register itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HookDef>,
    /// The agent's MCP settings, where agentlens can register its MCP server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpDef>,
}

/// Where an agent keeps its hooks and what its events mean.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HookDef {
    /// Settings file holding `hooks`; `~/` is the user's home.
    pub settings: String,
    /// Agent event (or `<event>:<notification_type>`) -> `start`, `working`, `waiting`,
    /// `attention` or `end`.
    pub events: std::collections::BTreeMap<String, String>,
}

/// Where an agent keeps its user-level MCP servers and what an entry looks like.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpDef {
    /// Settings file; `~/` is the user's home.
    pub config: String,
    /// `json` or `toml`.
    pub format: String,
    /// Keys from the top of the file to the map of servers, which are keyed by name.
    pub key: Vec<String>,
    /// The server's entry; `{exe}` stands for agentlens's own path.
    pub entry: toml::Table,
    /// For agents that keep rewriting their settings file: the agent's own commands that add
    /// and remove the server, with `{exe}`. The file is then only read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostDef {
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exe: Vec<String>,
    /// Substrings of the executable path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    /// Commands on `PATH` that show the host is installed, for one whose process goes by
    /// another name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Names of the host in the system's app launchers, whole or as the leading words.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub app: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub traces: Vec<String>,
    /// Environment variable whose value identifies the tab a process belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_env: Option<String>,
    /// Environment variable whose value labels the tab.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_env: Option<String>,
    /// Environment variable set on everything an agent in this host launched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_env: Option<String>,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

impl Catalog {
    pub fn builtin() -> Self {
        toml::from_str::<Catalog>(include_str!("../catalog.toml"))
            .expect("the embedded catalog.toml is valid (checked by tests)")
            .normalized()
    }

    /// Built-in knowledge extended by the user's rules.
    pub fn with_rules(mut self, rules: &Rules) -> Self {
        self.extend(rules.catalog.clone().normalized());
        // Compared with normalized command lines, so normalized the same way.
        self.protect.extend(rules.protect.iter().map(|s| normalize(s)));
        self
    }

    /// Adds command-line fragments discovered by the install scan.
    pub fn with_installed(mut self, found: &[(String, String)]) -> Self {
        for (id, fragment) in found {
            if let Some(def) = self.agents.iter_mut().find(|a| &a.id == id) {
                push_new(&mut def.cmd, [fragment.clone()]);
            }
        }
        self
    }

    /// Environment variables the model reads; the collector keeps only these.
    pub fn env_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        for host in &self.hosts {
            push_new(
                &mut keys,
                [&host.tab_env, &host.label_env, &host.launch_env]
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        push_new(&mut keys, self.providers.env.iter().cloned());
        push_new(&mut keys, self.processes.agent_env.iter().cloned());
        for agent in &self.agents {
            push_new(
                &mut keys,
                [&agent.session_env, &agent.pid_env].into_iter().flatten().cloned(),
            );
        }
        keys
    }

    /// List entries are appended; an agent or host with an existing id gains the other's
    /// entries, and its set name and variables replace the existing ones.
    fn extend(&mut self, other: Catalog) {
        let p = &mut self.processes;
        let o = other.processes;
        for (list, more) in [
            (&mut p.shells, o.shells),
            (&mut p.runtimes, o.runtimes),
            (&mut p.pty_glue, o.pty_glue),
            (&mut p.system, o.system),
            (&mut p.reapers, o.reapers),
            (&mut p.system_dirs, o.system_dirs),
            (&mut p.browsers, o.browsers),
            (&mut p.repl, o.repl),
            (&mut p.sandbox, o.sandbox),
            (&mut p.mcp, o.mcp),
            (&mut p.session_flags, o.session_flags),
            (&mut p.agent_env, o.agent_env),
            (&mut self.ssh.clients, other.ssh.clients),
            (&mut self.ssh.putty, other.ssh.putty),
            (&mut self.ssh.servers, other.ssh.servers),
            (&mut self.providers.env, other.providers.env),
            (&mut self.providers.first_party, other.providers.first_party),
        ] {
            push_new(list, more);
        }
        push_new(&mut self.ssh.ports, other.ssh.ports);
        self.providers.known.extend(other.providers.known);

        for agent in other.agents {
            match self.agents.iter_mut().find(|a| a.id == agent.id) {
                Some(def) => {
                    if !agent.name.is_empty() {
                        def.name = agent.name;
                    }
                    push_new(&mut def.exe, agent.exe);
                    push_new(&mut def.cmd, agent.cmd);
                    push_new(&mut def.traces, agent.traces);
                    def.provider = agent.provider.or(def.provider.take());
                    push_new(&mut def.ai_agent, agent.ai_agent);
                    def.session_env = agent.session_env.or(def.session_env.take());
                    def.pid_env = agent.pid_env.or(def.pid_env.take());
                    def.hooks = agent.hooks.or(def.hooks.take());
                    def.mcp = agent.mcp.or(def.mcp.take());
                }
                None => self.agents.push(agent),
            }
        }
        for host in other.hosts {
            match self.hosts.iter_mut().find(|h| h.id == host.id) {
                Some(def) => {
                    if !host.name.is_empty() {
                        def.name = host.name;
                    }
                    push_new(&mut def.exe, host.exe);
                    push_new(&mut def.path, host.path);
                    push_new(&mut def.command, host.command);
                    push_new(&mut def.app, host.app);
                    push_new(&mut def.traces, host.traces);
                    def.tab_env = host.tab_env.or(def.tab_env.take());
                    def.label_env = host.label_env.or(def.label_env.take());
                    def.launch_env = host.launch_env.or(def.launch_env.take());
                }
                None => self.hosts.push(host),
            }
        }
    }

    /// Lowercases names and normalizes paths so matching never has to; agents and hosts
    /// without a name are named after their id.
    fn normalized(mut self) -> Self {
        let lower = |list: &mut Vec<String>| list.iter_mut().for_each(|s| *s = s.to_lowercase());
        let paths = |list: &mut Vec<String>| list.iter_mut().for_each(|s| *s = normalize(s));
        let p = &mut self.processes;
        for list in [
            &mut p.shells,
            &mut p.runtimes,
            &mut p.pty_glue,
            &mut p.system,
            &mut p.reapers,
            &mut p.browsers,
            &mut p.repl,
            &mut p.sandbox,
            &mut p.mcp,
            &mut self.ssh.clients,
            &mut self.ssh.putty,
            &mut self.ssh.servers,
            &mut self.providers.first_party,
        ] {
            lower(list);
        }
        paths(&mut p.system_dirs);
        for known in &mut self.providers.known {
            lower(&mut known.url);
            lower(&mut known.cmd);
        }
        for agent in &mut self.agents {
            lower(&mut agent.exe);
            lower(&mut agent.ai_agent);
            paths(&mut agent.cmd);
            paths(&mut agent.traces);
            if agent.name.is_empty() {
                agent.name = agent.id.clone();
            }
        }
        for host in &mut self.hosts {
            lower(&mut host.exe);
            lower(&mut host.command);
            lower(&mut host.app);
            paths(&mut host.path);
            paths(&mut host.traces);
            if host.name.is_empty() {
                host.name = host.id.clone();
            }
        }
        self
    }
}

fn push_new<T: PartialEq>(list: &mut Vec<T>, items: impl IntoIterator<Item = T>) {
    for item in items {
        if !list.contains(&item) {
            list.push(item);
        }
    }
}

/// Lowercase with `/` separators, for case- and separator-insensitive matching.
pub fn normalize(s: &str) -> String {
    s.to_lowercase().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_parses() {
        let c = Catalog::builtin();
        assert_eq!(c.agents.len(), 45, "Orca's agent catalog");
        assert!(c.agents.iter().all(|a| !a.name.is_empty()));
        assert!(c.hosts.iter().all(|h| !h.name.is_empty()));
        assert!(!c.processes.shells.is_empty() && !c.ssh.ports.is_empty());
        let keys = c.env_keys();
        assert!(keys.contains(&"ORCA_TAB_ID".to_string()) && keys.contains(&"ANTHROPIC_BASE_URL".to_string()));
    }

    #[test]
    fn protect_rules_match_like_command_lines() {
        // Command lines are compared lowercased with `/` separators; Windows paths must match.
        let rules: Rules = toml::from_str(r#"protect = ['C:\Work\Dev-Server']"#).unwrap();
        assert_eq!(Catalog::builtin().with_rules(&rules).protect, ["c:/work/dev-server"]);
    }

    #[test]
    fn rules_extend_by_id_and_add_new_entries() {
        // A whole rules file, so the flattened catalog tables are parsed as users write them.
        let rules: Rules = toml::from_str(
            r#"
            idle_minutes = 10
            [processes]
            shells = ["Elvish"]
            [[agents]]
            id = "codex"
            cmd = ['C:\Tools\Codex']
            [[agents]]
            id = "mine"
            exe = ["Mine"]
            [[hosts]]
            id = "orca"
            tab_env = "OTHER_TAB"
            "#,
        )
        .unwrap();
        let c = Catalog::builtin().with_rules(&rules);
        assert!(c.processes.shells.contains(&"elvish".to_string()));
        let codex = c.agents.iter().find(|a| a.id == "codex").unwrap();
        assert!(codex.cmd.contains(&"c:/tools/codex".to_string()) && codex.exe == ["codex"]);
        let mine = c.agents.iter().find(|a| a.id == "mine").unwrap();
        assert_eq!(
            (mine.name.as_str(), mine.exe.as_slice()),
            ("mine", &["mine".to_string()][..])
        );
        let orca = c.hosts.iter().find(|h| h.id == "orca").unwrap();
        assert_eq!(orca.tab_env.as_deref(), Some("OTHER_TAB"));
        assert_eq!(
            orca.label_env.as_deref(),
            Some("ORCA_WORKTREE_ID"),
            "unset fields are kept"
        );
    }
}
