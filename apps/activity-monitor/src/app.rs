//! FUIDE Activity Monitor — the egui application.
//!
//! Activity Monitor's shape: five tabs (CPU / MEMORY / ENERGY / DISK / NETWORK), a process table
//! whose columns follow the tab, a summary strip for the tab's machine-wide numbers, plus the
//! FUIDE furniture: a detail panel for the selected process, an event log, the settings window
//! and the MCP agent. Data arrives from a [`Sampler`] thread as whole [`Snapshot`]s; the UI
//! never blocks on the OS.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use egui::{pos2, vec2, Align2, Color32, Key, Rect, RichText, Sense, Stroke, Ui};
use fuide::table::{self, Cell, Column, TableState, Width};
use fuide::widgets::{self, LogLine};
use fuide::{
    mono, palette, theme, type_scale, Dialog, PaletteKind, Panel, Settings, SettingsWindow, Shell,
};

use crate::sys::{
    self, Access, CoreKind, KillOutcome, Pressure, Proc, Sampler, Snapshot, Source, State,
};

const APP_ID: &str = "activity-monitor";
const RIGHT_W: f32 = 300.0;
const GAP: f32 = 14.0;
const TABS_H: f32 = 30.0;
const TOOLBAR_H: f32 = 32.0;
const SUMMARY_H: f32 = 196.0;
const LOG_H: f32 = 110.0;
const LOG_MIN: f32 = 60.0;
const LOG_CLOSED: f32 = 26.0;
/// Space kept for the tabs, table and summary when the log divider is dragged up.
const BODY_MIN: f32 = 460.0;
/// Samples kept for the graphs.
const HISTORY: usize = 180;
const INTERVALS: [f32; 3] = [1.0, 2.0, 5.0];
const DEFAULT_INTERVAL: usize = 1;
const FILTER_W: f32 = 200.0;

// ---- domain types ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Cpu,
    Memory,
    Energy,
    Disk,
    Network,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Cpu, Tab::Memory, Tab::Energy, Tab::Disk, Tab::Network];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Cpu => "CPU",
            Tab::Memory => "MEMORY",
            Tab::Energy => "ENERGY",
            Tab::Disk => "DISK",
            Tab::Network => "NETWORK",
        }
    }

    /// Table columns for this tab. Column 0 is always the name (rows are addressed by it),
    /// column 1 the tab's key metric (the default sort, descending).
    fn columns(self) -> Vec<Column> {
        let name = Column::new("PROCESS", Width::Flex);
        let pid = Column::new("PID", Width::Chars(6.5)).right();
        let user = Column::new("USER", Width::Chars(12.0));
        match self {
            Tab::Cpu => vec![
                name,
                Column::new("% CPU", Width::Chars(9.0)).right(),
                Column::new("CPU TIME", Width::Chars(14.0)).right(),
                Column::new("THREADS", Width::Chars(12.0)).right(),
                Column::new("WAKEUPS", Width::Chars(12.0)).right(),
                Column::new("STATE", Width::Chars(9.0)),
                pid,
                user,
            ],
            Tab::Memory => vec![
                name,
                Column::new("MEMORY", Width::Chars(11.0)).right(),
                Column::new("RESIDENT", Width::Chars(14.0)).right(),
                Column::new("THREADS", Width::Chars(12.0)).right(),
                pid,
                user,
            ],
            Tab::Energy => vec![
                name,
                Column::new("ENERGY", Width::Chars(11.0)).right(),
                Column::new("% CPU", Width::Chars(9.0)).right(),
                Column::new("WAKEUPS", Width::Chars(12.0)).right(),
                Column::new("NO SLEEP", Width::Chars(14.0)),
                pid,
                user,
            ],
            Tab::Disk => vec![
                name,
                Column::new("READ/S", Width::Chars(11.0)).right(),
                Column::new("WRITE/S", Width::Chars(12.0)).right(),
                Column::new("TOTAL READ", Width::Chars(17.0)).right(),
                Column::new("TOTAL WRITE", Width::Chars(19.0)).right(),
                pid,
                user,
            ],
            Tab::Network => vec![
                name,
                Column::new("IN/S", Width::Chars(10.0)).right(),
                Column::new("OUT/S", Width::Chars(10.0)).right(),
                Column::new("TOTAL IN", Width::Chars(14.0)).right(),
                Column::new("TOTAL OUT", Width::Chars(15.0)).right(),
                pid,
                user,
            ],
        }
    }
}

/// What a column sorts by.
enum SortKey {
    Num(f64),
    Text(String),
}

