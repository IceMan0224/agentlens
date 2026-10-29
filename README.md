# agentlens

**English** | [한국어](README.ko.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)
![Platforms](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey.svg)

**A terminal dashboard for the processes your coding agents leave running.**

agentlens groups every Claude Code, Codex, opencode, … process by **host → tab → session →
child processes**, shows what each session is doing, and finds the MCP servers, dev servers
and shells that ended sessions left behind, so you can end them safely. It also shows the SSH
connections and listening ports those processes own.

Task Manager shows 17 `node.exe` and 12 `node_repl.exe`. agentlens shows
*Orca › acme/web-app › Claude Code › its bash, MCP servers and REPL*.

## Why

Coding agents start a lot of processes: MCP servers, REPLs, language servers, dev servers,
shells. When a terminal tab is closed or an agent crashes, many of them keep running, and some
grow to hundreds of megabytes. They all look alike in a process list, so it is hard to tell which
ones are still in use, and ending the wrong one kills a live session.

## Features

- **Sessions by host and tab**: Orca, VS Code, Cursor, Windsurf (including remote servers),
  JetBrains, Windows Terminal, iTerm2, Ghostty, WezTerm, tmux, SSH logins, WSL and more. Tabs
  are told apart by the environment variables hosts set. Programs the catalog does not know
  are grouped as "unregistered" and can be registered with one key.
- **45 agents**: Claude Code, Codex, opencode, Gemini CLI, Cursor Agent, Copilot CLI, Aider,
  Goose, Amp, Kiro, Qwen Code, ZCode and more, including agents running as `node <script>`.
- **Session state**: working, quiet or idle from CPU use. With agent hooks switched on, the agent
  itself reports **waiting for input** and **needs you** (permission prompts).
- **Leftover detection** that does not rely on process names:
  - *lineage*: what the dashboard saw under a session that has since ended;
  - *birth record*: the session id and PID agents write into the environment of everything they
    start (`AI_AGENT`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`), which outlives the parent;
  - *closed channel* (Windows): stdio servers whose input pipe no longer has a writer.
- **Safe cleanup**: refuses hosts, system processes, agentlens's own terminal and anything matching
  your `protect` rules. Ends parents before children and re-checks each PID's start time first.
  Confirmation takes an explicit `y`. Optional automatic cleanup.
- **Network and ports**: SSH connections with `~/.ssh/config` aliases resolved, keys and known
  hosts (read-only), and listening ports with their owning session.
- **Light**: about 9 MB of memory and 1–3% of one core while idle; native kernel interfaces on
  each OS.
- **Host-independent, data-driven**: what agentlens knows about agents, hosts and processes lives
  in [`catalog.toml`](crates/core/catalog.toml). Your `rules.toml` extends it in the same format.
- English and Korean UI.

## Installation

Requires [Rust](https://rustup.rs) 1.88 or newer.

```sh
cargo install --git https://github.com/IceMan0224/agentlens agentlens
```

Or build from source:

```sh
git clone https://github.com/IceMan0224/agentlens
cd agentlens
cargo build --release
./target/release/agentlens
```

## Usage

```sh
agentlens                          # interactive dashboard
agentlens --report                 # print the current state once
agentlens --dump-json > snap.json  # raw snapshot (secrets in command lines are masked)
agentlens --report snap.json       # classify a saved snapshot
```

| View | Shows | Keys |
|---|---|---|
| 1 Sessions | Host › tab › session › process tree with memory, CPU, idle time and state | `←/→` fold, `enter` details, `k` end the selected tree, `h` register a host |
| 2 Cleanup | Cleanup candidates and the evidence for each (yellow = proposed) | `space` mark, `x` end marked |
| 3 Network | SSH connections, keys, known hosts | `d` disconnect |
| 4 Ports | Listening ports of sessions and leftovers | `k` end the owner, `a` all ports |
| 5 Rules | How leftovers are found, catalog, rules file | `e` edit, `r` reload |
| 6 Settings | Language, automatic cleanup, idle threshold, agent hooks | `←/→`/`enter` change |

Common keys: `1`–`6` or `tab` switch views, `s` sort (memory, CPU, idle), mouse wheel and
clicks, `q` quit.

## How it works

Every second agentlens lists all processes through each OS's own interface
(`NtQuerySystemInformation` on Windows, `sysctl`/libproc on macOS, `/proc` on Linux) and builds a
tree. Agents, hosts, shells and runtimes are recognized from the catalog. Each agent process
without an agent ancestor roots a session; the nearest host above it and the host's tab variables
place it in a tab.

A process counts as a leftover when its parent is gone (or its PID was reused, or on Unix it was
adopted by launchd/systemd), and also when the session that started it has ended. A leftover is
proposed for cleanup 15 seconds after its owner is seen gone. Leftovers with a visible window, or
that a live session is still connected to, are kept unless you choose otherwise in Settings.

Each candidate lists its evidence, for example
`owner-gone, from session Claude Code @ Orca / my-repo, agent-infra, age 12m, listening :3050`.
The full explanation is in the Rules view and in the [Korean documentation](README.ko.md).

### Agent hooks

Switching on **Agent hooks** in Settings adds agentlens to the agent's own hook settings
(Claude Code: `~/.claude/settings.json`). Other hooks and settings are kept, and the file is backed
up first. The agent then runs agentlens in the background on session start, prompt, stop,
notification and session end; you never run anything yourself. Codex and Gemini CLI will follow
once their hook formats and background execution are confirmed.

## Configuration

Rules live in `%APPDATA%\agentlens\rules.toml` on Windows and
`$XDG_CONFIG_HOME/agentlens/rules.toml` (default `~/.config/agentlens/rules.toml`) elsewhere.
They use the same tables as the catalog and extend it.

```toml
idle_minutes = 30
protect = ["dev-server"]          # never end processes whose name or command line contains this
language = "en"                   # en | ko
auto_cleanup_minutes = 0          # 0 = off
end_windowed = false              # also propose leftovers with a window
end_shared = false                # also propose leftovers a live session uses

[[agents]]                        # an agent the catalog does not know
id = "myagent"
exe = ["myagent"]

[[hosts]]                         # a terminal or IDE the catalog does not know
id = "myterm"
exe = ["myterm"]
tab_env = "MYTERM_TAB"
```

## Limitations

- Disconnecting SSH ends the owning process; it does not drop the TCP connection alone.
- Processes of other users and services cannot be read and are not classified.
- The closed-channel check is Windows-only for now: on macOS and Linux, Node-started MCP servers
  talk over socket pairs rather than pipes.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

Tests include a recorded real-world snapshot, macOS/Linux-shaped process trees, leftover detection
over time, and live tests that start, find and end real processes on each OS.

Contributions are welcome. For a new agent or host, an entry in
[`catalog.toml`](crates/core/catalog.toml) is usually all it takes. Please open an issue first for
larger changes.

## License

[MIT](LICENSE)
