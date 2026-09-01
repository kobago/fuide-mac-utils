//! FUIDE File Manager — Finder-like browser with a tactical-console look.

use std::path::{Path, PathBuf};

use egui::{pos2, vec2, Align2, Key, Rect, RichText, ScrollArea, Sense, Stroke, Ui};
use fuide::widgets::{self, LogLine};
use fuide::Dialog;
use fuide::{
    mono, palette, theme, type_scale, Palette, PaletteKind, Panel, Settings, SettingsWindow, Shell,
};

use crate::fs::{self, DiskInfo, Entry, Loader, OpChannel};

const LEFT_W: f32 = 220.0;
const RIGHT_W: f32 = 300.0;
const GAP: f32 = 14.0;
const TOOLBAR_H: f32 = 32.0;
/// Default log panel height; the divider above it is draggable (`Settings::log_height`).
const LOG_H: f32 = 110.0;
const LOG_MIN: f32 = 60.0;
/// Space kept for the panels above the log when the divider is dragged up.
const BODY_MIN: f32 = 300.0;
const STORAGE_H: f32 = 152.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Kind,
    Size,
    Modified,
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

/// The open dialog (if any) plus whether it is fading out.
struct OpenDialog {
    state: DialogState,
    closing: bool,
}

/// Modal dialogs. Only one can be open; input beneath it is blocked.
enum DialogState {
    Rename {
        idx: usize,
        name: String,
        error: Option<String>,
        focus: bool,
    },
    Delete {
        idx: usize,
        permanent: bool,
    },
    /// Big `ERROR` card; details live in the event log.
    Error {
        line: String,
    },
}

enum Action {
    OpenRename(usize),
    OpenDelete(usize, bool),
    ConfirmRename,
    ConfirmDelete,
    CloseDialog,
    Navigate(PathBuf),
    Back,
    Forward,
    Up,
    Select(Option<usize>),
    Activate(usize),
    Open(PathBuf),
    Reveal(PathBuf),
    CopyPath(PathBuf),
    Sort(SortKey),
    Palette(PaletteKind),
    OpenSettings,
}

/// Settings file name (`Settings::path`).
const APP_ID: &str = "file-manager";

pub struct Explorer {
    loader: Loader,
    cwd: PathBuf,
    pending: Option<u64>,
    entries: Vec<Entry>,
    view: Vec<usize>,
    dirty: bool,
    selected: Option<usize>,
    history: Vec<PathBuf>,
    hist_pos: usize,
    sort_key: SortKey,
    sort_desc: bool,
    show_hidden: bool,
    filter: String,
    log: Vec<Event>,
    disk: Option<DiskInfo>,
    places: Vec<(String, PathBuf)>,
    volumes: Vec<(String, PathBuf)>,
    last_error: Option<String>,
    load_ms: f32,
    settings: Settings,
    settings_win: SettingsWindow,
    /// Where settings are saved; `None` = not persisted (tests, or no config dir).
    settings_path: Option<PathBuf>,
    /// Current log panel height (draggable divider).
    log_h: f32,
    scroll_to_selected: bool,
    dialog: Option<OpenDialog>,
    /// Errors waiting for the dialog slot (only one dialog at a time).
    error_queue: std::collections::VecDeque<String>,
    ops: OpChannel,
    /// After a reload, select the entry with this name (used after rename).
    select_after_load: Option<String>,
    /// Dev aid: `FUIDE_DEV_DIALOG=rename|trash|delete` opens that dialog on the first entry after load.
    dev_dialog: Option<String>,
    /// Dev aid: `FUIDE_DEV_DIALOG_CLOSE=<frame>` closes the dev dialog at that frame (fade-out shots).
    dev_close_frame: Option<u32>,
    dev_frame: u32,
    devshot: fuide::devshot::DevShot,
}