fn sort_key(p: &Proc, tab: Tab, col: usize) -> SortKey {
    use SortKey::*;
    let n = |v: f64| Num(v);
    match (tab, col) {
        (_, 0) => Text(p.name.to_lowercase()),
        (Tab::Cpu, 1) => n(p.cpu as f64),
        (Tab::Cpu, 2) => n(p.cpu_time),
        (Tab::Cpu, 3) => n(p.threads as f64),
        (Tab::Cpu, 4) => n(p.wakeups as f64),
        (Tab::Cpu, 5) => Text(p.state.label().into()),
        (Tab::Cpu, 6) => n(p.pid as f64),
        (Tab::Cpu, _) => Text(p.user.clone()),
        (Tab::Memory, 1) => n(p.mem as f64),
        (Tab::Memory, 2) => n(p.rss as f64),
        (Tab::Memory, 3) => n(p.threads as f64),
        (Tab::Memory, 4) => n(p.pid as f64),
        (Tab::Memory, _) => Text(p.user.clone()),
        (Tab::Energy, 1) => n(p.energy as f64),
        (Tab::Energy, 2) => n(p.cpu as f64),
        (Tab::Energy, 3) => n(p.wakeups as f64),
        (Tab::Energy, 4) => n(p.prevents_sleep as u8 as f64),
        (Tab::Energy, 5) => n(p.pid as f64),
        (Tab::Energy, _) => Text(p.user.clone()),
        (Tab::Disk, 1) => n(p.disk_read_bps),
        (Tab::Disk, 2) => n(p.disk_write_bps),
        (Tab::Disk, 3) => n(p.disk_read as f64),
        (Tab::Disk, 4) => n(p.disk_write as f64),
        (Tab::Disk, 5) => n(p.pid as f64),
        (Tab::Disk, _) => Text(p.user.clone()),
        (Tab::Network, 1) => n(p.net_in_bps),
        (Tab::Network, 2) => n(p.net_out_bps),
        (Tab::Network, 3) => n(p.net_in as f64),
        (Tab::Network, 4) => n(p.net_out as f64),
        (Tab::Network, 5) => n(p.pid as f64),
        (Tab::Network, _) => Text(p.user.clone()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Level {
    Info,
    Ok,
    Warn,
    Danger,
}

#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub(crate) time: String,
    pub(crate) text: String,
    level: Level,
}

/// A fixed-length series for the graphs.
#[derive(Clone, Debug, Default)]
pub(crate) struct Series(VecDeque<f32>);

impl Series {
    fn push(&mut self, v: f32) {
        if self.0.len() == HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(v);
    }
    fn last(&self) -> f32 {
        self.0.back().copied().unwrap_or(0.0)
    }
    fn max(&self) -> f32 {
        self.0.iter().copied().fold(0.0, f32::max)
    }
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct History {
    pub(crate) cpu: Series,
    pub(crate) system: Series,
    pub(crate) mem: Series,
    pub(crate) energy: Series,
    pub(crate) disk_read: Series,
    pub(crate) disk_write: Series,
    pub(crate) net_in: Series,
    pub(crate) net_out: Series,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Tab(Tab),
    /// Row index into the current view.
    Select(Option<usize>),
    Interval(usize),
    ToggleMine,
    ClearFilter,
    /// Open the confirmation for the selected process (`force` = SIGKILL).
    RequestKill {
        force: bool,
    },
    ConfirmDialog,
    CloseDialog,
    Palette(PaletteKind),
    OpenSettings,
    /// Cmd+R: sample now.
    Refresh,
}

struct Confirm {
    pid: i32,
    name: String,
    user: String,
    force: bool,
    /// Another user's process: the signal will be refused; the dialog says so up front.
    foreign: bool,
}

enum DialogState {
    Confirm(Confirm),
    Error(String),
}

struct OpenDialog {
    state: DialogState,
    closing: bool,
}

// ---- the app --------------------------------------------------------------------------------

pub struct MonitorApp {
    sampler: Sampler,
    pub(crate) snap: Option<Snapshot>,
    pub(crate) tab: Tab,
    /// Indices into `snap.procs`, filtered and sorted.
    pub(crate) view: Vec<usize>,
    pub(crate) table: TableState,
    pub(crate) filter: String,
    pub(crate) mine_only: bool,
    /// Selection survives re-sorting by pid.
    pub(crate) selected_pid: Option<i32>,
    pub(crate) interval_idx: usize,
    dirty: bool,
    pub(crate) hist: History,
    pub(crate) samples: u32,
    last_pressure: Pressure,
    pub(crate) log: Vec<Event>,
    dialog: Option<OpenDialog>,
    error_queue: VecDeque<String>,
    settings: Settings,
    settings_win: SettingsWindow,
    settings_path: Option<PathBuf>,
    log_h: f32,
    devshot: fuide::devshot::DevShot,
    agent: fuide::Agent,
}

impl MonitorApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = Settings::load(APP_ID).unwrap_or_else(|| Settings::new(PaletteKind::Cyan));
        let mut app = Self::with_context(&cc.egui_ctx, settings, sys::native());
        if let Some(path) = Settings::path(APP_ID) {
            app.persist_settings_to(path);
        }
        app
    }

    pub fn persist_settings_to(&mut self, path: PathBuf) {
        self.settings_path = Some(path);
    }

    pub fn with_context(ctx: &egui::Context, settings: Settings, source: Box<dyn Source>) -> Self {
        theme::install(ctx, settings.palette.palette(), theme::macos_cjk_fallback());
        settings.apply(ctx);
        let wake_ctx = ctx.clone();
        let sampler = Sampler::spawn(
            source,
            Duration::from_secs_f32(INTERVALS[DEFAULT_INTERVAL]),
            move || wake_ctx.request_repaint(),
        );
        let mut app = Self {
            sampler,
            snap: None,
            tab: Tab::Cpu,
            view: Vec::new(),
            table: TableState {
                sort_col: 1,
                sort_desc: true,
                ..Default::default()
            },
            filter: String::new(),
            mine_only: false,
            selected_pid: None,
            interval_idx: DEFAULT_INTERVAL,
            dirty: false,
            hist: History::default(),
            samples: 0,
            last_pressure: Pressure::Normal,
            log: Vec::new(),
            dialog: None,
            error_queue: VecDeque::new(),
            log_h: settings.log_height.unwrap_or(LOG_H),
            settings,
            settings_win: SettingsWindow::default(),
            settings_path: None,
            devshot: fuide::devshot::DevShot::from_env(),
            agent: fuide::Agent::new(APP_ID, "FUIDE Activity Monitor"),
        };
        app.push_log(0.0, "monitor online :: sampling every 2 s", Level::Ok);
        app.agent.set_enabled(ctx, app.settings.agent);
        if app.settings.agent {
            app.push_log(
                0.0,
                "agent // interface on :: waiting for a client",
                Level::Warn,
            );
        }
        if std::env::var_os("FUIDE_DEV_SETTINGS").is_some() {
            app.settings_win.open();
        }
        if let Ok(text) = std::env::var("FUIDE_DEV_LOG") {
            app.push_log(0.0, text, Level::Danger);
        }
        app
    }

    fn push_log(&mut self, t: f64, text: impl Into<String>, level: Level) {
        self.log.push(Event {
            time: format!("[{}]", fuide::fmt::uptime(t)),
            text: text.into(),
            level,
        });
        if self.log.len() > 300 {
            self.log.drain(..100);
        }
    }

    fn fail(&mut self, t: f64, line: impl Into<String>, detail: &str) {
        let line = line.into();
        self.push_log(t, format!("{line} :: failed :: {detail}"), Level::Danger);
        self.error_queue.push_back(line);
    }

    fn save_settings(&mut self, t: f64) {
        if let Some(path) = self.settings_path.clone() {
            if let Err(e) = self.settings.save_to(&path) {
                self.push_log(t, format!("settings // save failed: {e}"), Level::Danger);
            }
        }
    }

    fn settings_changed(&mut self, t: f64) {
        let s = &self.settings;
        self.push_log(
            t,
            format!(
                "settings // palette {} :: {} :: {} :: agent {}{}",
                s.palette.name(),
                if s.chamfer { "chamfer" } else { "square" },
                if s.compact { "compact" } else { "normal" },
                if s.agent { "on" } else { "off" },
                if s.agent && s.agent_confirm {
                    " (may confirm)"
                } else {
                    ""
                },
            ),
            Level::Warn,
        );
        self.save_settings(t);
    }

    /// A new snapshot: graphs, log-worthy changes, then the table view.
    pub(crate) fn ingest(&mut self, snap: Snapshot, t: f64) {
        self.samples += 1;
        if snap.interval > 0.0 {
            let h = &mut self.hist;
            h.cpu.push(snap.cpu.user + snap.cpu.system);
            h.system.push(snap.cpu.system);
            h.mem
                .push(snap.mem.used as f32 / snap.mem.total.max(1) as f32);
            h.energy.push(snap.procs.iter().map(|p| p.energy).sum());
            h.disk_read.push(snap.disk.read_bps as f32);
            h.disk_write.push(snap.disk.write_bps as f32);
            h.net_in.push(snap.net.in_bps as f32);
            h.net_out.push(snap.net.out_bps as f32);
        }
        if snap.mem.pressure != self.last_pressure {
            let (text, level) = match snap.mem.pressure {
                Pressure::Normal => ("memory // pressure normal", Level::Ok),
                Pressure::Warn => ("memory // pressure elevated", Level::Warn),
                Pressure::Critical => ("memory // pressure critical", Level::Danger),
            };
            if self.samples > 1 || snap.mem.pressure != Pressure::Normal {
                self.push_log(t, text, level);
            }
            self.last_pressure = snap.mem.pressure;
        }
        if self.samples == 1 {
            let readable = snap
                .procs
                .iter()
                .filter(|p| p.access == Access::Full)
                .count();
            self.push_log(
                t,
                format!(
                    "scan // {} processes :: {} readable in full, the rest via ps (other users)",
                    snap.procs.len(),
                    readable
                ),
                Level::Info,
            );
        }
        self.snap = Some(snap);
        self.rebuild();
    }

    /// Filter + sort into `view`, keeping the selected pid selected.
    fn rebuild(&mut self) {
        self.dirty = false;
        let Some(snap) = &self.snap else {
            self.view.clear();
            self.table.selected = None;
            return;
        };
        let needle = self.filter.trim().to_lowercase();
        let uid = snap.host.uid;
        let mut view: Vec<usize> = snap
            .procs
            .iter()
            .enumerate()
            .filter(|(_, p)| !self.mine_only || p.uid == uid)
            .filter(|(_, p)| {
                needle.is_empty()
                    || p.name.to_lowercase().contains(&needle)
                    || p.user.to_lowercase().contains(&needle)
                    || p.pid.to_string() == needle
            })
            .map(|(i, _)| i)
            .collect();
        let (tab, col, desc) = (self.tab, self.table.sort_col, self.table.sort_desc);
        view.sort_by(|&a, &b| {
            let (pa, pb) = (&snap.procs[a], &snap.procs[b]);
            let ord = match (sort_key(pa, tab, col), sort_key(pb, tab, col)) {
                (SortKey::Num(x), SortKey::Num(y)) => {
                    x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
                }
                (SortKey::Text(x), SortKey::Text(y)) => x.cmp(&y),
                _ => std::cmp::Ordering::Equal,
            };
            let ord = if desc { ord.reverse() } else { ord };
            // stable tie-break so equal rows don't shuffle every sample
            ord.then_with(|| pa.pid.cmp(&pb.pid))
        });
        self.table.selected = self
            .selected_pid
            .and_then(|pid| view.iter().position(|&i| snap.procs[i].pid == pid));
        if self.table.selected.is_none() {
            self.selected_pid = None;
        }
        self.view = view;
    }

    pub(crate) fn selected(&self) -> Option<&Proc> {
        let snap = self.snap.as_ref()?;
        let pid = self.selected_pid?;
        snap.procs.iter().find(|p| p.pid == pid)
    }

    fn interval(&self) -> f32 {
        INTERVALS[self.interval_idx.min(INTERVALS.len() - 1)]
    }

    pub(crate) fn apply(&mut self, ctx: &egui::Context, action: Action, t: f64) {
        match action {
            Action::Tab(tab) => {
                if tab != self.tab {
                    self.tab = tab;
                    self.table.sort_col = 1;
                    self.table.sort_desc = true;
                    self.table.scroll_to_selected = true;
                    self.rebuild();
                    self.push_log(
                        t,
                        format!("view // {}", tab.label().to_lowercase()),
                        Level::Info,
                    );
                }
            }
            Action::Select(row) => {
                self.selected_pid = row
                    .and_then(|r| self.view.get(r).copied())
                    .and_then(|i| self.snap.as_ref().map(|s| s.procs[i].pid));
                self.table.selected = self.selected_pid.and(row);
                self.table.scroll_to_selected = true;
            }
            Action::Interval(idx) => {
                let idx = idx.min(INTERVALS.len() - 1);
                if idx != self.interval_idx {
                    self.interval_idx = idx;
                    let secs = INTERVALS[idx];
                    self.sampler.set_interval(Duration::from_secs_f32(secs));
                    self.push_log(t, format!("sampling // every {secs:.0} s"), Level::Info);
                }
            }
            Action::ToggleMine => {
                self.mine_only = !self.mine_only;
                self.rebuild();
            }
            Action::ClearFilter => {
                self.filter.clear();
                self.rebuild();
            }
            Action::RequestKill { force } => {
                let Some(p) = self.selected() else {
                    return;
                };
                let foreign = self.snap.as_ref().is_some_and(|s| p.uid != s.host.uid);
                self.dialog = Some(OpenDialog {
                    state: DialogState::Confirm(Confirm {
                        pid: p.pid,
                        name: p.name.clone(),
                        user: p.user.clone(),
                        force,
                        foreign,
                    }),
                    closing: false,
                });
            }
            Action::ConfirmDialog => {
                let Some(OpenDialog {
                    state: DialogState::Confirm(c),
                    closing: false,
                }) = &self.dialog
                else {
                    return;
                };
                let (pid, name, force) = (c.pid, c.name.clone(), c.force);
                let verb = if force { "force quit" } else { "quit" };
                match self.sampler.kill(pid, force) {
                    KillOutcome::Sent => {
                        self.push_log(
                            t,
                            format!(
                                "{verb} // {name} [{pid}] :: {} sent",
                                if force { "SIGKILL" } else { "SIGTERM" }
                            ),
                            Level::Warn,
                        );
                    }
                    KillOutcome::NotPermitted => self.fail(
                        t,
                        format!("{verb} // {name} [{pid}]"),
                        &format!("not permitted (another user's process; sudo kill {pid})"),
                    ),
                    KillOutcome::NoSuchProcess => self.fail(
                        t,
                        format!("{verb} // {name} [{pid}]"),
                        "no such process (already gone)",
                    ),
                    KillOutcome::Failed(e) => self.fail(t, format!("{verb} // {name} [{pid}]"), &e),
                }
                if let Some(d) = &mut self.dialog {
                    d.closing = true;
                }
            }
            Action::CloseDialog => {
                if let Some(d) = &mut self.dialog {
                    d.closing = true;
                }
            }
            Action::Palette(kind) => {
                self.settings.palette = kind;
                self.settings.apply(ctx);
                self.settings_changed(t);
            }
            Action::OpenSettings => self.settings_win.open(),
            Action::Refresh => {
                self.sampler
                    .set_interval(Duration::from_secs_f32(self.interval()));
            }
        }
    }

    fn handle_keys(&self, ui: &Ui, actions: &mut Vec<Action>) {
        if self.dialog.is_some() {
            return; // the dialog owns all input
        }
        // a focused text field owns the keyboard (Cmd shortcuts still pass)
        let focused = ui.memory(|m| m.focused());
        let typing = focused.is_some_and(|id| egui::TextEdit::load_state(ui.ctx(), id).is_some());
        ui.input(|i| {
            let cmd = i.modifiers.command;
            for (tab, key) in
                Tab::ALL
                    .into_iter()
                    .zip([Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5])
            {
                if cmd && i.key_pressed(key) {
                    actions.push(Action::Tab(tab));
                }
            }
            if cmd && i.key_pressed(Key::Comma) {
                actions.push(Action::OpenSettings);
            }
            if cmd && i.key_pressed(Key::R) {
                actions.push(Action::Refresh);
            }
            // Activity Monitor's own bindings: Cmd+Alt+Q quit, + Shift force quit
            if cmd && i.modifiers.alt && i.key_pressed(Key::Q) {
                actions.push(Action::RequestKill {
                    force: i.modifiers.shift,
                });
            }
            if typing {
                return;
            }
            if i.key_pressed(Key::ArrowDown) || i.key_pressed(Key::ArrowUp) {
                let dir: isize = if i.key_pressed(Key::ArrowDown) { 1 } else { -1 };
                let next = match self.table.selected {
                    Some(p) => (p as isize + dir).clamp(0, self.view.len() as isize - 1) as usize,
                    None => 0,
                };
                if !self.view.is_empty() {
                    actions.push(Action::Select(Some(next)));
                }
            }
            if i.key_pressed(Key::Escape) && self.selected_pid.is_some() {
                actions.push(Action::Select(None));
            }
        });
    }

    // ---- agent -----------------------------------------------------------------------------

    fn agent_blocked(&self) -> Vec<String> {
        match &self.dialog {
            Some(OpenDialog {
                state: DialogState::Confirm(c),
                closing: false,
            }) if !self.settings.agent_confirm => vec![confirm_verb(c).into()],
            _ => Vec::new(),
        }
    }

    fn agent_state(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "tab: {} (cmd+1..5) :: sort col {} {} :: filter {:?} :: {}",
            self.tab.label().to_lowercase(),
            self.table.sort_col,
            if self.table.sort_desc { "desc" } else { "asc" },
            self.filter,
            if self.mine_only {
                "my processes"
            } else {
                "all users"
            }
        );
        let Some(snap) = &self.snap else {
            s.push_str("no sample yet\n");
            return s;
        };
        let _ = writeln!(
            s,
            "machine: cpu {:.0}% (user {:.0} sys {:.0}) load {:.1} :: mem {} / {} used, pressure {:?} :: disk r {} w {} :: net in {} out {} :: {} procs",
            (snap.cpu.user + snap.cpu.system) * 100.0,
            snap.cpu.user * 100.0,
            snap.cpu.system * 100.0,
            snap.cpu.load[0],
            fmt_bytes(snap.mem.used),
            fmt_bytes(snap.mem.total),
            snap.mem.pressure,
            fmt_rate(snap.disk.read_bps),
            fmt_rate(snap.disk.write_bps),
            fmt_rate(snap.net.in_bps),
            fmt_rate(snap.net.out_bps),
            snap.procs.len()
        );
        let _ = writeln!(
            s,
            "sampling every {:.0} s :: values marked '~' come from ps (other users' processes: kernel-averaged cpu, rss)",
            self.interval()
        );
        match self.selected() {
            Some(p) => {
                let _ = writeln!(
                    s,
                    "selected: {} [{}] user {} :: cpu {:.1}% mem {} threads {} :: quit / force quit via the QUIT / FORCE QUIT buttons (cmd+alt+q / +shift)",
                    p.name, p.pid, p.user, p.cpu, fmt_bytes(p.mem), p.threads
                );
            }
            None => s.push_str("selected: none (click a row; rows are named by process)\n"),
        }
        let _ = writeln!(s, "top rows ({} shown):", self.view.len());
        for &i in self.view.iter().take(10) {
            let p = &snap.procs[i];
            let _ = writeln!(s, "  {}", agent_row(p, self.tab));
        }
        if let Some(OpenDialog {
            state,
            closing: false,
        }) = &self.dialog
        {
            match state {
                DialogState::Confirm(c) => {
                    let _ = writeln!(
                        s,
                        "dialog: {} {} [{}] :: buttons CANCEL / {}",
                        if c.force { "force quit" } else { "quit" },
                        c.name,
                        c.pid,
                        confirm_verb(c)
                    );
                }
                DialogState::Error(line) => {
                    let _ = writeln!(s, "dialog: ERROR {line} :: button ACKNOWLEDGE");
                }
            }
        }
        if self.settings.log_open {
            s.push_str("log (latest last):\n");
        } else {
            s.push_str("log (panel collapsed; latest last):\n");
        }
        let skip = self.log.len().saturating_sub(6);
        for e in &self.log[skip..] {
            let _ = writeln!(s, "  {} {}", e.time, e.text);
        }
        s
    }
}

