<h1 align="center">agentlens</h1>

<p align="center">
  <b>A task manager for coding agents: see which agent session owns every process, and safely clean up what ended sessions left behind.</b>
</p>

<p align="center">
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="Rust 1.88+" src="https://img.shields.io/badge/rust-1.88%2B-orange.svg">
  <img alt="Windows | macOS | Linux" src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey.svg">
</p>

<p align="center">
  <b>English</b> · <a href="README.ko.md">한국어</a> · <a href="README.ja.md">日本語</a> · <a href="README.zh.md">简体中文</a>
</p>

<p align="center">
  <img src="docs/images/demo.gif" alt="agentlens: agent sessions grouped by host and tab, a closed session's leftover MCP server with its evidence, listening ports and SSH connections" width="100%">
</p>

## Why agentlens?

Coding agents start a lot of processes: MCP servers, REPLs, language servers, dev servers,
shells. Then:

- **They outlive their sessions.** Close a terminal tab or let an agent crash, and its MCP servers,
  dev servers and shells keep running. Some of them use hundreds of megabytes each, and they pile
  up over a day.
- **They all look alike.** Task Manager or `htop` shows 17 `node.exe` and 12 `node_repl.exe`. Which
  ones belong to the session you are working in?
- **Guessing is dangerous.** End the wrong `node` and you kill a live session in the middle of its
  work.

agentlens answers two questions about every process: **whose is it, and is its owner still
there?** It groups processes by *host → tab → agent session*, finds what ended sessions left
behind from evidence rather than names, and ends them only when it is safe to.

## Highlights

