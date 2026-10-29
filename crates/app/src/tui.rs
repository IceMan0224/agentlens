//! Interactive dashboard: sessions grouped by host and tab, cleanup candidates, SSH, rules.

use std::collections::HashSet;
use std::sync::mpsc;
use std::time::Duration;

use agentlens_core::actions::{self, KillPlan, Outcome};
use agentlens_core::catalog::Catalog;
use agentlens_core::health::{Candidate, CandidateKind, History, candidates, fmt_duration, short_cmd};
use agentlens_core::model::{GroupKind, Model, Role, short_path};
use agentlens_core::net::{self, SshConn, SshHost};
use agentlens_core::rules::{Rules, rules_path};
use agentlens_core::{Collector, Snapshot};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::report::{mb, role_label};

const VIEWS: [&str; 4] = ["Sessions", "Cleanup", "Network", "Rules"];
const REFRESH: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Sessions,
    Cleanup,
    Network,
    Rules,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Memory,
    Cpu,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    Group(String),
    Tab(String),
    Session(u32),
    Proc(u32),
    OrphanHeader,
    Orphan(u32),
}

impl Item {
    fn key(&self) -> String {
        match self {
            Item::Group(k) => format!("g:{k}"),
            Item::Tab(k) => format!("t:{k}"),
            Item::Session(p) | Item::Orphan(p) => format!("s:{p}"),
            Item::Proc(p) => format!("p:{p}"),
            Item::OrphanHeader => "g:orphans".into(),
        }
    }

    /// Groups and tabs start open; sessions and orphans start closed.
    fn open_by_default(&self) -> bool {
        matches!(self, Item::Group(_) | Item::Tab(_) | Item::OrphanHeader)
    }
}

struct Confirm {
    title: String,
    plan: KillPlan,
    refusals: Vec<String>,
}

pub struct App {
    catalog: Catalog,
    rules: Rules,
    installed: Vec<(String, String)>,
    self_pid: u32,
    model: Option<Model>,
    history: History,
    candidates: Vec<Candidate>,
    ssh: Vec<SshConn>,
    ssh_config: Vec<SshHost>,
    view: View,
    sort: Sort,
    selected: [usize; 4],
    offsets: [usize; 4],
    flipped: HashSet<String>,
    marked: HashSet<u32>,
    seen_candidates: HashSet<u32>,
    confirm: Option<Confirm>,
    detail: bool,
    status: String,
    items: Vec<Item>,
    table_area: Rect,
    tabs_area: Rect,
    quit: bool,
    refresh_now: bool,
    /// Set when the catalog changed, for the collector thread.
    new_env_keys: Option<Vec<String>>,
}

impl App {
    pub fn new(rules: Rules, installed: Vec<(String, String)>) -> Self {
        let catalog = Catalog::builtin().with_rules(&rules).with_installed(&installed);
        Self {
            catalog,
            rules,
            installed,
            self_pid: std::process::id(),
            model: None,
            history: History::default(),
            candidates: Vec::new(),
            ssh: Vec::new(),
            ssh_config: net::load_ssh_config(),
            view: View::Sessions,
            sort: Sort::Memory,
            selected: [0; 4],
            offsets: [0; 4],
            flipped: HashSet::new(),
            marked: HashSet::new(),
            seen_candidates: HashSet::new(),
            confirm: None,
            detail: false,
            status: "q quit · ←→ fold · k end tree · enter details · s sort · 1-4 views".into(),
            items: Vec::new(),
            table_area: Rect::default(),
            tabs_area: Rect::default(),
            quit: false,
            refresh_now: false,
            new_env_keys: None,
        }
    }

    pub fn apply(&mut self, snapshot: Snapshot) {
        let model = Model::build(snapshot, &self.catalog);
        self.history.update(&model);
        self.candidates = candidates(&model, &self.history, self.rules.idle_minutes);
        self.ssh = net::ssh_connections(&model, &self.catalog, &self.ssh_config);

        // Pre-mark new eligible candidates and announce them; keep the user's choices otherwise.
        let mut fresh = Vec::new();
        for c in &self.candidates {
            if self.seen_candidates.insert(c.root_pid) && c.eligible {
                self.marked.insert(c.root_pid);
                fresh.push(format!("{} ({})", c.label, mb(c.private_bytes)));
            }
        }
        let alive: HashSet<u32> = self.candidates.iter().map(|c| c.root_pid).collect();
        self.marked.retain(|p| alive.contains(p));
        self.seen_candidates.retain(|p| alive.contains(p));
        if !fresh.is_empty() && self.model.is_some() {
            self.status = format!("new cleanup candidate: {}", fresh.join(", "));
        }
        self.model = Some(model);
    }

    fn is_open(&self, item: &Item) -> bool {
        item.open_by_default() != self.flipped.contains(&item.key())
    }

    fn view_index(&self) -> usize {
        self.view as usize
    }

    // ---- rows -------------------------------------------------------------------------------

    fn idle(&self, m: &Model, pids: &[u32]) -> u64 {
        self.history.idle_secs(m, pids)
    }

