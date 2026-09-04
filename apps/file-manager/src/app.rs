//! FUIDE File Manager — Finder-like browser with a tactical-console look.

use std::collections::BTreeSet;
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
/// Header strip left when the log panel is collapsed (`Settings::log_open` = false).
const LOG_CLOSED: f32 = 26.0;
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
        indices: Vec<usize>,
        permanent: bool,
    },
    /// Finder's "Go to Folder": a typed path (`~`, relative, `..` allowed), Tab completes.
    GoTo {
        path: String,
        error: Option<String>,
        focus: bool,
        /// Directory completions for `path` (recomputed when it changes).
        suggestions: Vec<String>,
        suggested_for: String,
    },
    /// Big `ERROR` card; details live in the event log.
    Error { line: String },
}

/// A drag of listing rows, before it is either dropped on an in-app directory or
/// handed to the OS (`drag::start_drag`) when the pointer leaves the window.
struct DragState {
    paths: Vec<PathBuf>,
    /// Ghost caption: the file name, or `N ITEMS`.
    label: String,
}

/// Files taken by Cmd+C / Cmd+X, waiting for Cmd+V. The files themselves live here; the OS
/// clipboard gets their paths as text (`text`), which also makes egui deliver a
/// `Event::Paste` on Cmd+V (it stays silent when the OS clipboard is empty). If the OS
/// clipboard holds something else by the time Cmd+V arrives, the files were superseded —
/// like the Finder after copying text elsewhere. A cut is one-shot and clears on paste; a
/// copy can be pasted repeatedly.
struct Clipboard {
    paths: Vec<PathBuf>,
    cut: bool,
    /// What was written to the OS clipboard: the paths, one per line.
    text: String,
}

impl Clipboard {
    fn label(&self) -> String {
        format!(
            "{} {}",
            self.paths.len(),
            if self.cut { "CUT" } else { "COPIED" }
        )
    }
}

