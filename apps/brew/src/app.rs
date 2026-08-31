//! FUIDE Brew — Homebrew front-end with a tactical-console look.

use std::collections::VecDeque;

use egui::{pos2, vec2, Align2, Key, Rect, RichText, Sense, Ui};
use fuide::table::{self, Cell, Column, TableState, Width};
use fuide::widgets::{self, LogLine};
use fuide::{mono, palette, theme, type_scale, Dialog, Palette, Panel, Shell};

use crate::brew::{self, Brew, Kind, Msg, Package, Status, SystemInfo};

const LEFT_W: f32 = 220.0;
const RIGHT_W: f32 = 320.0;
const GAP: f32 = 14.0;
const TOOLBAR_H: f32 = 32.0;
const LOG_H: f32 = 150.0;
const SYSTEM_H: f32 = 236.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Installed,
    Outdated,
    Casks,
    Search,
}

impl View {
    const ALL: [View; 4] = [View::Installed, View::Outdated, View::Casks, View::Search];
    fn label(self) -> &'static str {
        match self {
            View::Installed => "Installed",
            View::Outdated => "Outdated",
            View::Casks => "Casks",
            View::Search => "Search",
        }
    }
}

#[derive(Clone, Copy)]
enum Level {
    Info,
    Ok,
    Warn,
    Danger,
}

struct Event {
    time: String,
    text: String,
    level: Level,
}

/// A confirmation before running a mutating brew command.
struct Confirm {
    title: String,
    line: String,
    note: String,
    verb: String,
    danger: bool,
    /// Log label + `brew` args to run on confirm.
    label: String,
    args: Vec<String>,
}

enum DialogState {
    Confirm(Confirm),
    /// Big `ERROR` / `SUCCESS` card; details live in the output log.
    Notice {
        success: bool,
        line: String,
    },
}

struct OpenDialog {
    state: DialogState,
    closing: bool,
}

enum Action {
    SetView(View),
    Select(Option<usize>),
    Refresh,
    Update,
    UpgradeAll,
    Upgrade(String, Kind),
    Install(String, Kind),
    Uninstall(String, Kind),
    TogglePin(String, bool),
    Homepage(String),
    CopyName(String),
    Search,
    Run(String, Vec<String>),
    CloseDialog,
    ConfirmDialog,
}

pub struct BrewApp {
    brew: Brew,
    packages: Vec<Package>,
    search_results: Vec<Package>,
    search_query: String,
    search_pending: Option<String>,
    system: SystemInfo,
    view: View,
    /// Indices into `packages` (or `search_results` in the Search view), filtered + sorted.
    rows: Vec<usize>,
    table: TableState,
    dirty: bool,
    filter: String,
    log: Vec<Event>,
    dialog: Option<OpenDialog>,
    /// Notices waiting for the dialog slot: (success, line).
    notice_queue: VecDeque<(bool, String)>,
    last_error: Option<String>,
    fetch_ms: f32,
    devshot: fuide::devshot::DevShot,
    dev_dialog: Option<String>,
    /// Dev aids: `FUIDE_DEV_RUN="doctor"` runs a brew command at start; `FUIDE_DEV_SEARCH=q` opens Search.
    dev_run: Option<String>,
    dev_search: Option<String>,
    dev_frame: u32,
    dev_close_frame: Option<u32>,
}