impl Explorer {
    /// Production entry point: settings from disk, start directory from the command line
    /// (`fuide-file-manager [DIR]`, default `$HOME`).
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = Settings::load(APP_ID).unwrap_or_else(|| Settings::new(PaletteKind::Cyan));
        let home = std::env::args()
            .nth(1)
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut app = Self::with_context(&cc.egui_ctx, home, settings);
        if let Some(path) = Settings::path(APP_ID) {
            app.persist_settings_to(path);
        }
        app
    }

    /// Save settings changes to `path` from now on (apps built with [`Self::with_context`] do
    /// not persist by default).
    pub fn persist_settings_to(&mut self, path: PathBuf) {
        self.settings_path = Some(path);
    }

    /// Build the app on any `egui::Context` (tests use a bare `Context::default()` or an
    /// `egui_kittest` harness) with explicit settings and start directory.
    pub fn with_context(ctx: &egui::Context, home: PathBuf, settings: Settings) -> Self {
        theme::install(ctx, settings.palette.palette(), theme::macos_cjk_fallback());
        settings.apply(ctx);
        let mut app = Self {
            loader: Loader::new(),
            cwd: home.clone(),
            pending: None,
            entries: Vec::new(),
            view: Vec::new(),
            dirty: true,
            selected: None,
            history: vec![home.clone()],
            hist_pos: 0,
            sort_key: SortKey::Name,
            sort_desc: false,
            show_hidden: false,
            filter: String::new(),
            log: Vec::new(),
            disk: None,
            places: fs::places(),
            volumes: fs::volumes(),
            last_error: None,
            load_ms: 0.0,
            log_h: settings.log_height.unwrap_or(LOG_H),
            settings,
            settings_win: SettingsWindow::default(),
            settings_path: None,
            scroll_to_selected: false,
            dialog: None,
            error_queue: std::collections::VecDeque::new(),
            ops: OpChannel::new(),
            select_after_load: None,
            dev_dialog: std::env::var("FUIDE_DEV_DIALOG").ok(),
            dev_close_frame: std::env::var("FUIDE_DEV_DIALOG_CLOSE")
                .ok()
                .and_then(|v| v.parse().ok()),
            dev_frame: 0,
            devshot: fuide::devshot::DevShot::from_env(),
        };
        app.push_log(0.0, "file manager online :: fs link established", Level::Ok);
        // Dev aid: `FUIDE_DEV_SETTINGS=1` opens the settings window at start (screenshots).
        if std::env::var_os("FUIDE_DEV_SETTINGS").is_some() {
            app.settings_win.open();
        }
        if let Ok(text) = std::env::var("FUIDE_DEV_LOG") {
            app.push_log(0.0, text, Level::Danger);
        }
        app.load(ctx, home);
        app
    }

    // ------------------------------------------------------------------ state

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

    /// Log an error and queue the `ERROR` dialog for it. `line` is the short operation label
    /// shown on the card; `detail` only goes to the log.
    fn fail(&mut self, t: f64, line: impl Into<String>, detail: &str) {
        let line = line.into();
        self.push_log(t, format!("{line} :: failed :: {detail}"), Level::Danger);
        self.error_queue.push_back(line);
    }

    fn load(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.cwd = path.clone();
        self.pending = Some(self.loader.request(path.clone(), ctx.clone()));
        self.disk = fs::disk_info(&path);
        self.selected = None;
        self.filter.clear();
    }

    fn navigate(&mut self, ctx: &egui::Context, path: PathBuf, t: f64) {
        if path == self.cwd {
            return;
        }
        self.history.truncate(self.hist_pos + 1);
        self.history.push(path.clone());
        self.hist_pos = self.history.len() - 1;
        self.push_log(t, format!("nav // {}", path.display()), Level::Info);
        self.load(ctx, path);
    }

    fn rebuild_view(&mut self) {
        let filter = self.filter.to_lowercase();
        let mut idx: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.show_hidden || !e.hidden)
            .filter(|(_, e)| filter.is_empty() || e.name.to_lowercase().contains(&filter))
            .map(|(i, _)| i)
            .collect();
        let entries = &self.entries;
        let key = self.sort_key;
        let desc = self.sort_desc;
        idx.sort_by(|&a, &b| {
            let (ea, eb) = (&entries[a], &entries[b]);
            // directories always first (Finder-like "folders on top")
            let dir = eb.is_dir.cmp(&ea.is_dir);
            if dir != std::cmp::Ordering::Equal {
                return dir;
            }
            let ord = match key {
                SortKey::Name => ea.name.to_lowercase().cmp(&eb.name.to_lowercase()),
                SortKey::Kind => ea.kind.tag().cmp(eb.kind.tag()),
                SortKey::Size => ea.size.cmp(&eb.size),
                SortKey::Modified => ea.modified.cmp(&eb.modified),
            }
            .then_with(|| ea.name.to_lowercase().cmp(&eb.name.to_lowercase()));
            if desc {
                ord.reverse()
            } else {
                ord
            }
        });
        self.view = idx;
        if let Some(s) = self.selected {
            if !self.view.contains(&s) {
                self.selected = None;
            }
        }
        self.dirty = false;
    }

    fn poll_ops(&mut self, ctx: &egui::Context, t: f64) {
        while let Some(op) = self.ops.poll() {
            match op.result {
                Ok(()) => {
                    self.push_log(t, format!("{} :: done", op.label), Level::Ok);
                    self.load(ctx, self.cwd.clone());
                }
                Err(e) => self.fail(t, op.label, &e),
            }
        }
    }

    fn poll_loader(&mut self, t: f64) {
        while let Some(listing) = self.loader.poll() {
            if listing.generation != self.loader.current() {
                continue; // stale
            }
            self.pending = None;
            self.load_ms = listing.elapsed_ms;
            match listing.result {
                Ok(entries) => {
                    let hidden = entries.iter().filter(|e| e.hidden).count();
                    self.push_log(
                        t,
                        format!(
                            "scan complete :: {} items ({} hidden) in {:.1} ms",
                            entries.len(),
                            hidden,
                            listing.elapsed_ms
                        ),
                        Level::Ok,
                    );
                    self.entries = entries;
                    self.last_error = None;
                    if let Some(name) = self.select_after_load.take() {
                        self.selected = self.entries.iter().position(|e| e.name == name);
                        self.scroll_to_selected = true;
                    }
                }
                Err(e) => {
                    self.entries.clear();
                    self.last_error = Some(e.clone());
                    let dir = self
                        .cwd
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "/".into());
                    self.fail(t, format!("access denied // {dir}"), &e);
                }
            }
            self.dirty = true;
        }
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action, t: f64) {
        match action {
            Action::OpenRename(idx) => {
                if let Some(e) = self.entries.get(idx) {
                    self.dialog = Some(OpenDialog {
                        state: DialogState::Rename {
                            idx,
                            name: e.name.clone(),
                            error: None,
                            focus: true,
                        },
                        closing: false,
                    });
                }
            }
            Action::OpenDelete(idx, permanent) => {
                if idx < self.entries.len() {
                    self.dialog = Some(OpenDialog {
                        state: DialogState::Delete { idx, permanent },
                        closing: false,
                    });
                }
            }
            Action::CloseDialog => {
                if let Some(d) = &mut self.dialog {
                    d.closing = true;
                }
            }
            Action::ConfirmRename => {
                let Some(OpenDialog {
                    state:
                        DialogState::Rename {
                            idx, name, error, ..
                        },
                    closing: false,
                }) = &mut self.dialog
                else {
                    return;
                };
                let entry = self.entries[*idx].clone();
                match fs::rename(&entry.path, name) {
                    Ok(target) => {
                        let new_name = target
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if let Some(d) = &mut self.dialog {
                            d.closing = true;
                        }
                        self.push_log(
                            t,
                            format!("rename // {} -> {}", entry.name, new_name),
                            Level::Ok,
                        );
                        self.select_after_load = Some(new_name);
                        self.load(ctx, self.cwd.clone());
                    }
                    Err(e) => *error = Some(e),
                }
            }
            Action::ConfirmDelete => {
                let Some(OpenDialog {
                    state: DialogState::Delete { idx, permanent },
                    closing,
                }) = &mut self.dialog
                else {
                    return;
                };
                if *closing {
                    return;
                }
                let (idx, permanent) = (*idx, *permanent);
                *closing = true;
                let entry = self.entries[idx].clone();
                let path = entry.path.clone();
                if permanent {
                    self.push_log(t, format!("delete // {}", entry.name), Level::Danger);
                    self.ops.spawn(
                        format!("delete // {}", entry.name),
                        ctx.clone(),
                        move || fs::remove(&path),
                    );
                } else {
                    self.push_log(t, format!("trash // {}", entry.name), Level::Warn);
                    self.ops
                        .spawn(format!("trash // {}", entry.name), ctx.clone(), move || {
                            fs::trash(&path)
                        });
                }
            }
            Action::Navigate(p) => self.navigate(ctx, p, t),
            Action::Back => {
                if self.hist_pos > 0 {
                    self.hist_pos -= 1;
                    let p = self.history[self.hist_pos].clone();
                    self.push_log(t, format!("back // {}", p.display()), Level::Info);
                    self.load(ctx, p);
                }
            }
            Action::Forward => {
                if self.hist_pos + 1 < self.history.len() {
                    self.hist_pos += 1;
                    let p = self.history[self.hist_pos].clone();
                    self.push_log(t, format!("fwd // {}", p.display()), Level::Info);
                    self.load(ctx, p);
                }
            }
            Action::Up => {
                if let Some(parent) = self.cwd.parent() {
                    self.navigate(ctx, parent.to_path_buf(), t);
                }
            }
            Action::Select(s) => {
                self.selected = s;
                self.scroll_to_selected = true;
            }
            Action::Activate(i) => {
                let e = self.entries[i].clone();
                if e.navigates() {
                    self.navigate(ctx, e.path, t);
                } else {
                    self.apply(ctx, Action::Open(e.path), t);
                }
            }
            Action::Open(p) => {
                self.push_log(t, format!("open // {}", p.display()), Level::Ok);
                if let Err(e) = open::that_detached(&p) {
                    let name = p
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.fail(t, format!("open // {name}"), &e.to_string());
                }
            }
            Action::Reveal(p) => {
                self.push_log(t, format!("reveal // {}", p.display()), Level::Info);
                let _ = std::process::Command::new("open").arg("-R").arg(&p).spawn();
            }
            Action::CopyPath(p) => {
                ctx.copy_text(p.display().to_string());
                self.push_log(t, "path copied to clipboard", Level::Info);
            }
            Action::Sort(k) => {
                if self.sort_key == k {
                    self.sort_desc = !self.sort_desc;
                } else {
                    self.sort_key = k;
                    self.sort_desc = false;
                }
                self.dirty = true;
            }
            Action::Palette(kind) => {
                self.settings.palette = kind;
                self.settings.apply(ctx);
                self.settings_changed(t);
            }
            Action::OpenSettings => self.settings_win.open(),
        }
    }

    /// Log + persist after the settings changed (shortcut or settings window).
    fn settings_changed(&mut self, t: f64) {
        let s = &self.settings;
        self.push_log(
            t,
            format!(
                "settings // palette {} :: {} :: {}",
                s.palette.name(),
                if s.chamfer { "chamfer" } else { "square" },
                if s.compact { "compact" } else { "normal" }
            ),
            Level::Warn,
        );
        self.save_settings(t);
    }

    /// Persist the settings (silently; errors go to the log).
    fn save_settings(&mut self, t: f64) {
        if let Some(path) = self.settings_path.clone() {
            if let Err(e) = self.settings.save_to(&path) {
                self.push_log(t, format!("settings // save failed: {e}"), Level::Danger);
            }
        }
    }

    fn handle_keys(&self, ui: &Ui, actions: &mut Vec<Action>) {
        if self.dialog.is_some() {
            return; // the dialog owns all input
        }
        // Mouse back / forward buttons (winit Back/Forward -> egui Extra1/Extra2) work regardless of focus.
        ui.input(|i| {
            if i.pointer.button_pressed(egui::PointerButton::Extra1) {
                actions.push(Action::Back);
            }
            if i.pointer.button_pressed(egui::PointerButton::Extra2) {
                actions.push(Action::Forward);
            }
        });
        if ui.memory(|m| m.focused().is_some()) {
            return; // text field owns the keyboard
        }
        ui.input(|i| {
            let cmd = i.modifiers.command;
            if i.key_pressed(Key::ArrowDown) || i.key_pressed(Key::ArrowUp) {
                let dir: isize = if i.key_pressed(Key::ArrowDown) { 1 } else { -1 };
                let pos = self
                    .selected
                    .and_then(|s| self.view.iter().position(|&v| v == s));
                let next = match pos {
                    Some(p) => (p as isize + dir).clamp(0, self.view.len() as isize - 1) as usize,
                    None => 0,
                };
                if let Some(&e) = self.view.get(next) {
                    actions.push(Action::Select(Some(e)));
                }
            }
            if i.key_pressed(Key::Enter) {
                if let Some(s) = self.selected {
                    actions.push(Action::Activate(s));
                }
            }
            if cmd && i.key_pressed(Key::Backspace) {
                // Finder: Cmd+Backspace = move to Trash; Cmd+Option+Backspace = delete immediately
                if let Some(s) = self.selected {
                    actions.push(Action::OpenDelete(s, i.modifiers.alt));
                }
            } else if i.key_pressed(Key::Backspace) || (cmd && i.key_pressed(Key::ArrowUp)) {
                actions.push(Action::Up);
            }
            if cmd && i.key_pressed(Key::R) {
                if let Some(s) = self.selected {
                    actions.push(Action::OpenRename(s));
                }
            }
            if cmd && i.key_pressed(Key::OpenBracket) {
                actions.push(Action::Back);
            }
            if cmd && i.key_pressed(Key::CloseBracket) {
                actions.push(Action::Forward);
            }
            for (kind, key) in PaletteKind::ALL
                .into_iter()
                .zip([Key::Num1, Key::Num2, Key::Num3])
            {
                if cmd && i.key_pressed(key) {
                    actions.push(Action::Palette(kind));
                }
            }
            if cmd && i.key_pressed(Key::Comma) {
                actions.push(Action::OpenSettings);
            }
        });
    }
}