fn confirm_verb(c: &Confirm) -> &'static str {
    if c.force {
        "FORCE QUIT"
    } else {
        "QUIT"
    }
}

fn agent_row(p: &Proc, tab: Tab) -> String {
    let approx = if p.access == Access::Limited { "~" } else { "" };
    match tab {
        Tab::Cpu => format!(
            "{} [{}] {}: cpu {approx}{:.1}% time {} threads {} wakeups {:.0}/s {}",
            p.name,
            p.pid,
            p.user,
            p.cpu,
            fmt_cpu_time(p.cpu_time),
            p.threads,
            p.wakeups,
            p.state.label()
        ),
        Tab::Memory => format!(
            "{} [{}] {}: mem {approx}{} rss {} threads {}",
            p.name,
            p.pid,
            p.user,
            fmt_bytes(p.mem),
            fmt_bytes(p.rss),
            p.threads
        ),
        Tab::Energy => format!(
            "{} [{}] {}: energy {:.1} (estimate) cpu {approx}{:.1}% wakeups {:.0}/s{}",
            p.name,
            p.pid,
            p.user,
            p.energy,
            p.cpu,
            p.wakeups,
            if p.prevents_sleep {
                " :: prevents sleep"
            } else {
                ""
            }
        ),
        Tab::Disk => format!(
            "{} [{}] {}: read {} write {} :: total {} / {}",
            p.name,
            p.pid,
            p.user,
            fmt_rate(p.disk_read_bps),
            fmt_rate(p.disk_write_bps),
            fmt_bytes(p.disk_read),
            fmt_bytes(p.disk_write)
        ),
        Tab::Network => format!(
            "{} [{}] {}: in {} out {} :: total {} / {}",
            p.name,
            p.pid,
            p.user,
            fmt_rate(p.net_in_bps),
            fmt_rate(p.net_out_bps),
            fmt_bytes(p.net_in),
            fmt_bytes(p.net_out)
        ),
    }
}

