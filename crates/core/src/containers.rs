//! Docker containers, grouped by compose project, with the checkout each project was started
//! from. A compose container carries its project's working directory as a label: when that
//! directory is gone the project is a leftover; when a live session works in the same checkout,
//! that session owns it. Everything is read through the `docker` CLI, off the UI thread.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::catalog::normalize;
use crate::model::Model;

/// One line per container, tab-separated (labels can hold commas, paths cannot hold tabs).
const PS_FORMAT: &str = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.State}}\t{{.Status}}\t{{.Ports}}\t\
    {{.Label \"com.docker.compose.project\"}}\t{{.Label \"com.docker.compose.service\"}}\t\
    {{.Label \"com.docker.compose.project.working_dir\"}}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    /// created, running, paused, restarting, removing, exited or dead.
    pub state: String,
    /// Docker's own summary, e.g. `Up 3 minutes (healthy)` or `Exited (0) 2 hours ago`.
    pub status: String,
    pub ports: String,
    pub project: Option<String>,
    pub service: Option<String>,
    pub working_dir: Option<String>,
}

impl Container {
    /// Still holds a process: it has to be stopped before it can be removed.
    pub fn running(&self) -> bool {
        matches!(self.state.as_str(), "running" | "paused" | "restarting")
    }
}

/// Where a project's working directory stands on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Place {
    /// It exists; `checkout` is the git checkout holding it, or the directory itself.
    Here { checkout: String },
    /// It no longer exists: the checkout was deleted.
    Gone,
    /// No directory, one this machine cannot check (a WSL path, a remote engine), or an error.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// The compose project name, or the container name for a container started on its own.
    pub name: String,
    pub compose: bool,
    pub working_dir: Option<String>,
    pub place: Place,
    pub containers: Vec<Container>,
}

impl Project {
    /// Its checkout is gone: proposed for cleanup.
    pub fn leftover(&self) -> bool {
        self.place == Place::Gone
    }

    pub fn running(&self) -> usize {
        self.containers.iter().filter(|c| c.running()).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Docker {
    /// Not read yet.
    Pending,
    /// No `docker` command on this machine.
    Missing,
    /// The command failed, usually because the engine is not running.
    Failed(String),
    Ready {
        projects: Vec<Project>,
        /// Absent in a saved snapshot, which cannot be used for live cleanup.
        engine: Option<Engine>,
    },
}

/// The endpoint used to read a list, retained by its cleanup plan. Local context endpoints
/// are addressed by socket, so changing the current context cannot redirect a cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    Context { name: String, host: String },
    Host(String),
}

impl Engine {
    pub fn name(&self) -> &str {
        match self {
            Self::Context { name, .. } => name,
            Self::Host(host) => host,
        }
    }

    fn host(&self) -> &str {
        match self {
            Self::Context { host, .. } | Self::Host(host) => host,
        }
    }

