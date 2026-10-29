//! Interactive dashboard: sessions grouped by host and tab, cleanup candidates, SSH, rules,
//! settings.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::Duration;

use agentlens_core::actions::{self, KillPlan, Outcome};
use agentlens_core::catalog::Catalog;
use agentlens_core::health::{
    Candidate, CandidateKind, History, SessionState, candidates, fmt_duration, session_state, short_cmd,
};
use agentlens_core::hooks::{self, HookState};
use agentlens_core::keys::{self, KeyInfo, SshInventory};
use agentlens_core::model::{Group, GroupKind, Model, Role, short_path};
use agentlens_core::net::{self, Port, PortOwner, SshConn, SshHost};
use agentlens_core::origin::Origins;
use agentlens_core::rules::{Language, Rules, rules_path};
use agentlens_core::snapshot::windowed_pids;
use agentlens_core::stdio::StdioProbe;
use agentlens_core::{Collector, Snapshot};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::i18n::{phrase, t, tf};
use crate::report::{mb, role_label};

/// In the order of `View`'s variants: a view's index is `view as usize`.
const VIEWS: [View; 6] = [
    View::Sessions,
    View::Cleanup,
    View::Network,
    View::Ports,
    View::Rules,
    View::Settings,
];
const REFRESH: Duration = Duration::from_secs(1);
/// Choices offered in Settings; other values typed into the rules file are kept as they are.
const AUTO_CLEANUP_MINUTES: [u64; 6] = [0, 5, 10, 15, 30, 60];
const IDLE_MINUTES: [u64; 5] = [10, 15, 30, 60, 120];
/// Automatic cleanup leaves candidates alone until they have been seen this long, so a tree
/// caught mid-exit or mid-restart is not ended.
const AUTO_CLEANUP_GRACE_SECS: u64 = 60;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Sessions,
    Cleanup,
    Network,
    Ports,
    Rules,
    Settings,
}