// ---- frame ----------------------------------------------------------------------------------

impl eframe::App for MonitorApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.devshot.tick(ui.ctx());
        // agent first: injected input must be visible to this frame's widgets
        self.agent.set_enabled(ui.ctx(), self.settings.agent);
        self.agent.set_blocked(self.agent_blocked());
        let agent_state = self.agent.wants_state().then(|| self.agent_state());
        self.agent.tick(ui.ctx(), agent_state);
        let t = ui.input(|i| i.time);
        let ctx = ui.ctx().clone();
        if let Some(snap) = self.sampler.poll() {
            self.ingest(snap, t);
        }
        if self.dirty {
            self.rebuild();
        }
        let pal = palette(ui.ctx());
        let fps = 1.0 / ui.input(|i| i.stable_dt).max(1e-3);

        let mut actions: Vec<Action> = Vec::new();
        if self.dialog.is_none() {
            if let Some(line) = self.error_queue.pop_front() {
                self.dialog = Some(OpenDialog {
                    state: DialogState::Error(line),
                    closing: false,
                });
            }
        }
        self.handle_keys(ui, &mut actions);

        let (cpu_pct, cpu_color, mem_lamp) = match &self.snap {
            Some(s) => {
                let busy = s.cpu.user + s.cpu.system;
                let color = if busy > 0.9 {
                    pal.danger
                } else if busy > 0.7 {
                    pal.warn
                } else {
                    pal.accent
                };
                let mem = match s.mem.pressure {
                    Pressure::Normal => None,
                    Pressure::Warn => Some(("MEM PRESSURE", pal.warn)),
                    Pressure::Critical => Some(("MEM CRITICAL", pal.danger)),
                };
                (busy * 100.0, color, mem)
            }
            None => (0.0, pal.text_dim, None),
        };
        let (procs, threads) = self
            .snap
            .as_ref()
            .map(|s| (s.cpu.processes, s.cpu.threads))
            .unwrap_or((0, 0));
        let host = self
            .snap
            .as_ref()
            .map(|s| s.host.clone())
            .unwrap_or_default();
        let mut shell = Shell::new("FUIDE Activity Monitor")
            .subtitle(format!(
                "v0.1 :: {} :: macOS {}",
                if host.model.is_empty() {
                    "scanning".into()
                } else {
                    host.model.clone()
                },
                host.os
            ))
            .status_left(format!(
                "{} :: {} PROCS :: {} THREADS :: EVERY {:.0} S :: {:.0} FPS",
                fuide::fmt::uptime(t),
                procs,
                threads,
                self.interval(),
                fps
            ))
            .lamp(format!("CPU {cpu_pct:>3.0}%"), cpu_color, false)
            .settings_button(true);
        if let Some((text, color)) = mem_lamp {
            shell = shell.lamp(text, color, true);
        }
        if let Some((text, busy)) = self.agent.lamp() {
            shell = shell.lamp(text, if busy { pal.warn } else { pal.accent }, busy);
        }

        let log_open = self.settings.log_open;
        let mut log_resized = false;
        let mut log_toggled = false;
        let k_log = ctx.animate_bool_with_time_and_easing(
            egui::Id::new("monitor-log-open"),
            log_open,
            0.24,
            egui::emath::easing::cubic_out,
        );
        let out = shell.show_full(ui, |ui| {
            let c = ui.max_rect();
            let tabs = Rect::from_min_size(c.min, vec2(c.width(), TABS_H));
            let toolbar = Rect::from_min_size(
                pos2(c.left(), tabs.bottom() + 6.0),
                vec2(c.width(), TOOLBAR_H),
            );
            let log_max = c.height() - BODY_MIN;
            self.log_h = self.log_h.clamp(LOG_MIN, log_max.max(LOG_MIN));
            let log_h = egui::lerp(LOG_CLOSED..=self.log_h, k_log);
            let log_rect = Rect::from_min_max(pos2(c.left(), c.bottom() - log_h), c.max);
            let summary_bottom = log_rect.top() - GAP - 8.0;
            let summary = Rect::from_min_max(
                pos2(c.left(), summary_bottom - SUMMARY_H),
                pos2(c.right(), summary_bottom),
            );
            let body_top = toolbar.bottom() + GAP + 4.0; // room for the title chips
            let body_bottom = summary.top() - GAP - 8.0;
            let right = Rect::from_min_max(
                pos2(c.right() - RIGHT_W, body_top),
                pos2(c.right(), body_bottom),
            );
            let center = Rect::from_min_max(
                pos2(c.left(), body_top),
                pos2(right.left() - GAP, body_bottom),
            );

            self.ui_tabs(ui, tabs, &mut actions);
            self.ui_toolbar(ui, toolbar, &mut actions);
            self.ui_table(ui, center, &mut actions);
            self.ui_detail(ui, right);
            self.ui_summary(ui, summary, t);
            if log_open && k_log >= 1.0 {
                let strip = Rect::from_min_max(
                    pos2(c.left(), summary_bottom),
                    pos2(c.right(), log_rect.top()),
                );
                let resp = widgets::h_splitter(
                    ui,
                    strip,
                    "log",
                    &mut self.log_h,
                    LOG_MIN,
                    log_max,
                    "LOG HEIGHT",
                );
                log_resized = resp.drag_stopped();
            }
            log_toggled = self.ui_log(ui, log_rect, log_open, k_log > 0.0);
        });
        self.agent.paint(&ctx);
        if out.settings_clicked {
            actions.push(Action::OpenSettings);
        }
        if log_resized {
            self.settings.log_height = Some(self.log_h.round());
            self.save_settings(t);
        }
        if log_toggled {
            self.settings.log_open = !log_open;
            self.save_settings(t);
        }

        self.ui_dialog(&ctx, &mut actions);
        for a in actions {
            self.apply(&ctx, a, t);
        }
        // Settings window (child viewport) last: it pauses this viewport while it draws.
        self.settings_win
            .set_agent_status(&ctx, &self.agent.status_line());
        if self
            .settings_win
            .show(&ctx, &mut self.settings, "FUIDE Activity Monitor")
        {
            self.settings_changed(t);
        }
    }
}

// ---- panels ---------------------------------------------------------------------------------