    fn local(&self) -> bool {
        local_endpoint(self.host())
    }
}

impl Docker {
    pub fn engine(&self) -> Option<&Engine> {
        match self {
            Self::Ready { engine, .. } => engine.as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Context {
    name: String,
    current: bool,
    docker_endpoint: String,
}

fn local_endpoint(host: &str) -> bool {
    host.starts_with("npipe://") || host.starts_with("unix://")
}

/// Prefer the selected engine, then try other local Docker-compatible endpoints regardless
/// of vendor. An explicitly selected remote engine never falls back to a different machine.
fn engines(contexts: &[Context], context: Option<&str>, host: Option<&str>) -> Result<Vec<Engine>, String> {
    let context = context.filter(|c| !c.is_empty());
    let host = host.filter(|h| !h.is_empty());
    let selected = if let (None, Some(host)) = (context, host) {
        Engine::Host(host.into())
    } else {
        let selected = contexts
            .iter()
            .find(|c| context.map_or(c.current, |name| c.name == name))
            .ok_or_else(|| format!("Docker context not found: {}", context.unwrap_or("current")))?;
        Engine::Context {
            name: selected.name.clone(),
            host: selected.docker_endpoint.clone(),
        }
    };
    let mut engines = vec![selected];
    if engines[0].local() {
        for c in contexts.iter().filter(|c| local_endpoint(&c.docker_endpoint)) {
            if engines.iter().all(|e| e.host() != c.docker_endpoint) {
                engines.push(Engine::Context {
                    name: c.name.clone(),
                    host: c.docker_endpoint.clone(),
                });
            }
        }
    }
    Ok(engines)
}

/// Reads containers from a responsive engine without changing the user's Docker context.
pub fn read() -> Docker {
    let contexts = match docker(None, &["context", "ls", "--format", "{{json .}}"])
        .and_then(|out| {
            out.lines()
                .map(|line| serde_json::from_str::<Context>(line).map_err(|e| Some(e.to_string())))
                .collect::<Result<Vec<_>, _>>()
        })
        .and_then(|contexts| {
            engines(
                &contexts,
                std::env::var("DOCKER_CONTEXT").ok().as_deref(),
                std::env::var("DOCKER_HOST").ok().as_deref(),
            )
            .map_err(Some)
        }) {
        Ok(engines) => engines,
        Err(None) => return Docker::Missing,
        Err(Some(e)) => return Docker::Failed(e),
    };
    read_engines(contexts, |engine| {
        docker(Some(engine), &["ps", "--all", "--no-trunc", "--format", PS_FORMAT])
    })
}

fn read_engines(engines: Vec<Engine>, mut ps: impl FnMut(&Engine) -> Result<String, Option<String>>) -> Docker {
    let mut errors = Vec::new();
    for engine in engines {
        match ps(&engine) {
            Ok(out) => {
                return Docker::Ready {
                    projects: group(parse_ps(&out), |dir| place(dir, engine.local())),
                    engine: Some(engine),
                };
            }
            Err(None) => return Docker::Missing,
            Err(Some(e)) => errors.push(format!("{}: {e}", engine.name())),
        }
    }
    Docker::Failed(errors.join("; "))
}

pub fn parse_ps(text: &str) -> Vec<Container> {
    let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.trim_end_matches('\r').split('\t').collect();
            if f.len() < 9 || f[0].is_empty() {
                return None;
            }
            Some(Container {
                id: f[0].into(),
                name: f[1].into(),
                image: f[2].into(),
                state: f[3].into(),
                status: f[4].into(),
                ports: f[5].into(),
                project: some(f[6]),
                service: some(f[7]),
                working_dir: some(f[8]),
            })
        })
        .collect()
}

/// Groups containers by compose project (a container started on its own is a project of one),
/// leftovers first, then by name.
pub fn group(containers: Vec<Container>, place_of: impl Fn(&str) -> Place) -> Vec<Project> {
    let mut projects: Vec<Project> = Vec::new();
    for c in containers {
        let name = c.project.clone().unwrap_or_else(|| c.name.clone());
        let compose = c.project.is_some();
        match projects.iter_mut().find(|p| p.compose && compose && p.name == name) {
            Some(p) => {
                if p.working_dir.is_none() {
                    p.working_dir = c.working_dir.clone();
                }
                p.containers.push(c);
            }
            None => projects.push(Project {
                name,
                compose,
                working_dir: c.working_dir.clone(),
                place: Place::Unknown,
                containers: vec![c],
            }),
        }
    }
    for p in &mut projects {
        p.place = p.working_dir.as_deref().map(&place_of).unwrap_or(Place::Unknown);
        p.containers.sort_by(|a, b| a.name.cmp(&b.name));
    }
    projects.sort_by(|a, b| b.leftover().cmp(&a.leftover()).then_with(|| a.name.cmp(&b.name)));
    projects
}

/// Checks a working directory on this machine. Only an absolute path of this OS whose root
/// exists can be judged gone: a WSL path seen from Windows, a remote engine's path or an
/// unplugged drive stays unknown.
pub fn place(dir: &str, local: bool) -> Place {
    let path = Path::new(dir);
    if !local || !path.is_absolute() {
        return Place::Unknown;
    }
    match path.try_exists() {
        Ok(true) => Place::Here {
            checkout: path
                .ancestors()
                .find(|a| a.join(".git").exists())
                .unwrap_or(path)
                .display()
                .to_string(),
        },
        Ok(false) if path.ancestors().last().is_some_and(Path::exists) => Place::Gone,
        _ => Place::Unknown,
    }
}

/// The live sessions working in the project's checkout: some process of theirs has its
/// working directory inside it. Returned as (group, tab, session) indices.
pub fn owners(model: &Model, project: &Project) -> Vec<(usize, usize, usize)> {
    let Place::Here { checkout } = &project.place else {
        return Vec::new();
    };
    let root = path_key(checkout);
    let inside = |cwd: &str| {
        let cwd = path_key(cwd);
        cwd == root || cwd.starts_with(&format!("{root}/"))
    };
    let mut out = Vec::new();
    for (g, group) in model.groups.iter().enumerate() {
        for (t, tab) in group.tabs.iter().enumerate() {
            for (s, session) in tab.sessions.iter().enumerate() {
                let works_there = model
                    .session_pids(session.root_pid)
                    .iter()
                    .filter_map(|p| model.procs.get(p)?.cwd.as_deref())
                    .any(inside);
                if works_there {
                    out.push((g, t, s));
                }
            }
        }
    }
    out
}

/// Paths compared the way this machine's file system does: separators unified, no trailing
/// separator, and case ignored where the file system ignores it.
fn path_key(path: &str) -> String {
    let path = if cfg!(any(windows, target_os = "macos")) {
        normalize(path)
    } else {
        path.replace('\\', "/")
    };
    path.trim_end_matches('/').to_string()
}

/// Host ports only once per mapping: `0.0.0.0:5435->5432/tcp, [::]:5435->5432/tcp` becomes
/// `5435→5432/tcp`.
pub fn short_ports(ports: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for p in ports.split(", ").filter(|p| !p.is_empty()) {
        let p = match p.rsplit_once("->") {
            Some((host, inner)) => format!("{}→{inner}", host.rsplit(':').next().unwrap_or(host)),
            None => p.to_string(),
        };
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out.join(", ")
}

/// What a confirmed cleanup does: running containers are stopped, stopped ones removed.
/// Deleted checkouts also discard their containers' anonymous and project-owned volumes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerPlan {
    /// The engine that supplied the displayed containers, captured before confirmation.
    pub engine: Option<Engine>,
    /// (id, name)
    pub stop: Vec<(String, String)>,
    pub remove: Vec<(String, String)>,
    pub volume_projects: Vec<VolumeProject>,
    /// Failed volume deletions can be retried after their containers have gone.
    pub volume_targets: Vec<VolumeTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeProject {
    name: String,
    working_dir: String,
    containers: Vec<String>,
    protection: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeTarget {
    project: VolumeProject,
    volume: Volume,
}

impl VolumeTarget {
    pub fn name(&self) -> &str {
        &self.volume.name
    }
}

impl ContainerPlan {
    pub fn is_empty(&self) -> bool {
        self.stop.is_empty() && self.remove.is_empty() && self.volume_targets.is_empty()
    }
}

/// Stops the running containers and removes the stopped ones. A leftover project's running
/// containers are removed right after they stop: their checkout is gone, so nothing will start
/// them again. Elsewhere they are only stopped, and a second cleanup removes them.
pub fn plan(projects: &[&Project], only: Option<&str>) -> ContainerPlan {
    let mut plan = ContainerPlan::default();
    for p in projects {
        let mut containers = Vec::new();
        for c in p.containers.iter().filter(|c| only.is_none_or(|id| c.id == id)) {
            containers.push(c.id.clone());
            let entry = (c.id.clone(), c.name.clone());
            if c.running() {
                plan.stop.push(entry.clone());
                if p.leftover() {
                    plan.remove.push(entry);
                }
            } else {
                plan.remove.push(entry);
            }
        }
        if p.compose
            && p.leftover()
            && !containers.is_empty()
            && let Some(dir) = &p.working_dir
            && p.containers.iter().all(|c| {
                c.working_dir
                    .as_deref()
                    .is_some_and(|other| path_key(other) == path_key(dir))
            })
        {
            plan.volume_projects.push(VolumeProject {
                name: p.name.clone(),
                working_dir: dir.clone(),
                protection: p
                    .containers
                    .iter()
                    .filter(|c| containers.contains(&c.id))
                    .map(|c| format!("{} {} {}", p.name, c.name, c.image))
                    .collect::<Vec<_>>()
                    .join(" "),
                containers,
            });
        }
    }
    plan
}

/// Containers the `protect` rules cover, by name, image or project.
pub fn refusals(projects: &[&Project], plan: &ContainerPlan, protect: &[String]) -> Vec<String> {
    let ids: Vec<&str> = plan
        .stop
        .iter()
        .chain(&plan.remove)
        .map(|(id, _)| id.as_str())
        .collect();
    let mut out = Vec::new();
    for p in projects {
        for c in p.containers.iter().filter(|c| ids.contains(&c.id.as_str())) {
            let text = normalize(&format!("{} {} {}", c.name, c.image, p.name));
            if let Some(rule) = protect.iter().find(|r| text.contains(r.as_str())) {
                out.push(format!("{} is protected by rule '{rule}'", c.name));
            }
        }
    }
    for target in &plan.volume_targets {
        let text = normalize(&target.project.protection);
        if let Some(rule) = protect.iter().find(|rule| text.contains(rule.as_str())) {
            out.push(format!("{} is protected by rule '{rule}'", target.name()));
        }
    }
    out
}

/// Containers stopped and removed, and the errors docker reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Done {
    pub stopped: usize,
    pub removed: usize,
    /// Named volumes; Docker removes anonymous volumes with their orphaned containers.
    pub volumes_removed: usize,
    pub retry: Option<ContainerPlan>,
    pub errors: Vec<String>,
}

/// Runs the plan. Container IDs are never reused, so one that changed since the plan was made
/// can only have gone, and docker then reports it as an error.
pub fn execute(plan: &ContainerPlan) -> Done {
    execute_with(plan, docker_output)
}

fn execute_with(plan: &ContainerPlan, mut docker: impl FnMut(&Engine, &[&str]) -> (String, Option<String>)) -> Done {
    let mut done = Done::default();
    let Some(engine) = plan.engine.as_ref() else {
        done.errors
            .push("No Docker engine was captured for this cleanup; refresh the containers first".into());
        return done;
    };
    // Inspect before removing containers: otherwise their mount names are lost on failure.
    let mut volumes = match project_volumes(engine, &plan.volume_projects, &mut docker) {
        Ok(volumes) => volumes,
        Err(error) => {
            done.errors.push(error);
            return done;
        }
    };
    volumes.extend(plan.volume_targets.iter().cloned());
    let mut stop = vec!["stop"];
    stop.extend(plan.stop.iter().map(|(id, _)| id.as_str()));
    done.stopped = run_counted(engine, &stop, &mut docker, &mut done.errors);
    let mut remove = vec!["rm"];
    remove.extend(plan.remove.iter().filter_map(|(id, _)| {
        (!plan.volume_projects.iter().any(|p| p.containers.contains(id))).then_some(id.as_str())
    }));
    done.removed = run_counted(engine, &remove, &mut docker, &mut done.errors);
    for project in &plan.volume_projects {
        if let Err(error) = still_gone(engine, project) {
            done.errors.push(error);
            continue;
        }
        let mut remove = vec!["rm", "--volumes"];
        remove.extend(project.containers.iter().map(String::as_str));
        done.removed += run_counted(engine, &remove, &mut docker, &mut done.errors);
    }
    let mut pending = Vec::new();
    for target in volumes {
        let volume = &target.volume;
        let result = (|| {
            still_gone(engine, &target.project)?;
            let names = query(
                engine,
                &["volume", "ls", "--quiet", "--filter", &format!("name={}", volume.name)],
                &mut docker,
            )?;
            if !names.lines().any(|name| name.trim() == volume.name) {
                return Ok(());
            }
            let current = inspect_volume(engine, &volume.name, &mut docker)?;
            if current != *volume {
                return Err(format!("Volume '{}' changed; refresh before cleanup", volume.name));
            }
            let users = query(
                engine,
                &["ps", "--all", "--quiet", "--filter", &format!("volume={}", volume.name)],
                &mut docker,
            )?;
            if users.trim().is_empty() {
                // No force: Docker also refuses a volume attached after the reference check.
                let out = query(engine, &["volume", "rm", &volume.name], &mut docker)?;
                done.volumes_removed += out.lines().filter(|line| !line.trim().is_empty()).count();
            }
            Ok(())
        })();
        if let Err(error) = result {
            done.errors.push(error);
            pending.push(target);
        }
    }
    if !pending.is_empty() {
        done.retry = Some(ContainerPlan {
            engine: Some(engine.clone()),
            volume_targets: pending,
            ..ContainerPlan::default()
        });
    }
    done
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Mount {
    #[serde(rename = "Type")]
    kind: String,
    name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
struct Volume {
    name: String,
    created_at: Option<String>,
    labels: Option<HashMap<String, String>>,
}

fn still_gone(engine: &Engine, project: &VolumeProject) -> Result<(), String> {
    if place(&project.working_dir, engine.local()) != Place::Gone {
        return Err(format!(
            "Checkout for '{}' is no longer confirmed gone; refresh before cleanup",
            project.name
        ));
    }
    Ok(())
}

fn inspect_volume(
    engine: &Engine,
    name: &str,
    docker: &mut impl FnMut(&Engine, &[&str]) -> (String, Option<String>),
) -> Result<Volume, String> {
    let out = query(engine, &["volume", "inspect", "--format", "{{json .}}", name], docker)?;
    serde_json::from_str(&out).map_err(|error| format!("Cannot inspect volume '{name}': {error}"))
}

fn project_volumes(
    engine: &Engine,
    projects: &[VolumeProject],
    docker: &mut impl FnMut(&Engine, &[&str]) -> (String, Option<String>),
) -> Result<Vec<VolumeTarget>, String> {
    let mut volumes: Vec<VolumeTarget> = Vec::new();
    for project in projects {
        still_gone(engine, project)?;
        for id in &project.containers {
            let out = query(
                engine,
                &["container", "inspect", "--format", "{{json .Mounts}}", id],
                docker,
            )?;
            let mounts: Vec<Mount> =
                serde_json::from_str(&out).map_err(|error| format!("Cannot inspect mounts of '{id}': {error}"))?;
            for mount in mounts.into_iter().filter(|m| m.kind == "volume") {
                let Some(name) = mount.name else { continue };
                if volumes.iter().any(|target| target.volume.name == name) {
                    continue;
                }
                let volume = inspect_volume(engine, &name, docker)?;
                if volume.name == name
                    && volume.created_at.is_some()
                    && volume.labels.as_ref().is_some_and(|labels| {
                        labels.get("com.docker.compose.project") == Some(&project.name)
                            && labels
                                .get("com.docker.compose.volume")
                                .is_some_and(|name| !name.is_empty())
                    })
                {
                    volumes.push(VolumeTarget {
                        project: project.clone(),
                        volume,
                    });
                }
            }
        }
    }
    Ok(volumes)
}

fn query(
    engine: &Engine,
    args: &[&str],
    docker: &mut impl FnMut(&Engine, &[&str]) -> (String, Option<String>),
) -> Result<String, String> {
    match docker(engine, args) {
        (_, Some(error)) => Err(error),
        (out, None) => Ok(out),
    }
}

fn run_counted(
    engine: &Engine,
    args: &[&str],
    docker: &mut impl FnMut(&Engine, &[&str]) -> (String, Option<String>),
    errors: &mut Vec<String>,
) -> usize {
    let prefix = if args.starts_with(&["volume", "rm"]) || args.starts_with(&["rm", "--volumes"]) {
        2
    } else {
        1
    };
    if args.len() <= prefix {
        return 0;
    }
    let (out, error) = docker(engine, args);
    if let Some(error) = error {
        errors.push(error);
    }
    out.lines().filter(|line| !line.trim().is_empty()).count()
}

/// Runs docker and returns stdout; `Err(None)` when there is no docker command.
fn docker(engine: Option<&Engine>, args: &[&str]) -> Result<String, Option<String>> {
    let out = docker_command(engine, args)
        .output()
        .map_err(|e| (e.kind() != std::io::ErrorKind::NotFound).then(|| e.to_string()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else if out.stderr.iter().all(u8::is_ascii_whitespace) {
        // Docker Desktop's stand-in, in a WSL distro without its integration, explains on stdout.
        Err(Some(first_line(&out.stdout)))
    } else {
        Err(Some(first_line(&out.stderr)))
    }
}

/// Uses the captured endpoint for inspection and destructive commands alike.
fn docker_output(engine: &Engine, args: &[&str]) -> (String, Option<String>) {
    if let Err(error) = verify_endpoint(engine, |name| {
        docker(
            None,
            &["context", "inspect", name, "--format", "{{.Endpoints.docker.Host}}"],
        )
    }) {
        return (String::new(), Some(error));
    }
    match docker_command(Some(engine), args).output() {
        Ok(out) => (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            (!out.status.success()).then(|| first_line(&out.stderr)),
        ),
        Err(e) => (String::new(), Some(e.to_string())),
    }
}

/// Remote contexts carry TLS/SSH settings, so retain the context but reject endpoint edits
/// before each destructive command. Local commands already use their captured socket.
fn verify_endpoint(
    engine: &Engine,
    mut inspect: impl FnMut(&str) -> Result<String, Option<String>>,
) -> Result<(), String> {
    if let Engine::Context { name, host } = engine
        && !engine.local()
    {
        let current = inspect(name).map_err(|e| e.unwrap_or_else(|| "docker is not installed".into()))?;
        if current.trim() != host {
            return Err(format!(
                "Docker context '{name}' changed endpoints; refresh the containers before cleanup"
            ));
        }
    }
    Ok(())
}

fn docker_command(engine: Option<&Engine>, args: &[&str]) -> Command {
    let mut command = Command::new("docker");
    command.env_remove("DOCKER_CONTEXT").env_remove("DOCKER_HOST");
    if let Some(engine) = engine {
        match engine {
            Engine::Context { name, .. } if !engine.local() => {
                command.args(["--context", name]);
            }
            _ => {
                command.args(["--host", engine.host()]);
            }
        }
    }
    // Context enumeration does not connect to a daemon, but the CLI still initializes TLS.
    // Keep TLS environment settings only for an explicitly selected remote DOCKER_HOST.
    if engine.is_none_or(|e| matches!(e, Engine::Context { .. }) || e.local()) {
        for key in ["DOCKER_TLS", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"] {
            command.env_remove(key);
        }
    }
    command.args(args).stdin(Stdio::null());
    command
}

fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("docker failed")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "aaa\tweb-db-1\tpostgres:16\trunning\tUp 3 minutes\t0.0.0.0:5435->5432/tcp, [::]:5435->5432/tcp\tweb\tdb\t/work/web\n\
        bbb\tweb-redis-1\tredis:8\texited\tExited (255) 2 hours ago\t\tweb\tredis\t/work/web\n\
        ccc\tsnapshot-db\tpostgres:16\texited\tExited (0) 2 days ago\t\t\t\t\n\
        ddd\told-db-1\tpostgres:16\trunning\tUp 1 hour\t\told\tdb\t/work/old\n";

    fn contexts(current: &str) -> Vec<Context> {
        [
            ("default", "npipe:////./pipe/docker_engine"),
            ("another-engine", "unix:///tmp/another-engine.sock"),
            ("shared", "ssh://remote.example"),
        ]
        .into_iter()
        .map(|(name, host)| Context {
            name: name.into(),
            current: name == current,
            docker_endpoint: host.into(),
        })
        .collect()
    }

    #[test]
    fn discovers_any_local_engine_when_the_selected_one_is_unavailable() {
        for selected in ["default", "another-engine"] {
            let candidates = engines(&contexts(selected), None, None).unwrap();
            let mut tried = Vec::new();
            let docker = read_engines(candidates, |engine| {
                tried.push(engine.name().to_string());
                if engine.name() == selected {
                    Err(Some("engine is stopped".into()))
                } else {
                    Ok(PS.into())
                }
            });
            let Docker::Ready { projects, engine } = docker else {
                panic!("{docker:?}")
            };
            assert_eq!(tried.len(), 2);
            assert_eq!(tried[0], selected);
            assert_eq!(engine.unwrap().name(), tried[1]);
            assert_eq!(projects.iter().map(|p| p.containers.len()).sum::<usize>(), 4);
        }
    }

    #[test]
    fn a_working_selected_engine_wins_even_when_it_has_no_containers() {
        let mut tried = Vec::new();
        let docker = read_engines(engines(&contexts("another-engine"), None, None).unwrap(), |engine| {
            tried.push(engine.name().to_string());
            Ok(String::new())
        });
        assert_eq!(tried, ["another-engine"]);
        assert!(matches!(docker, Docker::Ready { projects, .. } if projects.is_empty()));
    }

    #[test]
    fn environment_selection_has_dockers_precedence() {
        let contexts = contexts("default");
        let selected = engines(&contexts, Some("shared"), Some("unix:///tmp/override.sock")).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name(), "shared");
        let selected = engines(&contexts, None, Some("unix:///tmp/override.sock")).unwrap();
        assert_eq!(selected[0], Engine::Host("unix:///tmp/override.sock".into()));
        assert_eq!(selected.len(), 3);
        assert_eq!(engines(&contexts, Some(""), Some("")).unwrap()[0].name(), "default");
        assert!(engines(&contexts, Some("deleted-context"), None).is_err());
    }

    #[test]
    fn remote_engines_are_only_used_when_selected_and_never_fall_back() {
        let contexts = contexts("shared");
        let selected = engines(&contexts, None, None).unwrap();
        assert_eq!(selected.len(), 1);
        let docker = read_engines(selected.clone(), |_| Ok(PS.into()));
        let Docker::Ready { projects, .. } = docker else {
            panic!("{docker:?}")
        };
        assert!(projects.iter().all(|p| p.place == Place::Unknown));
        let docker = read_engines(selected, |_| Err(Some("offline".into())));
        assert_eq!(docker, Docker::Failed("shared: offline".into()));
        let local = engines(&contexts, Some("default"), None).unwrap();
        assert!(local.iter().all(Engine::local));
        let remote_host = engines(&contexts, None, Some("tcp://remote.example:2376")).unwrap();
        assert_eq!(remote_host.len(), 1);
    }

    #[test]
    fn duplicate_local_endpoints_are_only_tried_once() {
        let mut contexts = contexts("default");
        contexts.push(Context {
            name: "alias".into(),
            current: false,
            docker_endpoint: contexts[0].docker_endpoint.clone(),
        });
        assert_eq!(engines(&contexts, None, None).unwrap().len(), 2);
    }

    #[test]
    fn reports_all_attempted_engines_and_a_missing_cli() {
        let candidates = engines(&contexts("default"), None, None).unwrap();
        assert_eq!(
            read_engines(candidates.clone(), |_| Err(Some("offline".into()))),
            Docker::Failed("default: offline; another-engine: offline".into())
        );
        assert_eq!(read_engines(candidates, |_| Err(None)), Docker::Missing);
    }

    #[test]
    fn local_commands_pin_the_endpoint_and_ignore_ambient_context_and_tls() {
        let selected = engines(&contexts("another-engine"), None, None).unwrap();
        let command = docker_command(Some(&selected[0]), &["ps"]);
        assert_eq!(
            command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["--host", "unix:///tmp/another-engine.sock", "ps"]
        );
        for key in [
            "DOCKER_CONTEXT",
            "DOCKER_HOST",
            "DOCKER_TLS",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
        ] {
            assert!(command.get_envs().any(|(k, v)| k == key && v.is_none()), "{key}");
        }
        let remote = engines(&contexts("shared"), None, None).unwrap();
        let command = docker_command(Some(&remote[0]), &["ps"]);
        assert_eq!(
            command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["--context", "shared", "ps"]
        );
    }

    #[test]
    fn discovery_ignores_stale_tls_but_explicit_remote_hosts_keep_it() {
        let discovery = docker_command(None, &["context", "ls"]);
        let remote = Engine::Host("tcp://remote.example:2376".into());
        let command = docker_command(Some(&remote), &["ps"]);
        for key in ["DOCKER_TLS", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"] {
            assert!(discovery.get_envs().any(|(k, v)| k == key && v.is_none()));
            assert!(!command.get_envs().any(|(k, _)| k == key));
        }
    }

    #[test]
    fn remote_cleanup_refuses_a_context_that_was_retargeted_or_removed() {
        let remote = Engine::Context {
            name: "shared".into(),
            host: "ssh://host-a".into(),
        };
        assert!(
            verify_endpoint(&remote, |name| {
                assert_eq!(name, "shared");
                Ok("ssh://host-a\n".into())
            })
            .is_ok()
        );
        let changed = verify_endpoint(&remote, |_| Ok("ssh://host-b\n".into())).unwrap_err();
        assert!(changed.contains("changed endpoints"));
        assert!(verify_endpoint(&remote, |_| Err(Some("context removed".into()))).is_err());
        let local = Engine::Context {
            name: "local".into(),
            host: "unix:///first.sock".into(),
        };
        assert!(verify_endpoint(&local, |_| panic!("local cleanup uses the captured socket")).is_ok());
    }

    #[test]
    fn cleanup_uses_the_captured_engine_and_never_discovers_a_replacement() {
        let selected = engines(&contexts("another-engine"), None, None).unwrap();
        let plan = ContainerPlan {
            engine: Some(selected[0].clone()),
            stop: vec![("aaa".into(), "db".into())],
            remove: vec![("bbb".into(), "redis".into())],
            ..ContainerPlan::default()
        };
        let mut calls = Vec::new();
        let done = execute_with(&plan, |engine, args| {
            assert_eq!(engine, &selected[0]);
            calls.push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            (String::new(), Some("engine is offline".into()))
        });
        assert_eq!(calls, [vec!["stop", "aaa"], vec!["rm", "bbb"]]);
        assert_eq!(done.errors.len(), 2);
        assert_eq!((done.stopped, done.removed), (0, 0));
        let unbound = ContainerPlan { engine: None, ..plan };
        let done = execute_with(&unbound, |_, _| panic!("an unbound plan must not run"));
        assert_eq!(done.errors.len(), 1);
    }

    fn projects() -> Vec<Project> {
        group(parse_ps(PS), |dir| match dir {
            "/work/web" => Place::Here {
                checkout: "/work/web".into(),
            },
            _ => Place::Gone,
        })
    }

    #[test]
    fn groups_by_compose_project_with_leftovers_first() {
        let p = projects();
        let names: Vec<&str> = p.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["old", "snapshot-db", "web"]);
        assert!(p[0].leftover());
        // A container started on its own has no working directory to judge.
        assert!(!p[1].compose && p[1].place == Place::Unknown);
        assert_eq!(p[2].containers.len(), 2);
        assert_eq!(p[2].running(), 1);
    }

    #[test]
    fn stops_running_removes_stopped_and_removes_leftovers_outright() {
        let p = projects();
        let all: Vec<&Project> = p.iter().collect();
        let plan = plan(&all, None);
        let ids = |l: &[(String, String)]| l.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&plan.stop), ["ddd", "aaa"]);
        // The live checkout's running database is only stopped.
        assert_eq!(ids(&plan.remove), ["ddd", "ccc", "bbb"]);
        let one = super::plan(&all, Some("aaa"));
        assert_eq!(ids(&one.stop), ["aaa"]);
        assert!(one.remove.is_empty());
    }

    #[test]
    fn leftover_cleanup_discards_database_volumes() {
        let plan = orphan_plan();
        let mut calls = Vec::new();
        let done = execute_with(&plan, |_, args| {
            calls.push(args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>());
            cleanup_reply(args)
        });
        assert_eq!((done.stopped, done.removed, done.volumes_removed), (1, 1, 1));
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert!(calls.contains(&vec!["rm".into(), "--volumes".into(), "ddd".into()]));
        let deleted: Vec<_> = calls
            .iter()
            .filter(|args| args.starts_with(&["volume".into(), "rm".into()]))
            .collect();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0], &["volume", "rm", "old_pgdata"]);
    }