// ---------------------------------------------------------------------------- UI

impl eframe::App for Explorer {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.devshot.tick(ui.ctx());
        let t = ui.input(|i| i.time);
        let ctx = ui.ctx().clone();
        self.poll_ops(&ctx, t);
        self.poll_loader(t);
        if self.dirty {
            self.rebuild_view();
        }
        let pal = palette(ui.ctx());
        let fps = 1.0 / ui.input(|i| i.stable_dt).max(1e-3);
        let hidden = self.entries.iter().filter(|e| e.hidden).count();

        let mut actions: Vec<Action> = Vec::new();
        if let Some(kind) = self.dev_dialog.take_if(|_| !self.view.is_empty()) {
            let first = self.view[0];
            actions.push(Action::Select(Some(first)));
            actions.push(match kind.as_str() {
                "rename" => Action::OpenRename(first),
                "error" => {
                    self.error_queue.push_back("delete // immutable.txt".into());
                    Action::Select(Some(first))
                }
                "delete" => Action::OpenDelete(first, true),
                _ => Action::OpenDelete(first, false),
            });
        }
        if self.dialog.is_none() {
            if let Some(line) = self.error_queue.pop_front() {
                self.dialog = Some(OpenDialog {
                    state: DialogState::Error { line },
                    closing: false,
                });
            }
        }
        self.dev_frame += 1;
        if self.dev_close_frame == Some(self.dev_frame) && self.dialog.is_some() {
            actions.push(Action::CloseDialog);
        }
        self.handle_keys(ui, &mut actions);