impl MonitorApp {
    fn ui_tabs(&self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let ts = type_scale(ui.ctx());
        let n = Tab::ALL.len() as f32;
        let w = ((rect.width() - GAP * 0.5 * (n - 1.0)) / n).floor();
        for (i, tab) in Tab::ALL.into_iter().enumerate() {
            let r = Rect::from_min_size(
                pos2(rect.left() + i as f32 * (w + GAP * 0.5), rect.top()),
                vec2(w, ts.row + 2.0),
            );
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r));
            if widgets::nav_tab(&mut child, tab.label(), tab == self.tab).clicked() {
                actions.push(Action::Tab(tab));
            }
        }
    }

    fn ui_toolbar(&mut self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let ui = &mut child;
        ui.spacing_mut().item_spacing.x = 8.0;
        let want_focus = ui.input(|i| i.modifiers.command && i.key_pressed(Key::F));
        let resp = widgets::text_input(ui, FILTER_W, &mut self.filter, "filter");
        if resp.changed() {
            self.dirty = true;
        }
        if want_focus {
            resp.request_focus();
        }
        // egui drops focus on Escape before widgets run: check `lost_focus` too
        if (resp.has_focus() || resp.lost_focus())
            && ui.input(|i| i.key_pressed(Key::Escape))
            && !self.filter.is_empty()
        {
            actions.push(Action::ClearFilter);
            resp.surrender_focus();
        }
        let mut mine = self.mine_only;
        if widgets::toggle_chip(ui, "my processes", &mut mine).clicked() {
            actions.push(Action::ToggleMine);
        }
        ui.add_space(6.0);
        let (lr, _) = ui.allocate_exact_size(vec2(58.0, ts.row), Sense::hover());
        ui.painter().text(
            pos2(lr.right(), lr.center().y),
            Align2::RIGHT_CENTER,
            "EVERY",
            mono(ts.label),
            pal.text_dim,
        );
        for (i, secs) in INTERVALS.iter().enumerate() {
            let mut on = i == self.interval_idx;
            let label = format!("{secs:.0} s");
            if widgets::toggle_chip(ui, &label, &mut on).clicked() && i != self.interval_idx {
                actions.push(Action::Interval(i));
            }
        }
        // right group: the verbs, enabled when a row is selected
        let selected = self.selected_pid.is_some();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if widgets::button_colored(ui, vec2(104.0, ts.row), "FORCE QUIT", selected, pal.danger)
                .clicked()
            {
                actions.push(Action::RequestKill { force: true });
            }
            if widgets::button_colored(ui, vec2(64.0, ts.row), "QUIT", selected, pal.warn).clicked()
            {
                actions.push(Action::RequestKill { force: false });
            }
            let shown = self.view.len();
            let total = self.snap.as_ref().map(|s| s.procs.len()).unwrap_or(0);
            let (tr, _) = ui.allocate_exact_size(vec2(150.0, ts.row), Sense::hover());
            ui.painter().text(
                pos2(tr.right(), tr.center().y),
                Align2::RIGHT_CENTER,
                if shown == total {
                    format!("{total} PROCESSES")
                } else {
                    format!("{shown} / {total} PROCESSES")
                },
                mono(ts.label),
                pal.text_dim,
            );
        });
    }

    fn ui_table(&mut self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let columns = self.tab.columns();
        let tab = self.tab;
        let Some(snap) = &self.snap else {
            Panel::new("Processes")
                .tag("scanning", pal.text_dim)
                .padding(8.0, 14.0)
                .show_rect(ui, rect, |ui| {
                    let r = ui.max_rect();
                    theme::display_text(
                        ui.painter(),
                        r.center(),
                        Align2::CENTER_CENTER,
                        "SCANNING",
                        type_scale(ui.ctx()).title,
                        pal.accent_dim,
                    );
                });
            return;
        };
        let procs = &snap.procs;
        let view = &self.view;
        let mut state = std::mem::take(&mut self.table);
        let tag = format!(
            "{} :: sort {}",
            tab.label(),
            columns[state.sort_col.min(columns.len() - 1)].label
        );
        let resp = Panel::new("Processes")
            .tag(tag, pal.text_dim)
            .padding(8.0, 14.0)
            .show_rect(ui, rect, |ui| {
                table::table(ui, "procs", &columns, view.len(), &mut state, |row, col| {
                    cell(&procs[view[row]], tab, col, &pal)
                })
            });
        self.table = state;
        if self.table.sort_col >= columns.len() {
            self.table.sort_col = 1;
        }
        if resp.sort_changed {
            self.dirty = true;
        }
        if let Some(r) = resp.clicked.or(resp.secondary_clicked) {
            actions.push(Action::Select(Some(r)));
        }
    }

    fn ui_detail(&self, ui: &mut Ui, rect: Rect) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let sel = self.selected();
        let tag = match sel {
            Some(p) => format!("pid {}", p.pid),
            None => "none".into(),
        };
        Panel::new("Selected")
            .tag(tag, pal.text_dim)
            .padding(12.0, 16.0)
            .show_rect(ui, rect, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let Some(p) = sel else {
                    self.ui_host(ui);
                    return;
                };
                let (nr, _) = ui.allocate_exact_size(
                    vec2(ui.available_width(), ts.title + 6.0),
                    Sense::hover(),
                );
                ui.painter().with_clip_rect(nr).text(
                    pos2(nr.left(), nr.center().y),
                    Align2::LEFT_CENTER,
                    &p.name,
                    mono(ts.title),
                    pal.accent,
                );
                let approx = p.access == Access::Limited;
                if approx {
                    let (r, _) = ui.allocate_exact_size(
                        vec2(ui.available_width(), ts.small + 6.0),
                        Sense::hover(),
                    );
                    ui.painter().text(
                        pos2(r.left(), r.center().y),
                        Align2::LEFT_CENTER,
                        "ANOTHER USER'S PROCESS :: VALUES VIA PS",
                        mono(ts.small),
                        pal.warn,
                    );
                }
                ui.add_space(4.0);
                widgets::readout(ui, "user", &p.user, None);
                widgets::readout(ui, "parent", &self.parent_label(p), None);
                widgets::readout(ui, "state", p.state.label(), None);
                widgets::readout(ui, "started", &fmt_started(p, self.snap.as_ref()), None);
                widgets::rule(ui);
                let cpu_color = if p.cpu > 100.0 { pal.warn } else { pal.text };
                widgets::readout(
                    ui,
                    "% cpu",
                    &format!("{}{:.1}", if approx { "~" } else { "" }, p.cpu),
                    Some(cpu_color),
                );
                widgets::readout(ui, "cpu time", &fmt_cpu_time(p.cpu_time), None);
                widgets::readout(
                    ui,
                    "threads",
                    &if approx {
                        "--".into()
                    } else {
                        p.threads.to_string()
                    },
                    None,
                );
                widgets::readout(
                    ui,
                    "idle wakeups",
                    &if approx {
                        "--".into()
                    } else {
                        format!("{:.0} /s", p.wakeups)
                    },
                    None,
                );
                widgets::readout(ui, "energy (est)", &format!("{:.1}", p.energy), None);
                widgets::readout(
                    ui,
                    "prevents sleep",
                    if p.prevents_sleep { "YES" } else { "no" },
                    p.prevents_sleep.then_some(pal.warn),
                );
                widgets::rule(ui);
                widgets::readout(
                    ui,
                    if approx { "resident" } else { "footprint" },
                    &fmt_bytes(p.mem),
                    None,
                );
                if !approx {
                    widgets::readout(ui, "resident", &fmt_bytes(p.rss), None);
                }
                widgets::readout(ui, "virtual", &fmt_bytes(p.vsz), None);
                widgets::rule(ui);
                widgets::readout(
                    ui,
                    "disk read",
                    &if approx {
                        "--".into()
                    } else {
                        format!(
                            "{} :: {}",
                            fmt_rate(p.disk_read_bps),
                            fmt_bytes(p.disk_read)
                        )
                    },
                    None,
                );
                widgets::readout(
                    ui,
                    "disk write",
                    &if approx {
                        "--".into()
                    } else {
                        format!(
                            "{} :: {}",
                            fmt_rate(p.disk_write_bps),
                            fmt_bytes(p.disk_write)
                        )
                    },
                    None,
                );
                widgets::readout(
                    ui,
                    "net in",
                    &format!("{} :: {}", fmt_rate(p.net_in_bps), fmt_bytes(p.net_in)),
                    None,
                );
                widgets::readout(
                    ui,
                    "net out",
                    &format!("{} :: {}", fmt_rate(p.net_out_bps), fmt_bytes(p.net_out)),
                    None,
                );
                widgets::rule(ui);
                if let Some(path) = &p.path {
                    wrapped(ui, "PATH", path, pal.text_dim, ts.small);
                }
                if let Some(cmd) = &p.cmdline {
                    if p.path.as_deref() != Some(cmd.as_str()) {
                        wrapped(ui, "COMMAND", cmd, pal.text_dim, ts.small);
                    }
                }
            });
    }

    /// The detail panel when nothing is selected: the machine itself.
    fn ui_host(&self, ui: &mut Ui) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let Some(snap) = &self.snap else {
            return;
        };
        let h = &snap.host;
        theme::display_text(
            ui.painter(),
            ui.cursor().min + vec2(0.0, ts.heading),
            Align2::LEFT_CENTER,
            "THIS MACHINE",
            ts.heading,
            pal.accent,
        );
        ui.add_space(ts.heading * 2.0 + 4.0);
        widgets::readout(ui, "host", &h.hostname, None);
        widgets::readout(ui, "model", &h.model, None);
        widgets::readout(ui, "macos", &h.os, None);
        widgets::readout(ui, "uptime", &fmt_span(h.uptime), None);
        widgets::readout(
            ui,
            "cores",
            &if h.p_cores > 0 && h.e_cores > 0 {
                format!("{} ({}P + {}E)", h.cores, h.p_cores, h.e_cores)
            } else {
                h.cores.to_string()
            },
            None,
        );
        widgets::readout(ui, "memory", &fmt_bytes(h.mem_total), None);
        widgets::readout(ui, "processes", &snap.cpu.processes.to_string(), None);
        widgets::rule(ui);
        let hints = [
            "CLICK A ROW :: SELECT",
            "CMD+ALT+Q QUIT  +SHIFT FORCE",
            "CMD+1..5 TABS  CMD+F FILTER",
            "~ = VIA PS (OTHER USERS)",
        ];
        for h in hints {
            let (r, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), ts.small + 6.0), Sense::hover());
            ui.painter().text(
                pos2(r.left(), r.center().y),
                Align2::LEFT_CENTER,
                h,
                mono(ts.small),
                pal.text_dim,
            );
        }
    }

    fn parent_label(&self, p: &Proc) -> String {
        let name = self
            .snap
            .as_ref()
            .and_then(|s| s.procs.iter().find(|q| q.pid == p.ppid))
            .map(|q| q.name.clone());
        match name {
            Some(n) => format!("{n} [{}]", p.ppid),
            None => p.ppid.to_string(),
        }
    }

    fn ui_summary(&self, ui: &mut Ui, rect: Rect, t: f64) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let title = match self.tab {
            Tab::Cpu => "CPU load",
            Tab::Memory => "Memory pressure",
            Tab::Energy => "Energy",
            Tab::Disk => "Disk",
            Tab::Network => "Network",
        };
        let tag = format!(
            "{} samples :: {} s window",
            self.hist.cpu.len(),
            self.hist.cpu.len() as f32 * self.interval()
        );
        Panel::new(title)
            .tag(tag, pal.text_dim)
            .padding(12.0, 16.0)
            .show_rect(ui, rect, |ui| {
                let r = ui.max_rect();
                let Some(snap) = &self.snap else {
                    return;
                };
                // graph | middle column | readouts
                let graph =
                    Rect::from_min_max(r.min, pos2(r.left() + r.width() * 0.46, r.bottom()));
                let mid = Rect::from_min_max(
                    pos2(graph.right() + GAP, r.top()),
                    pos2(r.right() - 262.0, r.bottom()),
                );
                let right = Rect::from_min_max(pos2(mid.right() + GAP, r.top()), r.max);
                let p = ui.painter();
                let h = &self.hist;
                match self.tab {
                    Tab::Cpu => {
                        graph_paint(
                            p,
                            graph,
                            &[(&h.cpu, pal.accent), (&h.system, pal.warn)],
                            1.0,
                            "TOTAL",
                            "SYSTEM",
                            |v| format!("{:.0}%", v * 100.0),
                            &pal,
                            ts,
                            t,
                        );
                        cores_paint(p, mid, &snap.cpu.cores, &pal, ts);
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(right));
                        c.spacing_mut().item_spacing.y = 1.0;
                        widgets::readout(
                            &mut c,
                            "system",
                            &format!("{:.1} %", snap.cpu.system * 100.0),
                            Some(pal.warn),
                        );
                        widgets::readout(
                            &mut c,
                            "user",
                            &format!("{:.1} %", snap.cpu.user * 100.0),
                            Some(pal.accent),
                        );
                        widgets::readout(
                            &mut c,
                            "idle",
                            &format!("{:.1} %", snap.cpu.idle * 100.0),
                            None,
                        );
                        widgets::readout(
                            &mut c,
                            "load 1 / 5 / 15",
                            &format!(
                                "{:.2}  {:.2}  {:.2}",
                                snap.cpu.load[0], snap.cpu.load[1], snap.cpu.load[2]
                            ),
                            None,
                        );
                        widgets::readout(
                            &mut c,
                            "gpu",
                            &snap
                                .cpu
                                .gpu
                                .map(|g| format!("{:.0} %", g * 100.0))
                                .unwrap_or_else(|| "--".into()),
                            None,
                        );
                        widgets::readout(
                            &mut c,
                            "processes",
                            &snap.cpu.processes.to_string(),
                            None,
                        );
                        widgets::readout(&mut c, "threads", &snap.cpu.threads.to_string(), None);
                    }
                    Tab::Memory => {
                        let m = &snap.mem;
                        graph_paint(
                            p,
                            graph,
                            &[(&h.mem, pressure_color(m.pressure, &pal))],
                            1.0,
                            "USED",
                            "",
                            |v| format!("{:.0}%", v * 100.0),
                            &pal,
                            ts,
                            t,
                        );
                        let used = m.used as f32 / m.total.max(1) as f32;
                        let color = pressure_color(m.pressure, &pal);
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(mid));
                        c.horizontal(|ui| {
                            widgets::arc_gauge(ui, 34.0, used, "used", color);
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.add_space(10.0);
                                ui.spacing_mut().item_spacing.y = 1.0;
                                widgets::readout(
                                    ui,
                                    "pressure",
                                    pressure_label(m.pressure),
                                    Some(color),
                                );
                                widgets::readout(ui, "physical", &fmt_bytes(m.total), None);
                                widgets::readout(ui, "used", &fmt_bytes(m.used), Some(color));
                                let (br, _) = ui.allocate_exact_size(
                                    vec2(ui.available_width(), 10.0),
                                    Sense::hover(),
                                );
                                widgets::segment_bar(ui.painter(), br, used, color, pal.accent_dim);
                            });
                        });
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(right));
                        c.spacing_mut().item_spacing.y = 1.0;
                        widgets::readout(&mut c, "app memory", &fmt_bytes(m.app), None);
                        widgets::readout(&mut c, "wired", &fmt_bytes(m.wired), None);
                        widgets::readout(&mut c, "compressed", &fmt_bytes(m.compressed), None);
                        widgets::readout(&mut c, "cached files", &fmt_bytes(m.cached), None);
                        widgets::readout(
                            &mut c,
                            "swap used",
                            &format!("{} / {}", fmt_bytes(m.swap_used), fmt_bytes(m.swap_total)),
                            (m.swap_used > 0).then_some(pal.text),
                        );
                    }
                    Tab::Energy => {
                        let max = h.energy.max().max(50.0);
                        graph_paint(
                            p,
                            graph,
                            &[(&h.energy, pal.accent)],
                            max,
                            "IMPACT (EST)",
                            "",
                            |v| format!("{v:.0}"),
                            &pal,
                            ts,
                            t,
                        );
                        let pw = &snap.power;
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(mid));
                        c.spacing_mut().item_spacing.y = 1.0;
                        match &pw.battery {
                            Some(b) => {
                                let color = if b.percent < 15.0 {
                                    pal.danger
                                } else if b.percent < 30.0 {
                                    pal.warn
                                } else {
                                    pal.accent
                                };
                                c.horizontal(|ui| {
                                    widgets::arc_gauge(
                                        ui,
                                        34.0,
                                        b.percent / 100.0,
                                        "charge",
                                        color,
                                    );
                                    ui.add_space(6.0);
                                    ui.vertical(|ui| {
                                        ui.add_space(10.0);
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        widgets::readout(
                                            ui,
                                            "source",
                                            if pw.on_ac { "AC POWER" } else { "BATTERY" },
                                            None,
                                        );
                                        widgets::readout(
                                            ui,
                                            "state",
                                            if b.charging {
                                                "CHARGING"
                                            } else if pw.on_ac {
                                                "CHARGED"
                                            } else {
                                                "DISCHARGING"
                                            },
                                            None,
                                        );
                                        widgets::readout(
                                            ui,
                                            if b.charging { "to full" } else { "remaining" },
                                            &b.minutes
                                                .map(|m| format!("{}:{:02}", m / 60, m % 60))
                                                .unwrap_or_else(|| "--".into()),
                                            None,
                                        );
                                    });
                                });
                            }
                            None => {
                                widgets::readout(&mut c, "source", "AC POWER", None);
                                widgets::readout(&mut c, "battery", "NONE", None);
                            }
                        }
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(right));
                        c.spacing_mut().item_spacing.y = 1.0;
                        widgets::readout(
                            &mut c,
                            "total impact (est)",
                            &format!("{:.0}", h.energy.last()),
                            None,
                        );
                        let mut blockers: Vec<&Proc> =
                            snap.procs.iter().filter(|p| p.prevents_sleep).collect();
                        blockers.sort_by(|a, b| a.name.cmp(&b.name));
                        widgets::readout(
                            &mut c,
                            "preventing sleep",
                            &blockers.len().to_string(),
                            (!blockers.is_empty()).then_some(pal.warn),
                        );
                        for b in blockers.iter().take(4) {
                            widgets::readout(
                                &mut c,
                                &format!("  {}", b.name.to_lowercase()),
                                &b.pid.to_string(),
                                None,
                            );
                        }
                        let (fr, _) = c.allocate_exact_size(
                            vec2(c.available_width(), ts.small + 8.0),
                            Sense::hover(),
                        );
                        c.painter().text(
                            pos2(fr.left(), fr.bottom() - 2.0),
                            Align2::LEFT_BOTTOM,
                            "ESTIMATE :: NOT APPLE'S SCALE",
                            mono(ts.small),
                            pal.text_dim,
                        );
                    }
                    Tab::Disk => {
                        let d = &snap.disk;
                        let max = h.disk_read.max().max(h.disk_write.max()).max(1.0e6);
                        graph_paint(
                            p,
                            graph,
                            &[(&h.disk_read, pal.accent), (&h.disk_write, pal.warn)],
                            max,
                            "READ",
                            "WRITE",
                            |v| fmt_rate(v as f64),
                            &pal,
                            ts,
                            t,
                        );
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(mid));
                        c.spacing_mut().item_spacing.y = 1.0;
                        widgets::readout(
                            &mut c,
                            "reads in",
                            &format!("{:.0} /s", d.read_ops),
                            Some(pal.accent),
                        );
                        widgets::readout(
                            &mut c,
                            "writes out",
                            &format!("{:.0} /s", d.write_ops),
                            Some(pal.warn),
                        );
                        widgets::readout(
                            &mut c,
                            "data read",
                            &fmt_rate(d.read_bps),
                            Some(pal.accent),
                        );
                        widgets::readout(
                            &mut c,
                            "data written",
                            &fmt_rate(d.write_bps),
                            Some(pal.warn),
                        );
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(right));
                        c.spacing_mut().item_spacing.y = 1.0;
                        widgets::readout(&mut c, "total read", &fmt_bytes(d.read_total), None);
                        widgets::readout(&mut c, "total written", &fmt_bytes(d.write_total), None);
                        widgets::readout(&mut c, "since boot", &fmt_span(snap.host.uptime), None);
                    }
                    Tab::Network => {
                        let n = &snap.net;
                        let max = h.net_in.max().max(h.net_out.max()).max(100.0e3);
                        graph_paint(
                            p,
                            graph,
                            &[(&h.net_in, pal.accent), (&h.net_out, pal.warn)],
                            max,
                            "IN",
                            "OUT",
                            |v| fmt_rate(v as f64),
                            &pal,
                            ts,
                            t,
                        );
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(mid));
                        c.spacing_mut().item_spacing.y = 1.0;
                        widgets::readout(
                            &mut c,
                            "packets in",
                            &format!("{:.0} /s", n.in_pps),
                            Some(pal.accent),
                        );
                        widgets::readout(
                            &mut c,
                            "packets out",
                            &format!("{:.0} /s", n.out_pps),
                            Some(pal.warn),
                        );
                        widgets::readout(
                            &mut c,
                            "data received",
                            &fmt_rate(n.in_bps),
                            Some(pal.accent),
                        );
                        widgets::readout(&mut c, "data sent", &fmt_rate(n.out_bps), Some(pal.warn));
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(right));
                        c.spacing_mut().item_spacing.y = 1.0;
                        widgets::readout(&mut c, "total received", &fmt_bytes(n.in_total), None);
                        widgets::readout(&mut c, "total sent", &fmt_bytes(n.out_total), None);
                        widgets::readout(&mut c, "per process", "VIA NETTOP", None);
                    }
                }
            });
    }

    /// Returns `true` when the title chip was clicked (the caller flips `Settings::log_open`).
    fn ui_log(&self, ui: &mut Ui, rect: Rect, open: bool, feed: bool) -> bool {
        let pal = palette(ui.ctx());
        let (_, toggled) = Panel::new("Event log")
            .tag(format!("{} events", self.log.len()), pal.text_dim)
            .padding(8.0, 12.0)
            .show_collapsible_rect(ui, rect, open, |ui| {
                if !feed {
                    return;
                }
                let lines: Vec<LogLine> = self
                    .log
                    .iter()
                    .map(|l| LogLine {
                        time: l.time.clone(),
                        text: l.text.clone(),
                        color: match l.level {
                            Level::Info => pal.text,
                            Level::Ok => pal.ok,
                            Level::Warn => pal.warn,
                            Level::Danger => pal.danger,
                        },
                    })
                    .collect();
                widgets::log_feed(
                    ui,
                    &lines,
                    pal.text_dim,
                    type_scale(ui.ctx()).label,
                    widgets::LogOrder::NewestFirst,
                );
            });
        toggled
    }

    fn ui_dialog(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let pal = palette(ctx);
        let ts = type_scale(ctx);
        let Some(OpenDialog { state, closing }) = &mut self.dialog else {
            return;
        };
        let open = !*closing;
        let enter = open && ctx.input(|i| i.key_pressed(Key::Enter));
        let finished;
        match state {
            DialogState::Confirm(c) => {
                let color = if c.force { pal.danger } else { pal.warn };
                let verb = confirm_verb(c);
                let title = if c.force {
                    "Force quit process"
                } else {
                    "Quit process"
                };
                let resp = Dialog::new(title)
                    .tag(format!("pid {}", c.pid), pal.text_dim)
                    .outline(color)
                    .width(460.0)
                    .show(ctx, open, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        ui.add(
                            egui::Label::new(
                                RichText::new(&c.name)
                                    .font(mono(ts.data + 2.0))
                                    .color(pal.accent),
                            )
                            .wrap(),
                        );
                        ui.add_space(2.0);
                        widgets::readout(ui, "user", &c.user, None);
                        widgets::readout(
                            ui,
                            "signal",
                            if c.force {
                                "SIGKILL (cannot be caught)"
                            } else {
                                "SIGTERM (asks it to exit)"
                            },
                            None,
                        );
                        ui.add_space(6.0);
                        widgets::rule(ui);
                        let note = if c.foreign {
                            "ANOTHER USER'S PROCESS :: THE KERNEL WILL REFUSE WITHOUT SUDO"
                        } else if c.force {
                            "UNSAVED WORK IN THIS PROCESS IS LOST"
                        } else {
                            "THE PROCESS MAY ASK TO SAVE, OR IGNORE THE REQUEST"
                        };
                        let (nr, _) = ui.allocate_exact_size(
                            vec2(ui.available_width(), ts.row),
                            Sense::hover(),
                        );
                        ui.painter().text(
                            pos2(nr.left() + 2.0, nr.center().y),
                            Align2::LEFT_CENTER,
                            note,
                            mono(ts.label),
                            color,
                        );
                        ui.add_space(8.0);
                        fuide::dialog::button_row(
                            ui,
                            &[("CANCEL", pal.text_dim, true), (verb, color, true)],
                        )
                    });
                finished = resp.finished;
                let clicked = resp.inner.flatten();
                if !open {
                } else if resp.should_close || clicked == Some(0) {
                    actions.push(Action::CloseDialog);
                } else if enter || clicked == Some(1) {
                    actions.push(Action::ConfirmDialog);
                }
            }
            DialogState::Error(line) => {
                let resp = fuide::dialog::alert(
                    ctx,
                    open,
                    "Error",
                    line,
                    "details :: event log",
                    pal.danger,
                );
                finished = resp.finished;
                if open && (resp.should_close || resp.inner == Some(true)) {
                    actions.push(Action::CloseDialog);
                }
            }
        }
        if finished {
            self.dialog = None;
        }
    }
}

