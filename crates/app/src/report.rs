//! Plain-text rendering of the model, for `--report` and quick checks without the TUI.

use std::fmt::Write;

use agentlens_core::health::{Candidate, fmt_duration, short_cmd, tree_label};
use agentlens_core::model::{HelperKind, Model, Role};
use agentlens_core::net::{self, Port, PortOwner, SshConn};

pub fn mb(bytes: u64) -> String {
    format!("{}MB", bytes >> 20)
}

pub fn role_label(model: &Model, pid: u32) -> &'static str {
    match model.roles.get(&pid) {
        Some(Role::System) => "system",
        Some(Role::Host) => "host",
        Some(Role::Shell) => "shell",
        Some(Role::Agent(_)) => "agent",
        Some(Role::Helper(HelperKind::Repl)) => "repl",
        Some(Role::Helper(HelperKind::Mcp)) => "mcp",
        Some(Role::Helper(HelperKind::Browser)) => "browser",
        Some(Role::Helper(HelperKind::Sandbox)) => "sandbox",
        Some(Role::Helper(HelperKind::Shell)) => "shell",
        Some(Role::Helper(HelperKind::Tool)) => "tool",
        Some(Role::Other) | None => "",
    }
}

pub fn render(model: &Model, candidates: &[Candidate], ssh: &[SshConn]) -> String {
    let mut out = String::new();
    for group in &model.groups {
        let _ = writeln!(
            out,
            "{}  [{} tabs, {}]  host procs {} {}",
            group.name,
            group.tabs.len(),
            mb(model.group_private(group)),
            group.host_pids.len(),
            mb(model.private_bytes(&group.host_pids))
        );
        for tab in &group.tabs {
            let pids = model.tab_pids(tab);
            let _ = writeln!(
                out,
                "  tab {}  ({})  {} procs {}",
                tab.label,
                tab.key,
                pids.len(),
                mb(model.private_bytes(&pids))
            );
            for session in &tab.sessions {
                let tree = model.subtree(session.root_pid);
                let all = model.session_pids(session.root_pid);
                let provider = session
                    .provider
                    .as_deref()
                    .map(|p| format!(" ({p})"))
                    .unwrap_or_default();
                let sid = session
                    .session_id
                    .as_deref()
                    .map(|s| format!(" session {s}"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "    {}{provider}  pid {}  {} procs {}{sid}",
                    session.agent_name,
                    session.root_pid,
                    all.len(),
                    mb(model.private_bytes(&all))
                );
                for pid in tree.iter().skip(1) {
                    let p = &model.procs[pid];
                    let depth = model
                        .ancestors(*pid)
                        .iter()
                        .take_while(|a| **a != session.root_pid)
                        .count()
                        + 1;
                    let _ = writeln!(
                        out,
                        "    {}{} {} [{}] {} {}",
                        "  ".repeat(depth),
                        p.name,
                        pid,
                        role_label(model, *pid),
                        mb(p.private_bytes),
                        short_cmd(p)
                    );
                }
                for pid in model.detached.get(&session.root_pid).into_iter().flatten() {
                    let tree = model.subtree(*pid);
                    let _ = writeln!(
                        out,
                        "      detached {} {} {}",
                        pid,
                        mb(model.private_bytes(&tree)),
                        tree_label(model, *pid)
                    );
                }
            }
        }
    }
    let orphans: Vec<_> = model.orphans.iter().filter(|o| !model.is_detached(o.pid)).collect();
    if !orphans.is_empty() {
        let _ = writeln!(out, "Orphans");
        for o in orphans {
            let p = &model.procs[&o.pid];
            let tree = model.subtree(o.pid);
            let _ = writeln!(
                out,
                "  {} {} {:?} agent_related={} {} procs {}  {}",
                p.name,
                o.pid,
                o.reason,
                o.agent_related,
                tree.len(),
                mb(model.private_bytes(&tree)),
                short_cmd(p)
            );
        }
    }
    if !candidates.is_empty() {
        let _ = writeln!(out, "Cleanup candidates");
        for c in candidates {
            let mark = if c.eligible { "*" } else { " " };
            let _ = writeln!(
                out,
                " {mark} {:?} {} {} [{}]",
                c.kind,
                c.label,
                mb(c.private_bytes),
                c.reasons.join(", ")
            );
        }
    }
    let ports: Vec<Port> = net::ports(model).into_iter().filter(|p| p.agent_related()).collect();
    if !ports.is_empty() {
        let _ = writeln!(out, "Ports");
        for p in ports {
            let owner = match p.owner {
                PortOwner::Session(g, t, s) => {
                    let tab = &model.groups[g].tabs[t];
                    format!(
                        "{} @ {} / {}",
                        tab.sessions[s].agent_name, model.groups[g].name, tab.label
                    )
                }
                PortOwner::Orphan(root) => format!("orphan {root}"),
                PortOwner::Other => String::new(),
            };
            let _ = writeln!(
                out,
                "  :{} {} {} pid {} clients {}  {owner}{}",
                p.port,
                if p.exposed { "exposed" } else { "local" },
                p.process,
                p.pid,
                p.clients,
                if p.agent_internal { " (agent's own)" } else { "" }
            );
        }
    }
    if !ssh.is_empty() {
        let _ = writeln!(out, "SSH");
        for c in ssh {
            let dir = if c.inbound { "in " } else { "out" };
            let who = c.user.as_deref().map(|u| format!("{u}@")).unwrap_or_default();
            let alias = c.alias.as_deref().map(|a| format!(" ({a})")).unwrap_or_default();
            let since = fmt_duration(model.taken_at.saturating_sub(c.since));
            let _ = writeln!(
                out,
                "  {dir} {who}{}:{}{alias}  {}  pid {} {}  {since}",
                c.host, c.port, c.remote, c.pid, c.process
            );
        }
    }
    out
}
