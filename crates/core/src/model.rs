//! Classifies a snapshot and groups agent sessions as host -> tab -> session -> processes.
//! Every name, path and variable it matches comes from the catalog.

use std::collections::{HashMap, HashSet};

use crate::catalog::{AgentDef, Catalog, HostDef, normalize};
use crate::snapshot::{Listener, ProcInfo, Snapshot, TcpConn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperKind {
    Repl,
    Mcp,
    Browser,
    Sandbox,
    Shell,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The OS's own processes: never grouped, never killed.
    System,
    Host,
    Shell,
    /// Index into `Catalog::agents`.
    Agent(usize),
    Helper(HelperKind),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupKind {
    /// A catalog host, by id.
    Known(String),
    /// A non-shell ancestor the catalog does not know, by exe stem.
    Unregistered(String),
    /// Agents started from a plain shell window.
    Standalone,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub kind: GroupKind,
    pub name: String,
    /// The host's own processes (e.g. every `Orca.exe`).
    pub host_pids: Vec<u32>,
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Clone)]
pub struct Tab {
    /// `<host id>:<tab_env value>` when the host marks its tabs, else `pid:<tab root>`.
    pub key: String,
    pub label: String,
    /// Processes directly under the host that contain this tab's sessions (usually shells).
    pub root_pids: Vec<u32>,
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub agent_id: String,
    pub agent_name: String,
    /// Model provider when it differs from the agent's default, e.g. GLM.
    pub provider: Option<String>,
    pub session_id: Option<String>,
    pub root_pid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanReason {
    ParentGone,
    /// The parent PID now belongs to a newer process.
    PidReused,
    /// Adopted by launchd/init/systemd after its parent exited (Unix).
    Reparented,
}

#[derive(Debug, Clone)]
pub struct Orphan {
    pub pid: u32,
    pub reason: OrphanReason,
    /// The tree contains agent machinery (an agent, REPL, MCP server, agent runtime).
    /// False for programs an agent merely started, such as a dev server.
    pub agent_related: bool,
    /// Started from an agent session (a host's `launch_env` is set).
    pub launched_by_agent: bool,
    /// Label of the tab it was started from, when that tab is still open.
    pub origin: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub taken_at: u64,
    pub procs: HashMap<u32, ProcInfo>,
    pub parent: HashMap<u32, u32>,
    pub children: HashMap<u32, Vec<u32>>,
    pub roles: HashMap<u32, Role>,
    pub groups: Vec<Group>,
    pub orphans: Vec<Orphan>,
    pub tcp: Vec<TcpConn>,
    pub listening: Vec<Listener>,
    /// Pseudo-console glue processes, not worth a row of their own.
    pub pty_glue: HashSet<u32>,
}

pub fn stem(p: &ProcInfo) -> String {
    let name = p.name.to_lowercase();
    name.strip_suffix(".exe").map(str::to_string).unwrap_or(name)
}

/// The executable path, or argv[0] when the path could not be read but argv[0] is absolute
/// (macOS denies `proc_pidpath` for some processes, such as Electron helpers).
fn exe_norm(p: &ProcInfo) -> String {
    let argv0 = p
        .cmd
        .first()
        .filter(|a| a.starts_with('/') || a.get(1..3) == Some(":\\"));
    p.exe
        .as_deref()
        .or(argv0.map(String::as_str))
        .map(normalize)
        .unwrap_or_default()
}

fn cmd_norm(p: &ProcInfo) -> String {
    normalize(&p.cmd.join(" "))
}

fn has(list: &[String], name: &str) -> bool {
    list.iter().any(|n| n == name)
}

/// PID 1 or a subreaper. Children linked to it count as having lost their real parent.
/// Other processes that share a reaper's name (WSL's per-session `init` relays) are not.
fn is_reaper(c: &Catalog, p: &ProcInfo) -> bool {
    p.pid == 1 || has(&c.processes.reapers, &stem(p))
}

fn is_system(c: &Catalog, p: &ProcInfo) -> bool {
    has(&c.processes.system, &stem(p))
}

/// A command line with an MCP marker as a whole word (`rea.mjs mcp`, `@modelcontextprotocol/x`).
fn is_mcp(c: &Catalog, cmd: &str) -> bool {
    cmd.split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|word| has(&c.processes.mcp, word))
}

fn env<'a>(p: &'a ProcInfo, key: &str) -> Option<&'a str> {
    p.env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// The last `n` components of a path, for compact labels.