impl View {
    fn name(self, lang: Language) -> &'static str {
        match self {
            View::Sessions => t(lang, "Sessions", "세션"),
            View::Cleanup => t(lang, "Cleanup", "정리"),
            View::Network => t(lang, "Network", "네트워크"),
            View::Ports => t(lang, "Ports", "포트"),
            View::Rules => t(lang, "Rules", "규칙"),
            View::Settings => t(lang, "Settings", "설정"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    Language,
    AutoCleanup,
    AutoCleanupIdle,
    IdleMinutes,
    EndWindowed,
    EndShared,
    /// Registering agentlens in an agent's hooks, by index into `Catalog::agents`.
    Hooks(usize),
}

/// Fixed settings; one `Hooks` row per agent with hooks follows them.
const SETTINGS: [Setting; 6] = [
    Setting::Language,
    Setting::AutoCleanup,
    Setting::AutoCleanupIdle,
    Setting::IdleMinutes,
    Setting::EndWindowed,
    Setting::EndShared,
];

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
    origins: Origins,
    stdio: StdioProbe,
    /// Latest hook state of each session, by agent process (pid, start time).
    hook_states: HashMap<(u32, u64), HookState>,
    /// Agent id -> whether agentlens is in its hook settings, read when Settings opens.
    hooks_registered: HashMap<String, Result<bool, String>>,
    candidates: Vec<Candidate>,
    ssh: Vec<SshConn>,
    ports: Vec<Port>,
    /// Ports shows every listening port, not only agents' and orphans'.
    all_ports: bool,
    ssh_config: Vec<SshHost>,
    /// Read again whenever Network is opened.
    ssh_keys: SshInventory,
    view: View,
    sort: Sort,
    selected: [usize; 6],
    offsets: [usize; 6],
    flipped: HashSet<String>,
    marked: HashSet<u32>,
    /// Candidate root -> when it was first seen, or first proposed.
    seen_candidates: HashMap<u32, u64>,
    /// Candidate roots proposed (and pre-marked) once; a later unmark is kept.
    proposed: HashSet<u32>,
    /// When automatic cleanup last ran, or was switched on.
    last_auto_cleanup: Option<u64>,
    /// When it last ended something or found nothing, and what happened.
    auto_cleanup_result: Option<(u64, String)>,
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
        let status = t(
            rules.language,
            "q quit · ←→ fold · k end tree · enter details · s sort · 1-6 views",
            "q 종료 · ←→ 접기 · k 트리 종료 · enter 상세 · s 정렬 · 1-6 화면",
        )
        .into();
        let ssh_config = net::load_ssh_config();
        Self {
            catalog,
            rules,
            installed,
            self_pid: std::process::id(),
            model: None,
            history: History::default(),
            origins: Origins::default(),
            stdio: StdioProbe::default(),
            hook_states: HashMap::new(),
            hooks_registered: HashMap::new(),
            candidates: Vec::new(),
            ssh: Vec::new(),
            ports: Vec::new(),
            all_ports: false,
            ssh_keys: keys::load_default(&ssh_config),
            ssh_config,
            view: View::Sessions,
            sort: Sort::Memory,
            selected: [0; 6],
            offsets: [0; 6],
            flipped: HashSet::new(),
            marked: HashSet::new(),
            seen_candidates: HashMap::new(),
            proposed: HashSet::new(),
            last_auto_cleanup: None,
            auto_cleanup_result: None,
            confirm: None,
            detail: false,
            status,
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
        self.origins.update(&model, &self.catalog);
        let closed = self.stdio.update(&model);
        self.origins.note_stdio_closed(&model, &closed);
        self.hook_states = hooks::state_dir()
            .map(|dir| hooks::read_states(&dir, &model))
            .unwrap_or_default();
        // The lists are rebuilt and re-sorted every refresh: keep the cursor on the same row.
        let cleanup_key = |c: &Candidate| (c.kind, c.root_pid);
        let ssh_key = |c: &SshConn| (c.pid, c.remote.clone(), c.local.clone());
        let was_candidate = self.candidates.get(self.selected[1]).map(cleanup_key);
        let was_ssh = self.ssh.get(self.selected[2]).map(ssh_key);
        let port_key = |p: &Port| (p.pid, p.port);
        let was_port = self.visible_ports().get(self.selected[3]).map(|p| port_key(p));
        self.candidates = candidates(
            &model,
            &self.history,
            &self.origins,
            &self.rules.policy(),
            windowed_pids,
        );
        self.ssh = net::ssh_connections(&model, &self.catalog, &self.ssh_config);
        self.ports = net::ports(&model);
        if let Some(i) = was_port.and_then(|k| self.visible_ports().iter().position(|p| port_key(p) == k)) {
            self.selected[3] = i;
        }
        if let Some(i) = was_candidate.and_then(|k| self.candidates.iter().position(|c| cleanup_key(c) == k)) {
            self.selected[1] = i;
        }
        if let Some(i) = was_ssh.and_then(|k| self.ssh.iter().position(|c| ssh_key(c) == k)) {
            self.selected[2] = i;
        }

        // Pre-mark candidates when they are first proposed, which can be after they first show
        // up (an owner gone long enough), and announce them; keep the user's choices otherwise.
        let mut fresh = Vec::new();
        for c in &self.candidates {
            if c.eligible && self.proposed.insert(c.root_pid) {
                // Automatic cleanup's grace counts from here.
                self.seen_candidates.insert(c.root_pid, model.taken_at);
                self.marked.insert(c.root_pid);
                fresh.push(format!("{} ({})", c.label, mb(c.private_bytes)));
            } else {
                self.seen_candidates.entry(c.root_pid).or_insert(model.taken_at);
            }
        }
        let alive: HashSet<u32> = self.candidates.iter().map(|c| c.root_pid).collect();
        self.marked.retain(|p| alive.contains(p));
        self.proposed.retain(|p| alive.contains(p));
        self.seen_candidates.retain(|p, _| alive.contains(p));
        if !fresh.is_empty() && self.model.is_some() {
            let fresh = fresh.join(", ");
            self.status = tf!(self.lang(), "new cleanup candidate: {fresh}", "새 정리 후보: {fresh}");
        }
        self.model = Some(model);
        self.auto_cleanup();
    }

    fn lang(&self) -> Language {
        self.rules.language
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
        let was_selected = self.items.get(self.selected[0]).cloned();
        let lang = self.lang();
        let mut items = Vec::new();
        let mut rows = Vec::new();

        let stats = |pids: &[u32], idle: Option<u64>| -> Vec<Cell<'static>> {
            vec![
                Cell::from(pids.len().to_string()),
                Cell::from(mb(m.private_bytes(pids))),
                Cell::from(format!("{:.1}", m.cpu(pids))),
                Cell::from(idle.map(fmt_duration).unwrap_or_default()),
                Cell::from(""),
            ]
        };

        if !m.orphans.is_empty() {
            let header = Item::OrphanHeader;
            let all: Vec<u32> = m.orphans.iter().flat_map(|o| m.subtree(o.pid)).collect();
            let mut cells = vec![
                Cell::from(fold(
                    self.is_open(&header),
                    t(lang, "⚠ No parent (orphans)", "⚠ 부모 없음 (고아)"),
                )),
                Cell::from(""),
            ];
            cells.extend(stats(&all, None));
            cells.push(Cell::from(t(
                lang,
                "parent exited; see Cleanup, how it is judged in Rules",
                "부모 종료됨 · 정리 화면 참고, 판정 원리는 규칙 화면",
            )));
            rows.push(Row::new(cells).style(Style::new().fg(Color::Yellow).bold()));
            let open = self.is_open(&header);
            items.push(header);
            if open {
                for o in &m.orphans {
                    let item = Item::Orphan(o.pid);
                    let tree = m.subtree(o.pid);
                    let p = &m.procs[&o.pid];
                    let note = match (o.agent_related, &o.origin) {
                        (true, _) => t(lang, "agent leftover", "에이전트 잔여물").to_string(),
                        (false, Some(tab)) => tf!(lang, "started by agent in {tab}", "{tab}에서 에이전트가 실행"),
                        (false, None) => t(lang, "not agent", "에이전트 아님").to_string(),
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
            let item = Item::Group(group_key(g));
            let all: Vec<u32> = g.tabs.iter().flat_map(|t| m.tab_pids(t)).collect();
            let sessions: usize = g.tabs.iter().map(|t| t.sessions.len()).sum();
            let mut cells = vec![Cell::from(fold(self.is_open(&item), &g.name)), Cell::from("")];
            cells.extend(stats(&all, None));
            let (tabs, app, procs) = (g.tabs.len(), mb(m.private_bytes(&g.host_pids)), g.host_pids.len());
            let mut info = tf!(
                lang,
                "{tabs} tabs · {sessions} sessions · app {app} in {procs} procs",
                "탭 {tabs}개 · 세션 {sessions}개 · 앱 {app} (프로세스 {procs}개)"
            );
            if matches!(g.kind, GroupKind::Unregistered(_)) {
                info.push_str(t(lang, " · h: register as host", " · h: 호스트로 등록"));
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
                    // What the agent itself reported wins over what its CPU use suggests.
                    let start = m.procs[&s.root_pid].start_time;
                    *cells.last_mut().unwrap() = match self.hook_states.get(&(s.root_pid, start)) {
                        Some(state) => hook_cell(lang, *state),
                        None => state_cell(lang, session_state(m, &self.history, &tree, self.rules.idle_minutes)),
                    };
                    let sid = s
                        .session_id
                        .as_deref()
                        .map(|id| {
                            let id: String = id.chars().take(13).collect();
                            tf!(lang, "session {id}", "세션 {id}")
                        })
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
        // Rows are re-sorted every refresh: keep the cursor on the same item.
        if let Some(i) = was_selected.and_then(|was| items.iter().position(|i| *i == was)) {
            self.selected[0] = i;
        }
        self.items = items;
        rows
    }

    fn cleanup_rows(&self) -> Vec<Row<'static>> {
        let lang = self.lang();
        self.candidates
            .iter()
            .map(|c| {
                let mark = if self.marked.contains(&c.root_pid) {
                    "[x]"
                } else {
                    "[ ]"
                };
                let kind = match c.kind {
                    CandidateKind::Orphan => t(lang, "orphan", "고아"),
                    CandidateKind::IdleSession => t(lang, "idle", "유휴"),
                    CandidateKind::DuplicateMcp => t(lang, "dup mcp", "중복 MCP"),
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
                    Cell::from(c.reasons.iter().map(|r| phrase(lang, r)).collect::<Vec<_>>().join(", ")),
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
                    Cell::from(if c.inbound {
                        t(self.lang(), "in", "수신")
                    } else {
                        t(self.lang(), "out", "발신")
                    }),
                    Cell::from(format!("{who}{}:{}{alias}", c.host, c.port)),
                    Cell::from(c.remote.clone()),
                    Cell::from(format!("{} {}", c.process, c.pid)),
                    Cell::from(fmt_duration(m.taken_at.saturating_sub(c.since))),
                    Cell::from(owner),
                ])
            })
            .collect()
    }

    fn visible_ports(&self) -> Vec<&Port> {
        self.ports
            .iter()
            .filter(|p| self.all_ports || p.agent_related())
            .collect()
    }

    fn port_rows(&self) -> Vec<Row<'static>> {
        let lang = self.lang();
        let Some(m) = self.model.as_ref() else {
            return Vec::new();
        };
        self.visible_ports()
            .into_iter()
            .map(|p| {
                let mut owner = match p.owner {
                    PortOwner::Session(g, t, s) => {
                        let tab = &m.groups[g].tabs[t];
                        format!("{} @ {} / {}", tab.sessions[s].agent_name, m.groups[g].name, tab.label)
                    }
                    PortOwner::Orphan(root) => {
                        let name = m.procs.get(&root).map(|r| r.name.as_str()).unwrap_or("?");
                        match m
                            .orphans
                            .iter()
                            .find(|o| o.pid == root)
                            .and_then(|o| o.origin.as_deref())
                        {
                            Some(tab) => tf!(
                                lang,
                                "orphan {name} {root} · from tab {tab}",
                                "고아 {name} {root} · 출처 탭 {tab}"
                            ),
                            None => tf!(lang, "orphan {name} {root}", "고아 {name} {root}"),
                        }
                    }
                    PortOwner::Other => role_label(m, p.pid).to_string(),
                };
                if p.agent_internal {
                    owner.push_str(t(lang, " · agent's own server", " · 에이전트 자체 서버"));
                }
                let bind = if p.exposed {
                    t(lang, "exposed", "외부 노출")
                } else {
                    t(lang, "local", "로컬")
                };
                let style = match p.owner {
                    PortOwner::Orphan(_) => Style::new().fg(Color::Yellow),
                    _ if p.agent_internal => Style::new().dim(),
                    _ => Style::new(),
                };
                let tree = m.subtree(p.pid);
                Row::new(vec![
                    Cell::from(p.port.to_string()),
                    Cell::from(format!("{bind} ({})", p.addrs.join(", "))),
                    Cell::from(format!("{} {}", p.process, p.pid)),
                    Cell::from(p.clients.to_string()),
                    Cell::from(mb(m.private_bytes(&tree))),
                    Cell::from(owner),
                ])
                .style(style)
            })
            .collect()
    }

    fn settings(&self) -> Vec<Setting> {
        let hooks = self
            .catalog
            .agents
            .iter()
            .enumerate()
            .filter(|(_, a)| a.hooks.is_some())
            .map(|(i, _)| Setting::Hooks(i));
        SETTINGS.into_iter().chain(hooks).collect()
    }

    /// Reads, for each agent with hooks, whether agentlens is registered there.
    fn read_hooks_registered(&mut self) {
        self.hooks_registered = self
            .catalog
            .agents
            .iter()
            .filter_map(|a| Some((a.id.clone(), hooks::registered(a.hooks.as_ref()?))))
            .collect();
    }

    fn hook_row(&self, lang: Language, agent: usize) -> Row<'static> {
        let a = &self.catalog.agents[agent];
        let value = match self.hooks_registered.get(&a.id) {
            Some(Ok(true)) => t(lang, "on", "켜짐").to_string(),
            Some(Ok(false)) | None => t(lang, "off", "꺼짐").to_string(),
            Some(Err(e)) => tf!(lang, "error: {e}", "오류: {e}"),
        };
        let settings = a.hooks.as_ref().map(|h| h.settings.clone()).unwrap_or_default();
        Row::new(vec![
            Cell::from(tf!(lang, "Agent hooks: {}", "에이전트 훅: {}", a.name)),
            Cell::from(format!("‹ {value} ›")),
            Cell::from(tf!(
                lang,
                "Shows working / waiting / needs you, reported by the agent ({settings})",
                "에이전트가 알려 주는 작업 중·입력 대기·확인 필요 표시 ({settings})"
            )),
        ])
    }

    fn settings_rows(&self) -> Vec<Row<'static>> {
        let lang = self.lang();
        let r = &self.rules;
        self.settings()
            .iter()
            .map(|s| {
                let (name, value, about) = match s {
                    Setting::Hooks(i) => return self.hook_row(lang, *i),
                    Setting::Language => (
                        t(lang, "Language", "언어"),
                        match r.language {
                            Language::En => "English".to_string(),
                            Language::Ko => "한국어".to_string(),
                        },
                        t(lang, "Dashboard language", "대시보드 표시 언어"),
                    ),
                    Setting::AutoCleanup => (
                        t(lang, "Automatic cleanup", "자동 정리"),
                        match r.auto_cleanup_minutes {
                            0 => t(lang, "off", "끔").to_string(),
                            n => tf!(lang, "every {n} min", "{n}분마다"),
                        },
                        t(
                            lang,
                            "Ends the proposed (yellow) candidates still marked in Cleanup",
                            "정리 화면에서 표시된 기본 제안(노란색) 후보를 종료",
                        ),
                    ),
                    Setting::AutoCleanupIdle => (
                        t(lang, "Auto cleanup: idle sessions", "자동 정리: 유휴 세션"),
                        if r.auto_cleanup_idle {
                            t(lang, "included", "포함")
                        } else {
                            t(lang, "excluded", "제외")
                        }
                        .to_string(),
                        t(
                            lang,
                            "Also end agent sessions idle past the threshold",
                            "유휴 기준을 넘긴 에이전트 세션도 종료",
                        ),
                    ),
                    Setting::IdleMinutes => (
                        t(lang, "Idle threshold", "유휴 기준"),
                        tf!(lang, "{} min", "{}분", r.idle_minutes),
                        t(
                            lang,
                            "A session whose processes stay under 1% CPU this long is idle",
                            "세션 프로세스가 이 시간 동안 CPU 1% 미만이면 유휴",
                        ),
                    ),
                    Setting::EndWindowed => (
                        t(lang, "Leftovers with a window", "창이 있는 잔여 프로세스"),
                        end_or_keep(lang, r.end_windowed),
                        t(
                            lang,
                            "Apps an ended session opened that you can see, such as a browser",
                            "끝난 세션이 연, 화면에 보이는 앱(브라우저 등)",
                        ),
                    ),
                    Setting::EndShared => (
                        t(lang, "Leftovers in use", "사용 중인 잔여 프로세스"),
                        end_or_keep(lang, r.end_shared),
                        t(
                            lang,
                            "Leftovers a live session is connected to, such as a shared build daemon",
                            "살아 있는 세션이 연결해 쓰는 잔여 프로세스(공용 빌드 데몬 등)",
                        ),
                    ),
                };
                Row::new(vec![
                    Cell::from(name),
                    Cell::from(format!("‹ {value} ›")),
                    Cell::from(about),
                ])
            })
            .collect()
    }