    fn sort_key(&self, m: &Model, pids: &[u32]) -> i64 {
        match self.sort {
            Sort::Memory => -(m.private_bytes(pids) as i64),
            Sort::Cpu => -((m.cpu(pids) * 100.0) as i64),
            Sort::Idle => -(self.idle(m, pids) as i64),
        }
    }

    fn session_rows(&mut self) -> Vec<Row<'static>> {
        let Some(m) = self.model.as_ref() else {
            return Vec::new();
        };
        let mut items = Vec::new();
        let mut rows = Vec::new();

        let stats = |pids: &[u32], idle: Option<u64>| -> Vec<Cell<'static>> {
            vec![
                Cell::from(pids.len().to_string()),
                Cell::from(mb(m.private_bytes(pids))),
                Cell::from(format!("{:.1}", m.cpu(pids))),
                Cell::from(idle.map(fmt_duration).unwrap_or_default()),
            ]
        };

        if !m.orphans.is_empty() {
            let header = Item::OrphanHeader;
            let all: Vec<u32> = m.orphans.iter().flat_map(|o| m.subtree(o.pid)).collect();
            let mut cells = vec![
                Cell::from(fold(self.is_open(&header), "⚠ No parent (orphans)")),
                Cell::from(""),
            ];
            cells.extend(stats(&all, None));
            cells.push(Cell::from("parent exited; see Cleanup"));
            rows.push(Row::new(cells).style(Style::new().fg(Color::Yellow).bold()));
            let open = self.is_open(&header);
            items.push(header);
            if open {
                for o in &m.orphans {
                    let item = Item::Orphan(o.pid);
                    let tree = m.subtree(o.pid);
                    let p = &m.procs[&o.pid];
                    let note = match (o.agent_related, &o.origin) {
                        (true, _) => "agent leftover".to_string(),
                        (false, Some(tab)) => format!("started by agent in {tab}"),
                        (false, None) => "not agent".to_string(),
                    };
                    let mut cells = vec![
                        Cell::from(format!("  {}", fold(self.is_open(&item), &p.name))),
                        Cell::from(o.pid.to_string()),
                    ];
                    cells.extend(stats(&tree, Some(self.idle(m, &tree))));
                    cells.push(Cell::from(format!("{note} · {}", short_cmd(p))));
                    rows.push(Row::new(cells));
                    let open = self.is_open(&item);
                    items.push(item);
                    if open {
                        push_procs(m, o.pid, 2, &mut rows, &mut items);
                    }
                }
            }
        }

        let mut groups: Vec<_> = m.groups.iter().collect();
        groups.sort_by_key(|g| {
            let pids: Vec<u32> = g
                .tabs
                .iter()
                .flat_map(|t| m.tab_pids(t))
                .chain(g.host_pids.iter().copied())
                .collect();
            self.sort_key(m, &pids)
        });
        for g in groups {
            let gkey = match &g.kind {
                GroupKind::Known(id) => id.clone(),
                GroupKind::Unregistered(s) => format!("?{s}"),
                GroupKind::Standalone => "standalone".into(),
            };
            let item = Item::Group(gkey.clone());
            let all: Vec<u32> = g.tabs.iter().flat_map(|t| m.tab_pids(t)).collect();
            let sessions: usize = g.tabs.iter().map(|t| t.sessions.len()).sum();
            let mut cells = vec![Cell::from(fold(self.is_open(&item), &g.name)), Cell::from("")];
            cells.extend(stats(&all, None));
            let mut info = format!(
                "{} tabs · {sessions} sessions · app {} in {} procs",
                g.tabs.len(),
                mb(m.private_bytes(&g.host_pids)),
                g.host_pids.len()
            );
            if matches!(g.kind, GroupKind::Unregistered(_)) {
                info.push_str(" · h: register as host");
            }
            cells.push(Cell::from(info));
            rows.push(Row::new(cells).style(Style::new().fg(Color::Cyan).bold()));
            let open = self.is_open(&item);
            items.push(item);
            if !open {
                continue;
            }
            let mut tabs: Vec<_> = g.tabs.iter().collect();
            tabs.sort_by_key(|t| self.sort_key(m, &m.tab_pids(t)));
            for t in tabs {
                let item = Item::Tab(t.key.clone());
                let pids = m.tab_pids(t);
                let agents: Vec<&str> = t.sessions.iter().map(|s| s.agent_id.as_str()).collect();
                let mut cells = vec![
                    Cell::from(format!("  {}", fold(self.is_open(&item), &t.label))),
                    Cell::from(""),
                ];
                cells.extend(stats(&pids, Some(self.idle(m, &pids))));
                cells.push(Cell::from(format!("{} · {}", agents.join(", "), short_key(&t.key))));
                rows.push(Row::new(cells).style(Style::new().bold()));
                let open = self.is_open(&item);
                items.push(item);
                if !open {
                    continue;
                }
                let mut sessions: Vec<_> = t.sessions.iter().collect();
                sessions.sort_by_key(|s| self.sort_key(m, &m.subtree(s.root_pid)));
                for s in sessions {
                    let item = Item::Session(s.root_pid);
                    let tree = m.subtree(s.root_pid);
                    let provider = s.provider.as_deref().map(|p| format!(" ({p})")).unwrap_or_default();
                    let name = format!(
                        "    {}",
                        fold(self.is_open(&item), &format!("{}{provider}", s.agent_name))
                    );
                    let mut cells = vec![Cell::from(name), Cell::from(s.root_pid.to_string())];
                    cells.extend(stats(&tree, Some(self.idle(m, &tree))));
                    let sid = s
                        .session_id
                        .as_deref()
                        .map(|id| format!("session {}", id.chars().take(13).collect::<String>()))
                        .unwrap_or_default();
                    cells.push(Cell::from(sid));
                    rows.push(Row::new(cells).style(Style::new().fg(Color::Green)));
                    let open = self.is_open(&item);
                    items.push(item);
                    if open {
                        push_procs(m, s.root_pid, 3, &mut rows, &mut items);
                    }
                }
            }
        }
        self.items = items;
        rows
    }

    fn cleanup_rows(&self) -> Vec<Row<'static>> {
        self.candidates
            .iter()
            .map(|c| {
                let mark = if self.marked.contains(&c.root_pid) {
                    "[x]"
                } else {
                    "[ ]"
                };
                let kind = match c.kind {
                    CandidateKind::Orphan => "orphan",
                    CandidateKind::IdleSession => "idle",
                    CandidateKind::DuplicateMcp => "dup mcp",
                };
                let style = if c.eligible {
                    Style::new().fg(Color::Yellow)
                } else {
                    Style::new()
                };
                Row::new(vec![
                    Cell::from(mark),
                    Cell::from(kind),
                    Cell::from(c.label.clone()),
                    Cell::from(c.root_pid.to_string()),
                    Cell::from(mb(c.private_bytes)),
                    Cell::from(c.reasons.join(", ")),
                ])
                .style(style)
            })
            .collect()
    }

    fn network_rows(&self) -> Vec<Row<'static>> {
        let Some(m) = self.model.as_ref() else {
            return Vec::new();
        };
        self.ssh
            .iter()
            .map(|c| {
                let who = c.user.as_deref().map(|u| format!("{u}@")).unwrap_or_default();
                let alias = c.alias.as_deref().map(|a| format!(" ({a})")).unwrap_or_default();
                let owner = m
                    .locate(c.pid)
                    .map(|(g, t, s)| {
                        let tab = &m.groups[g].tabs[t];
                        format!("{} @ {} / {}", tab.sessions[s].agent_name, m.groups[g].name, tab.label)
                    })
                    .unwrap_or_else(|| "-".into());
                Row::new(vec![
                    Cell::from(if c.inbound { "in" } else { "out" }),
                    Cell::from(format!("{who}{}:{}{alias}", c.host, c.port)),
                    Cell::from(c.remote.clone()),
                    Cell::from(format!("{} {}", c.process, c.pid)),
                    Cell::from(fmt_duration(m.taken_at.saturating_sub(c.since))),
                    Cell::from(owner),
                ])
            })
            .collect()
    }

    fn row_count(&self) -> usize {
        match self.view {
            View::Sessions => self.items.len(),
            View::Cleanup => self.candidates.len(),
            View::Network => self.ssh.len(),
            View::Rules => 0,
        }
    }

    // ---- drawing ----------------------------------------------------------------------------

    pub fn draw(&mut self, frame: &mut Frame) {
        let [top, body, bottom] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
        let [tabs_area, summary_area] = Layout::horizontal([Constraint::Length(46), Constraint::Min(10)]).areas(top);
        self.tabs_area = tabs_area;
        let titles = VIEWS.iter().enumerate().map(|(i, v)| format!("{} {v}", i + 1));
        frame.render_widget(
            Tabs::new(titles)
                .select(self.view_index())
                .highlight_style(Style::new().reversed()),
            tabs_area,
        );
        frame.render_widget(Paragraph::new(self.summary()).right_aligned(), summary_area);

        let (table_area, detail_area) = if self.detail && self.view == View::Sessions {
            let [a, b] = Layout::vertical([Constraint::Min(5), Constraint::Length(12)]).areas(body);
            (a, Some(b))
        } else {
            (body, None)
        };
        match self.view {
            View::Rules => frame.render_widget(self.rules_text(), table_area),
            _ => self.draw_table(frame, table_area),
        }
        if let Some(area) = detail_area {
            frame.render_widget(self.detail_text(), area);
        }
        frame.render_widget(Paragraph::new(self.status.clone()).style(Style::new().dim()), bottom);
        if self.confirm.is_some() {
            self.draw_confirm(frame);
        }
    }

    fn summary(&self) -> Line<'static> {
        let Some(m) = self.model.as_ref() else {
            return Line::from("collecting…");
        };
        let pids: Vec<u32> = m
            .groups
            .iter()
            .flat_map(|g| {
                g.tabs
                    .iter()
                    .flat_map(|t| m.tab_pids(t))
                    .chain(g.host_pids.iter().copied())
            })
            .collect();
        let sessions: usize = m.groups.iter().flat_map(|g| &g.tabs).map(|t| t.sessions.len()).sum();
        let eligible: u64 = self
            .candidates
            .iter()
            .filter(|c| c.eligible)
            .map(|c| c.private_bytes)
            .sum();
        let sort = match self.sort {
            Sort::Memory => "mem",
            Sort::Cpu => "cpu",
            Sort::Idle => "idle",
        };
        Line::from(vec![
            Span::raw(format!(
                "{sessions} sessions · {} in {} procs · ",
                mb(m.private_bytes(&pids)),
                pids.len()
            )),
            Span::styled(format!("cleanup {}", mb(eligible)), Style::new().fg(Color::Yellow)),
            Span::raw(format!(" · ssh {} · sort {sort} ", self.ssh.len())),
        ])
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let (header, widths, rows, title): (Vec<&str>, Vec<Constraint>, Vec<Row>, &str) = match self.view {
            View::Sessions => (
                vec!["Name", "PID", "Procs", "Private", "CPU%", "Idle", "Info"],
                vec![
                    Constraint::Min(30),
                    Constraint::Length(7),
                    Constraint::Length(6),
                    Constraint::Length(9),
                    Constraint::Length(6),
                    Constraint::Length(7),
                    Constraint::Fill(2),
                ],
                self.session_rows(),
                "Sessions by host / tab",
            ),
            View::Cleanup => (
                vec!["", "Kind", "What", "PID", "Private", "Evidence"],
                vec![
                    Constraint::Length(3),
                    Constraint::Length(8),
                    Constraint::Fill(2),
                    Constraint::Length(7),
                    Constraint::Length(9),
                    Constraint::Fill(3),
                ],
                self.cleanup_rows(),
                "Cleanup · space mark · x end marked (yellow = proposed)",
            ),
            View::Network => (
                vec!["Dir", "Destination", "Remote", "Process", "Up", "Owner"],
                vec![
                    Constraint::Length(4),
                    Constraint::Fill(2),
                    Constraint::Length(22),
                    Constraint::Length(16),
                    Constraint::Length(7),
                    Constraint::Fill(2),
                ],
                self.network_rows(),
                "SSH connections · d disconnect",
            ),
            View::Rules => unreachable!(),
        };
        let count = rows.len();
        let v = self.view_index();
        self.selected[v] = self.selected[v].min(count.saturating_sub(1));
        let empty = rows.is_empty();
        let table = Table::new(rows, widths)
            .header(Row::new(header).style(Style::new().add_modifier(Modifier::UNDERLINED)))
            .row_highlight_style(Style::new().reversed())
            .block(Block::bordered().title(title));
        let mut state = TableState::default()
            .with_offset(self.offsets[v])
            .with_selected((!empty).then_some(self.selected[v]));
        frame.render_stateful_widget(table, area, &mut state);
        self.offsets[v] = state.offset();
        self.table_area = area;
        if empty {
            let msg = match self.view {
                View::Cleanup => "nothing to clean up",
                View::Network => "no SSH connections",
                _ => "no agent sessions found",
            };
            let inner = Rect {
                y: area.y + 2,
                height: 1,
                ..area.inner(ratatui::layout::Margin::new(2, 0))
            };
            frame.render_widget(Paragraph::new(msg).dim(), inner);
        }
    }

    fn selected_pid(&self) -> Option<u32> {
        match self.items.get(self.selected[0])? {
            Item::Session(p) | Item::Proc(p) | Item::Orphan(p) => Some(*p),
            _ => None,
        }
    }

    fn detail_text(&self) -> Paragraph<'static> {
        let block = Block::bordered().title("Details");
        let (Some(m), Some(pid)) = (self.model.as_ref(), self.selected_pid()) else {
            return Paragraph::new("select a session or process").block(block);
        };
        let Some(p) = m.procs.get(&pid) else {
            return Paragraph::new("").block(block);
        };
        let tree = m.subtree(pid);
        let mut lines = vec![
            Line::from(format!(
                "{} {}  [{}]  started {} ago",
                p.name,
                p.pid,
                role_label(m, pid),
                fmt_duration(m.taken_at.saturating_sub(p.start_time))
            )),
            Line::from(format!("cmd: {}", p.cmd.join(" "))),
            Line::from(format!("cwd: {}", p.cwd.as_deref().unwrap_or("-"))),
            Line::from(format!(
                "tree: {} procs · private {} · working set {}",
                tree.len(),
                mb(m.private_bytes(&tree)),
                mb(tree.iter().map(|q| m.procs[q].working_set).sum())
            )),
        ];
        // Only the catalog's variables are collected, so all of them are worth showing.
        let env: Vec<String> = p.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        if !env.is_empty() {
            lines.push(Line::from(format!("env: {}", env.join("  "))));
        }
        let conns: Vec<String> = net::connections_of(m, &tree)
            .map(|c| {
                format!(
                    "{} → {}",
                    net::endpoint(&c.local_addr, c.local_port),
                    net::endpoint(&c.remote_addr, c.remote_port)
                )
            })
            .collect();
        lines.push(Line::from(format!(
            "connections ({}): {}",
            conns.len(),
            conns.join(", ")
        )));
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(block)
    }

    fn rules_text(&self) -> Paragraph<'static> {
        let path = rules_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".into());
        let mut installed: Vec<&str> = self.installed.iter().map(|(id, _)| id.as_str()).collect();
        installed.dedup();
        let mut lines = vec![
            Line::from(format!("rules file: {path}  (e: edit · r: reload)")).bold(),
            Line::from(format!(
                "idle after {} min · protect: {}",
                self.rules.idle_minutes,
                if self.catalog.protect.is_empty() {
                    "-".into()
                } else {
                    self.catalog.protect.join(", ")
                }
            )),
            Line::from(""),
            Line::from(format!("agents in catalog: {}", self.catalog.agents.len())).bold(),
            Line::from(format!("installed here: {}", installed.join(", "))),
            Line::from(
                self.catalog
                    .agents
                    .iter()
                    .map(|a| a.id.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .dim(),
            Line::from(""),
            Line::from(format!("hosts: {}", self.catalog.hosts.len())).bold(),
            Line::from(
                self.catalog
                    .hosts
                    .iter()
                    .map(|h| h.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" · "),
            )
            .dim(),
            Line::from(""),
            Line::from("rules.toml example:").bold(),
            Line::from("  idle_minutes = 30"),
            Line::from("  protect = [\"dev-server\"]"),
            Line::from("  [[agents]]  id = \"myagent\"  exe = [\"myagent\"]  cmd = [\"@me/agent\"]"),
            Line::from("  [[hosts]]   id = \"myterm\"   exe = [\"myterm\"]"),
        ];
        lines.push(Line::from(""));
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title("Rules"))
    }

    fn draw_confirm(&self, frame: &mut Frame) {
        let Some(c) = &self.confirm else { return };
        let area = popup(frame.area(), 72, 18);
        frame.render_widget(Clear, area);
        let mut lines = vec![Line::from(c.title.clone()).bold(), Line::from("")];
        for t in c.plan.targets.iter().take(10) {
            lines.push(Line::from(format!("  {} {}", t.name, t.pid)));
        }
        if c.plan.targets.len() > 10 {
            lines.push(Line::from(format!("  … and {} more", c.plan.targets.len() - 10)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "frees ~{} private · closes {} connections",
            mb(c.plan.private_bytes),
            c.plan.connections
        )));
        if c.refusals.is_empty() {
            lines.push(Line::from("y / enter: end   n / esc: cancel").fg(Color::Yellow));
        } else {
            for r in &c.refusals {
                lines.push(Line::from(format!("refused: {r}")).fg(Color::Red));
            }
            lines.push(Line::from("esc: close").fg(Color::Yellow));
        }
        frame.render_widget(Paragraph::new(lines).block(Block::bordered().title("Confirm")), area);
    }

    // ---- input ------------------------------------------------------------------------------

    /// Returns whether the screen may need redrawing. Mouse motion arrives as a stream of
    /// events while the pointer moves, and none of it changes anything.
    pub fn on_event(&mut self, ev: Event) -> bool {
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => self.move_by(3),
                MouseEventKind::ScrollUp => self.move_by(-3),
                MouseEventKind::Down(MouseButton::Left) if self.confirm.is_none() => {
                    self.on_click(mouse.column, mouse.row)
                }
                _ => return false,
            },
            Event::Resize(..) => {}
            _ => return false,
        }
        true
    }

    fn on_click(&mut self, x: u16, y: u16) {
        if y == self.tabs_area.y && x < self.tabs_area.x + self.tabs_area.width {
            // Each title is "N Name" plus a 3-column divider.
            let mut start = self.tabs_area.x;
            for (i, v) in VIEWS.iter().enumerate() {
                let end = start + v.len() as u16 + 4;
                if x < end {
                    self.set_view(i);
                    return;
                }
                start = end + 1;
            }
            return;
        }
        let body_top = self.table_area.y + 2; // border + header
        if y >= body_top && y < self.table_area.y + self.table_area.height.saturating_sub(1) {
            let v = self.view_index();
            let idx = self.offsets[v] + (y - body_top) as usize;
            if idx < self.row_count() {
                if self.selected[v] == idx && self.view == View::Sessions {
                    self.toggle_fold();
                }
                self.selected[v] = idx;
            }
        }
    }

    fn set_view(&mut self, i: usize) {
        self.view = [View::Sessions, View::Cleanup, View::Network, View::Rules][i];
    }

    fn move_by(&mut self, delta: isize) {
        let v = self.view_index();
        let max = self.row_count().saturating_sub(1) as isize;
        self.selected[v] = (self.selected[v] as isize + delta).clamp(0, max.max(0)) as usize;
    }

    fn toggle_fold(&mut self) {
        if let Some(item) = self.items.get(self.selected[0]) {
            let key = item.key();
            if !self.flipped.remove(&key) {
                self.flipped.insert(key);
            }
        }
    }

    fn set_fold(&mut self, open: bool) {
        if let Some(item) = self.items.get(self.selected[0]).cloned()
            && self.is_open(&item) != open
        {
            self.toggle_fold();
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if let Some(c) = &self.confirm {
            match k.code {
                KeyCode::Char('y') | KeyCode::Enter if c.refusals.is_empty() => self.run_confirmed(),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.confirm = None,
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char(d @ '1'..='4') => self.set_view(d as usize - '1' as usize),
            KeyCode::Tab => self.set_view((self.view_index() + 1) % 4),
            KeyCode::BackTab => self.set_view((self.view_index() + 3) % 4),
            KeyCode::Down => self.move_by(1),
            KeyCode::Up => self.move_by(-1),
            KeyCode::PageDown => self.move_by(15),
            KeyCode::PageUp => self.move_by(-15),
            KeyCode::Home => self.selected[self.view_index()] = 0,
            KeyCode::End => self.selected[self.view_index()] = self.row_count().saturating_sub(1),
            KeyCode::Char('s') => {
                self.sort = match self.sort {
                    Sort::Memory => Sort::Cpu,
                    Sort::Cpu => Sort::Idle,
                    Sort::Idle => Sort::Memory,
                }
            }
            KeyCode::Left if self.view == View::Sessions => self.set_fold(false),
            KeyCode::Right if self.view == View::Sessions => self.set_fold(true),
            KeyCode::Char(' ') if self.view == View::Sessions => self.toggle_fold(),
            KeyCode::Enter if self.view == View::Sessions => self.detail = !self.detail,
            KeyCode::Char(' ') if self.view == View::Cleanup => {
                if let Some(c) = self.candidates.get(self.selected[1])
                    && !self.marked.remove(&c.root_pid)
                {
                    self.marked.insert(c.root_pid);
                }
            }
            KeyCode::Char('x') | KeyCode::Enter if self.view == View::Cleanup => self.ask_cleanup(),
            KeyCode::Char('d') | KeyCode::Enter if self.view == View::Network => self.ask_disconnect(),
            KeyCode::Char('k') | KeyCode::Delete if self.view == View::Sessions => self.ask_end_selected(),
            KeyCode::Char('h') if self.view == View::Sessions => self.register_host(),
            KeyCode::Char('e') if self.view == View::Rules => self.edit_rules(),
            KeyCode::Char('r') if self.view == View::Rules => self.reload_rules(),
            _ => {}
        }
    }

    // ---- actions ----------------------------------------------------------------------------

    fn ask(&mut self, title: String, roots: Vec<u32>) {
        let Some(m) = self.model.as_ref() else { return };
        if roots.is_empty() {
            self.status = "nothing selected".into();
            return;
        }
        let plan = actions::plan(m, &roots);
        self.ask_plan(title, plan);
    }

    fn ask_plan(&mut self, title: String, plan: actions::KillPlan) {
        let Some(m) = self.model.as_ref() else { return };
        let refusals = actions::refusals(&plan, m, &self.catalog, self.self_pid);
        self.confirm = Some(Confirm { title, plan, refusals });
    }

    fn ask_end_selected(&mut self) {
        let Some(m) = self.model.as_ref() else { return };
        let Some(item) = self.items.get(self.selected[0]).cloned() else {
            return;
        };
        let (title, roots) = match item {
            Item::Tab(key) => {
                let tab = m.groups.iter().flat_map(|g| &g.tabs).find(|t| t.key == key);
                let roots: Vec<u32> = tab
                    .map(|t| t.sessions.iter().map(|s| s.root_pid).collect())
                    .unwrap_or_default();
                (
                    format!(
                        "End every agent session in tab {}?",
                        tab.map(|t| t.label.as_str()).unwrap_or("?")
                    ),
                    roots,
                )
            }
            // Items come from the last Sessions draw; the process may have exited since.
            Item::Session(p) | Item::Proc(p) | Item::Orphan(p) => match m.procs.get(&p) {
                Some(proc) => (format!("End {} ({p}) and everything under it?", proc.name), vec![p]),
                None => {
                    self.status = format!("process {p} already exited");
                    return;
                }
            },
            Item::OrphanHeader => (
                "End every orphan tree?".into(),
                m.orphans.iter().map(|o| o.pid).collect(),
            ),
            Item::Group(_) => {
                self.status = "select a tab, session or process to end".into();
                return;
            }
        };
        self.ask(title, roots);
    }

    fn ask_cleanup(&mut self) {
        let mut roots: Vec<u32> = self
            .candidates
            .iter()
            .map(|c| c.root_pid)
            .filter(|p| self.marked.contains(p))
            .collect();
        if roots.is_empty()
            && let Some(c) = self.candidates.get(self.selected[1])
        {
            roots.push(c.root_pid);
        }
        let n = roots.len();
        self.ask(format!("End {n} cleanup target(s) and their children?"), roots);
    }

    fn ask_disconnect(&mut self) {
        let Some(c) = self.ssh.get(self.selected[2]) else {
            return;
        };
        let title = format!("Disconnect {}:{} by ending {} ({})?", c.host, c.port, c.process, c.pid);
        let pid = c.pid;
        let Some(m) = self.model.as_ref() else { return };
        let plan = actions::disconnect_plan(m, pid);
        self.ask_plan(title, plan);
    }

    fn run_confirmed(&mut self) {
        let Some(c) = self.confirm.take() else { return };
        let results = actions::execute(&c.plan);
        let count = |want: fn(&Outcome) -> bool| results.iter().filter(|(_, o)| want(o)).count();
        let killed = count(|o| *o == Outcome::Killed);
        let gone = count(|o| *o == Outcome::AlreadyGone);
        let replaced = count(|o| *o == Outcome::Replaced);
        let failed: Vec<String> = results
            .iter()
            .filter_map(|(t, o)| match o {
                Outcome::Failed(e) => Some(format!("{} {}: {}", t.name, t.pid, e.trim())),
                _ => None,
            })
            .collect();
        self.status = format!(
            "ended {killed}, already gone {gone}, skipped (pid reused) {replaced}, failed {}",
            failed.len()
        );
        if let Some(first) = failed.first() {
            self.status.push_str(&format!(" — {first}"));
        }
        self.refresh_now = true;
    }

    fn register_host(&mut self) {
        let Some(Item::Group(key)) = self.items.get(self.selected[0]).cloned() else {
            self.status = "select an unregistered host group to register it".into();
            return;
        };
        let Some(exe) = key.strip_prefix('?') else {
            self.status = "this group is already a known host".into();
            return;
        };
        // Appended to the file, then everything reloaded: edits made in the editor since the
        // last reload are kept and take effect too.
        match Rules::register_host(exe) {
            Ok(path) => {
                self.reload_rules();
                self.status = format!("registered {exe} as a host in {}", path.display());
            }
            Err(e) => self.status = format!("could not register host: {e}"),
        }
    }

    fn edit_rules(&mut self) {
        let Some(path) = rules_path() else { return };
        if !path.exists()
            && let Err(e) = self.rules.save()
        {
            self.status = format!("could not create rules file: {e}");
            return;
        }
        // A GUI editor: a terminal one would fight the dashboard for the screen.
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("notepad.exe", &[])
        } else if cfg!(target_os = "macos") {
            ("open", &["-t"])
        } else {
            ("xdg-open", &[])
        };
        match std::process::Command::new(program).args(args).arg(&path).spawn() {
            Ok(_) => self.status = format!("editing {}; press r to reload after saving", path.display()),
            Err(e) => self.status = format!("could not open editor: {e}"),
        }
    }

    fn reload_rules(&mut self) {
        match Rules::load() {
            Ok(rules) => {
                self.rules = rules;
                self.rebuild_catalog();
                self.status = "rules reloaded".into();
            }
            Err(e) => self.status = format!("rules not reloaded: {e}"),
        }
    }

    fn rebuild_catalog(&mut self) {
        self.catalog = Catalog::builtin()
            .with_rules(&self.rules)
            .with_installed(&self.installed);
        self.new_env_keys = Some(self.catalog.env_keys());
        self.refresh_now = true;
    }
}