// ---- cells & painting -----------------------------------------------------------------------

fn cell(p: &Proc, tab: Tab, col: usize, pal: &theme::Palette) -> Cell {
    let approx = p.access == Access::Limited;
    let tilde = |s: String| if approx { format!("~{s}") } else { s };
    let dash = || Cell::dim("--");
    let name = || Cell::text(&p.name).color(if approx { pal.text_dim } else { pal.text });
    let pid = || Cell::dim(p.pid.to_string());
    let user = || Cell::dim(&p.user);
    let cpu = || {
        let c = if p.cpu >= 100.0 {
            pal.warn
        } else if approx {
            pal.text_dim
        } else {
            pal.text
        };
        Cell::text(tilde(format!("{:.1}", p.cpu))).color(c)
    };
    let threads = || {
        if approx {
            dash()
        } else {
            Cell::dim(p.threads.to_string())
        }
    };
    let wakeups = || {
        if approx {
            dash()
        } else {
            Cell::dim(format!("{:.0}", p.wakeups))
        }
    };
    match (tab, col) {
        (_, 0) => name(),
        (Tab::Cpu, 1) => cpu(),
        (Tab::Cpu, 2) => Cell::dim(fmt_cpu_time(p.cpu_time)),
        (Tab::Cpu, 3) => threads(),
        (Tab::Cpu, 4) => wakeups(),
        (Tab::Cpu, 5) => Cell::tag(p.state.label()).color(match p.state {
            State::Running => pal.accent,
            State::Zombie | State::Stopped => pal.warn,
            _ => pal.text_dim,
        }),
        (Tab::Cpu, 6) => pid(),
        (Tab::Cpu, _) => user(),
        (Tab::Memory, 1) => {
            Cell::text(tilde(fmt_bytes(p.mem))).color(if approx { pal.text_dim } else { pal.text })
        }
        (Tab::Memory, 2) => Cell::dim(fmt_bytes(p.rss)),
        (Tab::Memory, 3) => threads(),
        (Tab::Memory, 4) => pid(),
        (Tab::Memory, _) => user(),
        (Tab::Energy, 1) => Cell::text(format!("{:.1}", p.energy)),
        (Tab::Energy, 2) => cpu(),
        (Tab::Energy, 3) => wakeups(),
        (Tab::Energy, 4) => {
            if p.prevents_sleep {
                Cell::tag("YES").color(pal.warn)
            } else {
                Cell::dim("")
            }
        }
        (Tab::Energy, 5) => pid(),
        (Tab::Energy, _) => user(),
        (Tab::Disk, 1) => {
            if approx {
                dash()
            } else {
                rate_cell(p.disk_read_bps, pal)
            }
        }
        (Tab::Disk, 2) => {
            if approx {
                dash()
            } else {
                rate_cell(p.disk_write_bps, pal)
            }
        }
        (Tab::Disk, 3) => {
            if approx {
                dash()
            } else {
                Cell::dim(fmt_bytes(p.disk_read))
            }
        }
        (Tab::Disk, 4) => {
            if approx {
                dash()
            } else {
                Cell::dim(fmt_bytes(p.disk_write))
            }
        }
        (Tab::Disk, 5) => pid(),
        (Tab::Disk, _) => user(),
        (Tab::Network, 1) => rate_cell(p.net_in_bps, pal),
        (Tab::Network, 2) => rate_cell(p.net_out_bps, pal),
        (Tab::Network, 3) => Cell::dim(fmt_bytes(p.net_in)),
        (Tab::Network, 4) => Cell::dim(fmt_bytes(p.net_out)),
        (Tab::Network, 5) => pid(),
        (Tab::Network, _) => user(),
    }
}