pub fn short_path(path: &str, n: usize) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    parts[parts.len().saturating_sub(n)..].join("/")
}

/// The executable's file name without `.exe`. Processes can rename themselves (tmux shows up
/// as `tmux: server`); the executable keeps its name.
fn exe_stem(exe: &str) -> &str {
    let file = exe.rsplit('/').next().unwrap_or(exe);
    file.strip_suffix(".exe").unwrap_or(file)
}

fn match_host(c: &Catalog, p: &ProcInfo) -> Option<usize> {
    let s = stem(p);
    let exe = exe_norm(p);
    let file = exe_stem(&exe);
    c.hosts.iter().position(|h| {
        has(&h.exe, &s) || (!file.is_empty() && has(&h.exe, file)) || h.path.iter().any(|f| exe.contains(f.as_str()))
    })
}

fn match_agent(c: &Catalog, p: &ProcInfo) -> Option<usize> {
    let s = stem(p);
    let exe = exe_norm(p);
    // OS binaries never are agents, even when their names collide with short agent commands.
    if c.processes.system_dirs.iter().any(|d| exe.starts_with(d.as_str())) {
        return None;
    }
    // argv[0] too: native installs may run from a versioned file (`versions/2.1.276`).
    let argv0 = p.cmd.first().map(|a| {
        let file = a.rsplit(['/', '\\']).next().unwrap_or(a).to_lowercase();
        file.strip_suffix(".exe").map(str::to_string).unwrap_or(file)
    });
    if let Some(i) = c
        .agents
        .iter()
        .position(|a| has(&a.exe, &s) || argv0.as_deref().is_some_and(|v| has(&a.exe, v)))
    {
        return Some(i);
    }
    // Package paths identify agents run as scripts. Other binaries that merely live inside an
    // agent's package are helpers, and shells stay shells.
    if !has(&c.processes.runtimes, &s) {
        return None;
    }
    let cmd = cmd_norm(p);
    c.agents.iter().position(|a| {
        a.cmd
            .iter()
            .any(|f| cmd.contains(f.as_str()) || exe.contains(f.as_str()))
    })
}

fn helper_kind(c: &Catalog, p: &ProcInfo) -> HelperKind {
    let s = stem(p);
    let procs = &c.processes;
    if has(&procs.repl, &s) {
        HelperKind::Repl
    } else if has(&procs.shells, &s) {
        HelperKind::Shell
    } else if has(&procs.browsers, &s) {
        HelperKind::Browser
    } else if procs.sandbox.iter().any(|m| s.contains(m.as_str())) {
        HelperKind::Sandbox
    } else if is_mcp(c, &cmd_norm(p)) {
        HelperKind::Mcp
    } else {
        HelperKind::Tool
    }
}

/// The provider an agent is pointed at: a known provider or the host of a third-party
/// endpoint in the environment, a known provider's model name on the command line, else the
/// agent's own default.
fn provider(c: &Catalog, agent: &AgentDef, p: &ProcInfo) -> Option<String> {
    let known = &c.providers.known;
    for url in c.providers.env.iter().filter_map(|k| env(p, k)).map(str::to_lowercase) {
        if let Some(def) = known.iter().find(|d| d.url.iter().any(|u| url.contains(u.as_str()))) {
            return Some(def.name.clone());
        }
        if !c.providers.first_party.iter().any(|f| url.contains(f.as_str())) {
            let host = url
                .split("://")
                .nth(1)
                .unwrap_or(&url)
                .split('/')
                .next()
                .unwrap_or_default();
            // Credentials in the URL are masked by the collector; the host is what names it.
            let host = host.rsplit('@').next().unwrap_or(host);
            return Some(host.to_string());
        }
    }
    let cmd = cmd_norm(p);
    known
        .iter()
        .find(|d| d.cmd.iter().any(|m| cmd.contains(m.as_str())))
        .map(|d| d.name.clone())
        .or_else(|| agent.provider.clone())
}

