mod i18n;
mod mcp;
mod report;
mod tools;
mod tui;

use std::path::Path;
use std::time::Duration;

use agentlens_core::catalog::Catalog;
use agentlens_core::health::{Candidate, History, Policy, candidates};
use agentlens_core::model::Model;
use agentlens_core::origin::Origins;
use agentlens_core::rules::Rules;
use agentlens_core::stdio::StdioProbe;
use agentlens_core::{Collector, Snapshot, hooks, install, locks, net};

const USAGE: &str = "usage: agentlens [--report [snapshot.json] | --dump-json | --replay snapshot.json | locks <path>]";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `hook <agent>` is not for people: agents run it once it is switched on in Settings.
    if args.first().map(String::as_str) == Some("hook") {
        // Run for every event: never disturb the agent, whatever happens.
        let _ = hook(args.get(1).map(String::as_str));
        return Ok(());
    }
    // `mcp` is not for people either: agents start it once it is registered in Settings.
    if args.first().map(String::as_str) == Some("mcp") {
        return mcp::serve();
    }
    let rules = Rules::load().map_err(anyhow::Error::msg)?;
    let installed = install::scan(&Catalog::builtin().with_rules(&rules));
    let catalog = Catalog::builtin().with_rules(&rules).with_installed(&installed);
    match args.first().map(String::as_str) {
        None => tui::run(rules, installed),
        Some("--replay") => {
            let path = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("--replay needs a snapshot file\n{USAGE}"))?;
            tui::run_replay(rules, serde_json::from_str(&std::fs::read_to_string(path)?)?)
        }
        Some("--dump-json") => {
            let snapshot = live_snapshot(&catalog)?;
            serde_json::to_writer_pretty(std::io::stdout().lock(), &snapshot)?;
            Ok(())
        }
        Some("--report") => {
            let (model, found) = match args.get(1) {
                Some(path) => classify(
                    serde_json::from_str(&std::fs::read_to_string(path)?)?,
                    &catalog,
                    &rules,
                    false,
                ),
                None => classify(live_snapshot(&catalog)?, &catalog, &rules, true),
            };
            let ssh = net::ssh_connections(&model, &catalog, &net::load_ssh_config());
            print!("{}", report::render(&model, &found, &ssh));
            Ok(())
        }
        Some("locks") => {
            let path = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("locks needs a file or folder\n{USAGE}"))?;
            let (model, found) = classify(live_snapshot(&catalog)?, &catalog, &rules, true);
            let holders = locks::find(&model, Path::new(path)).map_err(anyhow::Error::msg)?;
            print!("{}", tools::lock_holders(&model, &found, &holders, None));
            Ok(())
        }
        Some("-h" | "--help") => {
            println!(
                "{USAGE}\n\nWithout arguments, opens the interactive dashboard. `locks <path>` lists \
                 the processes that hold a file or folder, by agent session."
            );
            Ok(())
        }
        Some(other) => anyhow::bail!("unknown argument {other}\n{USAGE}"),
    }
}

/// `agentlens hook <agent>`: records the event the agent passes on standard input.
fn hook(agent: Option<&str>) -> Result<(), String> {
    use std::io::Read;
    let agent = agent.ok_or("no agent")?;
    // The user's rules can add or change hook definitions; a broken rules file must not.
    let catalog = match Rules::load() {
        Ok(rules) => Catalog::builtin().with_rules(&rules),
        Err(_) => Catalog::builtin(),
    };
    let mut input = String::new();
    std::io::stdin()
        .take(1 << 20)
        .read_to_string(&mut input)
        .map_err(|e| e.to_string())?;
    let dir = hooks::state_dir().ok_or("no state directory")?;
    hooks::record(&dir, &catalog, agent, hooks::owner(&catalog), &input)
}

/// The model and its cleanup candidates, as a one-shot report sees them. `live` means the
/// snapshot was just taken, so the processes' pipes can be checked.
fn classify(snapshot: Snapshot, catalog: &Catalog, rules: &Rules, live: bool) -> (Model, Vec<Candidate>) {
    let mut model = Model::build(snapshot, catalog);
    let mut history = History::default();
    history.update(&model);
    let mut origins = Origins::default();
    origins.update(&model, catalog);
    origins.attach(&mut model);
    if live {
        let mut probe = StdioProbe::default();
        probe.update(&model);
        std::thread::sleep(Duration::from_millis(300));
        origins.note_stdio_closed(&model, &probe.update(&model));
    }
    // A report sees one moment, so there is no waiting for an owner to stay gone.
    let policy = Policy {
        owner_gone_grace: 0,
        ..rules.policy()
    };
    let found = candidates(
        &model,
        &history,
        &origins,
        &policy,
        agentlens_core::snapshot::windowed_pids,
    );
    (model, found)
}

fn live_snapshot(catalog: &Catalog) -> anyhow::Result<Snapshot> {
    let mut collector = Collector::new(catalog.env_keys());
    // CPU usage is a delta, so take a second sample.
    collector.snapshot();
    std::thread::sleep(Duration::from_millis(500));
    Ok(collector.snapshot())
}