    fn row_count(&self) -> usize {
        match self.view {
            View::Sessions => self.items.len(),
            View::Cleanup => self.candidates.len(),
            View::Network => self.ssh.len(),
            View::Ports => self.visible_ports().len(),
            View::Rules => self.rules_lines().len(),
            View::Settings => self.settings().len(),
        }
    }

    // ---- drawing ----------------------------------------------------------------------------

    fn view_titles(&self) -> Vec<String> {
        VIEWS
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{} {}", i + 1, v.name(self.lang())))
            .collect()
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let [top, body, bottom] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
        let titles = self.view_titles();
        // Each title is padded by a column on both sides, with a one-column divider between.
        let width: usize = titles.iter().map(|t| Line::from(t.as_str()).width() + 3).sum();
        let [tabs_area, summary_area] =
            Layout::horizontal([Constraint::Length(width as u16), Constraint::Min(10)]).areas(top);
        self.tabs_area = tabs_area;
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
            View::Rules => self.draw_rules(frame, table_area),
            View::Settings => {
                let [table, info] =
                    Layout::vertical([Constraint::Length(self.settings().len() as u16 + 3), Constraint::Min(6)])
                        .areas(table_area);
                self.draw_table(frame, table);
                frame.render_widget(self.auto_cleanup_text(), info);
            }
            View::Network => {
                let lines = self.ssh_key_lines();
                let height = (lines.len() as u16 + 2).min(table_area.height / 2);
                let [table, keys] =
                    Layout::vertical([Constraint::Min(5), Constraint::Length(height)]).areas(table_area);
                self.draw_table(frame, table);
                let title = t(
                    self.lang(),
                    "SSH keys and hosts (~/.ssh, read only)",
                    "SSH 키와 호스트 (~/.ssh, 조회 전용)",
                );
                let text = Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(title));
                frame.render_widget(text, keys);
            }
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
        let lang = self.lang();
        let Some(m) = self.model.as_ref() else {
            return Line::from(t(lang, "collecting…", "수집 중…"));
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
            Sort::Memory => t(lang, "mem", "메모리"),
            Sort::Cpu => "cpu",
            Sort::Idle => t(lang, "idle", "유휴"),
        };
        let (mem, procs, eligible, ssh) = (mb(m.private_bytes(&pids)), pids.len(), mb(eligible), self.ssh.len());
        Line::from(vec![
            Span::raw(tf!(
                lang,
                "{sessions} sessions · {mem} in {procs} procs · ",
                "세션 {sessions}개 · {mem} (프로세스 {procs}개) · "
            )),
            Span::styled(
                tf!(lang, "cleanup {eligible}", "정리 가능 {eligible}"),
                Style::new().fg(Color::Yellow),
            ),
            Span::raw(tf!(lang, " · ssh {ssh} · sort {sort} ", " · SSH {ssh} · 정렬 {sort} ")),
        ])
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let lang = self.lang();
        let (header, widths, rows, title): (Vec<&str>, Vec<Constraint>, Vec<Row>, &str) = match self.view {
            View::Sessions => (
                vec![
                    t(lang, "Name", "이름"),
                    "PID",
                    t(lang, "Procs", "개수"),
                    t(lang, "Private", "메모리"),
                    "CPU%",
                    t(lang, "Idle", "유휴"),
                    t(lang, "State", "상태"),
                    t(lang, "Info", "정보"),
                ],
                vec![
                    Constraint::Min(30),
                    Constraint::Length(7),
                    Constraint::Length(6),
                    Constraint::Length(9),
                    Constraint::Length(6),
                    Constraint::Length(7),
                    Constraint::Length(10),
                    Constraint::Fill(2),
                ],
                self.session_rows(),
                t(
                    lang,
                    "Sessions by host / tab · k/x/del end the selected tree · enter details",
                    "호스트/탭별 세션 · k/x/del 선택한 트리 종료 · enter 상세",
                ),
            ),
            View::Cleanup => (
                vec![
                    "",
                    t(lang, "Kind", "종류"),
                    t(lang, "What", "대상"),
                    "PID",
                    t(lang, "Private", "메모리"),
                    t(lang, "Evidence", "근거"),
                ],
                vec![
                    Constraint::Length(3),
                    Constraint::Length(8),
                    Constraint::Fill(2),
                    Constraint::Length(7),
                    Constraint::Length(9),
                    Constraint::Fill(3),
                ],
                self.cleanup_rows(),
                t(
                    lang,
                    "Cleanup · space mark · x end marked (yellow = proposed)",
                    "정리 · space 표시 · x 표시한 항목 종료 (노란색 = 기본 제안)",
                ),
            ),
            View::Network => (
                vec![
                    t(lang, "Dir", "방향"),
                    t(lang, "Destination", "목적지"),
                    t(lang, "Remote", "원격 주소"),
                    t(lang, "Process", "프로세스"),
                    t(lang, "Up", "경과"),
                    t(lang, "Owner", "소유 세션"),
                ],
                vec![
                    Constraint::Length(4),
                    Constraint::Fill(2),
                    Constraint::Length(22),
                    Constraint::Length(16),
                    Constraint::Length(7),
                    Constraint::Fill(2),
                ],
                self.network_rows(),
                t(lang, "SSH connections · d disconnect", "SSH 연결 · d 연결 끊기"),
            ),
            View::Ports => (
                vec![
                    t(lang, "Port", "포트"),
                    t(lang, "Bound to", "바인딩"),
                    t(lang, "Process", "프로세스"),
                    t(lang, "Clients", "접속"),
                    t(lang, "Private", "메모리"),
                    t(lang, "Owner", "주인"),
                ],
                vec![
                    Constraint::Length(6),
                    Constraint::Length(26),
                    Constraint::Length(20),
                    Constraint::Length(5),
                    Constraint::Length(9),
                    Constraint::Fill(1),
                ],
                self.port_rows(),
                if self.all_ports {
                    t(
                        lang,
                        "Listening ports: all · k/x/del end the owner · a agents' only",
                        "열린 포트: 전체 · k/x/del 주인 종료 · a 에이전트 관련만",
                    )
                } else {
                    t(
                        lang,
                        "Listening ports of agents and orphans (yellow) · k/x/del end the owner · a all",
                        "에이전트·고아(노란색)가 연 포트 · k/x/del 주인 종료 · a 전체 보기",
                    )
                },
            ),
            View::Settings => (
                vec![
                    t(lang, "Setting", "설정"),
                    t(lang, "Value", "값"),
                    t(lang, "Description", "설명"),
                ],
                vec![Constraint::Length(30), Constraint::Length(18), Constraint::Fill(1)],
                self.settings_rows(),
                t(
                    lang,
                    "Settings · ←/→ enter change · saved to the rules file",
                    "설정 · ←/→ enter 변경 · 규칙 파일에 저장",
                ),
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
                View::Cleanup => t(lang, "nothing to clean up", "정리할 항목 없음"),
                View::Network => t(lang, "no SSH connections", "SSH 연결 없음"),
                View::Ports => t(
                    lang,
                    "no listening ports from agents or orphans (a: show all)",
                    "에이전트나 고아가 연 포트 없음 (a: 전체 보기)",
                ),
                _ => t(lang, "no agent sessions found", "에이전트 세션 없음"),
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
        let lang = self.lang();
        let block = Block::bordered().title(t(lang, "Details", "상세"));
        let (Some(m), Some(pid)) = (self.model.as_ref(), self.selected_pid()) else {
            return Paragraph::new(t(lang, "select a session or process", "세션이나 프로세스를 선택하세요"))
                .block(block);
        };
        let Some(p) = m.procs.get(&pid) else {
            return Paragraph::new("").block(block);
        };
        let tree = m.subtree(pid);
        let (name, role, age) = (
            &p.name,
            role_label(m, pid),
            fmt_duration(m.taken_at.saturating_sub(p.start_time)),
        );
        let (procs, private, working) = (
            tree.len(),
            mb(m.private_bytes(&tree)),
            mb(tree.iter().map(|q| m.procs[q].working_set).sum()),
        );
        let mut lines = vec![
            Line::from(tf!(
                lang,
                "{name} {pid}  [{role}]  started {age} ago",
                "{name} {pid}  [{role}]  {age} 전 시작"
            )),
            Line::from(format!("cmd: {}", p.cmd.join(" "))),
            Line::from(format!("cwd: {}", p.cwd.as_deref().unwrap_or("-"))),
            Line::from(tf!(
                lang,
                "tree: {procs} procs · private {private} · working set {working}",
                "트리: 프로세스 {procs}개 · 전용 {private} · 작업 집합 {working}"
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
        let (n, list) = (conns.len(), conns.join(", "));
        lines.push(Line::from(tf!(lang, "connections ({n}): {list}", "연결 ({n}): {list}")));
        let mut ports: Vec<String> = m
            .listening
            .iter()
            .filter(|l| tree.contains(&l.pid))
            .map(|l| net::endpoint(&l.addr, l.port))
            .collect();
        ports.sort();
        ports.dedup();
        if !ports.is_empty() {
            let list = ports.join(", ");
            lines.push(Line::from(tf!(lang, "listening: {list}", "열린 포트: {list}")));
        }
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(block)
    }

    fn draw_rules(&mut self, frame: &mut Frame, area: Rect) {
        let lines = self.rules_lines();
        let v = self.view_index();
        self.selected[v] = self.selected[v].min(lines.len().saturating_sub(1));
        let title = t(
            self.lang(),
            "Rules · ↑↓ scroll · e edit · r reload",
            "규칙 · ↑↓ 스크롤 · e 편집 · r 다시 읽기",
        );
        let text = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.selected[v] as u16, 0))
            .block(Block::bordered().title(title));
        frame.render_widget(text, area);
        self.table_area = area;
    }

    fn rules_lines(&self) -> Vec<Line<'static>> {
        let lang = self.lang();
        let path = rules_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".into());
        let mut installed: Vec<&str> = self.installed.iter().map(|(id, _)| id.as_str()).collect();
        installed.dedup();
        let (idle, protect) = (
            self.rules.idle_minutes,
            if self.catalog.protect.is_empty() {
                "-".into()
            } else {
                self.catalog.protect.join(", ")
            },
        );
        // Mirrors `Model::build` and `health::candidates`.
        let orphans: [&str; 6] = match lang {
            Language::En => [
                "A parent link counts only when the parent PID is alive, started no later than the child, \
                 and is not init/launchd/systemd (which adopt orphans).",
                "Without one the process has no parent: parent-gone (PID not running), parent-pid-reused \
                 (PID now belongs to a newer process), adopted-by-init (Unix, after the parent exited).",
                "An agent session whose ancestors end at a shell, agent or runtime with no host above lost \
                 whatever started it: the whole session is an orphan.",
                "Other parentless processes outside sessions count only when their tree holds agent machinery \
                 (agent, REPL, MCP server, agent or host traces), or they are a runtime (node, python …) or a \
                 shell running one. Hosts and system processes never do.",
                "Whatever an ended session left behind is an orphan too, found two ways: the dashboard saw it \
                 under that session, or its environment names the session (AI_AGENT, CLAUDE_PID, \
                 CLAUDE_CODE_SESSION_ID), which also covers leftovers from before the dashboard started. \
                 It counts 15 s after its owner is seen gone (owner-gone, from session …). On Windows a \
                 parentless process whose input pipe lost its writer (stdio-closed) can never be reached \
                 again; it is proposed when it is a runtime or shell, shown otherwise.",
                "Proposed for cleanup (yellow): orphans holding agent machinery, and everything an ended \
                 session left, dev servers included. Leftovers with a window or that a live session is \
                 connected to are kept unless Settings says otherwise.",
            ],
            Language::Ko => [
                "부모 PID가 살아 있고, 자식보다 먼저 시작됐고, init/launchd/systemd(고아를 입양하는 프로세스)가 \
                 아닐 때만 부모로 인정합니다.",
                "인정되는 부모가 없으면 부모 없음입니다: parent-gone(그 PID가 없음), parent-pid-reused(PID가 \
                 더 늦게 시작된 다른 프로세스로 재사용됨), adopted-by-init(Unix에서 부모가 죽은 뒤 입양됨).",
                "에이전트 세션의 조상이 호스트 없이 셸·에이전트·런타임에서 끊기면 세션을 띄운 쪽이 종료된 \
                 것이므로 세션 전체가 고아입니다.",
                "세션 밖의 부모 없는 프로세스는 트리에 에이전트 구성요소(에이전트, REPL, MCP 서버, 에이전트·호스트 \
                 흔적)가 있거나, 런타임(node, python …)이거나 런타임을 돌리는 셸일 때만 고아로 봅니다. 호스트와 \
                 시스템 프로세스는 제외합니다.",
                "끝난 세션이 남긴 것도 고아입니다. 대시보드가 그 세션 아래에서 본 적이 있거나, 환경변수(AI_AGENT, \
                 CLAUDE_PID, CLAUDE_CODE_SESSION_ID)에 세션이 적혀 있으면 알 수 있어 대시보드를 켜기 전에 남은 것도 \
                 찾습니다. 주인이 사라진 것을 본 뒤 15초가 지나면 고아로 봅니다(주인 세션 종료, 출처 세션 …). Windows에서는 \
                 부모 없는 프로세스의 입력 파이프에 쓰는 쪽이 모두 닫혔으면(입력 채널 끊김) 다시는 요청을 받을 수 \
                 없으므로, 런타임·셸이면 제안하고 그 밖에는 보여 줍니다.",
                "정리 후보로 기본 제안(노란색)하는 것: 에이전트 구성요소가 남은 고아, 그리고 dev 서버를 포함해 끝난 \
                 세션이 남긴 모든 것. 창이 있거나 살아 있는 세션이 연결해 쓰는 것은 설정에서 바꾸지 않는 한 남겨 둡니다.",
            ],
        };
        let mut lines = vec![
            Line::from(tf!(
                lang,
                "rules file: {path}  (e: edit · r: reload)",
                "규칙 파일: {path}  (e: 편집 · r: 다시 읽기)"
            ))
            .bold(),
            Line::from(tf!(
                lang,
                "idle after {idle} min · protect: {protect}",
                "유휴 기준 {idle}분 · 보호: {protect}"
            )),
            Line::from(""),
            Line::from(t(lang, "How orphans are found", "고아 프로세스 판정 원리")).bold(),
        ];
        lines.extend(orphans.iter().map(|l| Line::from(format!("  · {l}"))));
        let (agents, installed) = (self.catalog.agents.len(), installed.join(", "));
        lines.extend([
            Line::from(""),
            Line::from(tf!(
                lang,
                "agents in catalog: {agents}",
                "카탈로그의 에이전트: {agents}"
            ))
            .bold(),
            Line::from(tf!(lang, "installed here: {installed}", "이 PC에 설치됨: {installed}")),
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
            Line::from(tf!(lang, "hosts: {}", "호스트: {}", self.catalog.hosts.len())).bold(),
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
            Line::from(t(lang, "rules.toml example:", "rules.toml 예시:")).bold(),
            Line::from("  idle_minutes = 30"),
            Line::from("  protect = [\"dev-server\"]"),
            Line::from("  language = \"ko\"            # en | ko"),
            Line::from("  auto_cleanup_minutes = 15  # 0 = off"),
            Line::from("  auto_cleanup_idle = false"),
            Line::from("  end_windowed = false       # leftovers with a window"),
            Line::from("  end_shared = false         # leftovers a live session uses"),
            Line::from("  [[agents]]  id = \"myagent\"  exe = [\"myagent\"]  cmd = [\"@me/agent\"]"),
            Line::from("  [[hosts]]   id = \"myterm\"   exe = [\"myterm\"]"),
        ]);
        lines
    }

    fn ssh_key_lines(&self) -> Vec<Line<'static>> {
        let lang = self.lang();
        let inv = &self.ssh_keys;
        let dir = inv
            .dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "-".into());
        let key_line = |k: &KeyInfo| {
            let mut parts = vec![format!("  {}", k.name)];
            parts.push(if k.kind.is_empty() {
                t(lang, "(private key only, no .pub)", "(개인키만 있음, .pub 없음)").to_string()
            } else {
                k.kind.clone()
            });
            parts.extend(k.fingerprint.clone());
            if !k.comment.is_empty() {
                parts.push(k.comment.clone());
            }
            if k.has_private && !k.kind.is_empty() {
                parts.push(t(lang, "private key present", "개인키 있음").to_string());
            }
            if !k.used_by.is_empty() {
                let hosts = k.used_by.join(", ");
                parts.push(tf!(lang, "used for {hosts}", "사용 호스트: {hosts}"));
            }
            Line::from(parts.join("  "))
        };
        let (n, none) = (inv.keys.len(), t(lang, "  none", "  없음"));
        let mut lines = vec![Line::from(tf!(lang, "keys ({n}) · {dir}", "키 ({n}개) · {dir}")).bold()];
        lines.extend(inv.keys.iter().map(key_line));
        if inv.keys.is_empty() {
            lines.push(Line::from(none).dim());
        }
        let n = inv.known.len();
        lines.push(
            Line::from(tf!(
                lang,
                "known_hosts ({n}): servers this machine has connected to",
                "known_hosts ({n}개): 이 PC가 접속했던 서버"
            ))
            .bold(),
        );
        lines.extend(
            inv.known
                .iter()
                .map(|h| Line::from(format!("  {}  {}", h.hosts, h.kinds.join(", ")))),
        );
        if inv.known.is_empty() {
            lines.push(Line::from(none).dim());
        }
        let n = inv.authorized.len();
        lines.push(
            Line::from(tf!(
                lang,
                "authorized_keys ({n}): keys allowed to log in to this machine",
                "authorized_keys ({n}개): 이 PC에 로그인할 수 있는 키"
            ))
            .bold(),
        );
        lines.extend(inv.authorized.iter().map(key_line));
        if inv.authorized.is_empty() {
            lines.push(Line::from(none).dim());
        }
        lines
    }

    fn auto_cleanup_text(&self) -> Paragraph<'static> {
        let lang = self.lang();
        let now = self.model.as_ref().map(|m| m.taken_at);
        let state = match (self.rules.auto_cleanup_minutes, self.last_auto_cleanup, now) {
            (0, _, _) => t(lang, "automatic cleanup is off", "자동 정리 꺼짐").to_string(),
            (minutes, Some(last), Some(now)) => {
                let next = fmt_duration((last + minutes * 60).saturating_sub(now));
                tf!(lang, "next automatic cleanup in {next}", "다음 자동 정리까지 {next}")
            }
            _ => t(lang, "waiting for the first snapshot", "첫 수집 대기 중").to_string(),
        };
        let last = match (&self.auto_cleanup_result, now) {
            (Some((at, text)), Some(now)) => {
                let ago = fmt_duration(now.saturating_sub(*at));
                tf!(lang, "last run {ago} ago: {text}", "마지막 실행 {ago} 전: {text}")
            }
            _ => t(lang, "last run: -", "마지막 실행: -").to_string(),
        };
        let lines = vec![
            Line::from(state).bold(),
            Line::from(last),
            Line::from(tf!(
                lang,
                "Runs only while the dashboard is open. Skips candidates seen for under \
                 {AUTO_CLEANUP_GRACE_SECS}s, unmarked in Cleanup, or refused (hosts, system, protect rules).",
                "대시보드가 열려 있는 동안만 동작합니다. 발견된 지 {AUTO_CLEANUP_GRACE_SECS}초가 안 된 후보, \
                 정리 화면에서 표시를 해제한 후보, 거부 대상(호스트·시스템·보호 규칙)은 건너뜁니다."
            ))
            .dim(),
        ];
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(t(lang, "Automatic cleanup", "자동 정리")))
    }

    fn draw_confirm(&self, frame: &mut Frame) {
        let lang = self.lang();
        let Some(c) = &self.confirm else { return };
        let area = popup(frame.area(), 72, 18);
        frame.render_widget(Clear, area);
        let mut lines = vec![Line::from(c.title.clone()).bold(), Line::from("")];
        for t in c.plan.targets.iter().take(10) {
            lines.push(Line::from(format!("  {} {}", t.name, t.pid)));
        }
        if c.plan.targets.len() > 10 {
            let more = c.plan.targets.len() - 10;
            lines.push(Line::from(tf!(lang, "  … and {more} more", "  … 외 {more}개")));
        }
        lines.push(Line::from(""));
        let (freed, conns) = (mb(c.plan.private_bytes), c.plan.connections);
        lines.push(Line::from(tf!(
            lang,
            "frees ~{freed} private · closes {conns} connections",
            "전용 메모리 약 {freed} 확보 · 연결 {conns}개 끊김"
        )));
        if c.refusals.is_empty() {
            lines.push(Line::from(t(lang, "y: end   n / esc: cancel", "y: 종료   n / esc: 취소")).fg(Color::Yellow));
        } else {
            for r in &c.refusals {
                let r = phrase(lang, r);
                lines.push(Line::from(tf!(lang, "refused: {r}", "거부: {r}")).fg(Color::Red));
            }
            lines.push(Line::from(t(lang, "esc: close", "esc: 닫기")).fg(Color::Yellow));
        }
        let block = Block::bordered().title(t(lang, "Confirm", "확인"));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
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
            // Each title is padded by a column on both sides and followed by a divider.
            let mut start = self.tabs_area.x;
            for (i, title) in self.view_titles().iter().enumerate() {
                let end = start + Line::from(title.as_str()).width() as u16 + 2;
                if x < end {
                    self.set_view(i);
                    return;
                }
                start = end + 1;
            }
            return;
        }
        if self.view == View::Rules {
            return;
        }
        let body_top = self.table_area.y + 2; // border + header
        if y >= body_top && y < self.table_area.y + self.table_area.height.saturating_sub(1) {
            let v = self.view_index();
            let idx = self.offsets[v] + (y - body_top) as usize;
            if idx < self.row_count() {
                if self.selected[v] == idx {
                    match self.view {
                        View::Sessions => self.toggle_fold(),
                        View::Settings => self.change_setting(1),
                        _ => {}
                    }
                }
                self.selected[v] = idx;
            }
        }
    }

    fn set_view(&mut self, i: usize) {
        if VIEWS[i] == View::Settings && self.view != View::Settings {
            self.read_hooks_registered();
        }
        if VIEWS[i] == View::Network && self.view != View::Network {
            self.ssh_config = net::load_ssh_config();
            self.ssh_keys = keys::load_default(&self.ssh_config);
        }
        self.view = VIEWS[i];
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
            // Only `y` confirms: Enter also opens this dialog, so a double-tap or a repeating
            // Enter would end processes nobody confirmed.
            match k.code {
                KeyCode::Char('y') if c.refusals.is_empty() => self.run_confirmed(),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => self.confirm = None,
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char(d @ '1'..='6') => self.set_view(d as usize - '1' as usize),
            KeyCode::Tab => self.set_view((self.view_index() + 1) % VIEWS.len()),
            KeyCode::BackTab => self.set_view((self.view_index() + VIEWS.len() - 1) % VIEWS.len()),
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
            KeyCode::Char('k') | KeyCode::Char('x') | KeyCode::Delete if self.view == View::Sessions => {
                self.ask_end_selected()
            }
            KeyCode::Char('k') | KeyCode::Char('x') | KeyCode::Delete if self.view == View::Ports => {
                self.ask_end_port()
            }
            KeyCode::Char('a') if self.view == View::Ports => self.all_ports = !self.all_ports,
            KeyCode::Left if self.view == View::Settings => self.change_setting(-1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') if self.view == View::Settings => {
                self.change_setting(1)
            }
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
            self.status = t(self.lang(), "nothing selected", "선택된 항목 없음").into();
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
        let lang = self.lang();
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
                let label = tab.map(|t| t.label.as_str()).unwrap_or("?");
                (
                    tf!(
                        lang,
                        "End every agent session in tab {label}?",
                        "탭 {label}의 에이전트 세션을 모두 종료할까요?"
                    ),
                    roots,
                )
            }
            // Items come from the last Sessions draw; the process may have exited since.
            Item::Session(p) | Item::Proc(p) | Item::Orphan(p) => match m.procs.get(&p) {
                Some(proc) => {
                    let name = &proc.name;
                    (
                        tf!(
                            lang,
                            "End {name} ({p}) and everything under it?",
                            "{name} ({p})와 그 하위 프로세스를 모두 종료할까요?"
                        ),
                        vec![p],
                    )
                }
                None => {
                    self.status = tf!(lang, "process {p} already exited", "프로세스 {p}는 이미 종료됨");
                    return;
                }
            },
            Item::OrphanHeader => (
                t(lang, "End every orphan tree?", "고아 트리를 모두 종료할까요?").into(),
                m.orphans.iter().map(|o| o.pid).collect(),
            ),
            // Like a tab: the agent sessions only, never the host application itself.
            Item::Group(key) => {
                let group = m.groups.iter().find(|g| group_key(g) == key);
                let roots: Vec<u32> = group
                    .iter()
                    .flat_map(|g| &g.tabs)
                    .flat_map(|t| &t.sessions)
                    .map(|s| s.root_pid)
                    .collect();
                let name = group.map(|g| g.name.as_str()).unwrap_or("?");
                (
                    tf!(
                        lang,
                        "End every agent session in {name}?",
                        "{name}의 에이전트 세션을 모두 종료할까요?"
                    ),
                    roots,
                )
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
        let title = tf!(
            self.lang(),
            "End {n} cleanup target(s) and their children?",
            "정리 대상 {n}개와 하위 프로세스를 종료할까요?"
        );
        self.ask(title, roots);
    }

    /// Ends the whole orphan tree holding the port, or else the listening process and its
    /// children: never the agent session around it.
    fn ask_end_port(&mut self) {
        let lang = self.lang();
        let Some(p) = self.visible_ports().get(self.selected[3]).map(|p| (*p).clone()) else {
            return;
        };
        let Some(m) = self.model.as_ref() else { return };
        let port = p.port;
        let (title, root) = match p.owner {
            PortOwner::Orphan(root) => {
                let name = m.procs.get(&root).map(|r| r.name.clone()).unwrap_or_default();
                (
                    tf!(
                        lang,
                        "End the orphan tree {name} ({root}) holding :{port}?",
                        ":{port} 포트를 연 고아 트리 {name} ({root})를 종료할까요?"
                    ),
                    root,
                )
            }
            _ => {
                let (name, pid) = (&p.process, p.pid);
                (
                    tf!(
                        lang,
                        "End {name} ({pid}) listening on :{port}, and everything under it?",
                        ":{port} 포트를 연 {name} ({pid})와 그 하위 프로세스를 종료할까요?"
                    ),
                    pid,
                )
            }
        };
        self.ask(title, vec![root]);
    }

    fn ask_disconnect(&mut self) {
        let Some(c) = self.ssh.get(self.selected[2]) else {
            return;
        };
        let title = tf!(
            self.lang(),
            "Disconnect {}:{} by ending {} ({})?",
            "{}:{} 연결을 끊기 위해 {} ({})를 종료할까요?",
            c.host,
            c.port,
            c.process,
            c.pid
        );
        let pid = c.pid;
        let Some(m) = self.model.as_ref() else { return };
        let plan = actions::disconnect_plan(m, &self.catalog, pid);
        self.ask_plan(title, plan);
    }

    fn run_confirmed(&mut self) {
        let Some(c) = self.confirm.take() else { return };
        self.status = self.summarize(&actions::execute(&c.plan));
        self.refresh_now = true;
    }

    fn summarize(&self, results: &[(actions::Target, Outcome)]) -> String {
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
        let n = failed.len();
        let mut text = tf!(
            self.lang(),
            "ended {killed}, already gone {gone}, skipped (pid reused) {replaced}, failed {n}",
            "종료 {killed}, 이미 종료됨 {gone}, 건너뜀(PID 재사용) {replaced}, 실패 {n}"
        );
        if let Some(first) = failed.first() {
            text.push_str(&format!(" — {first}"));
        }
        text
    }

    /// Candidate roots automatic cleanup would end now, and how many it leaves for refusals.
    fn auto_cleanup_roots(&self, now: u64) -> (Vec<u32>, usize) {
        let Some(m) = self.model.as_ref() else {
            return (Vec::new(), 0);
        };
        let mut refused = 0;
        let roots = self
            .candidates
            .iter()
            .filter(|c| {
                (c.eligible && self.marked.contains(&c.root_pid))
                    || (self.rules.auto_cleanup_idle && c.kind == CandidateKind::IdleSession)
            })
            .filter(|c| {
                self.seen_candidates
                    .get(&c.root_pid)
                    .is_some_and(|seen| now.saturating_sub(*seen) >= AUTO_CLEANUP_GRACE_SECS)
            })
            .map(|c| c.root_pid)
            .filter(|root| {
                let plan = actions::plan(m, &[*root]);
                let allowed = actions::refusals(&plan, m, &self.catalog, self.self_pid).is_empty();
                refused += usize::from(!allowed);
                allowed
            })
            .collect();
        (roots, refused)
    }

    /// Runs automatic cleanup when it is due. The interval counts from when it was switched on
    /// (or the dashboard started), so turning it on never ends anything right away.
    fn auto_cleanup(&mut self) {
        let minutes = self.rules.auto_cleanup_minutes;
        let Some(now) = self.model.as_ref().map(|m| m.taken_at) else {
            return;
        };
        if minutes == 0 {
            self.last_auto_cleanup = None;
            return;
        }
        let last = *self.last_auto_cleanup.get_or_insert(now);
        if now < last + minutes * 60 {
            return;
        }
        self.last_auto_cleanup = Some(now);
        let lang = self.lang();
        let (roots, refused) = self.auto_cleanup_roots(now);
        let Some(m) = self.model.as_ref() else { return };
        let result = if roots.is_empty() {
            tf!(
                lang,
                "nothing to end, {refused} refused",
                "종료할 항목 없음, 거부 {refused}"
            )
        } else {
            let plan = actions::plan(m, &roots);
            let (trees, freed) = (roots.len(), mb(plan.private_bytes));
            let summary = self.summarize(&actions::execute(&plan));
            self.refresh_now = true;
            let result = tf!(
                lang,
                "{trees} tree(s), ~{freed}, {refused} refused: {summary}",
                "트리 {trees}개, 약 {freed}, 거부 {refused}: {summary}"
            );
            self.status = tf!(lang, "automatic cleanup: {result}", "자동 정리: {result}");
            result
        };
        self.auto_cleanup_result = Some((now, result));
    }

    /// Changes the selected setting, applies it and saves it to the rules file.
    fn change_setting(&mut self, step: isize) {
        let Some(setting) = self.settings().get(self.selected[View::Settings as usize]).copied() else {
            return;
        };
        if let Setting::Hooks(i) = setting {
            self.toggle_hooks(i);
            return;
        }
        let r = &mut self.rules;
        let (key, value) = match setting {
            Setting::Language => {
                r.language = match r.language {
                    Language::En => Language::Ko,
                    Language::Ko => Language::En,
                };
                let code = match r.language {
                    Language::En => "en",
                    Language::Ko => "ko",
                };
                ("language", format!("\"{code}\""))
            }
            Setting::AutoCleanup => {
                r.auto_cleanup_minutes = cycle(&AUTO_CLEANUP_MINUTES, r.auto_cleanup_minutes, step);
                self.last_auto_cleanup = None;
                ("auto_cleanup_minutes", r.auto_cleanup_minutes.to_string())
            }
            Setting::AutoCleanupIdle => {
                r.auto_cleanup_idle = !r.auto_cleanup_idle;
                ("auto_cleanup_idle", r.auto_cleanup_idle.to_string())
            }
            Setting::IdleMinutes => {
                r.idle_minutes = cycle(&IDLE_MINUTES, r.idle_minutes, step);
                ("idle_minutes", r.idle_minutes.to_string())
            }
            Setting::EndWindowed => {
                r.end_windowed = !r.end_windowed;
                ("end_windowed", r.end_windowed.to_string())
            }
            Setting::EndShared => {
                r.end_shared = !r.end_shared;
                ("end_shared", r.end_shared.to_string())
            }
            Setting::Hooks(_) => unreachable!("handled above"),
        };
        // Starts the interval now, from the current snapshot.
        self.auto_cleanup();
        let lang = self.lang();
        self.status = match Rules::save_setting(key, &value) {
            Ok(path) => {
                let path = path.display();
                tf!(
                    lang,
                    "saved {key} = {value} to {path}",
                    "{key} = {value} 저장됨: {path}"
                )
            }
            Err(e) => tf!(
                lang,
                "applied for this run only, not saved: {e}",
                "이번 실행에만 적용됨, 저장 실패: {e}"
            ),
        };
    }

    /// Adds or removes agentlens in the agent's own hook settings.
    fn toggle_hooks(&mut self, agent: usize) {
        let lang = self.lang();
        let a = &self.catalog.agents[agent];
        let Some(def) = a.hooks.clone() else { return };
        let id = a.id.clone();
        let on = !matches!(self.hooks_registered.get(&id), Some(Ok(true)));
        let result = std::env::current_exe()
            .map_err(|e| e.to_string())
            .and_then(|exe| hooks::set_registered(&def, &id, &exe, on));
        self.status = match result {
            Ok(path) if on => tf!(
                lang,
                "registered agentlens in {} (backup next to it); new sessions report their state",
                "{}에 agentlens를 등록함(옆에 백업); 새 세션부터 상태를 알려 줌",
                path.display()
            ),
            Ok(path) => tf!(
                lang,
                "removed agentlens from {}",
                "{}에서 agentlens를 뺌",
                path.display()
            ),
            Err(e) => tf!(lang, "hooks not changed: {e}", "훅을 바꾸지 못함: {e}"),
        };
        self.read_hooks_registered();
    }

    fn register_host(&mut self) {
        let Some(Item::Group(key)) = self.items.get(self.selected[0]).cloned() else {
            self.status = t(
                self.lang(),
                "select an unregistered host group to register it",
                "등록할 미등록 호스트 그룹을 선택하세요",
            )
            .into();
            return;
        };
        let Some(exe) = key.strip_prefix('?') else {
            self.status = t(
                self.lang(),
                "this group is already a known host",
                "이미 등록된 호스트입니다",
            )
            .into();
            return;
        };
        // Appended to the file, then everything reloaded: edits made in the editor since the
        // last reload are kept and take effect too.
        match Rules::register_host(exe) {
            Ok(path) => {
                self.reload_rules();
                let path = path.display();
                self.status = tf!(
                    self.lang(),
                    "registered {exe} as a host in {path}",
                    "{exe}를 호스트로 등록함: {path}"
                );
            }
            Err(e) => self.status = tf!(self.lang(), "could not register host: {e}", "호스트 등록 실패: {e}"),
        }
    }

    fn edit_rules(&mut self) {
        let Some(path) = rules_path() else { return };
        if !path.exists()
            && let Err(e) = self.rules.save()
        {
            self.status = tf!(
                self.lang(),
                "could not create rules file: {e}",
                "규칙 파일을 만들지 못함: {e}"
            );
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
            Ok(_) => {
                let path = path.display();
                self.status = tf!(
                    self.lang(),
                    "editing {path}; press r to reload after saving",
                    "{path} 편집 중, 저장한 뒤 r로 다시 읽으세요"
                )
            }
            Err(e) => self.status = tf!(self.lang(), "could not open editor: {e}", "편집기를 열지 못함: {e}"),
        }
    }

    fn reload_rules(&mut self) {
        match Rules::load() {
            Ok(rules) => {
                self.rules = rules;
                self.rebuild_catalog();
                self.status = t(self.lang(), "rules reloaded", "규칙을 다시 읽음").into();
            }
            Err(e) => self.status = tf!(self.lang(), "rules not reloaded: {e}", "규칙을 다시 읽지 못함: {e}"),
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
                Cell::from(""),
                Cell::from(short_cmd(p)),
            ])
            .style(style),
        );
        items.push(Item::Proc(pid));
    }
}