fn session_id(c: &Catalog, p: &ProcInfo) -> Option<String> {
    let flags = &c.processes.session_flags;
    let mut args = p.cmd.iter().skip(1);
    while let Some(arg) = args.next() {
        if let Some((flag, value)) = arg.split_once('=')
            && has(flags, flag)
        {
            return Some(value.to_string());
        }
        if has(flags, arg) {
            return args.next().filter(|v| !v.starts_with('-')).cloned();
        }
    }
    None
}

/// Start times have one-second resolution, so a PID reused within that second can pass the
/// "parent started first" check and close a loop (A -> B -> A). Drops the link that closes
/// each loop so every walk up the tree ends.
fn break_cycles(parent: &mut HashMap<u32, u32>) {
    let pids: Vec<u32> = parent.keys().copied().collect();
    for start in pids {
        let mut seen = HashSet::new();
        let mut cur = start;
        while let Some(&pp) = parent.get(&cur) {
            if !seen.insert(cur) {
                parent.remove(&cur);
                break;
            }
            cur = pp;
        }
    }
}

/// The value of the first of `vars` set on any of `procs`.
fn first_env<'a>(
    procs: impl IntoIterator<Item = &'a ProcInfo>,
    vars: &[(&'a HostDef, &'a str)],
) -> Option<(&'a HostDef, &'a str)> {
    procs
        .into_iter()
        .find_map(|p| vars.iter().find_map(|(host, key)| env(p, key).map(|v| (*host, v))))
}

impl Model {
    /// Takes the snapshot by value: it is rebuilt every second, and copying every process's
    /// strings would double the work and the peak memory.
    pub fn build(snapshot: Snapshot, catalog: &Catalog) -> Self {
        let procs: HashMap<u32, ProcInfo> = snapshot.processes.into_iter().map(|p| (p.pid, p)).collect();

        let mut parent: HashMap<u32, u32> = HashMap::new();
        for p in procs.values() {
            if let Some(pp) = p.ppid
                && pp != p.pid
                && procs
                    .get(&pp)
                    .is_some_and(|q| q.start_time <= p.start_time && !is_reaper(catalog, q))
            {
                parent.insert(p.pid, pp);
            }
        }
        break_cycles(&mut parent);
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&child, &pp) in &parent {
            children.entry(pp).or_default().push(child);
        }
        for list in children.values_mut() {
            list.sort_unstable();
        }

        let mut roles: HashMap<u32, Role> = procs
            .values()
            .map(|p| {
                let role = if is_system(catalog, p) {
                    Role::System
                } else if match_host(catalog, p).is_some() {
                    Role::Host
                } else if has(&catalog.processes.shells, &stem(p)) {
                    Role::Shell
                } else if let Some(i) = match_agent(catalog, p) {
                    Role::Agent(i)
                } else {
                    Role::Other
                };
                (p.pid, role)
            })
            .collect();

        let mut model = Model {
            taken_at: snapshot.taken_at,
            procs,
            parent,
            children,
            roles: HashMap::new(),
            groups: Vec::new(),
            orphans: Vec::new(),
            tcp: snapshot.tcp,
            listening: snapshot.listening,
            pty_glue: HashSet::new(),
        };
        model.pty_glue = model
            .procs
            .values()
            .filter(|p| has(&catalog.processes.pty_glue, &stem(p)))
            .map(|p| p.pid)
            .collect();
        // Hosts that mark their tabs or launches, with the variable they use.
        let tab_vars: Vec<(&HostDef, &str)> = catalog
            .hosts
            .iter()
            .filter_map(|h| Some((h, h.tab_env.as_deref()?)))
            .collect();
        let launch_vars: Vec<(&HostDef, &str)> = catalog
            .hosts
            .iter()
            .filter_map(|h| Some((h, h.launch_env.as_deref()?)))
            .collect();

