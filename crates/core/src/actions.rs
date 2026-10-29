//! Ending process trees, with every target re-verified right before it is terminated.

use crate::catalog::{Catalog, normalize};
use crate::model::{Model, Role, stem};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub pid: u32,
    pub start_time: u64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct KillPlan {
    /// Parents before children, so no parent can react to a child's exit (run its next
    /// command, restart it) before it is ended itself. Each target is identified by PID and
    /// start time, so children are still found after they are re-parented.
    pub targets: Vec<Target>,
    pub private_bytes: u64,
    pub connections: usize,
    /// A host process the user chose to end on purpose: the one serving a connection being
    /// disconnected, such as an `sshd-session`. Other host processes stay refused.
    pub chosen_host: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Killed,
    AlreadyGone,
    /// The PID now belongs to a different process; left alone.
    Replaced,
    Failed(String),
}

pub fn plan(model: &Model, roots: &[u32]) -> KillPlan {
    let mut pids: Vec<u32> = Vec::new();
    for &root in roots {
        for pid in model.subtree(root) {
            if !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    KillPlan {
        targets: pids
            .iter()
            .map(|pid| {
                let p = &model.procs[pid];
                Target {
                    pid: *pid,
                    start_time: p.start_time,
                    name: p.name.clone(),
                }
            })
            .collect(),
        private_bytes: model.private_bytes(&pids),
        connections: crate::net::connections_of(model, &pids).count(),
        chosen_host: None,
    }
}

/// Ends the process that owns a connection, even when it is a host process: closing an SSH
/// login means ending its `sshd-session`.
pub fn disconnect_plan(model: &Model, owner: u32) -> KillPlan {
    KillPlan {
        chosen_host: Some(owner),
        ..plan(model, &[owner])
    }
}

/// Reasons the plan must not run; empty when it is safe.
pub fn refusals(plan: &KillPlan, model: &Model, catalog: &Catalog, self_pid: u32) -> Vec<String> {
    let mut out = Vec::new();
    for t in &plan.targets {
        let Some(p) = model.procs.get(&t.pid) else { continue };
        if model.is_ancestor_or_self(t.pid, self_pid) {
            out.push(format!("{} ({}) runs agentlens itself", p.name, p.pid));
        } else if model.roles.get(&t.pid) == Some(&Role::Host) && plan.chosen_host != Some(t.pid) {
            out.push(format!("{} ({}) is a host application", p.name, p.pid));
        } else if model.roles.get(&t.pid) == Some(&Role::System) {
            out.push(format!("{} ({}) is a system process", p.name, p.pid));
        } else {
            let text = format!("{} {}", stem(p), normalize(&p.cmd.join(" ")));
            if let Some(rule) = catalog.protect.iter().find(|r| text.contains(r.as_str())) {
                out.push(format!("{} ({}) is protected by rule '{rule}'", p.name, p.pid));
            }
        }
    }
    out
}

pub fn execute(plan: &KillPlan) -> Vec<(Target, Outcome)> {
    let targets: Vec<(u32, u64)> = plan.targets.iter().map(|t| (t.pid, t.start_time)).collect();
    plan.targets
        .iter()
        .cloned()
        .zip(crate::sys::terminate_all(&targets))
        .collect()
}
