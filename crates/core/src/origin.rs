//! Who started a process, and whether that owner is still there. Two sources, neither tied to a
//! host: what the dashboard saw (the process sat under a live agent session) and what agents
//! write into the environment of everything they start, which outlives them.

use std::collections::{HashMap, HashSet};

use crate::catalog::{AgentDef, Catalog};
use crate::model::{Model, Role, env};
use crate::snapshot::ProcInfo;

/// How long an owner must have been gone before its leftovers count. A session that exits
/// normally takes a moment to end its children, and a `docker run` server up to 10 s.
pub const OWNER_GONE_GRACE_SECS: u64 = 15;

/// (pid, start time): identifies a process even after its PID is reused.
type ProcKey = (u32, u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Starter {
    /// The agent itself started it (`AI_AGENT=…_harness`): MCP servers and other machinery.
    Harness,
    /// A command the model ran (`AI_AGENT=…_agent`), such as a dev server.
    Command,
}

#[derive(Debug, Clone)]
pub struct OwnerGone {
    /// The session it came from, for display.
    pub origin: String,
    pub starter: Option<Starter>,
    /// When it was first seen without its owner.
    pub since: u64,
}

#[derive(Debug, Default)]
pub struct Origins {
    /// Processes seen under a live session -> that session's root and label.
    seen: HashMap<ProcKey, (ProcKey, String)>,
    /// When each process was first seen without its owner.
    gone_since: HashMap<ProcKey, u64>,
    /// Owner gone as of the last update, by PID.
    gone: HashMap<u32, OwnerGone>,
}

fn key(p: &ProcInfo) -> ProcKey {
    (p.pid, p.start_time)
}

fn alive(model: &Model, (pid, start): ProcKey) -> bool {
    model.procs.get(&pid).is_some_and(|p| p.start_time == start)
}

/// The agent named by the process's `agent_env` value, and what kind of starter it records.
fn ai_agent<'a>(catalog: &'a Catalog, p: &ProcInfo) -> (Option<&'a AgentDef>, Option<Starter>) {
    let Some(value) = catalog.processes.agent_env.iter().find_map(|k| env(p, k)) else {
        return (None, None);
    };
    let value = value.to_lowercase();
    let name = value.split('_').next().unwrap_or_default();
    let starter = match value.rsplit('_').next() {
        Some("harness") => Some(Starter::Harness),
        Some("agent") => Some(Starter::Command),
        _ => None,
    };
    let agent = catalog.agents.iter().find(|a| a.ai_agent.iter().any(|n| n == name));
    (agent, starter)
}

impl Origins {
    pub fn update(&mut self, model: &Model, catalog: &Catalog) {
        let now = model.taken_at;
        let roles = &model.roles;
        let is_agent = |pid: &u32| matches!(roles.get(pid), Some(Role::Agent(_)));
        let mut labels: HashMap<u32, String> = HashMap::new();
        for g in &model.groups {
            for t in &g.tabs {
                for s in &t.sessions {
                    labels.insert(s.root_pid, format!("{} @ {} / {}", s.agent_name, g.name, t.label));
                }
            }
        }
        let agent_name = |pid: u32| match roles.get(&pid) {
            Some(Role::Agent(i)) => catalog.agents[*i].name.clone(),
            _ => String::new(),
        };

        // Processes under a live agent have a live owner. Remember which session that is, and
        // which session ids are still in use.
        let mut live_sessions: HashSet<&str> = HashSet::new();
        let mut unowned: Vec<&ProcInfo> = Vec::new();
        for p in model.procs.values() {
            if matches!(roles.get(&p.pid), Some(Role::Agent(_) | Role::Host | Role::System))
                || model.pty_glue.contains(&p.pid)
            {
                continue;
            }
            let chain = model.ancestors(p.pid);
            match chain.iter().rev().find(|a| is_agent(a)) {
                Some(&root) => {
                    let label = labels.get(&root).cloned().unwrap_or_else(|| agent_name(root));
                    self.seen.insert(key(p), (key(&model.procs[&root]), label));
                    live_sessions.extend(catalog.agents.iter().filter_map(|a| env(p, a.session_env.as_deref()?)));
                }
                None => unowned.push(p),
            }
        }

        let mut gone = HashMap::new();
        for p in unowned {
            let (agent, starter) = ai_agent(catalog, p);
            let origin = match self.seen.get(&key(p)) {
                // Seen under a session that is still running: moved out on purpose (`nohup`).
                Some((root, _)) if alive(model, *root) => continue,
                Some((_, label)) => label.clone(),
                None => match self.birth_owner_gone(model, catalog, p, agent, &live_sessions) {
                    Some(origin) => origin,
                    None => continue,
                },
            };
            let since = *self.gone_since.entry(key(p)).or_insert(now);
            gone.insert(p.pid, OwnerGone { origin, starter, since });
        }

        let live: HashSet<ProcKey> = model.procs.values().map(key).collect();
        self.seen.retain(|k, _| live.contains(k));
        self.gone_since
            .retain(|k, _| gone.get(&k.0).is_some_and(|_| live.contains(k)));
        self.gone = gone;
    }

    /// The origin of a process whose environment names an owner that is gone; `None` when the
    /// owner is alive or not named.
    fn birth_owner_gone(
        &self,
        model: &Model,
        catalog: &Catalog,
        p: &ProcInfo,
        agent: Option<&AgentDef>,
        live_sessions: &HashSet<&str>,
    ) -> Option<String> {
        // The agent named by `AI_AGENT`, else any agent whose variables are set.
        let agents = agent.into_iter().chain(catalog.agents.iter());
        for a in agents {
            if let Some(pid) = a.pid_env.as_deref().and_then(|k| env(p, k)?.parse::<u32>().ok()) {
                let owner_alive = model.procs.get(&pid).is_some_and(|o| {
                    matches!(model.roles.get(&pid), Some(Role::Agent(_))) && o.start_time <= p.start_time
                });
                return (!owner_alive).then(|| format!("{} pid {pid}", a.name));
            }
            if let Some(id) = a.session_env.as_deref().and_then(|k| env(p, k)) {
                let short: String = id.chars().take(8).collect();
                return (!live_sessions.contains(id)).then(|| format!("{} session {short}", a.name));
            }
        }
        None
    }

    /// The process's owner is gone and has been for at least `grace` seconds.
    pub fn owner_gone(&self, pid: u32, now: u64, grace: u64) -> Option<&OwnerGone> {
        self.gone.get(&pid).filter(|g| now.saturating_sub(g.since) >= grace)
    }

    /// Topmost processes whose owner has been gone `grace` seconds: their parent is not one of
    /// them. Trees that hold a live agent are left out, since ending them would end a session.
    pub fn gone_roots(&self, model: &Model, grace: u64) -> Vec<u32> {
        let now = model.taken_at;
        let settled = |pid: &u32| self.owner_gone(*pid, now, grace).is_some();
        let mut roots: Vec<u32> = self
            .gone
            .keys()
            .copied()
            .filter(settled)
            .filter(|pid| !model.parent.get(pid).is_some_and(settled))
            .filter(|pid| {
                !model
                    .subtree(*pid)
                    .iter()
                    .any(|q| matches!(model.roles.get(q), Some(Role::Agent(_))))
            })
            .collect();
        roots.sort_unstable();
        roots
    }
}