        // Session roots: agent processes without an agent ancestor.
        let mut roots: Vec<u32> = roles
            .iter()
            .filter(|(pid, role)| {
                matches!(role, Role::Agent(_))
                    && !model
                        .ancestors(**pid)
                        .iter()
                        .any(|a| matches!(roles[a], Role::Agent(_)))
            })
            .map(|(pid, _)| *pid)
            .collect();
        roots.sort_unstable();

        // Everything under a session that is not itself an agent is a helper.
        for &root in &roots {
            for pid in model.subtree(root) {
                if !matches!(roles[&pid], Role::Agent(_)) {
                    roles.insert(pid, Role::Helper(helper_kind(catalog, &model.procs[&pid])));
                }
            }
        }

        let mut covered: HashSet<u32> = HashSet::new();
        for &root in &roots {
            let Role::Agent(agent_idx) = roles[&root] else {
                unreachable!()
            };
            let chain = model.ancestors(root);
            let top = *chain.last().unwrap_or(&root);
            let top_proc = &model.procs[&top];

            let host_at = chain.iter().position(|a| roles[a] == Role::Host);
            // A chain that stops at a shell, agent or runtime lost the process that started it:
            // the whole session is orphaned. One that stops at another program (on Unix every
            // GUI app is a child of launchd/systemd) is a host the catalog does not know.
            let headless = matches!(roles[&top], Role::Shell | Role::Agent(_))
                || has(&catalog.processes.runtimes, &stem(top_proc));
            if host_at.is_none() && headless && top_proc.ppid.is_some() {
                if covered.insert(top) {
                    let reason = model.orphan_reason(catalog, top);
                    model.orphans.push(Orphan {
                        pid: top,
                        reason,
                        agent_related: true,
                        launched_by_agent: true,
                        origin: None,
                    });
                }
                continue;
            }

            // Ancestors of a session are its host, tab and launchers, never separate orphans.
            covered.extend(chain.iter().copied());
            let (kind, name, tab_root, host_def) = match host_at {
                Some(i) => {
                    let host = &model.procs[&chain[i]];
                    let def = &catalog.hosts[match_host(catalog, host).unwrap()];
                    let tab_root = if i == 0 { root } else { chain[i - 1] };
                    (GroupKind::Known(def.id.clone()), def.name.clone(), tab_root, Some(def))
                }
                None => {
                    // Highest non-critical ancestor: a shell means a plain terminal window,
                    // anything else is an orchestrator the catalog does not know yet.
                    let below_critical: Vec<u32> = std::iter::once(root)
                        .chain(chain.iter().copied())
                        .take_while(|a| roles[a] != Role::System)
                        .collect();
                    let top = *below_critical.last().unwrap();
                    if roles[&top] == Role::Shell || top == root {
                        (GroupKind::Standalone, "Terminal".to_string(), top, None)
                    } else {
                        let tab_root = below_critical[below_critical.len().saturating_sub(2)];
                        let s = stem(&model.procs[&top]);
                        (
                            GroupKind::Unregistered(s.clone()),
                            format!("{} (unregistered)", model.procs[&top].name),
                            tab_root,
                            None,
                        )
                    }
                }
            };

            let root_proc = &model.procs[&root];
            let tab_proc = &model.procs[&tab_root];
            // The host's tab marker, from the session or (set earlier) its tab's shell.
            let marked = |var: &Option<String>| {
                let var = var.as_deref()?;
                env(root_proc, var).or_else(|| env(tab_proc, var))
            };
            let key = match host_def.and_then(|h| Some((h, marked(&h.tab_env)?))) {
                Some((h, id)) => format!("{}:{id}", h.id),
                None => format!("pid:{tab_root}"),
            };
            let label = host_def
                .and_then(|h| marked(&h.label_env))
                .and_then(|v| v.rsplit("::").next())
                .or(root_proc.cwd.as_deref())
                .map(|p| short_path(p, 2))
                .unwrap_or_else(|| tab_proc.name.clone());
            let agent = &catalog.agents[agent_idx];
            let session = Session {
                agent_id: agent.id.clone(),
                agent_name: agent.name.clone(),
                provider: provider(catalog, agent, root_proc),
                session_id: session_id(catalog, root_proc),
                root_pid: root,
            };

            let group = match model.groups.iter_mut().find(|g| g.kind == kind) {
                Some(g) => g,
                None => {
                    model.groups.push(Group {
                        kind: kind.clone(),
                        name,
                        host_pids: Vec::new(),
                        tabs: Vec::new(),
                    });
                    model.groups.last_mut().unwrap()
                }
            };
            let tab = match group.tabs.iter_mut().find(|t| t.key == key) {
                Some(t) => t,
                None => {
                    group.tabs.push(Tab {
                        key,
                        label,
                        root_pids: Vec::new(),
                        sessions: Vec::new(),
                    });
                    group.tabs.last_mut().unwrap()
                }
            };
            if !tab.root_pids.contains(&tab_root) {
                tab.root_pids.push(tab_root);
            }
            tab.sessions.push(session);
        }

