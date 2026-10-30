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
    /// Outside any running session, but its owner is known to be alive (moved out on purpose):
    /// process -> the root of the session that owns it.
    owned: HashMap<u32, u32>,
    /// When each process was first seen with the pipe on its input closed (`stdio::StdioProbe`).
    stdio_since: HashMap<ProcKey, u64>,
}

/// What a process's environment says about its owner.
enum Birth {
    Gone(String),
    /// With the root of the owner's session.
    Alive(u32),
    Unnamed,
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

/// The owner a process's environment names: the agent named by `AI_AGENT`, else any agent
/// whose variables are set.
fn birth(
    model: &Model,
    catalog: &Catalog,
    p: &ProcInfo,
    agent: Option<&AgentDef>,
    live_sessions: &HashMap<&str, u32>,
) -> Birth {
    for a in agent.into_iter().chain(catalog.agents.iter()) {
        if let Some(pid) = a.pid_env.as_deref().and_then(|k| env(p, k)?.parse::<u32>().ok()) {
            let alive = model
                .procs
                .get(&pid)
                .is_some_and(|o| matches!(model.roles.get(&pid), Some(Role::Agent(_))) && o.start_time <= p.start_time);
            return if alive {
                let is_agent = |a: &u32| matches!(model.roles.get(a), Some(Role::Agent(_)));
                Birth::Alive(model.ancestors(pid).into_iter().rev().find(is_agent).unwrap_or(pid))
            } else {
                Birth::Gone(format!("{} pid {pid}", a.name))
            };
        }
        if let Some(id) = a.session_env.as_deref().and_then(|k| env(p, k)) {
            let short: String = id.chars().take(8).collect();
            return if let Some(root) = live_sessions.get(id) {
                Birth::Alive(*root)
            } else {
                Birth::Gone(format!("{} session {short}", a.name))
            };
        }
    }
    Birth::Unnamed
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
        let mut live_sessions: HashMap<&str, u32> = HashMap::new();
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
                    let ids = catalog.agents.iter().filter_map(|a| env(p, a.session_env.as_deref()?));
                    live_sessions.extend(ids.map(|id| (id, root)));
                }
                None => unowned.push(p),
            }
        }

        let mut gone = HashMap::new();
        self.owned.clear();
        for p in unowned {
            let (agent, starter) = ai_agent(catalog, p);
            let origin = match self.seen.get(&key(p)) {
                // Seen under a session that is still running: moved out on purpose (`nohup`).
                Some((root, _)) if alive(model, *root) => {
                    self.owned.insert(p.pid, root.0);
                    continue;
                }
                Some((_, label)) => label.clone(),
                None => match birth(model, catalog, p, agent, &live_sessions) {
                    Birth::Gone(origin) => origin,
                    Birth::Alive(root) => {
                        self.owned.insert(p.pid, root);
                        continue;
                    }
                    Birth::Unnamed => continue,
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

    /// Puts what left a live session's process tree back in that session: the topmost process
    /// of each such tree, or the orphan it runs under (a launcher, a shell). A host the session
    /// started (an editor opened from an agent) keeps what it runs, though those inherit the
    /// marks; one older than the session did not start them, it adopted them (WSL's relays).
    pub fn attach(&self, model: &mut Model) {
        let listed: HashSet<u32> = model.orphans.iter().map(|o| o.pid).collect();
        let mut detached: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&pid, &owner) in &self.owned {
            let chain = model.ancestors(pid);
            let started = model.procs.get(&owner).map_or(0, |o| o.start_time);
            let host_runs_it = chain
                .iter()
                .any(|a| model.roles.get(a) == Some(&Role::Host) && model.procs[a].start_time >= started);
            if host_runs_it || chain.first().is_some_and(|pp| self.owned.contains_key(pp)) {
                continue;
            }
            let tops = detached.entry(owner).or_default();
            tops.push(*chain.last().filter(|t| listed.contains(*t)).unwrap_or(&pid));
        }
        for tops in detached.values_mut() {
            tops.sort_unstable();
            tops.dedup();
        }
        model.detached = detached;
    }

    /// Records which processes have lost the writer of their input pipe.
    pub fn note_stdio_closed(&mut self, model: &Model, closed: &HashSet<ProcKey>) {
        self.stdio_since.retain(|k, _| closed.contains(k));
        for k in closed {
            self.stdio_since.entry(*k).or_insert(model.taken_at);
        }
    }

    /// Part of the tree has a live owner. A launcher or shell at the root shows no marks of its
    /// own, so the whole tree has to be asked.
    fn owned_tree(&self, model: &Model, pid: u32) -> bool {
        model.subtree(pid).iter().any(|q| self.owned.contains_key(q))
    }

    /// Nobody has been able to write to the process's input for at least `grace` seconds, and
    /// no owner of its tree is known to be alive.
    pub fn stdio_closed(&self, model: &Model, pid: u32, grace: u64) -> bool {
        !self.owned_tree(model, pid)
            && model.procs.get(&pid).is_some_and(|p| {
                self.stdio_since
                    .get(&key(p))
                    .is_some_and(|since| model.taken_at.saturating_sub(*since) >= grace)
            })
    }

    /// Processes whose input pipe has been closed for at least `grace` seconds.
    pub fn stdio_closed_pids(&self, model: &Model, grace: u64) -> Vec<u32> {
        let mut pids: Vec<u32> = self
            .stdio_since
            .iter()
            .filter(|(k, since)| {
                alive(model, **k) && !self.owned_tree(model, k.0) && model.taken_at.saturating_sub(**since) >= grace
            })
            .map(|(k, _)| k.0)
            .collect();
        pids.sort_unstable();
        pids
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