fn rate_cell(bps: f64, pal: &theme::Palette) -> Cell {
    if bps < 1.0 {
        Cell::dim("0")
    } else {
        Cell::text(fmt_rate(bps)).color(pal.text)
    }
}

fn pressure_color(p: Pressure, pal: &theme::Palette) -> Color32 {
    match p {
        Pressure::Normal => pal.accent,
        Pressure::Warn => pal.warn,
        Pressure::Critical => pal.danger,
    }
}

fn pressure_label(p: Pressure) -> &'static str {
    match p {
        Pressure::Normal => "NORMAL",
        Pressure::Warn => "ELEVATED",
        Pressure::Critical => "CRITICAL",
    }
}

/// Key: value where the value may wrap over several lines (paths, command lines).
fn wrapped(ui: &mut Ui, key: &str, value: &str, color: Color32, size: f32) {
    let pal = palette(ui.ctx());
    let ts = type_scale(ui.ctx());
    let (kr, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), ts.label + 4.0), Sense::hover());
    ui.painter().text(
        pos2(kr.left(), kr.center().y),
        Align2::LEFT_CENTER,
        key,
        mono(ts.label),
        pal.text_dim,
    );
    ui.add(
        egui::Label::new(RichText::new(value).font(mono(size)).color(color))
            .wrap()
            .selectable(false),
    );
    ui.add_space(4.0);
}