/// Rows for every process under `root` (excluding it), indented by depth.
fn push_procs(m: &Model, root: u32, indent: usize, rows: &mut Vec<Row<'static>>, items: &mut Vec<Item>) {
    let root_depth = m.ancestors(root).len();
    for pid in m.subtree(root).into_iter().skip(1) {
        let p = &m.procs[&pid];
        if m.pty_glue.contains(&pid) {
            continue;
        }
        let depth = m.ancestors(pid).len() - root_depth;
        let style = match m.roles.get(&pid) {
            Some(Role::Agent(_)) => Style::new().fg(Color::Green),
            _ => Style::new(),
        };
        rows.push(
            Row::new(vec![
                Cell::from(format!("{}{} ", "  ".repeat(indent + depth), p.name)),
                Cell::from(pid.to_string()),
                Cell::from(role_label(m, pid)),
                Cell::from(mb(p.private_bytes)),
                Cell::from(format!("{:.1}", p.cpu)),
                Cell::from(""),
                Cell::from(short_cmd(p)),
            ])
            .style(style),
        );
        items.push(Item::Proc(pid));
    }
}

fn fold(open: bool, label: &str) -> String {
    format!("{} {label}", if open { "▾" } else { "▸" })
}

/// `pid:<pid>` or `<host>:<tab marker>`, shortened for the Info column.
fn short_key(key: &str) -> String {
    match key.split_once(':') {
        Some(("pid", pid)) => format!("pid {pid}"),
        Some((_, id)) => format!("tab {}", id.chars().take(8).collect::<String>()),
        None => short_path(key, 1),
    }
}