    fn orphan_plan() -> ContainerPlan {
        let mut old = projects().into_iter().find(|p| p.name == "old").unwrap();
        let dir = std::env::temp_dir().join(format!("agentlens-gone-checkout-{}", std::process::id()));
        old.working_dir = Some(dir.display().to_string());
        old.containers[0].working_dir = old.working_dir.clone();
        let mut plan = plan(&[&old], None);
        plan.engine = Some(Engine::Host("unix:///tmp/docker.sock".into()));
        plan
    }

    fn cleanup_reply(args: &[&str]) -> (String, Option<String>) {
        let out = match args {
            ["container", "inspect", "--format", "{{json .Mounts}}", "ddd"] => serde_json::json!([
                {"Type":"volume", "Name":"old_pgdata"},
                {"Type":"volume", "Name":"shared_pgdata"},
                {"Type":"volume", "Name":"external_pgdata"},
                {"Type":"volume", "Name":"anonymous"},
                {"Type":"bind", "Source":"/some/files"},
                {"Type":"tmpfs"}
            ])
            .to_string(),
            ["volume", "inspect", "--format", "{{json .}}", name] => {
                let labels = match *name {
                    "old_pgdata" | "shared_pgdata" => serde_json::json!({
                        "com.docker.compose.project":"old", "com.docker.compose.volume":"pgdata"
                    }),
                    "external_pgdata" => serde_json::json!({
                        "com.docker.compose.project":"other", "com.docker.compose.volume":"pgdata"
                    }),
                    "anonymous" => serde_json::json!({"com.docker.volume.anonymous":""}),
                    _ => panic!("unexpected volume: {name}"),
                };
                serde_json::json!({"Name":name, "CreatedAt":"2026-11-11T12:00:00Z", "Labels":labels}).to_string()
            }
            ["ps", "--all", "--quiet", "--filter", "volume=shared_pgdata"] => "another-container\n".into(),
            ["ps", "--all", "--quiet", "--filter", "volume=old_pgdata"] => String::new(),
            ["volume", "ls", "--quiet", "--filter", name] => name.strip_prefix("name=").unwrap().into(),
            ["stop", ids @ ..] | ["rm", "--volumes", ids @ ..] | ["volume", "rm", ids @ ..] => ids.join("\n"),
            _ => panic!("unexpected command: {args:?}"),
        };
        (out, None)
    }

