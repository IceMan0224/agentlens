use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use agentlens_core::containers::{self, Docker, Engine};

struct Fixture {
    host: String,
    prefix: String,
    dir: PathBuf,
    containers: Vec<String>,
    volumes: Vec<String>,
}

impl Fixture {
    fn command(&self, args: &[&str]) -> Output {
        Command::new("docker")
            .args(["--host", &self.host])
            .args(args)
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST")
            .env_remove("DOCKER_TLS")
            .env_remove("DOCKER_TLS_VERIFY")
            .env_remove("DOCKER_CERT_PATH")
            .stdin(Stdio::null())
            .output()
            .expect("docker must be installed")
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.command(args);
        assert!(
            out.status.success(),
            "docker {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().into()
    }

    fn volume(&mut self, suffix: &str, owner: Option<&str>) -> String {
        let name = format!("{}-{suffix}", self.prefix);
        self.volumes.push(name.clone());
        let mut args = vec!["volume", "create"];
        let project = format!("com.docker.compose.project={}", owner.unwrap_or_default());
        if owner.is_some() {
            args.extend(["--label", &project, "--label", "com.docker.compose.volume=pgdata"]);
        }
        args.push(&name);
        self.run(&args);
        name
    }

    fn exists(&self, kind: &str, name: &str) -> bool {
        self.command(&[kind, "inspect", name]).status.success()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for name in &self.containers {
            self.command(&["container", "rm", "--force", "--volumes", name]);
        }
        for name in &self.volumes {
            self.command(&["volume", "rm", name]);
        }
        let _ = std::fs::remove_dir(self.dir.join("live"));
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[test]
#[ignore = "requires a local Docker engine and an existing alpine:latest image; creates disposable fixtures"]
fn orphan_cleanup_discards_only_its_unused_volumes() {
    let engine = containers::read().engine().cloned().expect("Docker must be running");
    let host = match &engine {
        Engine::Host(host) | Engine::Context { host, .. } => host.clone(),
    };
    assert!(
        host.starts_with("unix://") || host.starts_with("npipe://"),
        "requires a local engine"
    );
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let prefix = format!("agentlens-cleanup-{}-{nonce}", std::process::id());
    let mut fixture = Fixture {
        host,
        dir: std::env::temp_dir().join(&prefix),
        containers: ["orphan", "shared-user", "live"]
            .map(|suffix| format!("{prefix}-{suffix}"))
            .into(),
        volumes: Vec::new(),
        prefix: prefix.clone(),
    };
    fixture.run(&["image", "inspect", "alpine:latest"]);
    std::fs::create_dir_all(fixture.dir.join("live")).unwrap();
    let owned = fixture.volume("owned", Some(&prefix));
    let shared = fixture.volume("shared", Some(&prefix));
    let foreign = fixture.volume("foreign", Some("some-other-project"));
    let live_project = format!("{prefix}-live");
    let live_volume = fixture.volume("live-data", Some(&live_project));
    let orphan = fixture.containers[0].clone();
    let shared_user = fixture.containers[1].clone();
    let live = fixture.containers[2].clone();
    fixture.run(&[
        "container",
        "create",
        "--pull=never",
        "--name",
        &orphan,
        "--label",
        &format!("com.docker.compose.project={prefix}"),
        "--label",
        &format!(
            "com.docker.compose.project.working_dir={}",
            fixture.dir.join("gone").display()
        ),
        "--volume",
        &format!("{owned}:/owned"),
        "--volume",
        &format!("{shared}:/shared"),
        "--volume",
        &format!("{foreign}:/foreign"),
        "--volume",
        "/anonymous",
        "alpine:latest",
    ]);
    let mounts: serde_json::Value =
        serde_json::from_str(&fixture.run(&["container", "inspect", "--format", "{{json .Mounts}}", &orphan])).unwrap();
    let anonymous = mounts
        .as_array()
        .unwrap()
        .iter()
        .find(|mount| mount["Destination"] == "/anonymous")
        .unwrap()["Name"]
        .as_str()
        .unwrap()
        .to_string();
    fixture.volumes.push(anonymous.clone());
    fixture.run(&[
        "container",
        "create",
        "--pull=never",
        "--name",
        &shared_user,
        "--volume",
        &format!("{shared}:/shared"),
        "alpine:latest",
    ]);
    fixture.run(&[
        "container",
        "create",
        "--pull=never",
        "--name",
        &live,
        "--label",
        &format!("com.docker.compose.project={live_project}"),
        "--label",
        &format!(
            "com.docker.compose.project.working_dir={}",
            fixture.dir.join("live").display()
        ),
        "--volume",
        &format!("{live_volume}:/data"),
        "alpine:latest",
    ]);
    let Docker::Ready {
        projects,
        engine: current,
    } = containers::read()
    else {
        panic!("Docker read failed")
    };
    assert_eq!(current.as_ref(), Some(&engine), "engine changed during the test");
    let selected: Vec<_> = projects
        .iter()
        .filter(|p| p.name == prefix || p.name == live_project)
        .collect();
    assert_eq!(selected.len(), 2);
    let mut plan = containers::plan(&selected, None);
    plan.engine = Some(engine);
    let done = containers::execute(&plan);
    assert!(done.errors.is_empty(), "{:?}", done.errors);
    assert_eq!((done.stopped, done.removed, done.volumes_removed), (0, 2, 1));
    assert!(!fixture.exists("container", &orphan));
    assert!(!fixture.exists("container", &live));
    assert!(!fixture.exists("volume", &owned));
    assert!(!fixture.exists("volume", &anonymous));
    assert!(fixture.exists("volume", &shared));
    assert!(fixture.exists("volume", &foreign));
    assert!(fixture.exists("volume", &live_volume));
    assert!(fixture.exists("container", &shared_user));
}