/// Messages to the collector thread.
enum Wake {
    /// Snapshot now; connections may have closed.
    Refresh,
    /// The catalog changed: collect these environment variables from now on.
    EnvKeys(Vec<String>),
}

fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    area
}

pub fn run(rules: Rules, installed: Vec<(String, String)>) -> anyhow::Result<()> {
    let mut app = App::new(rules, installed);
    let (snap_tx, snap_rx) = mpsc::channel::<Snapshot>();
    let (wake_tx, wake_rx) = mpsc::channel::<Wake>();
    let mut collector = Collector::new(app.catalog.env_keys());
    std::thread::spawn(move || {
        loop {
            if snap_tx.send(collector.snapshot()).is_err() {
                return;
            }
            match wake_rx.recv_timeout(REFRESH) {
                Ok(Wake::Refresh) => collector.refresh_network(),
                Ok(Wake::EnvKeys(keys)) => collector.set_env_keys(keys),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });

    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
    let result = event_loop(&mut terminal, &mut app, &snap_rx, &wake_tx);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    snaps: &mpsc::Receiver<Snapshot>,
    wake: &mpsc::Sender<Wake>,
) -> anyhow::Result<()> {
    let mut dirty = true;
    while !app.quit {
        while let Ok(snapshot) = snaps.try_recv() {
            app.apply(snapshot);
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| app.draw(f))?;
            dirty = false;
        }
        // Handle everything already queued before drawing once, so bursts of input cost one frame.
        if event::poll(Duration::from_millis(100))? {
            loop {
                dirty |= app.on_event(event::read()?);
                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        if let Some(keys) = app.new_env_keys.take() {
            let _ = wake.send(Wake::EnvKeys(keys));
        }
        if app.refresh_now {
            app.refresh_now = false;
            let _ = wake.send(Wake::Refresh);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let snapshot: Snapshot =
            serde_json::from_str(include_str!("../../core/tests/fixtures/orca_2026-10-29.json")).unwrap();
        let mut app = App::new(Rules::default(), Vec::new());
        app.apply(snapshot);
        app
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect()
    }

    fn key(app: &mut App, code: KeyCode) {
        app.on_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn renders_every_view() {
        let mut app = app();
        let s = screen(&mut app);
        assert!(s.contains("Orca"), "{s}");
        assert!(s.contains("acme/web-app"));
        assert!(s.contains("No parent (orphans)"));
        key(&mut app, KeyCode::Char('2'));
        assert!(screen(&mut app).contains("dev-server.mjs"));
        key(&mut app, KeyCode::Char('3'));
        assert!(screen(&mut app).contains("no SSH connections"));
        key(&mut app, KeyCode::Char('4'));
        assert!(screen(&mut app).contains("agents in catalog: 45"));
    }

    #[test]
    fn selection_rows_match_items_after_folding() {
        let mut app = app();
        screen(&mut app);
        let target = app
            .items
            .iter()
            .position(|i| matches!(i, Item::Session(33472)))
            .unwrap();
        app.selected[0] = target;
        key(&mut app, KeyCode::Right);
        let s = screen(&mut app);
        assert!(app.items.len() > target + 1);
        assert!(
            matches!(app.items[target + 1], Item::Proc(_)),
            "session children follow it"
        );
        assert!(s.contains("bash.exe"));

        // Open an orphan and check that its rows line up with items.
        let orphan = app.items.iter().position(|i| matches!(i, Item::Orphan(25480))).unwrap();
        app.selected[0] = orphan;
        key(&mut app, KeyCode::Right);
        screen(&mut app);
        assert_eq!(app.items[orphan], Item::Orphan(25480));
        assert!(matches!(app.items[orphan + 1], Item::Proc(_)));
    }

    #[test]
    fn ending_a_tab_targets_its_sessions_only() {
        let mut app = app();
        screen(&mut app);
        let tab = app
            .items
            .iter()
            .position(|i| matches!(i, Item::Tab(k) if k.starts_with("orca:c1933913")))
            .unwrap();
        app.selected[0] = tab;
        key(&mut app, KeyCode::Char('k'));
        let m = app.model.as_ref().unwrap();
        let shell = m
            .groups
            .iter()
            .flat_map(|g| &g.tabs)
            .find(|t| t.key.starts_with("orca:c1933913"))
            .unwrap()
            .root_pids[0];
        let confirm = app.confirm.as_ref().unwrap();
        assert_eq!(confirm.plan.targets.first().unwrap().pid, 33472);
        assert!(
            confirm.plan.targets.iter().all(|t| t.pid != shell),
            "the tab's shell stays"
        );
        key(&mut app, KeyCode::Esc);
        assert!(app.confirm.is_none());
        assert!(!app.quit);
    }

    #[test]
    fn host_processes_are_refused() {
        let mut app = app();
        let host = app.model.as_ref().unwrap().groups[0].host_pids[0];
        app.ask("x".into(), vec![host]);
        assert!(!app.confirm.as_ref().unwrap().refusals.is_empty());
    }

    #[test]
    fn non_ascii_session_ids_render() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../core/tests/fixtures/orca_2026-10-29.json")).unwrap();
        let claude = snapshot.processes.iter_mut().find(|p| p.pid == 33472).unwrap();
        claude.cmd = vec!["claude".into(), "--resume".into(), "출석체크기능테스트".into()];
        let mut app = App::new(Rules::default(), Vec::new());
        app.apply(snapshot);
        // Wide characters span two cells, so check characters rather than the joined text.
        let s = screen(&mut app);
        assert!(s.contains('출') && s.contains('체'), "{s}");
    }

    #[test]
    fn ending_a_process_that_left_the_model_does_not_panic() {
        // Items come from the last Sessions draw; the model may have moved on since.
        let mut app = app();
        screen(&mut app);
        app.items = vec![Item::Proc(4_000_000)];
        app.selected[0] = 0;
        key(&mut app, KeyCode::Char('k'));
        assert!(app.confirm.is_none());
    }

    #[test]
    fn mouse_motion_does_not_request_a_redraw() {
        use crossterm::event::MouseEvent;
        let mut app = app();
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(!app.on_event(mouse(MouseEventKind::Moved)));
        assert!(app.on_event(mouse(MouseEventKind::ScrollDown)));
        assert!(app.on_event(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))));
    }
}