    #[test]
    fn volume_inspection_failure_preserves_containers_for_retry() {
        let mut mutations = Vec::new();
        let done = execute_with(&orphan_plan(), |_, args| {
            if args.starts_with(&["volume", "inspect"]) {
                return (String::new(), Some("volume inspection failed".into()));
            }
            if args[0] != "container" {
                mutations.push(args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>());
            }
            cleanup_reply(args)
        });
        assert!(mutations.is_empty());
        assert_eq!(done.errors, ["volume inspection failed"]);
    }

    #[test]
    fn failed_volume_deletion_can_be_retried_without_its_container() {
        let done = execute_with(&orphan_plan(), |_, args| {
            if args == ["volume", "rm", "old_pgdata"] {
                return (String::new(), Some("temporary volume deletion failure".into()));
            }
            cleanup_reply(args)
        });
        assert_eq!(done.removed, 1);
        let retry = done.retry.unwrap();
        assert!(!retry.is_empty());
        assert_eq!(retry.volume_targets[0].name(), "old_pgdata");
        assert_eq!(
            refusals(&[], &retry, &["postgres".into()]),
            ["old_pgdata is protected by rule 'postgres'"]
        );
        let retried = execute_with(&retry, |_, args| {
            assert!(!matches!(args[0], "container" | "stop" | "rm"));
            cleanup_reply(args)
        });
        assert!(retried.errors.is_empty(), "{:?}", retried.errors);
        assert_eq!((retried.removed, retried.volumes_removed), (0, 1));
        assert!(retried.retry.is_none());
        let already_gone = execute_with(&retry, |_, args| {
            assert_eq!(args[0..2], ["volume", "ls"]);
            (String::new(), None)
        });
        assert!(already_gone.errors.is_empty());
        assert!(already_gone.retry.is_none());
    }