enum Action {
    OpenRename(usize),
    /// Confirmation dialog for the current selection (`true` = permanent).
    OpenDelete(bool),
    ConfirmRename,
    ConfirmDelete,
    /// Cmd+Shift+G / click on the current breadcrumb: the go-to-path dialog.
    OpenGoTo,
    ConfirmGoTo,
    /// Tab in the go-to dialog: fill in the single match or the common prefix.
    CompleteGoTo,
    CloseDialog,
    Navigate(PathBuf),
    Back,
    Forward,
    Up,
    Select(Option<usize>),
    SelectToggle(usize),
    SelectRange(usize),
    SelectAll,
    Activate(usize),
    Open(PathBuf),
    Reveal(PathBuf),
    /// Copy the selected paths as text (inspector `PATH` button).
    CopyPaths(Vec<PathBuf>),
    /// Cmd+C / Cmd+X: put the selection on the in-app clipboard.
    Copy,
    Cut,
    /// Cmd+V: copy or move the clipboard into the current directory. Carries the OS
    /// clipboard text when the request came from the OS paste event (`None` for the button).
    Paste(Option<String>),
    Sort(SortKey),
    Palette(PaletteKind),
    OpenSettings,
    /// Start dragging the row at this entry index (plus the rest of the selection).
    BeginDrag(usize),
    /// Move `paths` into the directory `dest` (drop of a drag, or an external drop).
    MoveTo {
        dest: PathBuf,
        paths: Vec<PathBuf>,
    },
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
    /// Multi-selection: indices into `entries`.
    selected: BTreeSet<usize>,
    /// Last row acted on (keyboard walking, inspector focus).
    lead: Option<usize>,
    /// Fixed end of a Shift range (set by plain / Cmd clicks).
    anchor: Option<usize>,
    /// An in-app drag of listing rows (`Action::BeginDrag`).
    drag: Option<DragState>,
    /// Cmd+C / Cmd+X selection waiting for Cmd+V.
    clipboard: Option<Clipboard>,
    /// Results of OS drag-out sessions (`drag::start_drag` callback, any thread).
    drag_out: (
        std::sync::mpsc::Sender<drag::DragResult>,
        std::sync::mpsc::Receiver<drag::DragResult>,
    ),
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
    /// Dev aid: `FUIDE_DEV_DIALOG=rename|trash|delete|error|goto` opens that dialog on the first entry after load.
    dev_dialog: Option<String>,
    /// Dev aid: `FUIDE_DEV_SELECT=<n>` selects the first n rows after load (screenshots).
    dev_select: Option<usize>,
    /// Dev aid: `FUIDE_DEV_DIALOG_CLOSE=<frame>` closes the dev dialog at that frame (fade-out shots).
    dev_close_frame: Option<u32>,
    dev_frame: u32,
    devshot: fuide::devshot::DevShot,
    /// MCP interface for AI agents (`fuide::agent`); on/off in the settings window.
    agent: fuide::Agent,
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
            selected: BTreeSet::new(),
            lead: None,
            anchor: None,
            drag: None,
            clipboard: None,
            drag_out: std::sync::mpsc::channel(),
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
            dev_select: std::env::var("FUIDE_DEV_SELECT")
                .ok()
                .and_then(|v| v.parse().ok()),
            dev_close_frame: std::env::var("FUIDE_DEV_DIALOG_CLOSE")
                .ok()
                .and_then(|v| v.parse().ok()),
            dev_frame: 0,
            devshot: fuide::devshot::DevShot::from_env(),
            agent: fuide::Agent::new(APP_ID, "FUIDE File Manager"),
        };
        app.push_log(0.0, "file manager online :: fs link established", Level::Ok);
        app.agent.set_enabled(ctx, app.settings.agent);
        if app.settings.agent {
            app.push_log(
                0.0,
                "agent // interface on :: waiting for a client",
                Level::Warn,
            );
        }
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

    /// While a delete / trash confirmation is open and the agent may not confirm, its verb is
    /// human-only (rename is reversible, so it stays open to the agent).
    fn agent_blocked(&self) -> Vec<String> {
        match &self.dialog {
            Some(OpenDialog {
                state: DialogState::Delete { permanent, .. },
                closing: false,
            }) if !self.settings.agent_confirm => vec![if *permanent {
                "DELETE PERMANENTLY".into()
            } else {
                "MOVE TO TRASH".into()
            }],
            _ => Vec::new(),
        }
    }

    /// State summary for the agent's `observe` (what the widgets alone do not say).
    fn agent_state(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(s, "cwd: {}", self.cwd.display());
        let _ = writeln!(
            s,
            "entries: {} shown of {} :: hidden files {} :: sort {} {} :: filter {:?}",
            self.view.len(),
            self.entries.len(),
            if self.show_hidden { "shown" } else { "hidden" },
            match self.sort_key {
                SortKey::Name => "name",
                SortKey::Kind => "kind",
                SortKey::Size => "size",
                SortKey::Modified => "modified",
            },
            if self.sort_desc { "desc" } else { "asc" },
            self.filter
        );
        match self.single_selected().and_then(|i| self.entries.get(i)) {
            Some(e) => {
                let _ = writeln!(
                    s,
                    "selected: {} ({}{}) {} bytes",
                    e.name,
                    e.kind.tag(),
                    if e.is_dir { ", directory" } else { "" },
                    e.size
                );
            }
            None if self.selected.is_empty() => s.push_str("selected: none\n"),
            None => {
                let names: Vec<&str> = self
                    .selected
                    .iter()
                    .filter_map(|&i| self.entries.get(i).map(|e| e.name.as_str()))
                    .collect();
                let _ = writeln!(
                    s,
                    "selected: {} items ({})",
                    self.selected.len(),
                    names.join(", ")
                );
            }
        }
        if let Some(d) = &self.drag {
            let _ = writeln!(s, "dragging: {} (drop on a directory to move)", d.label);
        }
        if let Some(c) = &self.clipboard {
            let names: Vec<String> = c
                .paths
                .iter()
                .map(|p| {
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            let _ = writeln!(
                s,
                "clipboard: {} items {} ({}); cmd+v pastes into cwd",
                c.paths.len(),
                if c.cut { "cut" } else { "copied" },
                names.join(", ")
            );
        }
        if self.pending.is_some() {
            s.push_str("loading: directory read in progress\n");
        }
        if self.ops.busy() {
            s.push_str("file operation: running\n");
        }
        if let Some(e) = &self.last_error {
            let _ = writeln!(s, "last error: {e}");
        }
        if self.dialog.as_ref().is_some_and(|d| d.closing) {
            s.push_str("dialog: closing\n");
        }
        match self.dialog.as_ref().filter(|d| !d.closing) {
            Some(OpenDialog {
                state: DialogState::Rename { name, error, .. },
                ..
            }) => {
                let _ = writeln!(
                    s,
                    "dialog: RENAME :: new name {name:?}{} :: buttons CANCEL / RENAME",
                    error
                        .as_deref()
                        .map(|e| format!(" :: {e}"))
                        .unwrap_or_default()
                );
            }
            Some(OpenDialog {
                state:
                    DialogState::GoTo {
                        path,
                        error,
                        suggestions,
                        ..
                    },
                ..
            }) => {
                let _ = writeln!(
                    s,
                    "dialog: GO TO :: path {path:?}{}{} :: buttons CANCEL / GO (tab completes)",
                    error
                        .as_deref()
                        .map(|e| format!(" :: {e}"))
                        .unwrap_or_default(),
                    if suggestions.is_empty() {
                        String::new()
                    } else {
                        format!(" :: completions {}", suggestions.join(", "))
                    }
                );
            }
            Some(OpenDialog {
                state: DialogState::Delete { indices, permanent },
                ..
            }) => {
                let names: Vec<&str> = indices
                    .iter()
                    .filter_map(|&i| self.entries.get(i).map(|e| e.name.as_str()))
                    .collect();
                let _ = writeln!(
                    s,
                    "dialog: {} :: {} :: buttons CANCEL / {}",
                    if *permanent {
                        "DELETE"
                    } else {
                        "MOVE TO TRASH"
                    },
                    names.join(", "),
                    if *permanent {
                        "DELETE PERMANENTLY"
                    } else {
                        "MOVE TO TRASH"
                    }
                );
            }
            Some(OpenDialog {
                state: DialogState::Error { line },
                ..
            }) => {
                let _ = writeln!(s, "dialog: ERROR :: {line} :: press ACKNOWLEDGE");
            }
            None => {}
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

    // ------------------------------------------------------------- selection

    /// Replace the selection with one entry (or clear it).
    fn select_one(&mut self, idx: Option<usize>) {
        self.selected = idx.into_iter().collect();
        self.lead = idx;
        self.anchor = idx;
    }

    /// Cmd+click: toggle membership; the toggled row becomes lead and anchor.
    fn select_toggle(&mut self, idx: usize) {
        if !self.selected.remove(&idx) {
            self.selected.insert(idx);
        }
        self.lead = Some(idx);
        self.anchor = Some(idx);
    }

    /// Shift+click / Shift+arrows: select the visible range between the anchor and `idx`.
    fn select_range(&mut self, idx: usize) {
        let anchor = self.anchor.or(self.lead).unwrap_or(idx);
        let pos_of = |e: usize| self.view.iter().position(|&v| v == e);
        let (Some(a), Some(b)) = (pos_of(anchor), pos_of(idx)) else {
            self.select_one(Some(idx));
            return;
        };
        let (lo, hi) = (a.min(b), a.max(b));
        self.selected = self.view[lo..=hi].iter().copied().collect();
        self.lead = Some(idx);
        self.anchor = Some(anchor);
    }

    fn select_all(&mut self) {
        self.selected = self.view.iter().copied().collect();
        if self.lead.is_none() {
            self.lead = self.view.first().copied();
        }
    }

    /// The selected entry, when exactly one is selected (rename, single-item inspector).
    fn single_selected(&self) -> Option<usize> {
        match self.selected.len() {
            1 => self.selected.first().copied(),
            _ => None,
        }
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected
            .iter()
            .filter_map(|&i| self.entries.get(i).map(|e| e.path.clone()))
            .collect()
    }

    // ------------------------------------------------------------------ fs

    fn load(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.cwd = path.clone();
        self.pending = Some(self.loader.request(path.clone(), ctx.clone()));
        self.disk = fs::disk_info(&path);
        self.select_one(None);
        self.drag = None;
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
        let visible: BTreeSet<usize> = self.view.iter().copied().collect();
        self.selected.retain(|i| visible.contains(i));
        if self.lead.is_some_and(|l| !visible.contains(&l)) {
            self.lead = None;
        }
        if self.anchor.is_some_and(|a| !visible.contains(&a)) {
            self.anchor = None;
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
                        let idx = self.entries.iter().position(|e| e.name == name);
                        self.select_one(idx);
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
            Action::OpenDelete(permanent) => {
                let indices: Vec<usize> = self
                    .selected
                    .iter()
                    .copied()
                    .filter(|&i| i < self.entries.len())
                    .collect();
                if !indices.is_empty() {
                    self.dialog = Some(OpenDialog {
                        state: DialogState::Delete { indices, permanent },
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
            Action::OpenGoTo => {
                let mut path = self.cwd.display().to_string();
                if !path.ends_with('/') {
                    path.push('/');
                }
                self.dialog = Some(OpenDialog {
                    state: DialogState::GoTo {
                        path,
                        error: None,
                        focus: true,
                        suggestions: Vec::new(),
                        suggested_for: String::new(),
                    },
                    closing: false,
                });
            }
            Action::ConfirmGoTo => {
                let Some(OpenDialog {
                    state: DialogState::GoTo { path, error, .. },
                    closing: false,
                }) = &mut self.dialog
                else {
                    return;
                };
                match fs::resolve_goto(path, &self.cwd) {
                    Ok((dir, select)) => {
                        if let Some(d) = &mut self.dialog {
                            d.closing = true;
                        }
                        self.push_log(t, format!("goto // {}", dir.display()), Level::Info);
                        self.navigate(ctx, dir, t);
                        if select.is_some() {
                            self.select_after_load = select;
                            // same directory: `navigate` did not reload, select right away
                            if self.pending.is_none() {
                                let name = self.select_after_load.take().unwrap_or_default();
                                let idx = self.entries.iter().position(|e| e.name == name);
                                self.select_one(idx);
                                self.scroll_to_selected = true;
                            }
                        }
                    }
                    Err(e) => *error = Some(e),
                }
            }
            Action::CompleteGoTo => {
                let Some(OpenDialog {
                    state:
                        DialogState::GoTo {
                            path, suggestions, ..
                        },
                    closing: false,
                }) = &mut self.dialog
                else {
                    return;
                };
                let filled = match suggestions.len() {
                    0 => return,
                    1 => suggestions[0].clone(),
                    _ => fs::common_prefix(suggestions),
                };
                if filled.len() > path.len() {
                    *path = filled;
                }
            }
            Action::ConfirmDelete => {
                let Some(OpenDialog {
                    state: DialogState::Delete { indices, permanent },
                    closing,
                }) = &mut self.dialog
                else {
                    return;
                };
                if *closing {
                    return;
                }
                let permanent = *permanent;
                let items: Vec<(String, PathBuf)> = indices
                    .iter()
                    .filter_map(|&i| self.entries.get(i))
                    .map(|e| (e.name.clone(), e.path.clone()))
                    .collect();
                *closing = true;
                let verb = if permanent { "delete" } else { "trash" };
                let what = match &items[..] {
                    [(name, _)] => name.clone(),
                    _ => format!("{} items", items.len()),
                };
                let label = format!("{verb} // {what}");
                self.push_log(
                    t,
                    &label,
                    if permanent {
                        Level::Danger
                    } else {
                        Level::Warn
                    },
                );
                self.ops.spawn(label, ctx.clone(), move || {
                    for (name, path) in &items {
                        let r = if permanent {
                            fs::remove(path)
                        } else {
                            fs::trash(path)
                        };
                        r.map_err(|e| format!("{name}: {e}"))?;
                    }
                    Ok(())
                });
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
                self.select_one(s);
                self.scroll_to_selected = true;
            }
            Action::SelectToggle(i) => self.select_toggle(i),
            Action::SelectRange(i) => {
                self.select_range(i);
                self.scroll_to_selected = true;
            }
            Action::SelectAll => self.select_all(),
            Action::BeginDrag(i) => {
                if !self.selected.contains(&i) {
                    self.select_one(Some(i));
                }
                let paths = self.selected_paths();
                if paths.is_empty() {
                    return;
                }
                let label = match self.single_selected().and_then(|i| self.entries.get(i)) {
                    Some(e) => e.name.clone(),
                    None => format!("{} ITEMS", paths.len()),
                };
                self.drag = Some(DragState { paths, label });
            }
            Action::MoveTo { dest, paths } => {
                // no-ops (dropped where they already live) and cycles are filtered here so
                // an external drop of a mixed set only moves what actually changes place
                let paths: Vec<PathBuf> = paths
                    .into_iter()
                    .filter(|p| p.parent() != Some(dest.as_path()) && !dest.starts_with(p))
                    .collect();
                if paths.is_empty() {
                    return;
                }
                let dest_name = dest
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".into());
                let label = format!("move // {} -> {dest_name}", Self::describe(&paths));
                self.push_log(t, &label, Level::Warn);
                // when files land in the visible directory, select the first arrival
                if dest == self.cwd {
                    self.select_after_load = paths[0]
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned());
                }
                self.ops.spawn(label, ctx.clone(), move || {
                    for p in &paths {
                        let name = p
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        fs::move_into(p, &dest).map_err(|e| format!("{name}: {e}"))?;
                    }
                    Ok(())
                });
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
            Action::CopyPaths(paths) => {
                let n = paths.len();
                let text: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                ctx.copy_text(text.join("\n"));
                self.push_log(
                    t,
                    if n == 1 {
                        "path copied to clipboard".to_string()
                    } else {
                        format!("{n} paths copied to clipboard")
                    },
                    Level::Info,
                );
            }
            Action::Copy | Action::Cut => {
                let cut = matches!(action, Action::Cut);
                let paths = self.selected_paths();
                if paths.is_empty() {
                    return;
                }
                self.push_log(
                    t,
                    format!(
                        "{} // {}",
                        if cut { "cut" } else { "copy" },
                        Self::describe(&paths)
                    ),
                    Level::Info,
                );
                let text: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                let text = text.join("\n");
                ctx.copy_text(text.clone());
                self.clipboard = Some(Clipboard { paths, cut, text });
            }
            Action::Paste(os_text) => {
                let Some(clip) = self.clipboard.take() else {
                    return;
                };
                if os_text.is_some_and(|t| t != clip.text) {
                    // something else was copied since (text in another app, files in the
                    // Finder): the file clipboard is stale
                    self.push_log(t, "paste // clipboard was replaced elsewhere", Level::Info);
                    return;
                }
                let dest = self.cwd.clone();
                if clip.cut {
                    // a cut is one-shot: the clipboard is already empty
                    self.apply(
                        ctx,
                        Action::MoveTo {
                            dest,
                            paths: clip.paths,
                        },
                        t,
                    );
                    return;
                }
                let pairs = fs::paste_targets(&dest, &clip.paths);
                self.clipboard = Some(clip); // a copy can be pasted again
                if pairs.is_empty() {
                    return;
                }
                let srcs: Vec<PathBuf> = pairs.iter().map(|(s, _)| s.clone()).collect();
                let label = format!(
                    "paste // {} -> {}",
                    Self::describe(&srcs),
                    dest.file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "/".into())
                );
                self.push_log(t, &label, Level::Warn);
                self.select_after_load = pairs[0]
                    .1
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned());
                self.ops.spawn(label, ctx.clone(), move || {
                    for (src, target) in &pairs {
                        let name = src
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        fs::copy_to(src, target).map_err(|e| format!("{name}: {e}"))?;
                    }
                    Ok(())
                });
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

    /// Log label for a set of paths: the file name, or `N items`.
    fn describe(paths: &[PathBuf]) -> String {
        match paths {
            [p] => p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => format!("{} items", paths.len()),
        }
    }

    /// Log + persist after the settings changed (shortcut or settings window).
    fn settings_changed(&mut self, t: f64) {
        let s = &self.settings;
        self.push_log(
            t,
            format!(
                "settings // palette {} :: {} :: {} :: {} :: agent {}{}",
                s.palette.name(),
                if s.chamfer { "chamfer" } else { "square" },
                if s.compact { "compact" } else { "normal" },
                if s.transparent {
                    "translucent"
                } else {
                    "opaque"
                },
                if s.agent { "on" } else { "off" },
                if s.agent && s.agent_confirm {
                    " (may confirm)"
                } else {
                    ""
                }
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
        // Finder-style clipboard for files. egui-winit turns Cmd+C / X / V into
        // `Event::Copy` / `Cut` / `Paste` and swallows the key press, so those events are
        // the real trigger; the key checks cover backends that send plain keys
        // (egui_kittest). Cmd+C / X leave a text selection in the log alone (egui copies
        // it). Read outside `ui.input` — the plugin takes the context lock.
        let text_selected = ui
            .ctx()
            .plugin::<egui::text_selection::LabelSelectionState>()
            .lock()
            .has_selection();
        ui.input(|i| {
            let cmd = i.modifiers.command;
            if i.key_pressed(Key::ArrowDown) || i.key_pressed(Key::ArrowUp) {
                let dir: isize = if i.key_pressed(Key::ArrowDown) { 1 } else { -1 };
                let pos = self
                    .lead
                    .and_then(|s| self.view.iter().position(|&v| v == s));
                let next = match pos {
                    Some(p) => (p as isize + dir).clamp(0, self.view.len() as isize - 1) as usize,
                    None => 0,
                };
                if let Some(&e) = self.view.get(next) {
                    // Finder: Shift+arrows grow the selection from the anchor
                    actions.push(if i.modifiers.shift {
                        Action::SelectRange(e)
                    } else {
                        Action::Select(Some(e))
                    });
                }
            }
            if i.key_pressed(Key::Enter) {
                if let Some(s) = self.lead {
                    actions.push(Action::Activate(s));
                }
            }
            if cmd && i.key_pressed(Key::A) {
                actions.push(Action::SelectAll);
            }
            let mut paste: Option<Option<String>> = None;
            for ev in &i.events {
                match ev {
                    egui::Event::Copy if !text_selected => actions.push(Action::Copy),
                    egui::Event::Cut if !text_selected => actions.push(Action::Cut),
                    egui::Event::Paste(text) => paste = Some(Some(text.clone())),
                    _ => {}
                }
            }
            if cmd && i.key_pressed(Key::C) && !text_selected {
                actions.push(Action::Copy);
            }
            if cmd && i.key_pressed(Key::X) && !text_selected {
                actions.push(Action::Cut);
            }
            if cmd && i.key_pressed(Key::V) && paste.is_none() {
                paste = Some(None);
            }
            if let Some(os_text) = paste {
                actions.push(Action::Paste(os_text));
            }
            if cmd && i.key_pressed(Key::Backspace) {
                // Finder: Cmd+Backspace = move to Trash; Cmd+Option+Backspace = delete immediately
                if !self.selected.is_empty() {
                    actions.push(Action::OpenDelete(i.modifiers.alt));
                }
            } else if i.key_pressed(Key::Backspace) || (cmd && i.key_pressed(Key::ArrowUp)) {
                actions.push(Action::Up);
            }
            if cmd && i.key_pressed(Key::R) {
                if let Some(s) = self.single_selected() {
                    actions.push(Action::OpenRename(s));
                }
            }
            if cmd && i.modifiers.shift && i.key_pressed(Key::G) {
                actions.push(Action::OpenGoTo);
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

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        self.devshot.tick(ui.ctx());
        // agent first: injected input must be visible to this frame's widgets
        self.agent.set_enabled(ui.ctx(), self.settings.agent);
        self.agent.set_blocked(self.agent_blocked());
        let agent_state = self.agent.wants_state().then(|| self.agent_state());
        self.agent.tick(ui.ctx(), agent_state);
        let t = ui.input(|i| i.time);
        let ctx = ui.ctx().clone();
        self.poll_ops(&ctx, t);
        self.poll_loader(t);
        while let Ok(result) = self.drag_out.1.try_recv() {
            match result {
                drag::DragResult::Dropped => {
                    self.push_log(t, "drag out :: delivered to the OS target", Level::Ok);
                    // the receiver may have moved the files; refresh the listing
                    self.load(&ctx, self.cwd.clone());
                }
                drag::DragResult::Cancel => {
                    self.push_log(t, "drag out :: cancelled", Level::Info);
                }
            }
        }
        if self.dirty {
            self.rebuild_view();
        }
        let pal = palette(ui.ctx());
        let fps = 1.0 / ui.input(|i| i.stable_dt).max(1e-3);
        let hidden = self.entries.iter().filter(|e| e.hidden).count();

        let mut actions: Vec<Action> = Vec::new();
        // Dev aid: `FUIDE_DEV_SELECT=<n>` selects the first n rows after load (screenshots).
        if let Some(n) = self.dev_select.take_if(|_| !self.view.is_empty()) {
            for &idx in self.view.iter().take(n) {
                actions.push(Action::SelectToggle(idx));
            }
        }
        if let Some(kind) = self.dev_dialog.take_if(|_| !self.view.is_empty()) {
            let first = self.view[0];
            actions.push(Action::Select(Some(first)));
            actions.push(match kind.as_str() {
                "rename" => Action::OpenRename(first),
                "goto" => Action::OpenGoTo,
                "error" => {
                    self.error_queue.push_back("delete // immutable.txt".into());
                    Action::Select(Some(first))
                }
                "delete" => Action::OpenDelete(true),
                _ => Action::OpenDelete(false),
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
        if let Some(c) = &self.clipboard {
            shell = shell.lamp(
                format!("CLIP {}", c.label()),
                if c.cut { pal.warn } else { pal.accent },
                false,
            );
        }
        if let Some((text, busy)) = self.agent.lamp() {
            shell = shell.lamp(text, if busy { pal.warn } else { pal.accent }, busy);
        }

        // drag & drop state for this frame: rows being dragged (in-app), the directory
        // under the pointer (filled while widgets draw), and files hovered in from outside
        let drag_paths: Option<Vec<PathBuf>> = self.drag.as_ref().map(|d| d.paths.clone());
        let mut drop_target: Option<PathBuf> = None;
        let drop_hover = ui.input(|i| !i.raw.hovered_files.is_empty());
        let dropped: Vec<PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            actions.push(Action::MoveTo {
                dest: self.cwd.clone(),
                paths: dropped,
            });
        }

        let log_open = self.settings.log_open;
        let mut log_resized = false;
        let mut log_toggled = false;
        let out = shell.show_full(ui, |ui| {
            let c = ui.max_rect();
            let top = c.top() + 10.0; // room for title chips above the first panels
            let log_max = c.height() - BODY_MIN;
            self.log_h = self.log_h.clamp(LOG_MIN, log_max.max(LOG_MIN));
            let log_h = if log_open { self.log_h } else { LOG_CLOSED };
            let log_rect = Rect::from_min_max(pos2(c.left(), c.bottom() - log_h), c.max);
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

            self.ui_locations(
                ui,
                locations,
                &mut actions,
                drag_paths.as_deref(),
                &mut drop_target,
            );
            self.ui_storage(ui, storage);
            self.ui_toolbar(ui, toolbar, &mut actions);
            self.ui_listing(
                ui,
                listing,
                &mut actions,
                drag_paths.as_deref(),
                &mut drop_target,
                drop_hover,
            );
            self.ui_inspector(ui, right, &mut actions);
            if log_open {
                // draggable divider in the gap above the log panel; registered before the
                // panel so its title chip (which straddles the strip's bottom edge) wins clicks
                let strip = Rect::from_min_max(
                    pos2(c.left(), body_bottom),
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
            log_toggled = self.ui_log(ui, log_rect, t, log_open);
        });
        self.agent.paint(&ctx);
        self.handle_drag(ui, frame, drop_target, &mut actions, t);
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
        if self.dirty {
            self.rebuild_view();
        }
        // Settings window (child viewport) last: it pauses this viewport while it draws.
        self.settings_win
            .set_agent_status(&ctx, &self.agent.status_line());
        if self
            .settings_win
            .show(&ctx, &mut self.settings, "FUIDE File Manager")
        {
            self.settings_changed(t);
        }
    }
}

impl Explorer {
    /// Per-frame life cycle of an in-app row drag: Esc cancels, releasing over a
    /// directory moves the files there, leaving the window hands the drag to the OS
    /// (Finder, browsers, ... take over), otherwise a ghost follows the pointer.
    fn handle_drag(
        &mut self,
        ui: &Ui,
        frame: &eframe::Frame,
        drop_target: Option<PathBuf>,
        actions: &mut Vec<Action>,
        t: f64,
    ) {
        let Some(drag) = &self.drag else { return };
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            self.drag = None;
            return;
        }
        if ui.input(|i| i.pointer.any_released()) {
            if let Some(dest) = drop_target {
                actions.push(Action::MoveTo {
                    dest,
                    paths: drag.paths.clone(),
                });
            }
            self.drag = None;
            return;
        }
        let Some(ptr) = ui.input(|i| i.pointer.latest_pos()) else {
            return;
        };
        if !ui.input(|i| i.content_rect()).contains(ptr) {
            self.start_os_drag(ui, frame, t);
            return;
        }

        // ghost: a small plate trailing the pointer (echo outline behind = "stack")
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let p = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::new("drag-ghost"),
        ));
        let galley = p.layout_no_wrap(drag.label.clone(), mono(ts.data), pal.accent);
        let rect = Rect::from_min_size(
            ptr + vec2(16.0, 12.0),
            vec2(galley.size().x + 26.0, ts.row + 6.0),
        );
        p.rect_stroke(
            rect.translate(vec2(4.0, 4.0)),
            egui::CornerRadius::ZERO,
            Stroke::new(1.0, pal.accent.gamma_multiply(0.25)),
            egui::StrokeKind::Inside,
        );
        p.rect_filled(rect, egui::CornerRadius::ZERO, pal.bg_deep);
        p.rect_stroke(
            rect,
            egui::CornerRadius::ZERO,
            Stroke::new(1.0, pal.accent.gamma_multiply(0.9)),
            egui::StrokeKind::Inside,
        );
        p.rect_filled(
            Rect::from_min_size(rect.min, vec2(3.0, rect.height())),
            egui::CornerRadius::ZERO,
            pal.accent,
        );
        let ty = rect.center().y - galley.size().y / 2.0;
        p.galley(pos2(rect.left() + 13.0, ty), galley, pal.accent);
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }

    /// Hand the active drag to macOS as an `NSDraggingSession` (`drag` crate): the files
    /// can then be dropped on the Finder, browsers, or any other app. Copy semantics —
    /// the receiver decides what to do with the file URLs.
    fn start_os_drag(&mut self, ui: &Ui, frame: &eframe::Frame, t: f64) {
        let Some(drag) = self.drag.take() else { return };
        let label = drag.label.clone();
        let badge =
            crate::dragbadge::badge_png(drag.paths.len(), palette(ui.ctx()).accent.to_array());
        let tx = self.drag_out.0.clone();
        let ctx = ui.ctx().clone();
        let result = drag::start_drag(
            frame,
            drag::DragItem::Files(drag.paths),
            drag::Image::Raw(badge),
            move |result, _cursor| {
                let _ = tx.send(result);
                ctx.request_repaint();
            },
            drag::Options {
                skip_animatation_on_cancel_or_failure: false,
                mode: drag::DragMode::Copy,
            },
        );
        match result {
            Ok(()) => self.push_log(
                t,
                format!("drag out // {label} :: session handed to macOS"),
                Level::Info,
            ),
            Err(e) => self.fail(t, format!("drag out // {label}"), &e.to_string()),
        }
    }

    fn ui_locations(
        &self,
        ui: &mut Ui,
        rect: Rect,
        actions: &mut Vec<Action>,
        drag_paths: Option<&[PathBuf]>,
        drop_target: &mut Option<PathBuf>,
    ) {
        // while rows are dragged, a sidebar entry under the pointer becomes a move target
        // (unless it is the current directory or inside the dragged items themselves)
        let tab = |ui: &mut Ui,
                   name: &str,
                   path: &PathBuf,
                   active: bool,
                   actions: &mut Vec<Action>,
                   drop_target: &mut Option<PathBuf>| {
            let resp = widgets::nav_tab(ui, name, active);
            if resp.clicked() {
                actions.push(Action::Navigate(path.clone()));
            }
            if let Some(dragged) = drag_paths {
                let valid = path != &self.cwd && !dragged.iter().any(|d| path.starts_with(d));
                if valid && ui.rect_contains_pointer(resp.rect) {
                    *drop_target = Some(path.clone());
                    ui.painter().rect_stroke(
                        resp.rect,
                        egui::CornerRadius::ZERO,
                        Stroke::new(1.5, palette(ui.ctx()).accent),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        };
        Panel::new("Locations").show_rect(ui, rect, |ui| {
            ScrollArea::vertical()
                .id_salt("locations")
                .auto_shrink([false, false])
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    widgets::section_label(ui, "Places");
                    for (name, path) in &self.places {
                        tab(ui, name, path, &self.cwd == path, actions, drop_target);
                    }
                    ui.add_space(6.0);
                    widgets::section_label(ui, "Volumes");
                    for (name, path) in &self.volumes {
                        tab(
                            ui,
                            name,
                            path,
                            self.cwd.starts_with(path),
                            actions,
                            drop_target,
                        );
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
                let resp = crumb(ui, label, last, &pal);
                if resp.clicked() {
                    actions.push(if last {
                        Action::OpenGoTo
                    } else {
                        Action::Navigate(comps[n - 1 - i].clone())
                    });
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

    fn ui_listing(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        actions: &mut Vec<Action>,
        drag_paths: Option<&[PathBuf]>,
        drop_target: &mut Option<PathBuf>,
        drop_hover: bool,
    ) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let row_h = ts.row;
        let count_tag = format!("{} items", self.view.len());
        let (title, title_tag_color) = match &self.last_error {
            None => ("Directory", pal.text_dim),
            Some(_) => ("Directory :: access denied", pal.danger),
        };
        let mut scroll_to = None;
        if self.scroll_to_selected {
            scroll_to = self
                .lead
                .and_then(|s| self.view.iter().position(|&v| v == s));
        }
        self.scroll_to_selected = false;
        let selected = &self.selected;
        // rows waiting on a Cmd+X are drawn faded until they are pasted
        let cut_paths: &[PathBuf] = match &self.clipboard {
            Some(c) if c.cut => &c.paths,
            _ => &[],
        };
        let view = &self.view;
        let entries = &self.entries;
        let sort_key = self.sort_key;
        let sort_desc = self.sort_desc;
        let cwd_label = self
            .cwd
            .file_name()
            .map(|s| s.to_string_lossy().to_uppercase())
            .unwrap_or_else(|| "ROOT".into());

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
                            let (r, resp) =
                                ui.allocate_exact_size(vec2(w, row_h), Sense::click_and_drag());
                            if scroll_to == Some(row) {
                                resp.scroll_to_me(None);
                            }
                            let is_sel = selected.contains(&idx);
                            // rows are addressable by file name (UI tests, assistive tech)
                            fuide::agent::describe(&resp, || {
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
                            // a directory row under an active drag is a move target
                            if let Some(dragged) = drag_paths {
                                if e.navigates()
                                    && !dragged.contains(&e.path)
                                    && ui.rect_contains_pointer(r)
                                {
                                    *drop_target = Some(e.path.clone());
                                    p.rect_filled(
                                        r,
                                        egui::CornerRadius::ZERO,
                                        pal.accent.gamma_multiply(0.10),
                                    );
                                    p.rect_stroke(
                                        r,
                                        egui::CornerRadius::ZERO,
                                        Stroke::new(1.5, pal.accent),
                                        egui::StrokeKind::Inside,
                                    );
                                }
                            }
                            let mut name_color = if e.hidden {
                                pal.text_dim
                            } else if e.is_dir {
                                pal.accent
                            } else {
                                pal.text
                            };
                            if cut_paths.contains(&e.path) {
                                name_color = name_color.gamma_multiply(0.45);
                            }
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
                            if resp.drag_started_by(egui::PointerButton::Primary) {
                                actions.push(Action::BeginDrag(idx));
                            }
                            if resp.double_clicked() {
                                actions.push(Action::Activate(idx));
                            } else if resp.clicked() {
                                // Finder-style modifiers: Cmd toggles, Shift extends
                                let mods = ui.input(|i| i.modifiers);
                                actions.push(if mods.command {
                                    Action::SelectToggle(idx)
                                } else if mods.shift {
                                    Action::SelectRange(idx)
                                } else {
                                    Action::Select(Some(idx))
                                });
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

                // files hovered in from another app (Finder, browser, ...): dropping
                // anywhere on the window moves them into the current directory
                if drop_hover {
                    let inner = ui.max_rect();
                    let p = ui.painter();
                    p.rect_filled(
                        inner,
                        egui::CornerRadius::ZERO,
                        pal.accent.gamma_multiply(0.06),
                    );
                    p.rect_stroke(
                        inner.shrink(2.0),
                        egui::CornerRadius::ZERO,
                        Stroke::new(1.5, pal.accent),
                        egui::StrokeKind::Inside,
                    );
                    let caption = format!("DROP // MOVE INTO {cwd_label}");
                    let galley = fuide::display_galley(p, caption, ts.heading + 3.0, pal.accent);
                    let plate =
                        Rect::from_center_size(inner.center(), galley.size() + vec2(28.0, 18.0));
                    p.rect_filled(plate, egui::CornerRadius::ZERO, pal.bg_deep);
                    p.rect_stroke(
                        plate,
                        egui::CornerRadius::ZERO,
                        Stroke::new(1.0, pal.accent),
                        egui::StrokeKind::Inside,
                    );
                    p.galley(plate.center() - galley.size() / 2.0, galley, pal.accent);
                }
            });
    }

    fn ui_inspector(&self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        if self.selected.len() > 1 {
            self.ui_inspector_multi(ui, rect, actions);
            return;
        }
        let sel = self.single_selected().map(|i| &self.entries[i]);
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
                    if widgets::button(ui, bsz, "PATH", true).clicked() {
                        actions.push(Action::CopyPaths(vec![path.clone()]));
                    }
                });
                ui.add_space(6.0);
                self.ui_clipboard_buttons(ui, actions);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let sel_idx = self.single_selected();
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
                        && sel_idx.is_some()
                    {
                        actions.push(Action::OpenDelete(false));
                    }
                });
                ui.add_space(8.0);
                let lh = ts.small + 5.0;
                // six lines of <= 41 chars: the inspector must still fit above the log at 1280x800
                let hints = [
                    "KEYS :: UP/DN SELECT  +SHIFT EXTEND",
                    "CMD+A ALL  ENTER OPEN  BKSP UP",
                    "CMD+C COPY  CMD+X CUT  CMD+V PASTE",
                    "CMD+R RENAME  CMD+BKSP TRASH  +OPT DELETE",
                    "CLICK CMD TOGGLE  SHIFT RANGE  DRAG MOVE",
                    "CMD+SHIFT+G GO TO PATH  CMD+[ ] HISTORY",
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

    /// `COPY / CUT / PASTE` row of the inspector (same as Cmd+C / X / V). Paste is enabled
    /// while the clipboard holds something and targets the current directory.
    fn ui_clipboard_buttons(&self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let ts = type_scale(ui.ctx());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let bsz = vec2(72.0, ts.row);
            let has_sel = !self.selected.is_empty();
            if widgets::button(ui, bsz, "COPY", has_sel).clicked() && has_sel {
                actions.push(Action::Copy);
            }
            if widgets::button(ui, bsz, "CUT", has_sel).clicked() && has_sel {
                actions.push(Action::Cut);
            }
            let can_paste = self.clipboard.is_some();
            if widgets::button(ui, bsz, "PASTE", can_paste).clicked() && can_paste {
                actions.push(Action::Paste(None));
            }
        });
    }

    /// Inspector when several rows are selected: aggregate stats and bulk actions.
    fn ui_inspector_multi(&self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let items: Vec<&Entry> = self
            .selected
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .collect();
        let dirs = items.iter().filter(|e| e.is_dir).count();
        let bytes: u64 = items.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
        Panel::new("Inspector")
            .tag("multi", pal.text_dim)
            .show_rect(ui, rect, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("{} ITEMS SELECTED", items.len()))
                            .font(mono(ts.data + 3.0))
                            .color(pal.accent),
                    )
                    .wrap(),
                );
                ui.add_space(6.0);
                widgets::rule(ui);
                widgets::readout(ui, "items", &items.len().to_string(), None);
                widgets::readout(ui, "dirs", &dirs.to_string(), None);
                widgets::readout(ui, "files", &(items.len() - dirs).to_string(), None);
                widgets::readout(ui, "bytes", &fs::fmt_size(bytes), None);
                ui.add_space(6.0);
                widgets::rule(ui);
                const SHOWN: usize = 9;
                for e in items.iter().take(SHOWN) {
                    widgets::readout(ui, e.kind.tag(), &e.name, None);
                }
                if items.len() > SHOWN {
                    widgets::readout(
                        ui,
                        "",
                        &format!("+ {} MORE", items.len() - SHOWN),
                        Some(pal.text_dim),
                    );
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if widgets::button(ui, vec2(80.0, ts.row), "PATHS", true).clicked() {
                        actions.push(Action::CopyPaths(
                            items.iter().map(|e| e.path.clone()).collect(),
                        ));
                    }
                    if widgets::button_colored(ui, vec2(80.0, ts.row), "TRASH", true, pal.warn)
                        .clicked()
                    {
                        actions.push(Action::OpenDelete(false));
                    }
                    if widgets::button_colored(ui, vec2(80.0, ts.row), "DELETE", true, pal.danger)
                        .clicked()
                    {
                        actions.push(Action::OpenDelete(true));
                    }
                });
                ui.add_space(6.0);
                self.ui_clipboard_buttons(ui, actions);
                ui.add_space(8.0);
                let (fr, _) = ui.allocate_exact_size(
                    vec2(ui.available_width(), ts.small + 5.0),
                    Sense::hover(),
                );
                ui.painter().text(
                    pos2(fr.left(), fr.center().y),
                    Align2::LEFT_CENTER,
                    "DRAG :: DIR = MOVE  OUTSIDE = OS",
                    mono(ts.small),
                    pal.text_dim,
                );
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
        let tab = open && ctx.input(|i| i.key_pressed(Key::Tab));
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
            DialogState::GoTo {
                path,
                error,
                focus,
                suggestions,
                suggested_for,
            } => {
                if *suggested_for != *path {
                    *suggestions = fs::complete_goto(path, &self.cwd, 6);
                    *suggested_for = path.clone();
                }
                let resp = Dialog::new("Go to")
                    .tag("path", pal.text_dim)
                    .width(560.0)
                    .show(ctx, open, |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        let input =
                            widgets::text_input(ui, ui.available_width(), path, "go to path");
                        if *focus {
                            // cursor at the end: the field opens with the current directory and
                            // a trailing `/`, and a completion is appended to
                            input.request_focus();
                            let mut st =
                                egui::TextEdit::load_state(ui.ctx(), input.id).unwrap_or_default();
                            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                                egui::text::CCursor::new(path.chars().count()),
                            )));
                            st.store(ui.ctx(), input.id);
                            *focus = false;
                        }
                        let submit = input.lost_focus() && enter;
                        // egui moves focus away on Tab before widgets run, so the field reports
                        // `lost_focus`; complete and take the focus back
                        let complete = tab && (input.has_focus() || input.lost_focus());
                        if complete {
                            *focus = true;
                        }
                        // completions: names only, one line, dim
                        let names: Vec<String> = suggestions
                            .iter()
                            .map(|s| {
                                s.trim_end_matches('/')
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or(s)
                                    .to_string()
                            })
                            .collect();
                        let (lr, _) = ui.allocate_exact_size(
                            vec2(ui.available_width(), ts.label + 6.0),
                            Sense::hover(),
                        );
                        let (text, color) = match error {
                            Some(e) => (format!("REJECTED :: {}", e.to_uppercase()), pal.danger),
                            None if names.is_empty() => (
                                "~ AND RELATIVE PATHS OK :: TAB COMPLETES".to_string(),
                                pal.text_dim,
                            ),
                            None => (format!("TAB :: {}", names.join("  ")), pal.text_dim),
                        };
                        ui.painter()
                            .with_clip_rect(lr.intersect(ui.clip_rect()))
                            .text(
                                pos2(lr.left(), lr.center().y),
                                Align2::LEFT_CENTER,
                                text,
                                mono(ts.label),
                                color,
                            );
                        ui.add_space(8.0);
                        let clicked = fuide::dialog::button_row(
                            ui,
                            &[
                                ("CANCEL", pal.text_dim, true),
                                ("GO", pal.accent, !path.trim().is_empty()),
                            ],
                        );
                        (submit, complete, clicked)
                    });
                finished = resp.finished;
                let (submit, complete, clicked) = resp.inner.unwrap_or((false, false, None));
                if !open {
                    // fading out: ignore input
                } else if resp.should_close || clicked == Some(0) {
                    actions.push(Action::CloseDialog);
                } else if submit || clicked == Some(1) {
                    actions.push(Action::ConfirmGoTo);
                } else if complete {
                    actions.push(Action::CompleteGoTo);
                }
            }
            DialogState::Delete { indices, permanent } => {
                let items: Vec<&Entry> = indices
                    .iter()
                    .filter_map(|&i| self.entries.get(i))
                    .collect();
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
                let tag = match &items[..] {
                    [e] => e.kind.tag().to_string(),
                    _ => format!("{} items", items.len()),
                };
                let resp = Dialog::new(title)
                    .tag(tag, pal.text_dim)
                    .outline(color)
                    .width(460.0)
                    .show(ctx, open, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        match &items[..] {
                            [entry] => {
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
                                widgets::readout(
                                    ui,
                                    "modified",
                                    &fs::fmt_time(entry.modified),
                                    None,
                                );
                            }
                            _ => {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(format!("{} ITEMS", items.len()))
                                            .font(mono(ts.data + 2.0))
                                            .color(pal.accent),
                                    )
                                    .wrap(),
                                );
                                ui.add_space(4.0);
                                const SHOWN: usize = 6;
                                for e in items.iter().take(SHOWN) {
                                    widgets::readout(ui, e.kind.tag(), &e.name, None);
                                }
                                if items.len() > SHOWN {
                                    widgets::readout(
                                        ui,
                                        "",
                                        &format!("+ {} MORE", items.len() - SHOWN),
                                        Some(pal.text_dim),
                                    );
                                }
                                let dirs = items.iter().filter(|e| e.is_dir).count();
                                let bytes: u64 =
                                    items.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
                                widgets::readout(ui, "dirs", &dirs.to_string(), None);
                                widgets::readout(ui, "files bytes", &fs::fmt_size(bytes), None);
                            }
                        }
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

    /// Returns `true` when the title chip was clicked (the caller flips `Settings::log_open`).
    fn ui_log(&self, ui: &mut Ui, rect: Rect, _t: f64, open: bool) -> bool {
        let pal = palette(ui.ctx());
        let (_, toggled) = Panel::new("Event log")
            .tag(format!("{} events", self.log.len()), pal.text_dim)
            .padding(8.0, 12.0)
            .show_collapsible_rect(ui, rect, open, |ui| {
                if !open {
                    return; // just the header strip
                }
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
        toggled
    }
}

/// Breadcrumb segment. Ancestors navigate; the last one (the current directory) opens the
/// go-to-path dialog.
fn crumb(ui: &mut Ui, label: &str, last: bool, pal: &Palette) -> egui::Response {
    let size = type_scale(ui.ctx()).heading;
    let galley = fuide::display_galley(ui.painter(), label, size, pal.text);
    let size = galley.size() + vec2(8.0, 6.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    fuide::agent::describe(&resp, || {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            if last {
                format!("{label} (go to path)")
            } else {
                label.to_string()
            },
        )
    });
    let color = if last {
        pal.accent
    } else if resp.hovered() {
        pal.text
    } else {
        pal.text.gamma_multiply(0.7)
    };
    if resp.hovered() {
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