        for group in &mut model.groups {
            if let GroupKind::Known(id) = &group.kind {
                let def = catalog.hosts.iter().find(|h| &h.id == id).unwrap();
                let mut pids: Vec<u32> = model
                    .procs
                    .values()
                    .filter(|p| {
                        roles[&p.pid] == Role::Host
                            && match_host(catalog, p).is_some_and(|i| catalog.hosts[i].id == def.id)
                    })
                    .map(|p| p.pid)
                    .collect();
                pids.sort_unstable();
                group.host_pids = pids;
            }
        }

        // Orphans outside sessions: helpers left behind after their agent exited, plus runtimes
        // (or shells running them) that nothing claims any more.
        let mut stray: Vec<u32> = model
            .procs
            .values()
            .filter(|p| p.ppid.is_some() && !model.parent.contains_key(&p.pid))
            .filter(|p| {
                !covered.contains(&p.pid)
                    && !matches!(roles[&p.pid], Role::Host | Role::System)
                    && !model.pty_glue.contains(&p.pid)
            })
            .map(|p| p.pid)
            .collect();
        stray.sort_unstable();
        for pid in stray {
            let subtree = model.subtree(pid);
            let reason = model.orphan_reason(catalog, pid);
            let root_stem = stem(&model.procs[&pid]);
            let is_shell = has(&catalog.processes.shells, &root_stem);
            // On Unix every app is adopted by launchd/systemd, so an adopted root counts as agent
            // machinery only by itself (or as a shell running it), not by an MCP server it runs.
            let agent_related = if reason == OrphanReason::Reparented && !is_shell {
                model.is_agent_component(catalog, &pid, &roles)
            } else {
                subtree.iter().any(|q| model.is_agent_component(catalog, q, &roles))
            };
            let runtimes = &catalog.processes.runtimes;
            let runs_runtime = subtree.iter().any(|q| has(runtimes, &stem(&model.procs[q])));
            let runtime_like = has(runtimes, &root_stem) || (is_shell && runs_runtime);
            if agent_related || runtime_like {
                let in_tree = || subtree.iter().map(|q| &model.procs[q]);
                let launched_by_agent = first_env(in_tree(), &launch_vars).is_some();
                let origin = first_env(in_tree(), &tab_vars)
                    .and_then(|(host, id)| model.tab_label(&format!("{}:{id}", host.id)));
                model.orphans.push(Orphan {
                    pid,
                    reason,
                    agent_related,
                    launched_by_agent,
                    origin,
                });
            }
        }