fn group_key(g: &Group) -> String {
    match &g.kind {
        GroupKind::Known(id) => id.clone(),
        GroupKind::Unregistered(s) => format!("?{s}"),
        GroupKind::Standalone => "standalone".into(),
    }
}

/// The choice `step` places after `current`, wrapping around; a value that is not among the
/// choices (typed into the rules file) moves to the first one.
fn cycle(choices: &[u64], current: u64, step: isize) -> u64 {
    match choices.iter().position(|c| *c == current) {
        Some(i) => choices[(i as isize + step).rem_euclid(choices.len() as isize) as usize],
        None => choices[0],
    }
}

fn state_cell(lang: Language, state: SessionState) -> Cell<'static> {
    match state {
        SessionState::Working => Cell::from(t(lang, "working", "작업 중")).green(),
        SessionState::Quiet => Cell::from(t(lang, "quiet", "조용함")),
        SessionState::Idle => Cell::from(t(lang, "idle", "유휴")).dim(),
    }
}

fn hook_cell(lang: Language, state: HookState) -> Cell<'static> {
    match state {
        HookState::Working => Cell::from(t(lang, "working", "작업 중")).green(),
        HookState::Waiting => Cell::from(t(lang, "waiting", "입력 대기")).cyan(),
        HookState::Attention => Cell::from(t(lang, "needs you", "확인 필요")).yellow().bold(),
    }
}

