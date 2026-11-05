//! Docker containers, grouped by compose project, with the checkout each project was started
//! from. A compose container carries its project's working directory as a label: when that
//! directory is gone the project is a leftover; when a live session works in the same checkout,
//! that session owns it. Everything is read through the `docker` CLI, off the UI thread.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::catalog::normalize;
use crate::model::Model;

/// One line per container, tab-separated (labels can hold commas, paths cannot hold tabs).
const PS_FORMAT: &str = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.State}}\t{{.Status}}\t{{.Ports}}\t\
    {{.Label \"com.docker.compose.project\"}}\t{{.Label \"com.docker.compose.service\"}}\t\
    {{.Label \"com.docker.compose.project.working_dir\"}}";

#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// It exists; `checkout` is the git checkout holding it, or the directory itself.
    Here { checkout: String },
    /// It no longer exists: the checkout was deleted.
    Gone,
    /// No directory, one this machine cannot check (a WSL path, a remote engine), or an error.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    Ready(Vec<Project>),
}

/// Reads the containers with the `docker` CLI. `local` says whether the engine runs on this
/// machine, so its working directories can be checked here.
pub fn read(local: bool) -> Docker {
    match docker(&["ps", "--all", "--no-trunc", "--format", PS_FORMAT]) {
        Ok(out) => Docker::Ready(group(parse_ps(&out), |dir| place(dir, local))),
        Err(None) => Docker::Missing,
        Err(Some(e)) => Docker::Failed(e),
    }
}

/// Whether the current engine runs on this machine (a named pipe or Unix socket), not on a
/// remote host whose paths mean nothing here.
pub fn engine_is_local() -> bool {
    let host = match std::env::var("DOCKER_HOST") {
        Ok(h) if !h.is_empty() => h,
        _ => docker(&["context", "inspect", "--format", "{{.Endpoints.docker.Host}}"]).unwrap_or_default(),
    };
    let host = host.trim();
    host.is_empty() || host.starts_with("npipe://") || host.starts_with("unix://")
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
/// Volumes are never removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerPlan {
    /// (id, name)
    pub stop: Vec<(String, String)>,
    pub remove: Vec<(String, String)>,
}

impl ContainerPlan {
    pub fn is_empty(&self) -> bool {
        self.stop.is_empty() && self.remove.is_empty()
    }
}

/// Stops the running containers and removes the stopped ones. A leftover project's running
/// containers are removed right after they stop: their checkout is gone, so nothing will start
/// them again. Elsewhere they are only stopped, and a second cleanup removes them.
pub fn plan(projects: &[&Project], only: Option<&str>) -> ContainerPlan {
    let mut plan = ContainerPlan::default();
    for p in projects {
        for c in p.containers.iter().filter(|c| only.is_none_or(|id| c.id == id)) {
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
    out
}

/// Containers stopped and removed, and the errors docker reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Done {
    pub stopped: usize,
    pub removed: usize,
    pub errors: Vec<String>,
}

/// Runs the plan. Container IDs are never reused, so one that changed since the plan was made
/// can only have gone, and docker then reports it as an error.
pub fn execute(plan: &ContainerPlan) -> Done {
    let mut done = Done::default();
    let mut run = |verb: &str, list: &[(String, String)]| -> usize {
        if list.is_empty() {
            return 0;
        }
        let mut args = vec![verb];
        args.extend(list.iter().map(|(id, _)| id.as_str()));
        match docker_counted(&args) {
            (n, None) => n,
            (n, Some(e)) => {
                done.errors.push(e);
                n
            }
        }
    };
    let stopped = run("stop", &plan.stop);
    let removed = run("rm", &plan.remove);
    done.stopped = stopped;
    done.removed = removed;
    done
}

/// Runs docker and returns stdout; `Err(None)` when there is no docker command.
fn docker(args: &[&str]) -> Result<String, Option<String>> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
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

/// Runs a docker command that prints one line per container it handled.
fn docker_counted(args: &[&str]) -> (usize, Option<String>) {
    match Command::new("docker").args(args).stdin(Stdio::null()).output() {
        Ok(out) => {
            let n = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count();
            (n, (!out.status.success()).then(|| first_line(&out.stderr)))
        }
        Err(e) => (0, Some(e.to_string())),
    }
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