        model.roles = roles;
        let mut groups = std::mem::take(&mut model.groups);
        groups.sort_by_key(|g| std::cmp::Reverse(model.group_private(g)));
        model.groups = groups;
        model
    }

    fn orphan_reason(&self, catalog: &Catalog, pid: u32) -> OrphanReason {
        match self.procs[&pid].ppid.and_then(|pp| self.procs.get(&pp)) {
            Some(parent) if is_reaper(catalog, parent) => OrphanReason::Reparented,
            Some(_) => OrphanReason::PidReused,
            None => OrphanReason::ParentGone,
        }
    }

    /// An agent, a REPL, a process carrying an agent's or host's traces, or an MCP server.
    fn is_agent_component(&self, catalog: &Catalog, pid: &u32, roles: &HashMap<u32, Role>) -> bool {
        let p = &self.procs[pid];
        if matches!(roles[pid], Role::Agent(_)) || has(&catalog.processes.repl, &stem(p)) {
            return true;
        }
        let text = format!("{} {}", exe_norm(p), cmd_norm(p));
        let traces = catalog
            .agents
            .iter()
            .flat_map(|a| &a.traces)
            .chain(catalog.hosts.iter().flat_map(|h| &h.traces));
        traces.into_iter().any(|t| text.contains(t.as_str()))
            || (has(&catalog.processes.runtimes, &stem(p)) && is_mcp(catalog, &text))
    }

    /// The (group, tab, session) indices of the session that contains `pid`.
    pub fn locate(&self, pid: u32) -> Option<(usize, usize, usize)> {
        let lineage: Vec<u32> = std::iter::once(pid).chain(self.ancestors(pid)).collect();
        for (gi, g) in self.groups.iter().enumerate() {
            for (ti, t) in g.tabs.iter().enumerate() {
                for (si, s) in t.sessions.iter().enumerate() {
                    if lineage.contains(&s.root_pid) {
                        return Some((gi, ti, si));
                    }
                }
            }
        }
        None
    }

    /// Valid ancestors of `pid`, nearest first.
    pub fn ancestors(&self, pid: u32) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut cur = pid;
        while let Some(&pp) = self.parent.get(&cur) {
            chain.push(pp);
            cur = pp;
        }
        chain
    }

    pub fn tab_label(&self, key: &str) -> Option<String> {
        self.groups
            .iter()
            .flat_map(|g| &g.tabs)
            .find(|t| t.key == key)
            .map(|t| t.label.clone())
    }

    /// `pid` and all its descendants, parents before children.
    pub fn subtree(&self, pid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = vec![pid];
        while let Some(cur) = stack.pop() {
            out.push(cur);
            if let Some(kids) = self.children.get(&cur) {
                stack.extend(kids.iter().rev());
            }
        }
        out
    }

    pub fn private_bytes(&self, pids: &[u32]) -> u64 {
        pids.iter()
            .filter_map(|p| self.procs.get(p))
            .map(|p| p.private_bytes)
            .sum()
    }

    pub fn cpu(&self, pids: &[u32]) -> f32 {
        pids.iter().filter_map(|p| self.procs.get(p)).map(|p| p.cpu).sum()
    }

    pub fn tab_pids(&self, tab: &Tab) -> Vec<u32> {
        tab.sessions.iter().flat_map(|s| self.subtree(s.root_pid)).collect()
    }

    pub fn group_private(&self, group: &Group) -> u64 {
        let tabs: u64 = group.tabs.iter().map(|t| self.private_bytes(&self.tab_pids(t))).sum();
        tabs + self.private_bytes(&group.host_pids)
    }

    pub fn is_ancestor_or_self(&self, ancestor: u32, pid: u32) -> bool {
        let mut cur = pid;
        loop {
            if cur == ancestor {
                return true;
            }
            match self.parent.get(&cur) {
                Some(&pp) => cur = pp,
                None => return false,
            }
        }
    }
}