- **Knows whose process it is.** It builds a host → tab → session → process tree for
  [45 agents](#supported-agents-and-hosts) (Claude Code, Codex, opencode, Gemini CLI, Copilot CLI, Aider, Goose, …). Supported hosts include
  Orca, Paseo, VS Code, Cursor, JetBrains, Windows Terminal, iTerm2, Ghostty, cmux, tmux, herdr, SSH logins
  and WSL.
- **Finds leftovers by evidence, not by name.** It records what it saw under a session (lineage). It
  reads the session and PID that agents write into everything they start (birth record), and on
  Windows it checks whether anyone can still talk to a stdio server. Every candidate shows the
  evidence, for example `owner-gone, from session Claude Code @ Orca / acme/web-app, agent-infra`.
- **Safe by design.**
  - It never ends hosts, system processes, its own terminal, or anything you `protect`.
  - It ends parents before children and re-checks each PID's start time right before ending it.
  - Confirmation takes an explicit `y`.
  - Apps with a window, and processes another live session is still using, are kept unless you
    ask otherwise.
- **Works with any host.** Judgments come from OS facts (process tree, start times, environment,
  sockets, pipes), never from one host app's internals.
- **Agents can tell you what they are doing.** Turn on agent hooks in Settings and Claude Code
  reports *waiting for input* and *needs you* (permission prompts). It never makes the agent wait.
- **Agents can ask it, too.** Register its MCP server from Settings, and an agent can list what
  ended sessions left behind and end it, under the same safety rules.
- **Featherweight and native.** It uses about 9 MB of memory and 1–3% of one core while idle. It is
  written in Rust and calls each OS's own kernel interfaces.
- **Data-driven.** Everything it knows about agents and hosts lives in
  [`catalog.toml`](crates/core/catalog.toml). Your `rules.toml` extends it in the same format.

## Screenshots

**Sessions**: what each agent runs, grouped by host and tab, with memory, CPU and state. CPU%
counts the whole machine as 100, and the footer lists the keys of the current view.

![Sessions view](docs/images/sessions.png)

| Cleanup: candidates and their evidence | Ports: who listens where |
|---|---|
| ![Cleanup view](docs/images/cleanup.png) | ![Ports view](docs/images/ports.png) |
| **Network: SSH connections and keys** | **Settings: cleanup, safety, agent hooks** |
| ![Network view](docs/images/network.png) | ![Settings view](docs/images/settings.png) |

The screenshots replay a made-up snapshot (`--replay`), so no real machine is shown. The UI is
also available in Korean ([screenshot](docs/images/sessions-ko.png)).

## Supported agents and hosts

agentlens recognizes these 45 agents by their command, npm installs running under `node`
included. For the ones in **bold**, Settings can register its MCP server with the agent; agent
hooks are for Claude Code only for now.

| Agent | Command | Agent | Command | Agent | Command |
|---|---|---|---|---|---|
| <img src="docs/images/agents/claude.svg" width="16" height="16" alt=""> **Claude Code** | `claude` | <img src="docs/images/agents/claude.svg" width="16" height="16" alt=""> Claude Agent Teams | via Orca | <img src="docs/images/agents/codebuddy.png" width="16" height="16" alt=""> **CodeBuddy** | `codebuddy` |
| <img src="docs/images/agents/openclaude.png" width="16" height="16" alt=""> OpenClaude | `openclaude` | <img src="docs/images/agents/codex.png" width="16" height="16" alt=""> **Codex** | `codex` | <img src="docs/images/agents/autohand.png" width="16" height="16" alt=""> Autohand | `autohand` |
| <img src="docs/images/agents/ante.png" width="16" height="16" alt=""> Ante | `ante` | <img src="docs/images/agents/trae.png" width="16" height="16" alt=""> Trae CLI | `traecli` | <img src="docs/images/agents/opencode.png" width="16" height="16" alt=""> **OpenCode** | `opencode` |
| <img src="docs/images/agents/opencode.png" width="16" height="16" alt=""> OpenCode 2 | `opencode2` | <img src="docs/images/agents/mimo-code.png" width="16" height="16" alt=""> MiMo Code | `mimo` | <img src="docs/images/agents/pi.svg" width="16" height="16" alt=""> Pi | `pi` |
| <img src="docs/images/agents/omp.svg" width="16" height="16" alt=""> OMP | `omp` | <img src="docs/images/agents/prime-agent.png" width="16" height="16" alt=""> Prime Agent | `prime-agent` | <img src="docs/images/agents/qoder.png" width="16" height="16" alt=""> **Qoder** | `qodercli` |
| <img src="docs/images/agents/qoder-cn.png" width="16" height="16" alt=""> Qoder CN | `qoderclicn` | <img src="docs/images/agents/gemini.png" width="16" height="16" alt=""> **Gemini CLI** | `gemini` | <img src="docs/images/agents/antigravity.png" width="16" height="16" alt=""> Antigravity CLI | `agy` |
| <img src="docs/images/agents/aider.png" width="16" height="16" alt=""> Aider | `aider` | <img src="docs/images/agents/goose.png" width="16" height="16" alt=""> Goose | `goose` | <img src="docs/images/agents/amp.png" width="16" height="16" alt=""> **Amp** | `amp` |
| <img src="docs/images/agents/kilo.png" width="16" height="16" alt=""> **Kilo Code** | `kilo` | <img src="docs/images/agents/kiro.png" width="16" height="16" alt=""> **Kiro CLI** | `kiro-cli` | <img src="docs/images/agents/crush.png" width="16" height="16" alt=""> Crush | `crush` |
| <img src="docs/images/agents/aug.png" width="16" height="16" alt=""> **Auggie** | `auggie` | <img src="docs/images/agents/cline.png" width="16" height="16" alt=""> Cline | `cline` | <img src="docs/images/agents/freebuff.png" width="16" height="16" alt=""> Freebuff | `freebuff` |
| <img src="docs/images/agents/codebuff.png" width="16" height="16" alt=""> Codebuff | `codebuff` | <img src="docs/images/agents/command-code.png" width="16" height="16" alt=""> Command Code | `command-code` | <img src="docs/images/agents/continue.png" width="16" height="16" alt=""> Continue | `cn` |
| <img src="docs/images/agents/cursor.png" width="16" height="16" alt=""> **Cursor Agent** | `cursor-agent` | <img src="docs/images/agents/droid.png" width="16" height="16" alt=""> **Droid** | `droid` | <img src="docs/images/agents/kimi.png" width="16" height="16" alt=""> **Kimi** | `kimi` |
| <img src="docs/images/agents/mistral-vibe.png" width="16" height="16" alt=""> Mistral Vibe | `vibe` | <img src="docs/images/agents/qwen-code.png" width="16" height="16" alt=""> **Qwen Code** | `qwen` | <img src="docs/images/agents/rovo.png" width="16" height="16" alt=""> **Rovo Dev** | `acli` |
| <img src="docs/images/agents/hermes.png" width="16" height="16" alt=""> Hermes | `hermes` | <img src="docs/images/agents/openclaw.png" width="16" height="16" alt=""> OpenClaw | `openclaw` | <img src="docs/images/agents/copilot.png" width="16" height="16" alt=""> **GitHub Copilot CLI** | `copilot` |
| <img src="docs/images/agents/grok.png" width="16" height="16" alt=""> Grok CLI | `grok` | <img src="docs/images/agents/muse.png" width="16" height="16" alt=""> Muse | `muse` | <img src="docs/images/agents/dsh.png" width="16" height="16" alt=""> DSH | `dsh-tui` |
| <img src="docs/images/agents/zcode.png" width="16" height="16" alt=""> ZCode | `zcode` | <img src="docs/images/agents/devin.png" width="16" height="16" alt=""> Devin | `devin` | <img src="docs/images/agents/jcode.png" width="16" height="16" alt=""> JCode | `jcode` |

Icons here and below are their projects' own; most agent icons come from
[Orca](https://github.com/stablyai/orca).

The host is the program an agent runs in. VS Code, Cursor and Windsurf include their remote
servers (Remote-SSH, WSL) and code-server.

| Host | Process | Host | Process | Host | Process |
|---|---|---|---|---|---|
| <img src="docs/images/hosts/orca.png" width="16" height="16" alt=""> Orca | `orca` | <img src="docs/images/hosts/paseo.png" width="16" height="16" alt=""> Paseo | `Paseo`, `Paseo Daemon` (`node`) | <img src="docs/images/agents/claude.svg" width="16" height="16" alt=""> Claude Desktop | `Claude.app`, `AnthropicClaude` |
| <img src="docs/images/hosts/vscode.png" width="16" height="16" alt=""> VS Code | `code` | <img src="docs/images/agents/cursor.png" width="16" height="16" alt=""> Cursor | `cursor` | <img src="docs/images/hosts/windsurf.png" width="16" height="16" alt=""> Windsurf | `windsurf` |
| <img src="docs/images/agents/kiro.png" width="16" height="16" alt=""> Kiro | `kiro` | <img src="docs/images/agents/antigravity.png" width="16" height="16" alt=""> Antigravity | `antigravity` | <img src="docs/images/agents/trae.png" width="16" height="16" alt=""> Trae | `trae` |
| <img src="docs/images/hosts/zed.png" width="16" height="16" alt=""> Zed | `zed` | <img src="docs/images/hosts/jetbrains.png" width="16" height="16" alt=""> JetBrains IDEs | `idea64`, `pycharm64`, … | <img src="docs/images/hosts/windows-terminal.png" width="16" height="16" alt=""> Windows Terminal | `WindowsTerminal` |
| <img src="docs/images/hosts/macos-terminal.png" width="16" height="16" alt=""> Terminal (macOS) | `Terminal.app` | <img src="docs/images/hosts/iterm2.png" width="16" height="16" alt=""> iTerm2 | `iTerm.app` | <img src="docs/images/hosts/ghostty.png" width="16" height="16" alt=""> Ghostty | `ghostty` |
| <img src="docs/images/hosts/kitty.png" width="16" height="16" alt=""> kitty | `kitty` | <img src="docs/images/hosts/wezterm.png" width="16" height="16" alt=""> WezTerm | `wezterm-gui` | <img src="docs/images/hosts/alacritty.png" width="16" height="16" alt=""> Alacritty | `alacritty` |
| <img src="docs/images/hosts/warp.png" width="16" height="16" alt=""> Warp | `warp` | <img src="docs/images/hosts/wave.png" width="16" height="16" alt=""> Wave | `wave` | <img src="docs/images/hosts/tabby.png" width="16" height="16" alt=""> Tabby | `tabby` |
| <img src="docs/images/hosts/cmux.png" width="16" height="16" alt=""> cmux | `cmux` | <img src="docs/images/hosts/gnome-terminal.png" width="16" height="16" alt=""> GNOME Terminal | `gnome-terminal-server` | <img src="docs/images/hosts/konsole.png" width="16" height="16" alt=""> Konsole | `konsole` |
| <img src="docs/images/hosts/xfce-terminal.svg" width="16" height="16" alt=""> Xfce Terminal | `xfce4-terminal` | <img src="docs/images/hosts/tilix.svg" width="16" height="16" alt=""> Tilix | `tilix` | <img src="docs/images/hosts/terminator.png" width="16" height="16" alt=""> Terminator | `terminator` |
| <img src="docs/images/hosts/xterm.png" width="16" height="16" alt=""> xterm | `xterm` | <img src="docs/images/hosts/ssh.png" width="16" height="16" alt=""> SSH login | `sshd`, `sshd-session` | <img src="docs/images/hosts/wsl.png" width="16" height="16" alt=""> WSL | `SessionLeader` |
| <img src="docs/images/hosts/tmux.svg" width="16" height="16" alt=""> tmux | `tmux` | <img src="docs/images/hosts/herdr.svg" width="16" height="16" alt=""> herdr | `herdr` | <img src="docs/images/hosts/screen.png" width="16" height="16" alt=""> screen | `screen` |
| <img src="docs/images/hosts/ego.png" width="16" height="16" alt=""> ego browser | `ego` |   |   |   |   |

Hosts that set a variable per tab are split by it: Orca (`ORCA_TAB_ID`, with one row per
worktree), Paseo (`PASEO_TERMINAL_ID`; an agent Paseo starts itself is a tab of its own), Windows
Terminal (`WT_SESSION`) and cmux (`CMUX_WORKSPACE_ID`, one tab per workspace). Each SSH login and each tmux, herdr or screen pane is a tab. A program not listed here
that starts an agent shows up as "(unregistered)", and `h` in Sessions adds it to your rules.

## Installation

Download the archive for your platform from the
[latest release](https://github.com/IceMan0224/agentlens/releases/latest) (Windows x64, Linux x64,
macOS Apple silicon only), unpack it and run `agentlens`. On macOS, the first run of a
downloaded binary may need `xattr -d com.apple.quarantine agentlens`.

To build it yourself instead, you need [Rust](https://rustup.rs) 1.88 or newer:

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
agentlens --replay snap.json       # browse a saved snapshot in the dashboard, read-only
agentlens locks <path>             # who holds a file or folder, by agent session
```

When an update or delete fails with `EBUSY`, "resource busy or locked" or "being used by another
process", `agentlens locks` lists the processes that run an executable from the path, work in it,
or (Windows) have anything in it open. They are grouped by agent session, and leftovers are marked
with their evidence:

```text
3 process(es) hold C:\Users\me\AppData\Roaming\npm\node_modules\@openai\codex:
- Codex @ Orca / acme/web-app (pid 28624)
  - pid 12040 codex.exe: runs node_modules\@openai\codex-win32-x64\...\bin\codex.exe
  - pid 39448 codex-code-mode-host.exe: runs node_modules\@openai\...\bin\codex-code-mode-host.exe
- leftover node.exe watcher.js (pid 25480): proposed, owner-gone, from session Codex @ Orca / acme/api
  - pid 25480 node.exe: has node_modules\@openai\codex\package.json open
```

| View | Shows | Keys |
|---|---|---|
| 1 Sessions | Host › tab › session › process tree with memory, CPU, idle time and state | `←/→` fold, `enter` details, `k` end the selected tree, `h` register a host |
| 2 Cleanup | Cleanup candidates and the evidence for each (yellow = proposed) | `space` mark, `x` end marked |
| 3 Network | SSH connections, keys, known hosts | `d` disconnect |
| 4 Ports | Listening ports of sessions and leftovers | `k` end the owner, `a` all ports |
| 5 Containers | Docker containers by compose project, with the checkout each was started from and the live session working there; projects whose checkout is gone are proposed (yellow) | `space` mark a project, `x` stop running and remove stopped containers (volumes kept) |
| 6 Rules | How leftovers are found, the agents and hosts on this PC in yellow (commands on `PATH`, app launchers: Start Menu, `.desktop`, `.app`, and whatever is running) and the rest of the catalog, rules file | `e` edit, `r` reload |
| 7 Settings | Language, automatic cleanup, idle threshold, leftovers to keep, agent hooks, agent MCP | `←/→`/`enter` change |

Common keys: `1`–`7` or `tab` switch views, `s` sort (memory, CPU, idle), mouse wheel and
clicks, `q` quit.

## How it works

Every second agentlens lists all processes through each OS's own interface
(`NtQuerySystemInformation` on Windows, `sysctl`/libproc on macOS, `/proc` on Linux) and builds a
tree. Agents, hosts, shells and runtimes are recognized from the catalog. Each agent process
without an agent ancestor roots a session. The nearest host above it, and the host's tab
variables, place it in a tab. Orca tabs of one worktree share a row.

A process counts as a leftover in either of these cases:

- **Its parent is gone.** This includes a parent PID that was reused, and on Unix a process adopted
  by launchd or systemd.
- **The session that started it has ended.** This is found three ways:
  - *lineage*: the dashboard saw the process under that session;
  - *birth record*: the process carries `AI_AGENT`, `CLAUDE_CODE_SESSION_ID` or `CLAUDE_PID`, which
    agents set and children inherit;
  - *closed channel* (Windows only): nobody can write to the process's input pipe any more.

A leftover is proposed for cleanup 15 seconds after its owner is seen gone. The full rules are in
the Rules view and the [Korean documentation](README.ko.md).

A process that left a running session's tree, such as `nohup uv run python run.py`, is not a
leftover. It stays under that session as *detached*, and ending the session ends it too.

### Agent hooks

Switching on **Agent hooks** in Settings adds agentlens to the agent's own hook settings
(Claude Code: `~/.claude/settings.json`). Other hooks and settings are kept, and the file is backed
up first. The agent then runs agentlens in the background on session start, prompt, stop,
notification and session end; you never run anything yourself. Codex and Gemini CLI will follow
once their hook formats and background execution are confirmed.

### Agent MCP

Switch on **Agent MCP** for an agent in Settings, and you can ask the agent "are there leftovers?"
or "clean them up". Settings lists the agents used on this machine that take MCP servers
(Claude Code, Codex, Gemini CLI, Qwen Code, opencode, Cursor, Copilot CLI, Kiro, Droid, Amp, Kimi, CodeBuddy, Qoder, Auggie, Kilo Code and Rovo Dev). agentlens adds `agentlens mcp` to
the agent's user-level MCP settings, the same binary with nothing else to install. Other settings
are kept and the file is backed up first; Claude Code, which rewrites its file all the time, is
changed through its own `claude mcp add`.

| Tool | What it does |
|---|---|
| `list_leftovers` | Cleanup candidates, the evidence for each, and which are proposed |
| `list_session_processes` | The asking session's process tree, with memory, commands and listening ports |
| `end_leftovers` | Ends the proposed candidates, or those named by pid. Nothing outside the candidate list, and never the asking session |
| `find_lock_holders` | Who holds a file or folder, as `agentlens locks` shows it, so an agent whose update or delete failed can tell which session to close |

The server only passes questions on. The dashboard, which has watched the sessions, decides and
ends, with the same refusals and re-checks as when you end something yourself, so keep it
running; without it the tools say so. They talk through small files in agentlens's state folder
(`%LOCALAPPDATA%\agentlens\requests`, `~/.local/state/agentlens/requests`), so no port is opened.
The agent still asks you before ending anything unless you allow the tool.

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
cmd = ["@me/term-server"]          # a host run by node: a path in its command line
```

## Limitations

- Disconnecting SSH ends the owning process; it does not drop the TCP connection alone.
- Processes of other users and services cannot be read and are not classified.
- Open files are found on Windows only, where an open file blocks replacing or deleting it.
  Elsewhere `locks` reports running executables and working directories. On Windows it reads every
  file handle of the processes it may open (about half a second); other users' processes are not
  seen.
- The closed-channel check is Windows-only for now: on macOS and Linux, Node-started MCP servers
  talk over socket pairs rather than pipes.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

Tests include a realistic made-up snapshot, macOS/Linux-shaped process trees, leftover detection
over time, and live tests that start, find and end real processes on each OS.

The screenshots and the animation are recorded with [VHS](https://github.com/charmbracelet/vhs)
from the tapes in [`docs/demo`](docs/demo), which replay the made-up snapshot. Run
`cargo build --release` first, then `vhs docs/demo/demo.tape`.

Contributions are welcome. For a new agent or host, an entry in
[`catalog.toml`](crates/core/catalog.toml) is usually all it takes. Please open an issue first for
larger changes.

## License

[MIT](LICENSE)