    #[test]
    fn a_failed_container_removal_keeps_its_volume() {
        let mut deleted = false;
        let done = execute_with(&orphan_plan(), |_, args| {
            if args.starts_with(&["rm"]) {
                return (String::new(), Some("container is still running".into()));
            }
            if args == ["ps", "--all", "--quiet", "--filter", "volume=old_pgdata"] {
                return ("ddd\n".into(), None);
            }
            deleted |= args.starts_with(&["volume", "rm"]);
            cleanup_reply(args)
        });
        assert!(!deleted);
        assert_eq!((done.removed, done.volumes_removed), (0, 0));
        assert_eq!(done.errors, ["container is still running"]);
    }

    #[test]
    fn a_recreated_volume_is_not_deleted() {
        let mut inspected = false;
        let mut deleted = false;
        let done = execute_with(&orphan_plan(), |_, args| {
            let (out, error) = cleanup_reply(args);
            if args == ["volume", "inspect", "--format", "{{json .}}", "old_pgdata"] {
                if inspected {
                    return (out.replace("12:00:00Z", "12:00:01Z"), error);
                }
                inspected = true;
            }
            deleted |= args.starts_with(&["volume", "rm"]);
            (out, error)
        });
        assert!(!deleted);
        assert_eq!(done.volumes_removed, 0);
        assert_eq!(done.errors.len(), 1);
        assert!(done.errors[0].contains("changed"));
    }