/// Time series with a faint grid, up to two glow lines and the latest value at the right.
#[allow(clippy::too_many_arguments)]
fn graph_paint(
    p: &egui::Painter,
    rect: Rect,
    series: &[(&Series, Color32)],
    max: f32,
    label_a: &str,
    label_b: &str,
    fmt: impl Fn(f32) -> String,
    pal: &theme::Palette,
    ts: theme::TypeScale,
    t: f64,
) {
    let legend_h = ts.small + 8.0;
    let plot = Rect::from_min_max(
        pos2(rect.left(), rect.top() + legend_h),
        pos2(rect.right() - 86.0, rect.bottom() - 4.0),
    );
    // legend: A in the first colour, B in the second
    let mut x = plot.left();
    for (label, (_, color)) in [label_a, label_b].iter().zip(series.iter()) {
        if label.is_empty() {
            continue;
        }
        p.rect_filled(
            Rect::from_min_size(pos2(x, rect.top() + 4.0), vec2(10.0, 3.0)),
            egui::CornerRadius::ZERO,
            *color,
        );
        let r = p.text(
            pos2(x + 14.0, rect.top() + 5.5),
            Align2::LEFT_CENTER,
            *label,
            mono(ts.small),
            pal.text_dim,
        );
        x = r.right() + 14.0;
    }
    // frame + grid
    p.rect_stroke(
        plot,
        egui::CornerRadius::ZERO,
        Stroke::new(1.0, pal.accent_dim.gamma_multiply(0.5)),
        egui::StrokeKind::Inside,
    );
    for k in 1..4 {
        let gy = plot.bottom() - plot.height() * k as f32 / 4.0;
        p.line_segment(
            [pos2(plot.left(), gy), pos2(plot.right(), gy)],
            Stroke::new(1.0, pal.accent.gamma_multiply(0.08)),
        );
    }
    // a slow sweep so the graph reads as live even when flat
    let sweep = plot.left() + ((t * 30.0) % plot.width() as f64) as f32;
    p.line_segment(
        [pos2(sweep, plot.top()), pos2(sweep, plot.bottom())],
        Stroke::new(1.0, pal.accent.gamma_multiply(0.10)),
    );
    let max = max.max(1e-6);
    let step = plot.width() / (HISTORY - 1) as f32;
    for (s, color) in series {
        let n = s.len();
        if n < 2 {
            continue;
        }
        let x0 = plot.right() - step * (n - 1) as f32;
        let pts: Vec<egui::Pos2> =
            s.0.iter()
                .enumerate()
                .map(|(i, &v)| {
                    pos2(
                        x0 + i as f32 * step,
                        plot.bottom() - (v / max).clamp(0.0, 1.0) * (plot.height() - 2.0) - 1.0,
                    )
                })
                .collect();
        // fill under the line, faint
        let mut poly = pts.clone();
        poly.push(pos2(plot.right(), plot.bottom()));
        poly.push(pos2(x0, plot.bottom()));
        p.add(egui::Shape::convex_polygon(
            poly,
            color.gamma_multiply(0.06),
            Stroke::NONE,
        ));
        fuide::geom::glow_line(p, &pts, *color, 1.5, 5.0);
    }
    // latest values, stacked at the right
    let mut y = plot.top() + ts.data * 0.5 + 2.0;
    for (s, color) in series {
        p.text(
            pos2(rect.right(), y),
            Align2::RIGHT_CENTER,
            fmt(s.last()),
            mono(ts.data),
            *color,
        );
        y += ts.data + 6.0;
    }
    p.text(
        pos2(rect.right(), plot.bottom()),
        Align2::RIGHT_BOTTOM,
        format!("MAX {}", fmt(max)),
        mono(ts.small),
        pal.text_dim,
    );
}

/// One segment bar per core, P and E clusters labelled.
fn cores_paint(
    p: &egui::Painter,
    rect: Rect,
    cores: &[sys::Core],
    pal: &theme::Palette,
    ts: theme::TypeScale,
) {
    if cores.is_empty() {
        return;
    }
    let label_w = 30.0;
    let row_h = ((rect.height() - 4.0) / cores.len() as f32).clamp(8.0, 16.0);
    for (i, c) in cores.iter().enumerate() {
        let y = rect.top() + i as f32 * row_h;
        let tag = match c.kind {
            CoreKind::Performance => format!("P{i}"),
            CoreKind::Efficiency => format!("E{i}"),
            CoreKind::Standard => format!("C{i}"),
        };
        p.text(
            pos2(rect.left(), y + row_h * 0.5),
            Align2::LEFT_CENTER,
            tag,
            mono(ts.small),
            pal.text_dim,
        );
        let bar = Rect::from_min_max(
            pos2(rect.left() + label_w, y + 2.0),
            pos2(rect.right() - 36.0, y + row_h - 2.0),
        );
        let color = if c.usage > 0.9 { pal.warn } else { pal.accent };
        widgets::segment_bar(p, bar, c.usage, color, pal.accent_dim);
        p.text(
            pos2(rect.right(), y + row_h * 0.5),
            Align2::RIGHT_CENTER,
            format!("{:>3.0}", c.usage * 100.0),
            mono(ts.small),
            pal.text_dim,
        );
    }
}

// ---- formatting -----------------------------------------------------------------------------

/// `1.23 GB` (decimal units, like Activity Monitor). `0 B` for zero.
pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if b < 1000 {
        return format!("{b} B");
    }
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// `12.3 MB/s`; below 1 B/s prints `0 B/s`.
pub fn fmt_rate(bps: f64) -> String {
    if bps < 1.0 {
        return "0 B/s".into();
    }
    format!("{}/s", fmt_bytes(bps as u64))
}

/// Activity Monitor's `H:MM:SS.cc` CPU time.
pub fn fmt_cpu_time(secs: f64) -> String {
    let total = secs.max(0.0);
    let h = (total / 3600.0).floor() as u64;
    let m = ((total % 3600.0) / 60.0).floor() as u64;
    let s = total % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{s:05.2}")
    } else {
        format!("{m}:{s:05.2}")
    }
}

/// `3d 04:12` / `04:12:07` for uptimes and ages.
pub fn fmt_span(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let d = total / 86400;
    let h = (total % 86400) / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if d > 0 {
        format!("{d}d {h:02}:{m:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

fn fmt_started(p: &Proc, snap: Option<&Snapshot>) -> String {
    match (p.started, snap) {
        (Some(started), Some(s)) => format!("{} ago", fmt_span((s.now - started).max(0) as f64)),
        _ => "--".into(),
    }
}

#[cfg(test)]
mod e2e;
#[cfg(test)]
mod tests;
