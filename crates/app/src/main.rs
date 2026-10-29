mod report;
mod tui;

use std::time::Duration;

use agentlens_core::catalog::Catalog;
use agentlens_core::health::{History, candidates};
use agentlens_core::model::Model;
use agentlens_core::rules::Rules;
use agentlens_core::{Collector, Snapshot, install, net};

const USAGE: &str = "usage: agentlens [--report [snapshot.json] | --dump-json]";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rules = Rules::load().map_err(anyhow::Error::msg)?;
    let installed = install::scan(&Catalog::builtin().with_rules(&rules));
    let catalog = Catalog::builtin().with_rules(&rules).with_installed(&installed);
    match args.first().map(String::as_str) {
        None => tui::run(rules, installed),
        Some("--dump-json") => {
            let snapshot = live_snapshot(&catalog)?;
            serde_json::to_writer_pretty(std::io::stdout().lock(), &snapshot)?;
            Ok(())
        }
        Some("--report") => {
            let snapshot = match args.get(1) {
                Some(path) => serde_json::from_str(&std::fs::read_to_string(path)?)?,
                None => live_snapshot(&catalog)?,
            };
            let model = Model::build(snapshot, &catalog);
            let mut history = History::default();
            history.update(&model);
            let found = candidates(&model, &history, rules.idle_minutes);
            let ssh = net::ssh_connections(&model, &catalog, &net::load_ssh_config());
            print!("{}", report::render(&model, &found, &ssh));
            Ok(())
        }
        Some("-h" | "--help") => {
            println!("{USAGE}\n\nWithout arguments, opens the interactive dashboard.");
            Ok(())
        }
        Some(other) => anyhow::bail!("unknown argument {other}\n{USAGE}"),
    }
}

fn live_snapshot(catalog: &Catalog) -> anyhow::Result<Snapshot> {
    let mut collector = Collector::new(catalog.env_keys());
    // CPU usage is a delta, so take a second sample.
    collector.snapshot();
    std::thread::sleep(Duration::from_millis(500));
    Ok(collector.snapshot())
}