    #[test]
    fn a_restored_checkout_prevents_volume_cleanup() {
        let mut plan = orphan_plan();
        plan.volume_projects[0].working_dir = std::env::temp_dir().display().to_string();
        let done = execute_with(&plan, |_, _| panic!("a restored checkout must not be touched"));
        assert_eq!(done.errors.len(), 1);
        assert!(done.errors[0].contains("no longer confirmed gone"));
        plan.engine = Some(Engine::Host("ssh://remote.example".into()));
        let done = execute_with(&plan, |_, _| panic!("an unverifiable checkout must not be touched"));
        assert_eq!(done.errors.len(), 1);
    }

    #[test]
    fn live_unknown_and_conflicting_checkouts_keep_their_volumes() {
        let mut projects = projects();
        let all: Vec<_> = projects.iter().collect();
        let selected = plan(&all, Some("aaa"));
        assert!(selected.volume_projects.is_empty());
        let selected = plan(&all, Some("ccc"));
        assert!(selected.volume_projects.is_empty());
        let old = projects.iter_mut().find(|p| p.name == "old").unwrap();
        let mut other = old.containers[0].clone();
        other.id = "eee".into();
        other.working_dir = Some("/different/checkout".into());
        old.containers.push(other);
        assert!(plan(&[old], None).volume_projects.is_empty());
    }