        let (link_text, link_color) = match &self.last_error {
            None => ("FS LINK OK", pal.ok),
            Some(_) => ("FS LINK DENIED", pal.danger),
        };
        let mut shell = Shell::new("FUIDE File Manager")
            .subtitle(format!(
                "v0.1 :: macOS :: {}",
                self.cwd
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".into())
            ))
            .status_left(format!(
                "{} :: {} ITEMS :: {} HIDDEN :: {:.0} FPS :: SCAN {:.1} MS",
                fuide::fmt::uptime(t),
                self.view.len(),
                hidden,
                fps,
                self.load_ms
            ))
            .lamp(link_text, link_color, false)
            .settings_button(true);
        if self.pending.is_some() {
            shell = shell.lamp("SCANNING", pal.warn, true);
        }
        if self.ops.busy() {
            shell = shell.lamp("FS WRITE", pal.warn, true);
        }

        let mut log_resized = false;
        let out = shell.show_full(ui, |ui| {
            let c = ui.max_rect();
            let top = c.top() + 10.0; // room for title chips above the first panels
            let log_max = c.height() - BODY_MIN;
            self.log_h = self.log_h.clamp(LOG_MIN, log_max.max(LOG_MIN));
            let log_rect = Rect::from_min_max(pos2(c.left(), c.bottom() - self.log_h), c.max);
            let body_bottom = log_rect.top() - GAP - 8.0;

            let left =
                Rect::from_min_max(pos2(c.left(), top), pos2(c.left() + LEFT_W, body_bottom));
            let right =
                Rect::from_min_max(pos2(c.right() - RIGHT_W, top), pos2(c.right(), body_bottom));
            let center = Rect::from_min_max(
                pos2(left.right() + GAP, top),
                pos2(right.left() - GAP, body_bottom),
            );
            let storage =
                Rect::from_min_max(pos2(left.left(), left.bottom() - STORAGE_H), left.max);
            let locations =
                Rect::from_min_max(left.min, pos2(left.right(), storage.top() - GAP - 8.0));
            let toolbar = Rect::from_min_size(
                pos2(center.left(), center.top() - 8.0),
                vec2(center.width(), TOOLBAR_H),
            );
            let listing =
                Rect::from_min_max(pos2(center.left(), toolbar.bottom() + 12.0), center.max);

            self.ui_locations(ui, locations, &mut actions);
            self.ui_storage(ui, storage);
            self.ui_toolbar(ui, toolbar, &mut actions);
            self.ui_listing(ui, listing, &mut actions);
            self.ui_inspector(ui, right, &mut actions);
            self.ui_log(ui, log_rect, t);
            // draggable divider in the gap above the log panel
            let strip =
                Rect::from_min_max(pos2(c.left(), body_bottom), pos2(c.right(), log_rect.top()));
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
        });
        if out.settings_clicked {
            actions.push(Action::OpenSettings);
        }
        if log_resized {
            self.settings.log_height = Some(self.log_h.round());
            self.save_settings(t);
        }

        self.ui_dialog(&ctx, &mut actions);
        for a in actions {
            self.apply(&ctx, a, t);
        }
        if self.dirty {
            self.rebuild_view();
        }
        // Settings window (child viewport) last: it pauses this viewport while it draws.
        if self
            .settings_win
            .show(&ctx, &mut self.settings, "FUIDE File Manager")
        {
            self.settings_changed(t);
        }
    }
}