impl BrewApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx, Palette::amber(), theme::macos_cjk_fallback());
        let mut app = Self {
            brew: Brew::new(),
            packages: Vec::new(),
            search_results: Vec::new(),
            search_query: String::new(),
            search_pending: None,
            system: SystemInfo::default(),
            view: View::Installed,
            rows: Vec::new(),
            table: TableState::default(),
            dirty: true,
            filter: String::new(),
            log: Vec::new(),
            dialog: None,
            notice_queue: VecDeque::new(),
            last_error: None,
            fetch_ms: 0.0,
            devshot: fuide::devshot::DevShot::from_env(),
            dev_dialog: std::env::var("FUIDE_DEV_DIALOG").ok(),
            dev_run: std::env::var("FUIDE_DEV_RUN").ok(),
            dev_search: std::env::var("FUIDE_DEV_SEARCH").ok(),
            dev_frame: 0,
            dev_close_frame: std::env::var("FUIDE_DEV_DIALOG_CLOSE")
                .ok()
                .and_then(|v| v.parse().ok()),
        };
        app.push_log(0.0, "brew console online :: reading inventory", Level::Ok);
        if let Ok(text) = std::env::var("FUIDE_DEV_LOG") {
            app.push_log(0.0, text, Level::Danger);
        }
        app.brew.fetch_inventory(cc.egui_ctx.clone());
        app.brew.fetch_system(cc.egui_ctx.clone());
        app
    }

    // ------------------------------------------------------------------ state

    fn push_log(&mut self, t: f64, text: impl Into<String>, level: Level) {
        self.log.push(Event {
            time: format!("[{}]", fuide::fmt::uptime(t)),
            text: text.into(),
            level,
        });
        if self.log.len() > 2000 {
            self.log.drain(..500);
        }
    }

    fn fail(&mut self, t: f64, line: impl Into<String>, detail: &str) {
        let line = line.into();
        self.push_log(t, format!("{line} :: failed :: {detail}"), Level::Danger);
        self.notice_queue.push_back((false, line));
    }

    fn succeed(&mut self, t: f64, line: impl Into<String>, detail: &str) {
        let line = line.into();
        self.push_log(t, format!("{line} :: {detail}"), Level::Ok);
        self.notice_queue.push_back((true, line));
    }

    fn source(&self) -> &[Package] {
        if self.view == View::Search {
            &self.search_results
        } else {
            &self.packages
        }
    }

    fn selected_package(&self) -> Option<&Package> {
        self.table
            .selected
            .and_then(|r| self.rows.get(r))
            .and_then(|&i| self.source().get(i))
    }

    fn rebuild_rows(&mut self) {
        let filter = self.filter.to_lowercase();
        let src = self.source();
        let mut rows: Vec<usize> = src
            .iter()
            .enumerate()
            .filter(|(_, p)| match self.view {
                View::Installed => true,
                View::Outdated => p.outdated,
                View::Casks => p.kind == Kind::Cask,
                View::Search => true,
            })
            .filter(|(_, p)| {
                filter.is_empty()
                    || p.name.to_lowercase().contains(&filter)
                    || p.desc.to_lowercase().contains(&filter)
            })
            .map(|(i, _)| i)
            .collect();
        let (col, desc) = (self.table.sort_col, self.table.sort_desc);
        rows.sort_by(|&a, &b| {
            let (pa, pb) = (&src[a], &src[b]);
            let ord = match col {
                1 => pa.installed_version().cmp(pb.installed_version()),
                2 => pa.latest.cmp(&pb.latest),
                3 => pa.kind.tag().cmp(pb.kind.tag()),
                4 => pa.status().cmp(&pb.status()),
                _ => std::cmp::Ordering::Equal,
            }
            .then_with(|| pa.name.to_lowercase().cmp(&pb.name.to_lowercase()));
            if desc {
                ord.reverse()
            } else {
                ord
            }
        });
        // keep the selection on the same package if it is still visible
        let keep = self
            .table
            .selected
            .and_then(|r| self.rows.get(r).copied())
            .and_then(|old| rows.iter().position(|&i| i == old));
        self.rows = rows;
        self.table.selected = keep;
        self.dirty = false;
    }

    fn poll(&mut self, ctx: &egui::Context, t: f64) {
        for msg in self.brew.poll() {
            match msg {
                Msg::Line { text, stderr } => {
                    let level = if text.starts_with("Error") {
                        Level::Danger
                    } else if text.starts_with("Warning") {
                        Level::Warn
                    } else if text.starts_with("==>") {
                        Level::Ok
                    } else {
                        let _ = stderr; // brew writes progress to stderr; not an error by itself
                        Level::Info
                    };
                    self.push_log(t, text, level);
                }
                Msg::Exit {
                    label,
                    args,
                    ok,
                    code,
                    elapsed_ms,
                } => {
                    if ok {
                        self.succeed(
                            t,
                            label,
                            &format!(
                                "brew {} done in {:.1} s",
                                args.join(" "),
                                elapsed_ms / 1000.0
                            ),
                        );
                    } else {
                        let detail = match code {
                            Some(c) => format!("exit code {c}"),
                            None => "terminated".into(),
                        };
                        self.fail(t, label, &detail);
                    }
                    // any mutating command changes the inventory
                    self.brew.fetch_inventory(ctx.clone());
                    self.brew.fetch_system(ctx.clone());
                }
                Msg::Inventory(result, ms) => {
                    self.fetch_ms = ms;
                    match result {
                        Ok(pkgs) => {
                            let outdated = pkgs.iter().filter(|p| p.outdated).count();
                            self.push_log(
                                t,
                                format!(
                                    "inventory :: {} formulae, {} casks, {} outdated in {:.0} ms",
                                    pkgs.iter().filter(|p| p.kind == Kind::Formula).count(),
                                    pkgs.iter().filter(|p| p.kind == Kind::Cask).count(),
                                    outdated,
                                    ms
                                ),
                                if outdated > 0 { Level::Warn } else { Level::Ok },
                            );
                            self.packages = pkgs;
                            self.last_error = None;
                        }
                        Err(e) => {
                            self.last_error = Some(e.clone());
                            self.fail(t, "inventory", &e);
                        }
                    }
                    self.dirty = true;
                }
                Msg::Search { query, result } => {
                    if self.search_pending.as_deref() == Some(query.as_str()) {
                        self.search_pending = None;
                    }
                    match result {
                        Ok(pkgs) => {
                            // mark hits that are installed locally
                            let mut pkgs = pkgs;
                            for p in &mut pkgs {
                                if let Some(local) = self
                                    .packages
                                    .iter()
                                    .find(|l| l.name == p.name && l.kind == p.kind)
                                {
                                    p.installed = local.installed.clone();
                                    p.outdated = local.outdated;
                                    p.pinned = local.pinned;
                                }
                            }
                            self.push_log(
                                t,
                                format!("search // {query} :: {} hits", pkgs.len()),
                                Level::Ok,
                            );
                            self.search_results = pkgs;
                        }
                        Err(e) => self.fail(t, format!("search // {query}"), &e),
                    }
                    self.dirty = true;
                }
                Msg::System(info) => self.system = info,
            }
        }
    }

    fn confirm(&mut self, c: Confirm) {
        self.dialog = Some(OpenDialog {
            state: DialogState::Confirm(c),
            closing: false,
        });
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action, t: f64) {
        match action {
            Action::SetView(v) => {
                if self.view != v {
                    self.view = v;
                    self.table.selected = None;
                    self.dirty = true;
                }
            }
            Action::Select(s) => {
                self.table.selected = s;
                self.table.scroll_to_selected = true;
            }
            Action::Refresh => {
                self.push_log(t, "refresh // inventory", Level::Info);
                self.brew.fetch_inventory(ctx.clone());
                self.brew.fetch_system(ctx.clone());
            }
            Action::Update => {
                self.apply(ctx, Action::Run("update".into(), vec!["update".into()]), t)
            }
            Action::UpgradeAll => {
                let n = self
                    .packages
                    .iter()
                    .filter(|p| p.outdated && !p.pinned)
                    .count();
                self.confirm(Confirm {
                    title: "Upgrade all".into(),
                    line: format!("{n} outdated packages"),
                    note: "PINNED PACKAGES ARE SKIPPED".into(),
                    verb: "UPGRADE ALL".into(),
                    danger: false,
                    label: "upgrade // all".into(),
                    args: vec!["upgrade".into()],
                });
            }
            Action::Upgrade(name, kind) => self.confirm(Confirm {
                title: "Upgrade".into(),
                line: name.clone(),
                note: "DOWNLOADS AND REPLACES THE INSTALLED VERSION".into(),
                verb: "UPGRADE".into(),
                danger: false,
                label: format!("upgrade // {name}"),
                args: vec!["upgrade".into(), kind.flag().into(), name],
            }),
            Action::Install(name, kind) => self.confirm(Confirm {
                title: "Install".into(),
                line: name.clone(),
                note: "DEPENDENCIES ARE INSTALLED AS NEEDED".into(),
                verb: "INSTALL".into(),
                danger: false,
                label: format!("install // {name}"),
                args: vec!["install".into(), kind.flag().into(), name],
            }),
            Action::Uninstall(name, kind) => self.confirm(Confirm {
                title: "Uninstall".into(),
                line: name.clone(),
                note: "REMOVES THE PACKAGE FROM THIS MACHINE".into(),
                verb: "UNINSTALL".into(),
                danger: true,
                label: format!("uninstall // {name}"),
                args: vec!["uninstall".into(), kind.flag().into(), name],
            }),
            Action::TogglePin(name, pinned) => {
                let verb = if pinned { "unpin" } else { "pin" };
                self.apply(
                    ctx,
                    Action::Run(format!("{verb} // {name}"), vec![verb.into(), name]),
                    t,
                );
            }
            Action::Homepage(url) => {
                self.push_log(t, format!("open // {url}"), Level::Info);
                if let Err(e) = open::that_detached(&url) {
                    self.fail(t, "open // homepage", &e.to_string());
                }
            }
            Action::CopyName(name) => {
                ctx.copy_text(name);
                self.push_log(t, "name copied to clipboard", Level::Info);
            }
            Action::Search => {
                let q = self.search_query.trim().to_string();
                if q.len() < 2 {
                    return;
                }
                self.push_log(t, format!("search // {q}"), Level::Info);
                self.search_pending = Some(q.clone());
                self.brew.search(q, ctx.clone());
            }
            Action::Run(label, args) => {
                if self.brew.run(label.clone(), args.clone(), ctx.clone()) {
                    self.push_log(t, format!("$ brew {}", args.join(" ")), Level::Info);
                } else {
                    self.fail(t, label, "another brew command is still running");
                }
            }
            Action::CloseDialog => {
                if let Some(d) = &mut self.dialog {
                    d.closing = true;
                }
            }
            Action::ConfirmDialog => {
                let Some(OpenDialog {
                    state: DialogState::Confirm(c),
                    closing,
                }) = &mut self.dialog
                else {
                    return;
                };
                if *closing {
                    return;
                }
                *closing = true;
                let (label, args) = (c.label.clone(), c.args.clone());
                self.apply(ctx, Action::Run(label, args), t);
            }
        }
    }

    fn handle_keys(&self, ui: &Ui, actions: &mut Vec<Action>) {
        if self.dialog.is_some() || ui.memory(|m| m.focused().is_some()) {
            return;
        }
        ui.input(|i| {
            let cmd = i.modifiers.command;
            if i.key_pressed(Key::ArrowDown) || i.key_pressed(Key::ArrowUp) {
                let dir: isize = if i.key_pressed(Key::ArrowDown) { 1 } else { -1 };
                let next = match self.table.selected {
                    Some(p) => (p as isize + dir).clamp(0, self.rows.len() as isize - 1) as usize,
                    None => 0,
                };
                if !self.rows.is_empty() {
                    actions.push(Action::Select(Some(next)));
                }
            }
            if i.key_pressed(Key::Enter) {
                if let Some(p) = self.selected_package() {
                    if !p.homepage.is_empty() {
                        actions.push(Action::Homepage(p.homepage.clone()));
                    }
                }
            }
            if cmd && i.key_pressed(Key::Backspace) {
                if let Some(p) = self.selected_package() {
                    if p.is_installed() {
                        actions.push(Action::Uninstall(p.name.clone(), p.kind));
                    }
                }
            }
            if cmd && i.key_pressed(Key::R) {
                actions.push(Action::Refresh);
            }
            for (n, key) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4]
                .iter()
                .enumerate()
            {
                if cmd && i.key_pressed(*key) {
                    actions.push(Action::SetView(View::ALL[n]));
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------- UI

impl eframe::App for BrewApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.devshot.tick(ui.ctx());
        let t = ui.input(|i| i.time);
        let ctx = ui.ctx().clone();
        self.poll(&ctx, t);
        if self.dirty {
            self.rebuild_rows();
        }
        let pal = palette(ui.ctx());
        let fps = 1.0 / ui.input(|i| i.stable_dt).max(1e-3);

        let mut actions: Vec<Action> = Vec::new();
        if self.dialog.is_none() {
            if let Some((success, line)) = self.notice_queue.pop_front() {
                self.dialog = Some(OpenDialog {
                    state: DialogState::Notice { success, line },
                    closing: false,
                });
            }
        }
        self.dev_frame += 1;
        if let Some(args) = self.dev_run.take() {
            let args: Vec<String> = args.split_whitespace().map(String::from).collect();
            actions.push(Action::Run(format!("dev // {}", args.join(" ")), args));
        }
        if let Some(q) = self.dev_search.take() {
            self.search_query = q;
            actions.push(Action::SetView(View::Search));
            actions.push(Action::Search);
        }
        if let Some(kind) = self.dev_dialog.take_if(|_| !self.rows.is_empty()) {
            let first = self.rows[0];
            let p = &self.source()[first];
            actions.push(Action::Select(Some(0)));
            actions.push(match kind.as_str() {
                "uninstall" => Action::Uninstall(p.name.clone(), p.kind),
                "upgrade" => Action::UpgradeAll,
                "error" => {
                    self.notice_queue
                        .push_back((false, "upgrade // cmake".into()));
                    Action::Select(Some(0))
                }
                "success" => {
                    self.notice_queue
                        .push_back((true, "upgrade // cmake".into()));
                    Action::Select(Some(0))
                }
                _ => Action::Select(Some(0)),
            });
        }
        if self.dev_close_frame == Some(self.dev_frame) && self.dialog.is_some() {
            actions.push(Action::CloseDialog);
        }
        self.handle_keys(ui, &mut actions);

        let outdated = self.packages.iter().filter(|p| p.outdated).count();
        let (link_text, link_color) = match &self.last_error {
            None => ("BREW LINK OK", pal.ok),
            Some(_) => ("BREW LINK FAILED", pal.danger),
        };
        let mut shell = Shell::new("FUIDE Brew")
            .subtitle(format!(
                "homebrew {} :: {}",
                if self.system.version.is_empty() {
                    "--"
                } else {
                    &self.system.version
                },
                self.system.prefix.display()
            ))
            .status_left(format!(
                "{} :: {} ROWS :: {} OUTDATED :: {:.0} FPS :: INVENTORY {:.0} MS",
                fuide::fmt::uptime(t),
                self.rows.len(),
                outdated,
                fps,
                self.fetch_ms
            ))
            .lamp(link_text, link_color, false);
        if self.brew.fetching() {
            shell = shell.lamp("INVENTORY", pal.warn, true);
        }
        if self.search_pending.is_some() {
            shell = shell.lamp("SEARCHING", pal.warn, true);
        }
        if let Some(label) = self.brew.running() {
            shell = shell.lamp(
                format!("BREW {}", label.split(" //").next().unwrap_or("")),
                pal.warn,
                true,
            );
        }

        shell.show(ui, |ui| {
            let c = ui.max_rect();
            let top = c.top() + 10.0;
            let log_rect = Rect::from_min_max(pos2(c.left(), c.bottom() - LOG_H), c.max);
            let body_bottom = log_rect.top() - GAP - 8.0;
            let left =
                Rect::from_min_max(pos2(c.left(), top), pos2(c.left() + LEFT_W, body_bottom));
            let right =
                Rect::from_min_max(pos2(c.right() - RIGHT_W, top), pos2(c.right(), body_bottom));
            let center = Rect::from_min_max(
                pos2(left.right() + GAP, top),
                pos2(right.left() - GAP, body_bottom),
            );
            let system = Rect::from_min_max(pos2(left.left(), left.bottom() - SYSTEM_H), left.max);
            let views = Rect::from_min_max(left.min, pos2(left.right(), system.top() - GAP - 8.0));
            let toolbar = Rect::from_min_size(
                pos2(center.left(), center.top() - 8.0),
                vec2(center.width(), TOOLBAR_H),
            );
            let listing =
                Rect::from_min_max(pos2(center.left(), toolbar.bottom() + 12.0), center.max);

            self.ui_views(ui, views, &mut actions);
            self.ui_system(ui, system);
            self.ui_toolbar(ui, toolbar, &mut actions);
            self.ui_listing(ui, listing, &mut actions);
            self.ui_inspector(ui, right, &mut actions);
            self.ui_log(ui, log_rect);
        });

        self.ui_dialog(&ctx, &mut actions);
        for a in actions {
            self.apply(&ctx, a, t);
        }
        if self.dirty {
            self.rebuild_rows();
        }
    }
}

impl BrewApp {
    fn ui_views(&self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        Panel::new("Views").show_rect(ui, rect, |ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            widgets::section_label(ui, "Packages");
            for v in View::ALL {
                let count = match v {
                    View::Installed => self.packages.len(),
                    View::Outdated => self.packages.iter().filter(|p| p.outdated).count(),
                    View::Casks => self
                        .packages
                        .iter()
                        .filter(|p| p.kind == Kind::Cask)
                        .count(),
                    View::Search => self.search_results.len(),
                };
                let label = format!("{}  {count}", v.label());
                let resp = widgets::nav_tab(ui, &label, self.view == v);
                if v == View::Outdated && count > 0 {
                    // warning dot at the right edge: something needs attention
                    let r = resp.rect;
                    ui.painter()
                        .circle_filled(pos2(r.right() - 12.0, r.center().y), 3.0, pal.warn);
                }
                if resp.clicked() {
                    actions.push(Action::SetView(v));
                }
            }
        });
    }

    fn ui_system(&self, ui: &mut Ui, rect: Rect) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let total = self.packages.len().max(1);
        let outdated = self.packages.iter().filter(|p| p.outdated).count();
        let current = 1.0 - outdated as f32 / total as f32;
        let color = if outdated == 0 {
            pal.ok
        } else if outdated < 5 {
            pal.warn
        } else {
            pal.danger
        };
        Panel::new("System")
            .padding(12.0, 14.0)
            .show_rect(ui, rect, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal(|ui| {
                    widgets::arc_gauge(ui, 30.0, current, "current", color);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.add_space(8.0);
                        widgets::readout(
                            ui,
                            "formulae",
                            &self
                                .packages
                                .iter()
                                .filter(|p| p.kind == Kind::Formula)
                                .count()
                                .to_string(),
                            None,
                        );
                        widgets::readout(
                            ui,
                            "casks",
                            &self
                                .packages
                                .iter()
                                .filter(|p| p.kind == Kind::Cask)
                                .count()
                                .to_string(),
                            None,
                        );
                        widgets::readout(ui, "outdated", &outdated.to_string(), Some(color));
                        widgets::readout(
                            ui,
                            "pinned",
                            &self
                                .packages
                                .iter()
                                .filter(|p| p.pinned)
                                .count()
                                .to_string(),
                            None,
                        );
                    });
                });
                widgets::rule(ui);
                widgets::readout(
                    ui,
                    "cellar",
                    &self
                        .system
                        .cellar_kb
                        .map(brew::fmt_kb)
                        .unwrap_or_else(|| "--".into()),
                    None,
                );
                widgets::readout(
                    ui,
                    "caskroom",
                    &self
                        .system
                        .caskroom_kb
                        .map(brew::fmt_kb)
                        .unwrap_or_else(|| "--".into()),
                    None,
                );
                widgets::readout(
                    ui,
                    "updated",
                    &brew::fmt_time(self.system.last_update),
                    None,
                );
                let (fr, _) = ui.allocate_exact_size(
                    vec2(ui.available_width(), ts.small + 6.0),
                    Sense::hover(),
                );
                ui.painter().text(
                    pos2(fr.left(), fr.center().y),
                    Align2::LEFT_CENTER,
                    "CMD+R REFRESH  CMD+1..4 VIEWS",
                    mono(ts.small),
                    pal.text_dim,
                );
            });
    }

    fn ui_toolbar(&mut self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let busy = self.brew.running().is_some();
        const FILTER_W: f32 = 180.0;
        let left_rect = Rect::from_min_max(
            rect.min,
            pos2(rect.right() - FILTER_W - 12.0, rect.bottom()),
        );
        let right_rect = Rect::from_min_max(pos2(rect.right() - FILTER_W, rect.top()), rect.max);

        let mut left = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("toolbar-left")
                .max_rect(left_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        left.set_clip_rect(left_rect.intersect(ui.clip_rect()));
        {
            let ui = &mut left;
            ui.spacing_mut().item_spacing.x = 6.0;
            if widgets::icon_button(
                ui,
                vec2(32.0, ts.row),
                widgets::Icon::ArrowDown,
                !self.brew.fetching(),
            )
            .on_hover_text("Reload inventory")
            .clicked()
            {
                actions.push(Action::Refresh);
            }
            if widgets::button(ui, vec2(84.0, ts.row), "UPDATE", !busy).clicked() {
                actions.push(Action::Update);
            }
            let n = self
                .packages
                .iter()
                .filter(|p| p.outdated && !p.pinned)
                .count();
            if widgets::button_colored(
                ui,
                vec2(150.0, ts.row),
                &format!("UPGRADE ALL  {n}"),
                !busy && n > 0,
                pal.warn,
            )
            .clicked()
            {
                actions.push(Action::UpgradeAll);
            }
            if self.view == View::Search {
                ui.add_space(10.0);
                let resp = widgets::text_input(
                    ui,
                    240.0,
                    &mut self.search_query,
                    "search formulae and casks",
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    actions.push(Action::Search);
                }
                if widgets::button(
                    ui,
                    vec2(84.0, ts.row),
                    "SEARCH",
                    self.search_pending.is_none(),
                )
                .clicked()
                {
                    actions.push(Action::Search);
                }
            } else {
                ui.add_space(10.0);
                let (r, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), ts.row), Sense::hover());
                let text = match self.view {
                    View::Installed => "ALL INSTALLED PACKAGES".to_string(),
                    View::Outdated => "PACKAGES WITH A NEWER VERSION AVAILABLE".to_string(),
                    View::Casks => "GUI APPLICATIONS (CASKS)".to_string(),
                    View::Search => String::new(),
                };
                ui.painter().with_clip_rect(r).text(
                    pos2(r.left(), r.center().y),
                    Align2::LEFT_CENTER,
                    text,
                    mono(ts.label),
                    pal.text_dim,
                );
            }
        }

        let want_focus = ui.input(|i| i.modifiers.command && i.key_pressed(Key::F));
        let mut right = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("toolbar-right")
                .max_rect(right_rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        {
            let ui = &mut right;
            let resp = widgets::text_input(ui, FILTER_W, &mut self.filter, "filter");
            if resp.changed() {
                self.dirty = true;
            }
            if want_focus {
                resp.request_focus();
            }
            if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                self.filter.clear();
                self.dirty = true;
                resp.surrender_focus();
            }
        }
    }

    fn ui_listing(&mut self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let title = match self.view {
            View::Installed => "Installed",
            View::Outdated => "Outdated",
            View::Casks => "Casks",
            View::Search => "Search results",
        };
        let tag = format!("{} items", self.rows.len());
        let columns = [
            Column::new("NAME", Width::Flex),
            Column::new("VERSION", Width::Chars(13.0)),
            Column::new("LATEST", Width::Chars(13.0)),
            Column::new("KIND", Width::Chars(8.0)),
            Column::new("STATUS", Width::Chars(10.0)),
        ];
        let src: &[Package] = if self.view == View::Search {
            &self.search_results
        } else {
            &self.packages
        };
        let rows = &self.rows;
        let mut state = std::mem::take(&mut self.table);
        let resp = Panel::new(title)
            .tag(tag, pal.text_dim)
            .padding(8.0, 14.0)
            .show_rect(ui, rect, |ui| {
                table::table(
                    ui,
                    "packages",
                    &columns,
                    rows.len(),
                    &mut state,
                    |row, col| {
                        let p = &src[rows[row]];
                        match col {
                            0 => Cell::text(&p.name).color(if p.is_installed() {
                                pal.text
                            } else {
                                pal.text_dim
                            }),
                            1 => Cell::dim(p.installed_version()),
                            2 => Cell::dim(&p.latest).color(if p.outdated {
                                pal.warn
                            } else {
                                pal.text_dim
                            }),
                            3 => Cell::tag(p.kind.tag()).color(if p.kind == Kind::Cask {
                                pal.accent_dim
                            } else {
                                pal.text_dim
                            }),
                            _ => {
                                let st = p.status();
                                let color = match st {
                                    Status::Outdated => pal.warn,
                                    Status::Pinned => pal.accent,
                                    Status::Current => pal.ok.gamma_multiply(0.8),
                                    Status::Dependency => pal.text_dim,
                                    Status::Available => pal.text_dim,
                                };
                                Cell::tag(st.tag()).color(color)
                            }
                        }
                    },
                )
            });
        self.table = state;
        if resp.sort_changed {
            self.dirty = true;
        }
        if let Some(r) = resp.double_clicked {
            if let Some(p) = rows.get(r).and_then(|&i| src.get(i)) {
                if !p.homepage.is_empty() {
                    actions.push(Action::Homepage(p.homepage.clone()));
                }
            }
        }
    }

    fn ui_inspector(&self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let busy = self.brew.running().is_some();
        let sel = self.selected_package();
        let tag = sel.map(|p| p.kind.tag()).unwrap_or("none");
        Panel::new("Package")
            .tag(tag, pal.text_dim)
            .show_rect(ui, rect, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let Some(p) = sel else {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("SELECT A PACKAGE")
                            .font(mono(ts.label))
                            .color(pal.text_dim),
                    );
                    return;
                };
                ui.add(
                    egui::Label::new(
                        RichText::new(&p.name)
                            .font(mono(ts.data + 3.0))
                            .color(pal.accent),
                    )
                    .wrap(),
                );
                if !p.desc.is_empty() {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&p.desc).font(mono(ts.label)).color(pal.text),
                        )
                        .wrap(),
                    );
                }
                ui.add_space(6.0);
                widgets::rule(ui);
                let st = p.status();
                widgets::readout(
                    ui,
                    "status",
                    st.tag(),
                    Some(match st {
                        Status::Outdated => pal.warn,
                        Status::Pinned => pal.accent,
                        Status::Current => pal.ok,
                        _ => pal.text_dim,
                    }),
                );
                widgets::readout(ui, "installed", p.installed_version(), None);
                widgets::readout(
                    ui,
                    "latest",
                    if p.latest.is_empty() { "--" } else { &p.latest },
                    Some(if p.outdated { pal.warn } else { pal.text }),
                );
                widgets::readout(
                    ui,
                    "tap",
                    if p.tap.is_empty() { "--" } else { &p.tap },
                    None,
                );
                widgets::readout(ui, "license", p.license.as_deref().unwrap_or("--"), None);
                if p.is_installed() {
                    widgets::readout(ui, "installed on", &brew::fmt_time(p.installed_time), None);
                    if p.kind == Kind::Formula {
                        widgets::readout(
                            ui,
                            "requested",
                            if p.on_request {
                                "YES"
                            } else {
                                "NO (DEPENDENCY)"
                            },
                            None,
                        );
                    }
                }
                if p.kind == Kind::Cask {
                    widgets::readout(
                        ui,
                        "auto-updates",
                        if p.auto_updates { "YES" } else { "NO" },
                        None,
                    );
                }
                if p.deprecated {
                    widgets::readout(ui, "deprecated", "YES", Some(pal.danger));
                }
                if !p.deps.is_empty() {
                    ui.add_space(6.0);
                    let (lr, _) = ui.allocate_exact_size(
                        vec2(ui.available_width(), ts.heading + 4.0),
                        Sense::hover(),
                    );
                    fuide::display_text(
                        ui.painter(),
                        pos2(lr.left(), lr.center().y),
                        Align2::LEFT_CENTER,
                        format!("DEPENDENCIES {}", p.deps.len()),
                        ts.heading,
                        pal.text_dim,
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(p.deps.join("  "))
                                .font(mono(ts.label))
                                .color(pal.text_dim),
                        )
                        .wrap(),
                    );
                }
                if !p.homepage.is_empty() {
                    ui.add_space(6.0);
                    widgets::rule(ui);
                    let (lr, _) = ui.allocate_exact_size(
                        vec2(ui.available_width(), ts.heading + 4.0),
                        Sense::hover(),
                    );
                    fuide::display_text(
                        ui.painter(),
                        pos2(lr.left(), lr.center().y),
                        Align2::LEFT_CENTER,
                        "HOMEPAGE",
                        ts.heading,
                        pal.text_dim,
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(&p.homepage)
                                .font(mono(ts.label))
                                .color(pal.text_dim),
                        )
                        .wrap(),
                    );
                }
                if let Some(cv) = &p.caveats {
                    ui.add_space(6.0);
                    let (lr, _) = ui.allocate_exact_size(
                        vec2(ui.available_width(), ts.heading + 4.0),
                        Sense::hover(),
                    );
                    fuide::display_text(
                        ui.painter(),
                        pos2(lr.left(), lr.center().y),
                        Align2::LEFT_CENTER,
                        "CAVEATS",
                        ts.heading,
                        pal.warn,
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(cv.trim())
                                .font(mono(ts.small))
                                .color(pal.text_dim),
                        )
                        .wrap(),
                    );
                }

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if widgets::button(ui, vec2(96.0, ts.row), "HOMEPAGE", !p.homepage.is_empty())
                        .clicked()
                    {
                        actions.push(Action::Homepage(p.homepage.clone()));
                    }
                    if widgets::button(ui, vec2(72.0, ts.row), "COPY", true).clicked() {
                        actions.push(Action::CopyName(p.name.clone()));
                    }
                    if p.kind == Kind::Formula && p.is_installed() {
                        let label = if p.pinned { "UNPIN" } else { "PIN" };
                        if widgets::button(ui, vec2(72.0, ts.row), label, !busy).clicked() {
                            actions.push(Action::TogglePin(p.name.clone(), p.pinned));
                        }
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if p.is_installed() {
                        if widgets::button_colored(
                            ui,
                            vec2(96.0, ts.row),
                            "UPGRADE",
                            !busy && p.outdated && !p.pinned,
                            pal.warn,
                        )
                        .clicked()
                        {
                            actions.push(Action::Upgrade(p.name.clone(), p.kind));
                        }
                        if widgets::button_colored(
                            ui,
                            vec2(104.0, ts.row),
                            "UNINSTALL",
                            !busy,
                            pal.danger,
                        )
                        .clicked()
                        {
                            actions.push(Action::Uninstall(p.name.clone(), p.kind));
                        }
                    } else if widgets::button(ui, vec2(96.0, ts.row), "INSTALL", !busy).clicked() {
                        actions.push(Action::Install(p.name.clone(), p.kind));
                    }
                });
            });
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
                let color = if c.danger { pal.danger } else { pal.warn };
                let resp = Dialog::new(&c.title)
                    .tag("brew", pal.text_dim)
                    .outline(color)
                    .width(460.0)
                    .show(ctx, open, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        ui.add(
                            egui::Label::new(
                                RichText::new(&c.line)
                                    .font(mono(ts.data + 2.0))
                                    .color(pal.accent),
                            )
                            .wrap(),
                        );
                        ui.add_space(2.0);
                        widgets::readout(
                            ui,
                            "command",
                            &format!("brew {}", c.args.join(" ")),
                            None,
                        );
                        ui.add_space(6.0);
                        widgets::rule(ui);
                        let (nr, _) = ui.allocate_exact_size(
                            vec2(ui.available_width(), ts.row),
                            Sense::hover(),
                        );
                        ui.painter().text(
                            pos2(nr.left() + 2.0, nr.center().y),
                            Align2::LEFT_CENTER,
                            &c.note,
                            mono(ts.label),
                            color,
                        );
                        ui.add_space(8.0);
                        fuide::dialog::button_row(
                            ui,
                            &[("CANCEL", pal.text_dim, true), (&c.verb, color, true)],
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
            DialogState::Notice { success, line } => {
                let (word, color) = if *success {
                    ("Success", pal.ok)
                } else {
                    ("Error", pal.danger)
                };
                let resp =
                    fuide::dialog::alert(ctx, open, word, line, "details :: brew output", color);
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

    fn ui_log(&self, ui: &mut Ui, rect: Rect) {
        let pal = palette(ui.ctx());
        Panel::new("Brew output")
            .tag(format!("{} lines", self.log.len()), pal.text_dim)
            .padding(8.0, 12.0)
            .show_rect(ui, rect, |ui| {
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
                    widgets::LogOrder::Chronological,
                );
            });
    }
}