    #[test]
    fn protect_rules_cover_containers() {
        let p = projects();
        let all: Vec<&Project> = p.iter().collect();
        let plan = plan(&all, None);
        let r = refusals(&all, &plan, &["redis".into()]);
        assert_eq!(r, ["web-redis-1 is protected by rule 'redis'"]);
    }

    #[test]
    fn only_paths_this_machine_can_check_are_judged() {
        assert_eq!(place("relative/dir", true), Place::Unknown);
        let missing = std::env::temp_dir().join("agentlens-no-such-dir-4b1d");
        let missing = missing.to_str().unwrap();
        assert_eq!(place(missing, true), Place::Gone);
        // A remote engine's paths mean nothing here.
        assert_eq!(place(missing, false), Place::Unknown);
        let here = std::env::temp_dir();
        assert!(matches!(place(here.to_str().unwrap(), true), Place::Here { .. }));
        #[cfg(windows)]
        assert_eq!(place("/home/me/project", true), Place::Unknown, "a WSL path");
    }

    #[test]
    fn shortens_port_mappings() {
        assert_eq!(
            short_ports("0.0.0.0:5435->5432/tcp, [::]:5435->5432/tcp, 6379/tcp"),
            "5435→5432/tcp, 6379/tcp"
        );
    }
}