impl Explorer {
    fn ui_locations(&self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        Panel::new("Locations").show_rect(ui, rect, |ui| {
            ScrollArea::vertical()
                .id_salt("locations")
                .auto_shrink([false, false])
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    widgets::section_label(ui, "Places");
                    for (name, path) in &self.places {
                        if widgets::nav_tab(ui, name, &self.cwd == path).clicked() {
                            actions.push(Action::Navigate(path.clone()));
                        }
                    }
                    ui.add_space(6.0);
                    widgets::section_label(ui, "Volumes");
                    for (name, path) in &self.volumes {
                        if widgets::nav_tab(ui, name, self.cwd.starts_with(path)).clicked() {
                            actions.push(Action::Navigate(path.clone()));
                        }
                    }
                });
        });
    }

    fn ui_storage(&self, ui: &mut Ui, rect: Rect) {
        let pal = palette(ui.ctx());
        let tag = self
            .cwd
            .ancestors()
            .find(|a| self.volumes.iter().any(|(_, v)| v == a))
            .and_then(|a| a.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "system".into());
        Panel::new("Storage")
            .tag(tag, pal.text_dim)
            .padding(12.0, 14.0)
            .show_rect(ui, rect, |ui| {
                let Some(d) = self.disk else {
                    ui.label(
                        RichText::new("NO VOLUME DATA")
                            .font(mono(type_scale(ui.ctx()).label))
                            .color(pal.text_dim),
                    );
                    return;
                };
                let used = d.used_fraction();
                let color = if used > 0.92 {
                    pal.danger
                } else if used > 0.80 {
                    pal.warn
                } else {
                    pal.accent
                };
                ui.horizontal(|ui| {
                    widgets::arc_gauge(ui, 32.0, used, "used", color);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.add_space(10.0);
                        widgets::readout(ui, "total", &fs::fmt_size(d.total), None);
                        widgets::readout(ui, "free", &fs::fmt_size(d.free), Some(color));
                        widgets::readout(ui, "used", &fs::fmt_size(d.total - d.free), None);
                        let (r, _) = ui
                            .allocate_exact_size(vec2(ui.available_width(), 10.0), Sense::hover());
                        widgets::segment_bar(ui.painter(), r, used, color, pal.accent_dim);
                    });
                });
            });
    }

    fn ui_toolbar(&mut self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        const FILTER_W: f32 = 160.0;
        const RIGHT_W: f32 = FILTER_W + 6.0 + 90.0;
        let left_rect =
            Rect::from_min_max(rect.min, pos2(rect.right() - RIGHT_W - 12.0, rect.bottom()));
        let right_rect = Rect::from_min_max(pos2(rect.right() - RIGHT_W, rect.top()), rect.max);

        // ---- left: history buttons + breadcrumb -------------------------------------------
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
            let bsz = vec2(32.0, ts.row);
            if widgets::icon_button(ui, bsz, widgets::Icon::ChevronLeft, self.hist_pos > 0)
                .clicked()
            {
                actions.push(Action::Back);
            }
            if widgets::icon_button(
                ui,
                bsz,
                widgets::Icon::ChevronRight,
                self.hist_pos + 1 < self.history.len(),
            )
            .clicked()
            {
                actions.push(Action::Forward);
            }
            if widgets::icon_button(ui, bsz, widgets::Icon::ArrowUp, self.cwd.parent().is_some())
                .clicked()
            {
                actions.push(Action::Up);
            }
            ui.add_space(6.0);

            // breadcrumb: ROOT // USERS // NAME ... — drop leading segments if it would overflow
            let comps: Vec<PathBuf> = self.cwd.ancestors().map(Path::to_path_buf).collect();
            let labels: Vec<String> = comps
                .iter()
                .rev()
                .map(|p| {
                    if p.as_os_str() == "/" {
                        "ROOT".to_string()
                    } else {
                        p.file_name()
                            .map(|s| s.to_string_lossy().to_uppercase())
                            .unwrap_or_default()
                    }
                })
                .collect();
            let widths: Vec<f32> = labels
                .iter()
                .map(|l| {
                    fuide::display_galley(ui.painter(), l.clone(), ts.heading, pal.text)
                        .size()
                        .x
                        + 8.0
                })
                .collect();
            let sep_w = 20.0;
            let avail = ui.available_width();
            let mut skip = 0;
            let total = |skip: usize| -> f32 {
                let n = widths.len() - skip;
                widths[skip..].iter().sum::<f32>()
                    + sep_w * n.saturating_sub(1) as f32
                    + if skip > 0 { 30.0 } else { 0.0 }
            };
            while skip + 1 < widths.len() && total(skip) > avail {
                skip += 1;
            }
            ui.spacing_mut().item_spacing.x = 0.0;
            if skip > 0 {
                let (r, _) = ui.allocate_exact_size(vec2(30.0, 20.0), Sense::hover());
                ui.painter().text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    "...//",
                    mono(ts.label),
                    pal.text_dim,
                );
            }
            let n = labels.len();
            for (i, label) in labels.iter().enumerate().skip(skip) {
                let last = i + 1 == n;
                if crumb(ui, label, last, &pal).clicked() && !last {
                    actions.push(Action::Navigate(comps[n - 1 - i].clone()));
                }
                if !last {
                    let (r, _) = ui.allocate_exact_size(vec2(sep_w, 20.0), Sense::hover());
                    ui.painter().text(
                        r.center(),
                        Align2::CENTER_CENTER,
                        "//",
                        mono(ts.label),
                        pal.text_dim,
                    );
                }
            }
        }

        // ---- right: filter box + hidden toggle ---------------------------------------------
        let want_focus = ui.input(|i| i.modifiers.command && i.key_pressed(Key::F));
        let mut right = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("toolbar-right")
                .max_rect(right_rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        {
            let ui = &mut right;
            ui.spacing_mut().item_spacing.x = 6.0;
            if widgets::toggle_chip(ui, "hidden", &mut self.show_hidden).clicked() {
                self.dirty = true;
            }
            let resp = widgets::text_input(ui, FILTER_W, &mut self.filter, "filter");
            if resp.changed() {
                self.dirty = true;
            }
            if want_focus {
                resp.request_focus();
            }
            // egui drops focus on Escape before widgets run, so the field reports `lost_focus`
            // (not `has_focus`) in the frame the key arrives
            if (resp.has_focus() || resp.lost_focus()) && ui.input(|i| i.key_pressed(Key::Escape)) {
                self.filter.clear();
                self.dirty = true;
                resp.surrender_focus();
            }
        }
    }

    fn ui_listing(&mut self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let row_h = ts.row;
        let count_tag = format!("{} items", self.view.len());
        let (title, title_tag_color) = match &self.last_error {
            None => ("Directory", pal.text_dim),
            Some(_) => ("Directory :: access denied", pal.danger),
        };
        let selected = self.selected;
        let view = &self.view;
        let entries = &self.entries;
        let sort_key = self.sort_key;
        let sort_desc = self.sort_desc;
        let mut scroll_to = None;
        if self.scroll_to_selected {
            scroll_to = selected.and_then(|s| view.iter().position(|&v| v == s));
        }
        self.scroll_to_selected = false;

        Panel::new(title)
            .tag(count_tag, title_tag_color)
            .padding(8.0, 14.0)
            .show_rect(ui, rect, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let w = ui.available_width();
                // column layout (from the right): modified / size / kind measured from the data font, name = rest
                let ch = ui
                    .painter()
                    .layout_no_wrap("0".into(), mono(ts.label), pal.text)
                    .size()
                    .x;
                let col_mod = ch * 17.0 + 14.0;
                let col_size = ch * 10.0 + 12.0;
                let col_kind = ch * 4.0 + 22.0;
                let x_mod = w - col_mod;
                let x_size = x_mod - col_size;
                let x_kind = x_size - col_kind;

                // header: each column is one hit area (whole width, whole height), lit on hover
                {
                    let (hr, _) = ui.allocate_exact_size(vec2(w, ts.heading + 8.0), Sense::hover());
                    let heads = [
                        (SortKey::Name, "NAME", 0.0, x_kind, Align2::LEFT_CENTER),
                        (SortKey::Kind, "KIND", x_kind, x_size, Align2::LEFT_CENTER),
                        (SortKey::Size, "SIZE", x_size, x_mod, Align2::RIGHT_CENTER),
                        (SortKey::Modified, "MODIFIED", x_mod, w, Align2::LEFT_CENTER),
                    ];
                    for (k, label, x0, x1, anchor) in heads {
                        let col = Rect::from_min_max(
                            pos2(hr.left() + x0, hr.top()),
                            pos2(hr.left() + x1, hr.bottom()),
                        );
                        let resp = widgets::hit(ui, col, ("sort", label), Sense::click());
                        let p = ui.painter();
                        let active = sort_key == k;
                        let color = if active || resp.hovered() {
                            pal.accent
                        } else {
                            pal.text_dim
                        };
                        if resp.hovered() {
                            p.rect_filled(
                                col,
                                egui::CornerRadius::ZERO,
                                pal.accent.gamma_multiply(0.08),
                            );
                        }
                        // right-anchored headers reserve room for the sort triangle on the right
                        let tri_w = if active { 12.0 } else { 0.0 };
                        let text_x = match anchor {
                            Align2::RIGHT_CENTER => col.right() - 8.0 - tri_w,
                            _ => col.left() + if x0 == 0.0 { 8.0 } else { 4.0 },
                        };
                        let tr = fuide::display_text(
                            p,
                            pos2(text_x, hr.center().y),
                            anchor,
                            label,
                            ts.heading,
                            color,
                        );
                        if active {
                            let icon = if sort_desc {
                                widgets::Icon::TriangleDown
                            } else {
                                widgets::Icon::TriangleUp
                            };
                            widgets::draw_icon(
                                p,
                                pos2(tr.right() + 8.0, hr.center().y),
                                6.0,
                                icon,
                                Stroke::new(1.0, color),
                            );
                        }
                        if resp.clicked() {
                            actions.push(Action::Sort(k));
                        }
                    }
                    let p = ui.painter();
                    p.hline(
                        hr.x_range(),
                        hr.bottom(),
                        Stroke::new(1.0, pal.accent_dim.gamma_multiply(0.8)),
                    );
                }

                ScrollArea::vertical()
                    .id_salt("listing")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, view.len(), |ui, range| {
                        let w = ui.available_width();
                        for row in range {
                            let idx = view[row];
                            let e = &entries[idx];
                            let (r, resp) = ui.allocate_exact_size(vec2(w, row_h), Sense::click());
                            if scroll_to == Some(row) {
                                resp.scroll_to_me(None);
                            }
                            let is_sel = selected == Some(idx);
                            // rows are addressable by file name (UI tests, assistive tech)
                            resp.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::SelectableLabel,
                                    true,
                                    is_sel,
                                    e.name.clone(),
                                )
                            });
                            let p = ui.painter().with_clip_rect(r.intersect(ui.clip_rect()));
                            if is_sel {
                                p.rect_filled(
                                    r,
                                    egui::CornerRadius::ZERO,
                                    pal.accent.gamma_multiply(0.13),
                                );
                                p.rect_filled(
                                    Rect::from_min_size(r.min, vec2(3.0, r.height())),
                                    egui::CornerRadius::ZERO,
                                    pal.accent,
                                );
                            } else if resp.hovered() {
                                p.rect_filled(
                                    r,
                                    egui::CornerRadius::ZERO,
                                    pal.accent.gamma_multiply(0.05),
                                );
                            } else if row % 2 == 1 {
                                p.rect_filled(
                                    r,
                                    egui::CornerRadius::ZERO,
                                    pal.accent.gamma_multiply(0.02),
                                );
                            }
                            let name_color = if e.hidden {
                                pal.text_dim
                            } else if e.is_dir {
                                pal.accent
                            } else {
                                pal.text
                            };
                            let cy = r.center().y;
                            // name (clipped to its column)
                            let name_clip = Rect::from_min_max(
                                r.min,
                                pos2(r.left() + x_kind - 4.0, r.bottom()),
                            );
                            p.with_clip_rect(name_clip.intersect(p.clip_rect())).text(
                                pos2(r.left() + 8.0, cy),
                                Align2::LEFT_CENTER,
                                &e.name,
                                mono(ts.data),
                                name_color,
                            );
                            // kind tag in a thin box
                            let tag_h = (ts.label + 5.0).round();
                            let tag_rect = Rect::from_min_size(
                                pos2(r.left() + x_kind + 2.0, cy - tag_h / 2.0),
                                vec2(col_kind - 10.0, tag_h),
                            );
                            p.rect_stroke(
                                tag_rect,
                                egui::CornerRadius::ZERO,
                                Stroke::new(1.0, pal.accent_dim.gamma_multiply(0.7)),
                                egui::StrokeKind::Inside,
                            );
                            p.text(
                                tag_rect.center(),
                                Align2::CENTER_CENTER,
                                e.kind.tag(),
                                mono(ts.label),
                                if e.is_dir {
                                    pal.accent_dim
                                } else {
                                    pal.text_dim
                                },
                            );
                            let size_text = if e.is_dir {
                                "--".to_string()
                            } else {
                                fs::fmt_size(e.size)
                            };
                            p.text(
                                pos2(r.left() + x_mod - 8.0, cy),
                                Align2::RIGHT_CENTER,
                                size_text,
                                mono(ts.label),
                                pal.text_dim,
                            );
                            p.text(
                                pos2(r.left() + x_mod + 6.0, cy),
                                Align2::LEFT_CENTER,
                                fs::fmt_time(e.modified),
                                mono(ts.label),
                                pal.text_dim,
                            );
                            if resp.double_clicked() {
                                actions.push(Action::Activate(idx));
                            } else if resp.clicked() {
                                actions.push(Action::Select(Some(idx)));
                            }
                        }
                        if view.is_empty() {
                            let (r, _) = ui.allocate_exact_size(vec2(w, 40.0), Sense::hover());
                            ui.painter().text(
                                r.center(),
                                Align2::CENTER_CENTER,
                                "NO ENTRIES",
                                mono(ts.label),
                                pal.text_dim,
                            );
                        }
                    });
            });
    }

    fn ui_inspector(&self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let sel = self.selected.map(|i| &self.entries[i]);
        let tag = match sel {
            Some(e) => e.kind.tag().to_string(),
            None => "cwd".to_string(),
        };
        Panel::new("Inspector")
            .tag(tag, pal.text_dim)
            .show_rect(ui, rect, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let (name, path, kind, is_dir): (String, PathBuf, Option<&Entry>, bool) = match sel
                {
                    Some(e) => (e.name.clone(), e.path.clone(), Some(e), e.is_dir),
                    None => (
                        self.cwd
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "/".into()),
                        self.cwd.clone(),
                        None,
                        true,
                    ),
                };
                ui.add(
                    egui::Label::new(
                        RichText::new(&name)
                            .font(mono(ts.data + 3.0))
                            .color(pal.accent),
                    )
                    .wrap(),
                );
                ui.add_space(6.0);
                widgets::rule(ui);
                match kind {
                    Some(e) => {
                        widgets::readout(ui, "kind", e.kind.tag(), None);
                        widgets::readout(
                            ui,
                            "size",
                            &if e.is_dir {
                                "--".into()
                            } else {
                                fs::fmt_size(e.size)
                            },
                            None,
                        );
                        widgets::readout(ui, "modified", &fs::fmt_time(e.modified), None);
                        widgets::readout(ui, "created", &fs::fmt_time(e.created), None);
                        widgets::readout(ui, "mode", &fs::fmt_mode(e.mode), None);
                        widgets::readout(
                            ui,
                            "symlink",
                            if e.is_symlink { "YES" } else { "NO" },
                            if e.is_symlink { Some(pal.warn) } else { None },
                        );
                        widgets::readout(
                            ui,
                            "hidden",
                            if e.hidden { "YES" } else { "NO" },
                            if e.hidden { Some(pal.warn) } else { None },
                        );
                        // size relative to the largest file in the directory
                        if !e.is_dir {
                            let max = self
                                .entries
                                .iter()
                                .filter(|x| !x.is_dir)
                                .map(|x| x.size)
                                .max()
                                .unwrap_or(1)
                                .max(1);
                            ui.add_space(6.0);
                            let (lr, _) = ui.allocate_exact_size(
                                vec2(ui.available_width(), ts.heading + 4.0),
                                Sense::hover(),
                            );
                            fuide::display_text(
                                ui.painter(),
                                pos2(lr.left(), lr.center().y),
                                Align2::LEFT_CENTER,
                                "SIZE VS LARGEST IN DIR",
                                ts.heading,
                                pal.text_dim,
                            );
                            let (br, _) = ui.allocate_exact_size(
                                vec2(ui.available_width(), 10.0),
                                Sense::hover(),
                            );
                            widgets::segment_bar(
                                ui.painter(),
                                br,
                                e.size as f32 / max as f32,
                                pal.accent,
                                pal.accent_dim,
                            );
                        }
                    }
                    None => {
                        widgets::readout(ui, "kind", "DIR", None);
                        widgets::readout(ui, "items", &self.entries.len().to_string(), None);
                        widgets::readout(
                            ui,
                            "dirs",
                            &self.entries.iter().filter(|e| e.is_dir).count().to_string(),
                            None,
                        );
                        widgets::readout(
                            ui,
                            "files",
                            &self
                                .entries
                                .iter()
                                .filter(|e| !e.is_dir)
                                .count()
                                .to_string(),
                            None,
                        );
                        let total: u64 = self
                            .entries
                            .iter()
                            .filter(|e| !e.is_dir)
                            .map(|e| e.size)
                            .sum();
                        widgets::readout(ui, "bytes", &fs::fmt_size(total), None);
                        widgets::readout(
                            ui,
                            "history",
                            &format!("{}/{}", self.hist_pos + 1, self.history.len()),
                            None,
                        );
                    }
                }
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
                    "PATH",
                    ts.heading,
                    pal.text_dim,
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(path.display().to_string())
                            .font(mono(ts.label))
                            .color(pal.text_dim),
                    )
                    .wrap(),
                );

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let bsz = vec2(72.0, ts.row);
                    if widgets::button(ui, bsz, if is_dir { "OPEN" } else { "LAUNCH" }, true)
                        .clicked()
                    {
                        match kind {
                            Some(e) if e.navigates() => {
                                actions.push(Action::Navigate(e.path.clone()))
                            }
                            _ => actions.push(Action::Open(path.clone())),
                        }
                    }
                    if widgets::button(ui, vec2(80.0, ts.row), "FINDER", true).clicked() {
                        actions.push(Action::Reveal(path.clone()));
                    }
                    if widgets::button(ui, bsz, "COPY", true).clicked() {
                        actions.push(Action::CopyPath(path.clone()));
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let sel_idx = self.selected;
                    if widgets::button(ui, vec2(80.0, ts.row), "RENAME", sel_idx.is_some())
                        .clicked()
                    {
                        if let Some(i) = sel_idx {
                            actions.push(Action::OpenRename(i));
                        }
                    }
                    if widgets::button_colored(
                        ui,
                        vec2(80.0, ts.row),
                        "DELETE",
                        sel_idx.is_some(),
                        pal.warn,
                    )
                    .clicked()
                    {
                        if let Some(i) = sel_idx {
                            actions.push(Action::OpenDelete(i, false));
                        }
                    }
                });
                ui.add_space(8.0);
                let lh = ts.small + 5.0;
                let hints = [
                    "KEYS :: UP/DN SELECT  ENTER OPEN  BKSP UP",
                    "CMD+[ / ]  MOUSE 4/5 :: HISTORY",
                    "CMD+R RENAME  CMD+BKSP TRASH  +OPT DELETE",
                    "CMD+1..3 PALETTE  CMD+, SETTINGS",
                ];
                let (fr, _) = ui.allocate_exact_size(
                    vec2(ui.available_width(), lh * hints.len() as f32),
                    Sense::hover(),
                );
                let p = ui.painter();
                for (i, h) in hints.iter().enumerate() {
                    p.text(
                        pos2(fr.left(), fr.top() + lh * (i as f32 + 0.5)),
                        Align2::LEFT_CENTER,
                        *h,
                        mono(ts.small),
                        pal.text_dim,
                    );
                }
            });
    }

    /// Rename / delete dialogs (modal, drawn on top of everything).
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
            DialogState::Rename {
                idx,
                name,
                error,
                focus,
            } => {
                let entry = &self.entries[*idx];
                let resp = Dialog::new("Rename")
                    .tag(entry.kind.tag(), pal.text_dim)
                    .width(460.0)
                    .show(ctx, open, |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        widgets::readout(ui, "current", &entry.name, None);
                        ui.add_space(4.0);
                        let input = widgets::text_input(ui, ui.available_width(), name, "new name");
                        if *focus {
                            input.request_focus();
                            *focus = false;
                        }
                        let submit = input.lost_focus() && enter;
                        let (lr, _) = ui.allocate_exact_size(
                            vec2(ui.available_width(), ts.label + 6.0),
                            Sense::hover(),
                        );
                        if let Some(e) = error {
                            ui.painter().text(
                                pos2(lr.left(), lr.center().y),
                                Align2::LEFT_CENTER,
                                format!("REJECTED :: {}", e.to_uppercase()),
                                mono(ts.label),
                                pal.danger,
                            );
                        }
                        ui.add_space(8.0);
                        let clicked = fuide::dialog::button_row(
                            ui,
                            &[
                                ("CANCEL", pal.text_dim, true),
                                ("RENAME", pal.accent, !name.trim().is_empty()),
                            ],
                        );
                        (submit, clicked)
                    });
                finished = resp.finished;
                let (submit, clicked) = resp.inner.unwrap_or((false, None));
                if !open {
                    // fading out: ignore input
                } else if resp.should_close || clicked == Some(0) {
                    actions.push(Action::CloseDialog);
                } else if submit || clicked == Some(1) {
                    actions.push(Action::ConfirmRename);
                }
            }
            DialogState::Delete { idx, permanent } => {
                let entry = &self.entries[*idx];
                let (title, color, verb, note) = if *permanent {
                    (
                        "Delete",
                        pal.danger,
                        "DELETE PERMANENTLY",
                        "THIS CANNOT BE UNDONE",
                    )
                } else {
                    (
                        "Move to Trash",
                        pal.warn,
                        "MOVE TO TRASH",
                        "RECOVERABLE FROM THE FINDER TRASH",
                    )
                };
                let resp = Dialog::new(title)
                    .tag(entry.kind.tag(), pal.text_dim)
                    .outline(color)
                    .width(460.0)
                    .show(ctx, open, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        ui.add(
                            egui::Label::new(
                                RichText::new(&entry.name)
                                    .font(mono(ts.data + 2.0))
                                    .color(pal.accent),
                            )
                            .wrap(),
                        );
                        ui.add_space(4.0);
                        widgets::readout(ui, "kind", entry.kind.tag(), None);
                        widgets::readout(
                            ui,
                            "size",
                            &if entry.is_dir {
                                "-- (recursive)".into()
                            } else {
                                fs::fmt_size(entry.size)
                            },
                            None,
                        );
                        widgets::readout(ui, "modified", &fs::fmt_time(entry.modified), None);
                        ui.add_space(6.0);
                        widgets::rule(ui);
                        ui.horizontal(|ui| {
                            widgets::toggle_chip(ui, "permanent", permanent);
                            let (nr, _) = ui.allocate_exact_size(
                                vec2(ui.available_width(), ts.row),
                                Sense::hover(),
                            );
                            ui.painter().text(
                                pos2(nr.left() + 4.0, nr.center().y),
                                Align2::LEFT_CENTER,
                                note,
                                mono(ts.label),
                                color,
                            );
                        });
                        ui.add_space(8.0);
                        fuide::dialog::button_row(
                            ui,
                            &[("CANCEL", pal.text_dim, true), (verb, color, true)],
                        )
                    });
                finished = resp.finished;
                let clicked = resp.inner.flatten();
                if !open {
                    // fading out: ignore input
                } else if resp.should_close || clicked == Some(0) {
                    actions.push(Action::CloseDialog);
                } else if enter || clicked == Some(1) {
                    actions.push(Action::ConfirmDelete);
                }
            }
            DialogState::Error { line } => {
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

    fn ui_log(&self, ui: &mut Ui, rect: Rect, _t: f64) {
        let pal = palette(ui.ctx());
        Panel::new("Event log")
            .tag(format!("{} events", self.log.len()), pal.text_dim)
            .padding(8.0, 12.0)
            .show_rect(ui, rect, |ui| {
                // resolve placeholder colours against the live palette
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
    }
}

/// One breadcrumb segment. The last one is the current directory (accent, not clickable).
fn crumb(ui: &mut Ui, label: &str, last: bool, pal: &Palette) -> egui::Response {
    let size = type_scale(ui.ctx()).heading;
    let galley = fuide::display_galley(ui.painter(), label, size, pal.text);
    let size = galley.size() + vec2(8.0, 6.0);
    let (rect, resp) =
        ui.allocate_exact_size(size, if last { Sense::hover() } else { Sense::click() });
    let color = if last {
        pal.accent
    } else if resp.hovered() {
        pal.text
    } else {
        pal.text.gamma_multiply(0.7)
    };
    if resp.hovered() && !last {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::ZERO,
            pal.accent.gamma_multiply(0.10),
        );
    }
    ui.painter()
        .galley(rect.min + vec2(4.0, 3.0), galley, color);
    resp
}

#[cfg(test)]
mod e2e;
#[cfg(test)]
mod tests;