fn end_or_keep(lang: Language, end: bool) -> String {
    if end {
        t(lang, "end too", "같이 종료")
    } else {
        t(lang, "keep", "남겨 둠")
    }
    .to_string()
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
        assert!(screen(&mut app).contains("no listening ports from agents or orphans"));
        key(&mut app, KeyCode::Char('5'));
        let s = screen(&mut app);
        assert!(s.contains("agents in catalog: 45"), "{s}");
        assert!(s.contains("How orphans are found"));
        key(&mut app, KeyCode::Char('6'));
        let s = screen(&mut app);
        assert!(s.contains("automatic cleanup is off"), "{s}");
    }

    fn korean_app() -> App {
        let snapshot: Snapshot =
            serde_json::from_str(include_str!("../../core/tests/fixtures/orca_2026-10-29.json")).unwrap();
        let rules = Rules {
            language: Language::Ko,
            ..Rules::default()
        };
        let mut app = App::new(rules, Vec::new());
        app.apply(snapshot);
        app
    }

    #[test]
    fn renders_korean_and_tab_clicks_follow_wide_titles() {
        let mut app = korean_app();
        // Wide characters span two cells, so check characters rather than the joined text.
        let s = screen(&mut app);
        assert!(s.contains('고') && s.contains('아') && s.contains('세'), "{s}");
        assert!(!s.contains("No parent"));

        let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        let x = (0..buf.area.width).find(|x| buf[(*x, 0)].symbol() == "6").unwrap();
        let click = |x| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.on_event(click(x + 3));
        assert!(app.view == View::Settings);
        let x = (0..buf.area.width).find(|x| buf[(*x, 0)].symbol() == "5").unwrap();
        app.on_event(click(x));
        assert!(app.view == View::Rules);
        let s = screen(&mut app);
        assert!(s.contains('판') && s.contains('정'), "{s}");
    }

    #[test]
    fn ports_name_their_owner_and_ending_one_spares_the_session() {
        use agentlens_core::Listener;
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../core/tests/fixtures/orca_2026-10-29.json")).unwrap();
        let mut app = App::new(Rules::default(), Vec::new());
        app.apply(snapshot.clone());
        let m = app.model.as_ref().unwrap();
        let orphan = m.orphans[0].pid;
        let orphan_child = *m.subtree(orphan).last().unwrap();
        let server = m.subtree(33472)[1];
        let system = m
            .procs
            .keys()
            .copied()
            .find(|p| {
                m.subtree(*p).len() == 1
                    && m.locate(*p).is_none()
                    && !m.orphans.iter().any(|o| m.is_ancestor_or_self(o.pid, *p))
            })
            .unwrap();
        let listen = |pid, addr: &str, port| Listener {
            pid,
            addr: addr.into(),
            port,
        };
        snapshot.listening = vec![
            listen(orphan_child, "0.0.0.0", 3050),
            listen(orphan_child, "::", 3050),
            listen(server, "127.0.0.1", 3150),
            listen(33472, "127.0.0.1", 50001),
            listen(system, "0.0.0.0", 135),
        ];
        app.apply(snapshot);

        let ports: Vec<(u16, bool)> = app.visible_ports().iter().map(|p| (p.port, p.exposed)).collect();
        assert_eq!(
            ports,
            [(3050, true), (3150, false), (50001, false)],
            "orphans first, system hidden"
        );
        assert_eq!(app.visible_ports()[0].addrs, ["0.0.0.0", "::"], "one row per port");
        assert!(app.visible_ports()[2].agent_internal);
        let reasons = &app.candidates.iter().find(|c| c.root_pid == orphan).unwrap().reasons;
        assert!(reasons.contains(&"listening :3050".to_string()) && reasons.contains(&"exposed".to_string()));

        app.view = View::Ports;
        let s = screen(&mut app);
        assert!(
            s.contains("3050") && s.contains("exposed (0.0.0.0, ::)") && s.contains("orphan"),
            "{s}"
        );
        key(&mut app, KeyCode::Char('a'));
        assert!(app.visible_ports().iter().any(|p| p.port == 135));
        key(&mut app, KeyCode::Char('a'));

        // The orphan: its whole tree. A session's server: that process, not the session.
        key(&mut app, KeyCode::Char('x'));
        assert_eq!(app.confirm.take().unwrap().plan.targets[0].pid, orphan);
        app.selected[3] = 1;
        key(&mut app, KeyCode::Char('x'));
        let plan = app.confirm.take().unwrap().plan;
        assert_eq!(plan.targets[0].pid, server);
        assert!(plan.targets.iter().all(|t| t.pid != 33472));
    }

    #[test]
    fn auto_cleanup_waits_for_the_grace_period_and_honors_unmarking() {
        let mut app = app();
        app.rules.auto_cleanup_minutes = 5;
        let now = app.model.as_ref().unwrap().taken_at;
        // The fixture's orphans are programs agents started, shown but not proposed: propose
        // one as if it held agent machinery.
        assert!(app.candidates.len() > 1);
        assert!(app.candidates.iter().all(|c| !c.eligible));
        let proposed = app.candidates[0].root_pid;
        app.candidates[0].eligible = true;
        app.marked.insert(proposed);
        assert_eq!(app.auto_cleanup_roots(now).0, Vec::<u32>::new(), "just seen");
        assert_eq!(
            app.auto_cleanup_roots(now + AUTO_CLEANUP_GRACE_SECS).0,
            [proposed],
            "proposed only, not informational candidates"
        );
        app.marked.remove(&proposed);
        assert_eq!(
            app.auto_cleanup_roots(now + AUTO_CLEANUP_GRACE_SECS).0,
            Vec::<u32>::new()
        );
    }

    #[test]
    fn auto_cleanup_interval_starts_when_switched_on() {
        let mut app = app();
        app.rules.auto_cleanup_minutes = 5;
        app.auto_cleanup();
        let now = app.model.as_ref().unwrap().taken_at;
        assert_eq!(app.last_auto_cleanup, Some(now));
        assert!(app.auto_cleanup_result.is_none(), "nothing runs when switched on");
        app.rules.auto_cleanup_minutes = 0;
        app.auto_cleanup();
        assert_eq!(app.last_auto_cleanup, None);
    }

    #[test]
    fn settings_cycle_through_their_choices() {
        assert_eq!(cycle(&AUTO_CLEANUP_MINUTES, 0, 1), 5);
        assert_eq!(cycle(&AUTO_CLEANUP_MINUTES, 0, -1), 60);
        assert_eq!(cycle(&AUTO_CLEANUP_MINUTES, 60, 1), 0);
        assert_eq!(
            cycle(&IDLE_MINUTES, 45, 1),
            10,
            "a hand-typed value moves to the first choice"
        );
    }

    #[test]
    fn ending_a_host_group_targets_its_sessions_only() {
        let mut app = app();
        screen(&mut app);
        let group = app.items.iter().position(|i| matches!(i, Item::Group(_))).unwrap();
        app.selected[0] = group;
        key(&mut app, KeyCode::Char('x'));
        let m = app.model.as_ref().unwrap();
        let Item::Group(gkey) = &app.items[group] else {
            unreachable!()
        };
        let g = m.groups.iter().find(|g| group_key(g) == *gkey).unwrap();
        let confirm = app.confirm.as_ref().unwrap();
        let sessions: Vec<u32> = g.tabs.iter().flat_map(|t| &t.sessions).map(|s| s.root_pid).collect();
        assert!(
            sessions
                .iter()
                .all(|s| confirm.plan.targets.iter().any(|t| t.pid == *s))
        );
        assert!(
            confirm.plan.targets.iter().all(|t| !g.host_pids.contains(&t.pid)),
            "the host application stays"
        );
        assert!(confirm.refusals.is_empty(), "{:?}", confirm.refusals);
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
    fn enter_opens_the_cleanup_dialog_but_never_confirms_it() {
        let mut app = app();
        key(&mut app, KeyCode::Char('2'));
        screen(&mut app);
        assert!(!app.candidates.is_empty());
        // A double-tap, or Enter repeating while held.
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        assert!(app.confirm.is_some(), "still waiting for y");
        key(&mut app, KeyCode::Esc);
        assert!(app.confirm.is_none());
    }

    #[test]
    fn cleanup_cursor_follows_its_row_across_refreshes() {
        let fixture = || -> Snapshot {
            serde_json::from_str(include_str!("../../core/tests/fixtures/orca_2026-10-29.json")).unwrap()
        };
        let mut app = App::new(Rules::default(), Vec::new());
        app.apply(fixture());
        app.selected[1] = app.candidates.len() - 1;
        let picked = app.candidates[app.selected[1]].root_pid;
        // A new orphan with a lower PID is listed before the picked row.
        let mut next = fixture();
        let mut stray = next.processes[0].clone();
        (stray.pid, stray.ppid, stray.name) = (4, Some(3_999_999), "node.exe".into());
        stray.cmd = vec!["node".into(), "server.js".into()];
        next.processes.push(stray);
        app.apply(next);
        assert!(app.candidates.iter().any(|c| c.root_pid == 4));
        assert_eq!(app.candidates[app.selected[1]].root_pid, picked);
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
    #[test]
    fn leftovers_proposed_after_the_grace_are_marked_then() {
        let fixture = || -> Snapshot {
            serde_json::from_str(include_str!("../../core/tests/fixtures/orca_2026-10-29.json")).unwrap()
        };
        // A native MCP server under a Claude Code session; the snapshot alone never lists it.
        let mut live = fixture();
        let claude = live.processes.iter().find(|p| p.pid == 33472).unwrap().clone();
        let mut server = claude.clone();
        (server.pid, server.ppid, server.name) = (4_100_000, Some(33472), "github-server.exe".into());
        (server.exe, server.cmd) = (Some(r"C:\tools\github-server.exe".into()), vec!["github-server".into()]);
        server.start_time += 1;
        // And a dev server: listed as an orphan at once, proposed once its owner is known gone.
        let mut shell = server.clone();
        (shell.pid, shell.name) = (4_100_001, "bash.exe".into());
        (shell.exe, shell.cmd) = (Some(r"C:\Git\bin\bash.exe".into()), vec!["bash".into()]);
        let mut dev = server.clone();
        (dev.pid, dev.ppid, dev.name) = (4_100_002, Some(4_100_001), "node.exe".into());
        (dev.exe, dev.cmd) = (
            Some(r"C:\nodejs\node.exe".into()),
            vec!["node".into(), "dev.mjs".into()],
        );
        live.processes.extend([server, shell, dev]);
        let mut app = App::new(Rules::default(), Vec::new());
        app.apply(live.clone());

        // The session is killed; its server stays.
        let ended = |after: u64| {
            let mut s = live.clone();
            s.processes.retain(|p| p.pid != 33472);
            s.taken_at += after;
            s
        };
        app.apply(ended(5));
        assert!(
            !app.candidates.iter().any(|c| c.root_pid == 4_100_000),
            "within the grace"
        );
        // Orphans the snapshot lists at once but proposes only once their owner is known gone.
        let listed_early: Vec<u32> = app
            .candidates
            .iter()
            .filter(|c| !c.eligible)
            .map(|c| c.root_pid)
            .collect();
        app.apply(ended(5 + agentlens_core::origin::OWNER_GONE_GRACE_SECS));
        let c = app
            .candidates
            .iter()
            .find(|c| c.root_pid == 4_100_000)
            .expect("proposed");
        assert!(c.eligible && app.marked.contains(&4_100_000), "{:?}", c.reasons);
        assert!(app.status.contains("github-server"), "{}", app.status);
        assert!(listed_early.contains(&4_100_001), "{listed_early:?}");
        let shell = app.candidates.iter().find(|c| c.root_pid == 4_100_001).unwrap();
        assert!(shell.eligible && app.marked.contains(&4_100_001), "{:?}", shell.reasons);
    }

    #[test]
    fn sessions_show_their_state() {
        let mut app = app();
        let s = screen(&mut app);
        // The recorded snapshot has sessions using CPU and sessions doing nothing.
        assert!(
            s.contains("State") && s.contains("working") && s.contains("quiet"),
            "{s}"
        );
        let line = s.lines().find(|l| l.contains("36608")).unwrap();
        assert!(line.contains("quiet"), "0% CPU: {line}");
    }

    #[test]
    fn hook_states_win_over_cpu_and_hooks_are_a_setting() {
        let mut app = app();
        let start = app.model.as_ref().unwrap().procs[&36608].start_time;
        app.hook_states.insert((36608, start), HookState::Attention);
        let line = screen(&mut app)
            .lines()
            .find(|l| l.contains("36608"))
            .unwrap()
            .to_string();
        assert!(line.contains("needs you"), "{line}");

        // Only shown here: toggling would change the real agent settings.
        app.view = View::Settings;
        let s = screen(&mut app);
        assert!(s.contains("Agent hooks: Claude Code"), "{s}");
        assert!(app.settings().contains(&Setting::Hooks(0)));
    }
}
