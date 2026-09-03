//! FUIDE Player — audio / video player with a tactical-console look.

use std::path::PathBuf;

use egui::{pos2, vec2, Align2, Color32, Id, Key, Rect, RichText, ScrollArea, Sense, Stroke, Ui};
use fuide::widgets::{self, Icon, LogLine};
use fuide::{
    fx, mono, palette, theme, type_scale, Dialog, PaletteKind, Panel, Settings, SettingsWindow,
    Shell,
};

use crate::audiotap::{Spectrum, FFT_LEN};
use crate::filepicker::FilePicker;
use crate::player::{Backend, Snapshot, Source, Status};

const LEFT_W: f32 = 260.0;
const RIGHT_W: f32 = 300.0;
const GAP: f32 = 14.0;
/// Transport panel padding (x, y) — its height follows the type scale, see `transport_h`.
const TRANSPORT_PAD: (f32, f32) = (10.0, 12.0);
/// Transport buttons are taller than the standard row: they are the main controls.
fn button_h(ts: &fuide::TypeScale) -> f32 {
    (ts.row + 10.0).round()
}
/// Height of the transport panel: padding, seek bar row, spacing, button row.
fn transport_h(ts: &fuide::TypeScale) -> f32 {
    2.0 * TRANSPORT_PAD.1 + ts.row + 6.0 + button_h(ts)
}
/// Default log panel height; the divider above it is draggable (`Settings::log_height`).
const LOG_H: f32 = 100.0;
const LOG_MIN: f32 = 60.0;
/// Header strip left when the log panel is collapsed (`Settings::log_open` = false).
const LOG_CLOSED: f32 = 26.0;
/// Seconds the panels take to slide in / out (Tab) and the log to fold (its chip).
const PANEL_ANIM: f32 = 0.24;
/// Space kept for the panels above the log when the divider is dragged up.
const BODY_MIN: f32 = 320.0;
/// Arrow keys: seconds per press (Shift multiplies by 6).
const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f32 = 0.05;
const SPEEDS: [f32; 5] = [0.5, 1.0, 1.25, 1.5, 2.0];

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopMode {
    Off,
    One,
    All,
}

impl LoopMode {
    fn next(self) -> Self {
        match self {
            LoopMode::Off => LoopMode::All,
            LoopMode::All => LoopMode::One,
            LoopMode::One => LoopMode::Off,
        }
    }
    fn label(self) -> &'static str {
        match self {
            LoopMode::Off => "LOOP OFF",
            LoopMode::One => "LOOP ONE",
            LoopMode::All => "LOOP ALL",
        }
    }
}

/// A queued item. `title` starts as the source's label and becomes the embedded title once
/// the track has played and its metadata loaded.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub source: Source,
    pub title: String,
    pub duration: Option<f64>,
}

/// The open dialog (if any) plus whether it is fading out.
struct OpenDialog {
    state: DialogState,
    closing: bool,
}

enum DialogState {
    /// Cmd+L: a URL or a local path (the in-app dialog; the agent's way to open things).
    Open {
        input: String,
        error: Option<String>,
        focus: bool,
        suggestions: Vec<String>,
        suggested_for: String,
    },
    /// Big `ERROR` card; details live in the event log.
    Error { line: String },
}

enum Action {
    /// Queue sources; `play` starts the first of them (otherwise only when nothing plays).
    Add {
        sources: Vec<Source>,
        play: bool,
    },
    PlayIndex(usize),
    Toggle,
    /// Tab: show / hide the queue, media, transport and log panels around the picture.
    TogglePanels,
    /// Pause and rewind.
    Stop,
    Next,
    Prev,
    Seek(f64),
    SeekBy(f64),
    Volume(f32),
    ToggleMute,
    CycleSpeed,
    CycleLoop,
    Select(Option<usize>),
    Remove(usize),
    /// Reorder the queue: the track at `from` ends up at index `to`.
    Move {
        from: usize,
        to: usize,
    },
    Clear,
    /// F / double-click on the picture: macOS full screen (the shell chrome is hidden there).
    ToggleFullscreen,
    /// C / the CC chip: off -> first subtitle track -> ... -> off.
    CycleSubtitle,
    SelectSubtitle(Option<usize>),
    /// `[` / `]`: jump to the previous / next chapter start.
    PrevChapter,
    NextChapter,
    /// Chapter row in the media panel.
    SeekChapter(usize),
    /// Cmd+L: the in-app URL / path dialog.
    OpenDialog,
    /// Cmd+O: the macOS open-file dialog (falls back to the in-app one where it cannot show).
    OpenFiles,
    /// Confirm the open dialog; `play` = start it now (vs. queue only).
    ConfirmOpen {
        play: bool,
    },
    CompleteOpen,
    CloseDialog,
    Palette(PaletteKind),
    OpenSettings,
}

/// Settings file name (`Settings::path`).
const APP_ID: &str = "player";

pub struct PlayerApp {
    backend: Box<dyn Backend>,
    queue: Vec<Track>,
    /// Index of the track loaded in the backend.
    current: Option<usize>,
    /// Highlighted queue row (Enter plays it, Backspace removes it).
    selected: Option<usize>,
    /// Latest backend state (updated every frame).
    snap: Snapshot,
    loop_mode: LoopMode,
    /// Base for relative paths typed in the open dialog.
    cwd: PathBuf,
    /// macOS open-file dialog; `native_open` = use it for Cmd+O (off in tests).
    picker: FilePicker,
    native_open: bool,
    /// Seek bar drag in progress: the time under the pointer.
    scrub: Option<f64>,
    /// Spectrum analyser fed from the backend's audio tap every frame.
    spectrum: Spectrum,
    /// Frame path last logged (`gpu` / `cpu`), so the log gets one line per item.
    frame_path_logged: &'static str,
    /// Latest stereo peaks (left, right), smoothed.
    meters: (f32, f32),
    /// Queue row being dragged (its index) and where it would land.
    queue_drag: Option<usize>,
    queue_drop: Option<usize>,
    /// Panels around the picture are shown (Tab). Off by default: the picture with its HUD
    /// strip and the transport under it, nothing else.
    panels: bool,
    log: Vec<Event>,
    dialog: Option<OpenDialog>,
    /// Errors waiting for the dialog slot (only one dialog at a time).
    error_queue: std::collections::VecDeque<String>,
    settings: Settings,
    settings_win: SettingsWindow,
    /// Where settings are saved; `None` = not persisted (tests, or no config dir).
    settings_path: Option<PathBuf>,
    /// Current log panel height (draggable divider).
    log_h: f32,
    devshot: fuide::devshot::DevShot,
    /// MCP interface for AI agents (`fuide::agent`); on/off in the settings window.
    agent: fuide::Agent,
    /// Dev aid: `FUIDE_DEV_DIALOG=open|error` opens that dialog at start.
    dev_dialog: Option<String>,
    /// Dev aid: `FUIDE_DEV_DIALOG_CLOSE=<frame>` closes the dev dialog at that frame.
    dev_close_frame: Option<u32>,
    dev_frame: u32,
}

impl PlayerApp {
    /// Production entry point: settings from disk, AVFoundation backend, command-line items
    /// (`fuide-player [FILE|URL]...`) queued and the first one playing.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = Settings::load(APP_ID).unwrap_or_else(|| Settings::new(PaletteKind::Green));
        let backend: Box<dyn Backend> = match crate::player::Engine::new() {
            Some(e) => Box::new(e),
            None => panic!("the player must be created on the main thread"),
        };
        let mut app = Self::with_context(&cc.egui_ctx, settings, backend);
        app.native_open = true;
        if let Some(path) = Settings::path(APP_ID) {
            app.persist_settings_to(path);
        }
        // command-line items are relative to where the command ran, not to $HOME
        let cwd = std::env::current_dir().unwrap_or_else(|_| app.cwd.clone());
        let mut sources = Vec::new();
        for arg in std::env::args().skip(1) {
            match Source::parse(&arg, &cwd) {
                Ok(s) => sources.push(s),
                Err(e) => app.push_log(0.0, format!("open // {arg} :: {e}"), Level::Danger),
            }
        }
        if !sources.is_empty() {
            app.apply(
                &cc.egui_ctx,
                Action::Add {
                    sources,
                    play: true,
                },
                0.0,
            );
        }
        app
    }

    /// Save settings changes to `path` from now on (apps built with [`Self::with_context`] do
    /// not persist by default).
    pub fn persist_settings_to(&mut self, path: PathBuf) {
        self.settings_path = Some(path);
    }

    /// Build the app on any `egui::Context` with an explicit backend (tests use the fake).
    pub fn with_context(
        ctx: &egui::Context,
        settings: Settings,
        backend: Box<dyn Backend>,
    ) -> Self {
        theme::install(ctx, settings.palette.palette(), theme::macos_cjk_fallback());
        settings.apply(ctx);
        let cwd = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut app = Self {
            backend,
            queue: Vec::new(),
            current: None,
            selected: None,
            snap: Snapshot::default(),
            loop_mode: LoopMode::Off,
            cwd,
            picker: FilePicker::default(),
            native_open: false,
            scrub: None,
            spectrum: Spectrum::default(),
            frame_path_logged: "none",
            meters: (0.0, 0.0),
            panels: false,
            queue_drag: None,
            queue_drop: None,
            log: Vec::new(),
            dialog: None,
            error_queue: std::collections::VecDeque::new(),
            log_h: settings.log_height.unwrap_or(LOG_H),
            settings,
            settings_win: SettingsWindow::default(),
            settings_path: None,
            devshot: fuide::devshot::DevShot::from_env(),
            agent: fuide::Agent::new(APP_ID, "FUIDE Player"),
            dev_dialog: std::env::var("FUIDE_DEV_DIALOG").ok(),
            dev_close_frame: std::env::var("FUIDE_DEV_DIALOG_CLOSE")
                .ok()
                .and_then(|v| v.parse().ok()),
            dev_frame: 0,
        };
        app.push_log(
            0.0,
            "player online :: avfoundation link established",
            Level::Ok,
        );
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
        // Dev aid: `FUIDE_DEV_MUTE=1` starts muted (screenshots, demos).
        if std::env::var_os("FUIDE_DEV_MUTE").is_some() {
            app.backend.set_muted(true);
        }
        if let Ok(text) = std::env::var("FUIDE_DEV_LOG") {
            app.push_log(0.0, text, Level::Danger);
        }
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

    fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|i| self.queue.get(i))
    }

    /// The row the inspector describes: the playing track, else the selected one.
    fn focus_index(&self) -> Option<usize> {
        self.current.or(self.selected)
    }

    /// State summary for the agent's `observe` (what the widgets alone do not say).
    fn agent_state(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let state = match (&self.snap.status, self.snap.playing) {
            (Status::Idle, _) => "idle".to_string(),
            (Status::Loading, _) => "loading".to_string(),
            (Status::Failed(e), _) => format!("failed ({e})"),
            (Status::Ready, true) if self.snap.stalled => "buffering".to_string(),
            (Status::Ready, true) => "playing".to_string(),
            (Status::Ready, false) => "paused".to_string(),
        };
        let _ = writeln!(
            s,
            "state: {state} :: {} / {} :: volume {:.0}%{} :: speed {}x :: {}",
            fmt_clock(self.snap.time),
            self.snap
                .duration
                .map(fmt_clock)
                .unwrap_or_else(|| "--:--".into()),
            self.snap.volume * 100.0,
            if self.snap.muted { " (muted)" } else { "" },
            self.snap.rate,
            self.loop_mode.label().to_lowercase()
        );
        let _ = writeln!(
            s,
            "panels: {} (tab toggles the queue / media / transport / log panels)",
            if self.panels { "shown" } else { "hidden" }
        );
        if self.queue_drag.is_some() {
            s.push_str("dragging a queue row\n");
        }
        s.push_str(
            "open: cmd+l = in-app dialog for a URL or a local path (use this); cmd+o = macOS file dialog (not visible to you)\n",
        );
        if self.picker.is_open() {
            s.push_str("macOS open-file dialog is up (a person has to finish or cancel it)\n");
        }
        let _ = writeln!(s, "queue: {} tracks", self.queue.len());
        for (i, t) in self.queue.iter().enumerate() {
            let _ = writeln!(
                s,
                "  {}{} {} ({})",
                if self.current == Some(i) { ">" } else { " " },
                if self.selected == Some(i) { "*" } else { " " },
                t.title,
                t.source.display()
            );
        }
        if let Some(t) = self.current_track() {
            let info = self.backend.info();
            if !info.chapters.is_empty() {
                let cur = info.chapter_at(self.snap.time);
                let list: Vec<String> = info
                    .chapters
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        format!(
                            "{}{} {} @{}",
                            if cur == Some(i) { ">" } else { "" },
                            i + 1,
                            c.title,
                            fmt_clock(c.start)
                        )
                    })
                    .collect();
                let _ = writeln!(s, "chapters: {} ([ ] keys, CHAPTER rows)", list.join(", "));
            }
            if info.video.is_some() {
                let _ = writeln!(s, "frames: {}", self.backend.frame_path());
            }
            let subs = self.backend.subtitles();
            if !subs.is_empty() {
                let _ = writeln!(
                    s,
                    "subtitles: {} :: current {} (c key cycles)",
                    subs.join(", "),
                    self.backend
                        .subtitle()
                        .and_then(|i| subs.get(i).cloned())
                        .unwrap_or_else(|| "off".into())
                );
            }
            let _ = writeln!(
                s,
                "now: {} :: {}{}{}",
                t.title,
                info.video
                    .as_ref()
                    .map(|v| format!("{} {}x{} ", v.codec, v.width, v.height))
                    .unwrap_or_default(),
                info.audio
                    .as_ref()
                    .map(|a| format!("{} {:.0} Hz {}ch", a.codec, a.sample_rate, a.channels))
                    .unwrap_or_default(),
                if t.source.is_remote() {
                    " :: network stream"
                } else {
                    ""
                }
            );
        }
        if self.dialog.as_ref().is_some_and(|d| d.closing) {
            s.push_str("dialog: closing\n");
        }
        match self.dialog.as_ref().filter(|d| !d.closing) {
            Some(OpenDialog {
                state:
                    DialogState::Open {
                        input,
                        error,
                        suggestions,
                        ..
                    },
                ..
            }) => {
                let _ = writeln!(
                    s,
                    "dialog: OPEN :: path or url {input:?}{}{} :: buttons CANCEL / ADD TO QUEUE / PLAY (tab completes)",
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
                state: DialogState::Error { line },
                ..
            }) => {
                let _ = writeln!(s, "dialog: ERROR :: {line} :: button ACKNOWLEDGE");
            }
            None => {}
        }
        s
    }

    // ------------------------------------------------------------------ playback

    /// Load queue entry `i` into the backend and start it.
    fn play_index(&mut self, t: f64, i: usize) {
        let Some(track) = self.queue.get(i).cloned() else {
            return;
        };
        self.current = Some(i);
        self.selected = Some(i);
        match self.backend.open(&track.source) {
            Ok(()) => {
                self.backend.play();
                self.push_log(
                    t,
                    format!(
                        "play // {}{}",
                        track.title,
                        if track.source.is_remote() {
                            " :: network"
                        } else {
                            ""
                        }
                    ),
                    Level::Ok,
                );
            }
            Err(e) => {
                self.current = None;
                self.fail(t, format!("open // {}", track.title), &e);
            }
        }
    }

    /// The queue index after `i` under the loop mode (`None` = end of queue).
    fn next_index(&self, i: usize) -> Option<usize> {
        match self.loop_mode {
            LoopMode::One => Some(i),
            LoopMode::All if !self.queue.is_empty() => Some((i + 1) % self.queue.len()),
            _ => (i + 1 < self.queue.len()).then_some(i + 1),
        }
    }

    /// Feed the spectrum analyser and the level meters from the audio tap (decays when the
    /// tap is silent or absent).
    fn analyse_audio(&mut self, dt: f32) {
        let mut rate = 0.0;
        let mut got = false;
        let mut peaks = (0.0, 0.0);
        if let Some(audio) = self.backend.audio() {
            if self.snap.playing {
                rate = audio.sample_rate() as f32;
                got = audio.latest(FFT_LEN, self.spectrum.scratch());
                peaks = audio.peaks();
            }
        }
        if got {
            let samples = std::mem::take(self.spectrum.scratch());
            self.spectrum.update(&samples, rate, dt);
            *self.spectrum.scratch() = samples;
        } else {
            self.spectrum.update(&[], rate, dt);
        }
        let ease = |cur: f32, target: f32| {
            if target > cur {
                target
            } else {
                cur + (target - cur) * (dt * 6.0).min(1.0)
            }
        };
        self.meters = (ease(self.meters.0, peaks.0), ease(self.meters.1, peaks.1));
    }

    /// Housekeeping after a backend poll: titles / durations from the loaded item, and what
    /// to do when a track ends.
    fn after_poll(&mut self, t: f64) {
        let path = self.backend.frame_path();
        if path != self.frame_path_logged {
            self.frame_path_logged = path;
            match path {
                "gpu" => self.push_log(
                    t,
                    "video // frames shared with the gpu (iosurface -> metal texture)",
                    Level::Ok,
                ),
                "cpu" => self.push_log(t, "video // frames copied through the cpu", Level::Warn),
                _ => {}
            }
        }
        if let Some(i) = self.current {
            let info = self.backend.info().clone();
            if let Some(track) = self.queue.get_mut(i) {
                if let Some(d) = self.snap.duration {
                    track.duration = Some(d);
                }
                if let Some(title) = info.title.filter(|s| !s.trim().is_empty()) {
                    track.title = title;
                }
            }
            if let Status::Failed(e) = &self.snap.status {
                let title = self.queue[i].title.clone();
                let e = e.clone();
                self.current = None;
                self.backend.close();
                self.fail(t, format!("play // {title}"), &e);
                return;
            }
        }
        if self.snap.ended {
            let Some(i) = self.current else { return };
            match self.next_index(i) {
                Some(n) => {
                    self.push_log(t, "end of track :: advancing", Level::Info);
                    self.play_index(t, n);
                }
                None => self.push_log(t, "end of queue", Level::Info),
            }
        }
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action, t: f64) {
        match action {
            Action::Add { sources, play } => {
                if sources.is_empty() {
                    return;
                }
                let first = self.queue.len();
                for s in sources {
                    self.push_log(
                        t,
                        format!(
                            "queue // {}{}",
                            s.label(),
                            if s.is_remote() { " :: network" } else { "" }
                        ),
                        Level::Info,
                    );
                    self.queue.push(Track {
                        title: s.label(),
                        source: s,
                        duration: None,
                    });
                }
                if play || self.current.is_none() {
                    self.play_index(t, first);
                } else if self.selected.is_none() {
                    self.selected = Some(first);
                }
            }
            Action::PlayIndex(i) => self.play_index(t, i),
            Action::Toggle => {
                if self.current.is_some() {
                    self.backend.toggle();
                } else if let Some(i) = self.selected.or((!self.queue.is_empty()).then_some(0)) {
                    self.play_index(t, i);
                }
            }
            Action::Stop => {
                self.backend.pause();
                self.backend.seek(0.0, true);
            }
            Action::Next => {
                if let Some(i) = self.current {
                    let n = if self.loop_mode == LoopMode::One {
                        (i + 1 < self.queue.len()).then_some(i + 1)
                    } else {
                        self.next_index(i)
                    };
                    match n {
                        Some(n) => self.play_index(t, n),
                        None => self.push_log(t, "end of queue", Level::Info),
                    }
                }
            }
            Action::Prev => {
                if let Some(i) = self.current {
                    // like most players: within the first seconds go to the previous track,
                    // otherwise restart this one
                    if self.snap.time > 3.0 || i == 0 {
                        self.backend.seek(0.0, true);
                    } else {
                        self.play_index(t, i - 1);
                    }
                }
            }
            Action::Seek(secs) => self.backend.seek(secs, true),
            Action::SeekBy(d) => self.backend.seek_by(d),
            Action::Volume(v) => {
                self.backend.set_volume(v);
                if v > 0.0 && self.backend.muted() {
                    self.backend.set_muted(false);
                }
            }
            Action::ToggleMute => {
                let m = !self.backend.muted();
                self.backend.set_muted(m);
                self.push_log(
                    t,
                    if m {
                        "audio // muted"
                    } else {
                        "audio // unmuted"
                    },
                    Level::Info,
                );
            }
            Action::CycleSpeed => {
                let cur = self.backend.speed();
                let pos = SPEEDS.iter().position(|&s| (s - cur).abs() < 1e-3);
                let next = SPEEDS[pos.map_or(1, |p| (p + 1) % SPEEDS.len())];
                self.backend.set_speed(next);
                self.push_log(t, format!("speed // {next}x"), Level::Info);
            }
            Action::TogglePanels => {
                self.panels = !self.panels;
                // egui's Tab navigation focused some widget on the way; drop that focus
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
                self.push_log(
                    t,
                    if self.panels {
                        "view // panels shown"
                    } else {
                        "view // picture only"
                    },
                    Level::Info,
                );
            }
            Action::CycleLoop => {
                self.loop_mode = self.loop_mode.next();
                self.push_log(
                    t,
                    format!("loop // {}", self.loop_mode.label().to_lowercase()),
                    Level::Info,
                );
            }
            Action::Select(i) => self.selected = i.filter(|&i| i < self.queue.len()),
            Action::Remove(i) => {
                if i >= self.queue.len() {
                    return;
                }
                let track = self.queue.remove(i);
                self.push_log(t, format!("dequeue // {}", track.title), Level::Warn);
                match self.current {
                    Some(c) if c == i => {
                        self.current = None;
                        self.backend.close();
                        self.snap = Snapshot::default();
                    }
                    Some(c) if c > i => self.current = Some(c - 1),
                    _ => {}
                }
                self.selected = match self.selected {
                    Some(s) if s > i => Some(s - 1),
                    Some(s) if s == i => (i < self.queue.len()).then_some(i).or(i.checked_sub(1)),
                    other => other,
                };
            }
            Action::Move { from, to } => {
                let n = self.queue.len();
                if from >= n || to >= n || from == to {
                    return;
                }
                let track = self.queue.remove(from);
                self.queue.insert(to, track);
                let remap = |i: usize| -> usize {
                    if i == from {
                        to
                    } else if from < to && i > from && i <= to {
                        i - 1
                    } else if to < from && i >= to && i < from {
                        i + 1
                    } else {
                        i
                    }
                };
                self.current = self.current.map(remap);
                self.selected = self.selected.map(remap);
                self.push_log(
                    t,
                    format!("queue // moved {} -> {}", from + 1, to + 1),
                    Level::Info,
                );
            }
            Action::CycleSubtitle => {
                let n = self.backend.subtitles().len();
                if n == 0 {
                    self.push_log(t, "subtitles // none in this item", Level::Info);
                    return;
                }
                let next = match self.backend.subtitle() {
                    None => Some(0),
                    Some(i) if i + 1 < n => Some(i + 1),
                    Some(_) => None,
                };
                self.apply(ctx, Action::SelectSubtitle(next), t);
            }
            Action::SelectSubtitle(idx) => {
                self.backend.select_subtitle(idx);
                let name = idx
                    .and_then(|i| self.backend.subtitles().get(i).cloned())
                    .unwrap_or_else(|| "off".into());
                self.push_log(t, format!("subtitles // {name}"), Level::Info);
            }
            Action::PrevChapter | Action::NextChapter => {
                let info = self.backend.info();
                if info.chapters.is_empty() {
                    return;
                }
                // the backend's clock, not the last snapshot: seeks in the same frame count
                let now = self.backend.time();
                let cur = info.chapter_at(now);
                let target = match (&action, cur) {
                    // like most players: within the first seconds of a chapter go back one
                    (Action::PrevChapter, Some(i)) => {
                        if now - info.chapters[i].start > 3.0 || i == 0 {
                            Some(i)
                        } else {
                            Some(i - 1)
                        }
                    }
                    (Action::PrevChapter, None) => Some(0),
                    (_, Some(i)) => (i + 1 < info.chapters.len()).then_some(i + 1),
                    (_, None) => Some(0),
                };
                if let Some(i) = target {
                    self.apply(ctx, Action::SeekChapter(i), t);
                }
            }
            Action::SeekChapter(i) => {
                if let Some(c) = self.backend.info().chapters.get(i).cloned() {
                    self.backend.seek(c.start, true);
                    self.push_log(t, format!("chapter // {} {}", i + 1, c.title), Level::Info);
                }
            }
            Action::ToggleFullscreen => {
                let now = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!now));
                self.push_log(
                    t,
                    if now {
                        "view // window"
                    } else {
                        "view // full screen"
                    },
                    Level::Info,
                );
            }
            Action::Clear => {
                if self.queue.is_empty() {
                    return;
                }
                self.push_log(
                    t,
                    format!("queue // cleared {} tracks", self.queue.len()),
                    Level::Warn,
                );
                self.queue.clear();
                self.current = None;
                self.selected = None;
                self.backend.close();
                self.snap = Snapshot::default();
            }
            Action::OpenFiles => {
                let start = self
                    .current_track()
                    .and_then(|t| match &t.source {
                        Source::File(p) => p.parent().map(|d| d.to_path_buf()),
                        Source::Url(_) => None,
                    })
                    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Movies")));
                if self.native_open && self.picker.show(start.as_deref()) {
                    self.push_log(t, "open // macOS file dialog", Level::Info);
                } else {
                    self.apply(ctx, Action::OpenDialog, t);
                }
            }
            Action::OpenDialog => {
                self.dialog = Some(OpenDialog {
                    state: DialogState::Open {
                        input: String::new(),
                        error: None,
                        focus: true,
                        suggestions: Vec::new(),
                        suggested_for: String::new(),
                    },
                    closing: false,
                });
            }
            Action::ConfirmOpen { play } => {
                let Some(OpenDialog {
                    state: DialogState::Open { input, error, .. },
                    closing: false,
                }) = &mut self.dialog
                else {
                    return;
                };
                match Source::parse(input, &self.cwd) {
                    Ok(source) => {
                        if let Some(d) = &mut self.dialog {
                            d.closing = true;
                        }
                        self.apply(
                            ctx,
                            Action::Add {
                                sources: vec![source],
                                play,
                            },
                            t,
                        );
                    }
                    Err(e) => *error = Some(e),
                }
            }
            Action::CompleteOpen => {
                let Some(OpenDialog {
                    state:
                        DialogState::Open {
                            input, suggestions, ..
                        },
                    closing: false,
                }) = &mut self.dialog
                else {
                    return;
                };
                let filled = match suggestions.len() {
                    0 => return,
                    1 => suggestions[0].clone(),
                    _ => fuide::pathinput::common_prefix(suggestions),
                };
                if filled.len() > input.len() {
                    *input = filled;
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
        }
    }

    /// Log + persist after the settings changed (shortcut or settings window).
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
        // only a focused *text field* owns the keyboard; a button focused by egui's Tab
        // navigation must not swallow the shortcuts
        let focused = ui.memory(|m| m.focused());
        if focused.is_some_and(|id| egui::TextEdit::load_state(ui.ctx(), id).is_some()) {
            return;
        }
        ui.input(|i| {
            let cmd = i.modifiers.command;
            let shift = i.modifiers.shift;
            if !cmd && i.key_pressed(Key::Space) {
                actions.push(Action::Toggle);
            }
            if !cmd && i.key_pressed(Key::Tab) {
                actions.push(Action::TogglePanels);
            }
            if cmd && i.key_pressed(Key::ArrowRight) {
                actions.push(Action::Next);
            } else if cmd && i.key_pressed(Key::ArrowLeft) {
                actions.push(Action::Prev);
            } else {
                let step = if shift { SEEK_STEP * 6.0 } else { SEEK_STEP };
                if i.key_pressed(Key::ArrowRight) {
                    actions.push(Action::SeekBy(step));
                }
                if i.key_pressed(Key::ArrowLeft) {
                    actions.push(Action::SeekBy(-step));
                }
            }
            if cmd && i.key_pressed(Key::ArrowUp) {
                if let Some(s) = self.selected.filter(|&s| s > 0) {
                    actions.push(Action::Move { from: s, to: s - 1 });
                }
            } else if cmd && i.key_pressed(Key::ArrowDown) {
                if let Some(s) = self.selected.filter(|&s| s + 1 < self.queue.len()) {
                    actions.push(Action::Move { from: s, to: s + 1 });
                }
            } else {
                if i.key_pressed(Key::ArrowUp) {
                    actions.push(Action::Volume(self.snap.volume + VOLUME_STEP));
                }
                if i.key_pressed(Key::ArrowDown) {
                    actions.push(Action::Volume(self.snap.volume - VOLUME_STEP));
                }
            }
            if !cmd && i.key_pressed(Key::F) {
                actions.push(Action::ToggleFullscreen);
            }
            if !cmd && i.key_pressed(Key::C) {
                actions.push(Action::CycleSubtitle);
            }
            if !cmd && i.key_pressed(Key::OpenBracket) {
                actions.push(Action::PrevChapter);
            }
            if !cmd && i.key_pressed(Key::CloseBracket) {
                actions.push(Action::NextChapter);
            }
            if i.key_pressed(Key::Escape) && i.viewport().fullscreen.unwrap_or(false) {
                actions.push(Action::ToggleFullscreen);
            }
            if !cmd && i.key_pressed(Key::M) {
                actions.push(Action::ToggleMute);
            }
            if !cmd && i.key_pressed(Key::L) {
                actions.push(Action::CycleLoop);
            }
            if !cmd && i.key_pressed(Key::S) {
                actions.push(Action::CycleSpeed);
            }
            if i.key_pressed(Key::Enter) {
                if let Some(s) = self.selected {
                    actions.push(Action::PlayIndex(s));
                }
            }
            if i.key_pressed(Key::Backspace) {
                if let Some(s) = self.selected {
                    actions.push(Action::Remove(s));
                }
            }
            if cmd && i.key_pressed(Key::O) {
                actions.push(Action::OpenFiles);
            }
            if cmd && i.key_pressed(Key::L) {
                actions.push(Action::OpenDialog);
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

impl eframe::App for PlayerApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        self.devshot.tick(ui.ctx());
        if let Some(rs) = frame.wgpu_render_state() {
            self.backend.attach_gpu(rs);
        }
        // agent first: injected input must be visible to this frame's widgets
        self.agent.set_enabled(ui.ctx(), self.settings.agent);
        let agent_state = self.agent.wants_state().then(|| self.agent_state());
        self.agent.tick(ui.ctx(), agent_state);
        let t = ui.input(|i| i.time);
        let ctx = ui.ctx().clone();

        self.snap = self.backend.poll(&ctx);
        self.after_poll(t);
        self.analyse_audio(ui.input(|i| i.stable_dt).min(0.1));
        if let Some(paths) = self.picker.poll() {
            if paths.is_empty() {
                self.push_log(t, "open // cancelled", Level::Info);
            } else {
                let sources = paths.into_iter().map(Source::File).collect();
                self.apply(
                    &ctx,
                    Action::Add {
                        sources,
                        play: true,
                    },
                    t,
                );
            }
        }
        if self.snap.playing {
            ctx.request_repaint(); // the timecode and the picture move every frame
        }

        let pal = palette(ui.ctx());
        let fps = 1.0 / ui.input(|i| i.stable_dt).max(1e-3);

        let mut actions: Vec<Action> = Vec::new();
        if let Some(kind) = self.dev_dialog.take() {
            match kind.as_str() {
                "open" => actions.push(Action::OpenDialog),
                _ => self.error_queue.push_back("play // broken.mp4".into()),
            }
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

        // files dropped from the Finder / the file manager join the queue
        let drop_hover = ui.input(|i| !i.raw.hovered_files.is_empty());
        let dropped: Vec<Source> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| Source::File(f.path().to_path_buf()))
                .collect()
        });
        if !dropped.is_empty() {
            actions.push(Action::Add {
                sources: dropped,
                play: false,
            });
        }

        let (state_text, state_color, blink) = match (&self.snap.status, self.snap.playing) {
            (Status::Idle, _) => ("STANDBY", pal.text_dim, false),
            (Status::Loading, _) => ("ACQUIRING", pal.warn, true),
            (Status::Failed(_), _) => ("SIGNAL LOST", pal.danger, false),
            (Status::Ready, true) if self.snap.stalled => ("BUFFERING", pal.warn, true),
            (Status::Ready, true) => ("PLAYING", pal.ok, false),
            (Status::Ready, false) => ("PAUSED", pal.accent, false),
        };
        let mut shell = Shell::new("FUIDE Player")
            .subtitle(format!(
                "v0.1 :: macOS :: {}",
                self.current_track()
                    .map(|t| t.title.clone())
                    .unwrap_or_else(|| "no media".into())
            ))
            .status_left(format!(
                "{} :: {} QUEUED :: {} / {} :: {:.0} FPS",
                fuide::fmt::uptime(t),
                self.queue.len(),
                fmt_clock(self.snap.time),
                self.snap
                    .duration
                    .map(fmt_clock)
                    .unwrap_or_else(|| "--:--".into()),
                fps
            ))
            .lamp(state_text, state_color, blink)
            .settings_button(true);
        if self.current_track().is_some_and(|t| t.source.is_remote()) {
            shell = shell.lamp("NET", pal.accent, false);
        }
        if self.snap.muted {
            shell = shell.lamp("MUTED", pal.warn, false);
        }
        if let Some((text, busy)) = self.agent.lamp() {
            shell = shell.lamp(text, if busy { pal.warn } else { pal.accent }, busy);
        }

        let log_open = self.settings.log_open;
        let mut log_resized = false;
        let mut log_toggled = false;
        let panels = self.panels;
        let transport_h = transport_h(&type_scale(ui.ctx()));
        let fullscreen = ui.input(|i| i.viewport().fullscreen.unwrap_or(false));
        if fullscreen {
            // full screen: no shell chrome, the screen and the transport on black
            let c = ui.max_rect();
            ui.painter()
                .rect_filled(c, egui::CornerRadius::ZERO, Color32::BLACK);
            let inner = c.shrink2(vec2(12.0, 8.0));
            let transport =
                Rect::from_min_max(pos2(inner.left(), inner.bottom() - transport_h), inner.max);
            let screen = Rect::from_min_max(
                pos2(inner.left(), inner.top() + 10.0),
                pos2(inner.right(), transport.top() - GAP - 8.0),
            );
            self.ui_screen(ui, screen, t, drop_hover, &mut actions);
            self.ui_transport(ui, transport, &mut actions);
            if ui.input(|i| i.modifiers.command && i.key_pressed(Key::W)) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            self.agent.paint(&ctx);
            self.ui_dialog(&ctx, &mut actions);
            for a in actions {
                self.apply(&ctx, a, t);
            }
            return;
        }
        // Tab and the log chip animate rather than snap: the side panels slide in from the
        // window edges, the log rises from the bottom edge, the picture follows the space that
        // is left, and the moving panels fade with the slide. `k` = 0 is the theater layout
        // (screen + transport only), 1 the full one; egui returns the target on the first frame
        // so nothing animates at startup.
        let k = ctx.animate_bool_with_time_and_easing(
            Id::new("player-panels"),
            panels,
            PANEL_ANIM,
            egui::emath::easing::cubic_out,
        );
        let k_log = ctx.animate_bool_with_time_and_easing(
            Id::new("player-log-open"),
            log_open,
            PANEL_ANIM,
            egui::emath::easing::cubic_out,
        );
        let out = shell.show_full(ui, |ui| {
            let c = ui.max_rect();
            let top = c.top() + 10.0; // room for title chips above the first panels
            let log_max = c.height() - BODY_MIN;
            self.log_h = self.log_h.clamp(LOG_MIN, log_max.max(LOG_MIN));
            // log height as drawn: header strip <-> open height
            let log_h = egui::lerp(LOG_CLOSED..=self.log_h, k_log);
            // how far the hidden panels sit past the window edges
            let slide = 1.0 - k;
            let dx_left = (LEFT_W + GAP) * slide;
            let dx_right = (RIGHT_W + GAP) * slide;
            let dy_log = (log_h + GAP + 8.0) * slide;
            let log_rect = Rect::from_min_max(
                pos2(c.left(), c.bottom() - log_h + dy_log),
                pos2(c.right(), c.bottom() + dy_log),
            );
            let body_bottom = log_rect.top() - GAP - 8.0;

            let left = Rect::from_min_max(
                pos2(c.left() - dx_left, top),
                pos2(c.left() + LEFT_W - dx_left, body_bottom),
            );
            let right = Rect::from_min_max(
                pos2(c.right() - RIGHT_W + dx_right, top),
                pos2(c.right() + dx_right, body_bottom),
            );
            // at k = 0 this is the whole content area: the picture is never overlaid by controls
            let center = Rect::from_min_max(
                pos2(left.right() + GAP, top),
                pos2(right.left() - GAP, body_bottom),
            );
            let transport = Rect::from_min_max(
                pos2(center.left(), center.bottom() - transport_h),
                center.max,
            );
            let screen = Rect::from_min_max(
                center.min,
                pos2(center.right(), transport.top() - GAP - 8.0),
            );

            self.ui_screen(ui, screen, t, drop_hover, &mut actions);
            self.ui_transport(ui, transport, &mut actions);
            if k <= 0.0 {
                return; // theater: the panels are fully off screen, none of their widgets exist
            }
            ui.scope(|ui| {
                ui.multiply_opacity(k);
                self.ui_queue(ui, left, &mut actions);
                self.ui_media(ui, right, &mut actions);
                if log_open && k >= 1.0 {
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
                // the feed stays while the panel folds up
                log_toggled = self.ui_log(ui, log_rect, log_open, k_log > 0.0);
            });
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
            .show(&ctx, &mut self.settings, "FUIDE Player")
        {
            self.settings_changed(t);
        }
    }
}

impl PlayerApp {
    fn ui_queue(&mut self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let row_h = ts.row;
        let mut drag = self.queue_drag;
        let mut drop: Option<usize> = None;
        let released = ui.input(|i| i.pointer.any_released());
        Panel::new("Queue")
            .tag(format!("{} tracks", self.queue.len()), pal.text_dim)
            .padding(8.0, 14.0)
            .show_rect(ui, rect, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let list_h = ui.available_height() - 2.0 * ts.row - 20.0;
                ScrollArea::vertical()
                    .id_salt("queue")
                    .max_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let w = ui.available_width();
                        for (i, track) in self.queue.iter().enumerate() {
                            let (r, resp) =
                                ui.allocate_exact_size(vec2(w, row_h), Sense::click_and_drag());
                            if resp.drag_started_by(egui::PointerButton::Primary) {
                                drag = Some(i);
                            }
                            // while a row is dragged, the pointer's row decides the insertion
                            if drag.is_some() {
                                if let Some(pos) = ui.input(|inp| inp.pointer.latest_pos()) {
                                    let above_first = i == 0 && pos.y < r.top();
                                    let below_last =
                                        i + 1 == self.queue.len() && pos.y > r.bottom();
                                    if r.contains(pos) || above_first || below_last {
                                        drop = Some(i);
                                    }
                                }
                            }
                            let is_cur = self.current == Some(i);
                            let is_sel = self.selected == Some(i);
                            fuide::agent::describe(&resp, || {
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::SelectableLabel,
                                    true,
                                    is_sel,
                                    track.title.clone(),
                                )
                            });
                            let p = ui.painter().with_clip_rect(r.intersect(ui.clip_rect()));
                            if is_sel {
                                p.rect_filled(
                                    r,
                                    egui::CornerRadius::ZERO,
                                    pal.accent.gamma_multiply(0.13),
                                );
                            } else if resp.hovered() {
                                p.rect_filled(
                                    r,
                                    egui::CornerRadius::ZERO,
                                    pal.accent.gamma_multiply(0.05),
                                );
                            } else if i % 2 == 1 {
                                p.rect_filled(
                                    r,
                                    egui::CornerRadius::ZERO,
                                    pal.accent.gamma_multiply(0.02),
                                );
                            }
                            if is_cur {
                                p.rect_filled(
                                    Rect::from_min_size(r.min, vec2(3.0, r.height())),
                                    egui::CornerRadius::ZERO,
                                    pal.accent,
                                );
                                widgets::draw_icon(
                                    &p,
                                    pos2(r.left() + 16.0, r.center().y),
                                    (ts.label * 0.7).round(),
                                    if self.snap.playing {
                                        Icon::Play
                                    } else {
                                        Icon::Pause
                                    },
                                    Stroke::new(1.0, pal.accent),
                                );
                            } else {
                                p.text(
                                    pos2(r.left() + 16.0, r.center().y),
                                    Align2::CENTER_CENTER,
                                    format!("{:02}", i + 1),
                                    mono(ts.label),
                                    pal.text_dim,
                                );
                            }
                            let right_text = match track.duration {
                                Some(d) => fmt_clock(d),
                                None if track.source.is_remote() => "NET".into(),
                                None => "--:--".into(),
                            };
                            let right_w = 52.0;
                            p.text(
                                pos2(r.right() - 6.0, r.center().y),
                                Align2::RIGHT_CENTER,
                                right_text,
                                mono(ts.label),
                                pal.text_dim,
                            );
                            let name_clip =
                                Rect::from_min_max(r.min, pos2(r.right() - right_w, r.bottom()));
                            p.with_clip_rect(name_clip.intersect(p.clip_rect())).text(
                                pos2(r.left() + 30.0, r.center().y),
                                Align2::LEFT_CENTER,
                                &track.title,
                                mono(ts.data),
                                if is_cur { pal.accent } else { pal.text },
                            );
                            if let (Some(from), Some(to)) = (drag, drop) {
                                if to == i && from != i {
                                    // insertion marker on the edge the row would move across
                                    let y = if from < to { r.bottom() - 1.0 } else { r.top() };
                                    p.line_segment(
                                        [pos2(r.left(), y), pos2(r.right(), y)],
                                        Stroke::new(2.0, pal.accent),
                                    );
                                }
                            }
                            if resp.double_clicked() {
                                actions.push(Action::PlayIndex(i));
                            } else if resp.clicked() {
                                actions.push(Action::Select(Some(i)));
                            }
                        }
                        if self.queue.is_empty() {
                            let (r, _) = ui.allocate_exact_size(vec2(w, 60.0), Sense::hover());
                            let p = ui.painter();
                            p.text(
                                pos2(r.center().x, r.center().y - 9.0),
                                Align2::CENTER_CENTER,
                                "QUEUE EMPTY",
                                mono(ts.label),
                                pal.text_dim,
                            );
                            p.text(
                                pos2(r.center().x, r.center().y + 9.0),
                                Align2::CENTER_CENTER,
                                "CMD+O FILE  CMD+L URL  //  DROP FILES",
                                mono(ts.small),
                                pal.text_dim.gamma_multiply(0.7),
                            );
                        }
                    });
                if let Some(from) = drag {
                    // ghost caption by the pointer
                    if let Some(pos) = ui.input(|inp| inp.pointer.latest_pos()) {
                        if let Some(track) = self.queue.get(from) {
                            let gp = ui.ctx().layer_painter(egui::LayerId::new(
                                egui::Order::Tooltip,
                                egui::Id::new("queue-drag-ghost"),
                            ));
                            let g =
                                gp.layout_no_wrap(track.title.clone(), mono(ts.label), pal.accent);
                            let plate = Rect::from_min_size(
                                pos + vec2(14.0, 10.0),
                                g.size() + vec2(12.0, 6.0),
                            );
                            gp.rect_filled(plate, egui::CornerRadius::ZERO, pal.bg_deep);
                            gp.rect_stroke(
                                plate,
                                egui::CornerRadius::ZERO,
                                Stroke::new(1.0, pal.accent),
                                egui::StrokeKind::Inside,
                            );
                            gp.galley(plate.min + vec2(6.0, 3.0), g, pal.accent);
                        }
                    }
                    if released {
                        if let Some(to) = drop {
                            if to != from {
                                actions.push(Action::Move { from, to });
                            }
                        }
                        drag = None;
                    }
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let bsz = vec2(112.0, ts.row);
                    if widgets::button(ui, bsz, "OPEN FILE", true).clicked() {
                        actions.push(Action::OpenFiles);
                    }
                    if widgets::button(ui, bsz, "OPEN URL", true).clicked() {
                        actions.push(Action::OpenDialog);
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let bsz = vec2(112.0, ts.row);
                    let has_sel = self.selected.is_some();
                    if widgets::button(ui, bsz, "REMOVE", has_sel).clicked() && has_sel {
                        actions.push(Action::Remove(self.selected.unwrap()));
                    }
                    let has_any = !self.queue.is_empty();
                    if widgets::button_colored(ui, bsz, "CLEAR", has_any, pal.warn).clicked()
                        && has_any
                    {
                        actions.push(Action::Clear);
                    }
                });
            });
        self.queue_drag = drag;
        self.queue_drop = drop;
    }

    /// The screen: a one-line HUD strip (file name, state, codec, timecode) above the picture,
    /// which is letterboxed into the rest and never drawn over. Audio-only tracks get a
    /// readout with a progress ring instead; idle / loading / failed states a plate. A click
    /// on the picture area toggles play / pause.
    fn ui_screen(
        &self,
        ui: &mut Ui,
        rect: Rect,
        t: f64,
        drop_hover: bool,
        actions: &mut Vec<Action>,
    ) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let info = self.backend.info();
        let tag = match &self.snap.status {
            Status::Idle => "no signal".to_string(),
            Status::Loading => "acquiring".to_string(),
            Status::Failed(_) => "signal lost".to_string(),
            Status::Ready => match &info.video {
                Some(v) => format!("{} {}x{}", v.codec, v.width, v.height),
                None => "audio".to_string(),
            },
        };
        Panel::new("Screen")
            .tag(tag, pal.text_dim)
            .padding(6.0, 12.0)
            .show_rect(ui, rect, |ui| {
                let area = ui.max_rect();
                let has_video = info.video.is_some();
                let track = self.current_track();
                // the HUD strip exists whenever a track is loaded; the picture gets the rest
                let strip =
                    track.map(|_| Rect::from_min_size(area.min, vec2(area.width(), ts.row)));
                // a subtitle track on: a caption band under the picture (two lines), so the
                // text never covers the picture
                let caption_h = (ts.data + 4.0) * 2.0 + 10.0;
                let caption_band = self.backend.subtitle().map(|_| {
                    Rect::from_min_max(pos2(area.left(), area.bottom() - caption_h), area.max)
                });
                let pic_bottom = caption_band.map_or(area.bottom(), |b| b.top() - 4.0);
                let pic = match strip {
                    Some(st) => Rect::from_min_max(
                        pos2(area.left(), st.bottom() + 6.0),
                        pos2(area.right(), pic_bottom),
                    ),
                    None => Rect::from_min_max(area.min, pos2(area.right(), pic_bottom)),
                };
                let click = ui.interact(pic, ui.id().with("screen"), Sense::click());
                fuide::agent::describe(&click, || {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        !self.queue.is_empty(),
                        "SCREEN (click = play/pause)",
                    )
                });
                if click.double_clicked() {
                    actions.push(Action::ToggleFullscreen);
                } else if click.clicked() && !self.queue.is_empty() {
                    actions.push(Action::Toggle);
                }
                let p = ui.painter().with_clip_rect(area);
                p.rect_filled(area, egui::CornerRadius::ZERO, Color32::BLACK);

                // ---- picture, letterboxed into `pic`; nothing else is drawn inside it ----
                let mut showing_picture = false;
                if let (Status::Ready, Some((tex_id, [tw, th])), true) =
                    (&self.snap.status, self.backend.frame(), has_video)
                {
                    let (tw, th) = (tw.max(1) as f32, th.max(1) as f32);
                    let scale = (pic.width() / tw).min(pic.height() / th);
                    let size = vec2(tw * scale, th * scale).floor();
                    let fit = Rect::from_center_size(pic.center(), size);
                    p.image(
                        tex_id,
                        fit,
                        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    // the picture is the one thing in a FUI shell that must not get scanlines
                    fuide::shell::keep_clear(ui.ctx(), fit);
                    showing_picture = true;
                }

                // ---- HUD strip: file name (+ embedded title), state, codec, timecode ----
                if let (Some(st), Some(track)) = (strip, track) {
                    let y = st.center().y;
                    let state = match (&self.snap.status, self.snap.playing) {
                        (Status::Loading, _) => Some(("ACQUIRING", pal.warn)),
                        (Status::Failed(_), _) => Some(("SIGNAL LOST", pal.danger)),
                        (Status::Ready, true) if self.snap.stalled => Some(("BUFFERING", pal.warn)),
                        (Status::Ready, false) => Some(("PAUSED", pal.accent)),
                        _ => None,
                    };
                    // right group, laid out from the right edge: timecode, meters, codec, state
                    let mut x = st.right() - 6.0;
                    if self.snap.status == Status::Ready {
                        let tc = format!(
                            "{} / {}",
                            fmt_clock_tenths(self.scrub.unwrap_or(self.snap.time)),
                            self.snap
                                .duration
                                .map(fmt_clock_tenths)
                                .unwrap_or_else(|| "LIVE".into())
                        );
                        let g = p.layout_no_wrap(tc, mono(ts.data), pal.accent);
                        x -= g.size().x;
                        p.galley(pos2(x, y - g.size().y / 2.0), g, pal.accent);
                        x -= 18.0;
                        if info.audio.is_some() {
                            // stereo level meters: two thin bars, L over R
                            let w = 64.0;
                            x -= w;
                            for (row, level) in [(0, self.meters.0), (1, self.meters.1)] {
                                let ry = y - 5.0 + row as f32 * 6.0;
                                let bar = Rect::from_min_size(pos2(x, ry), vec2(w, 4.0));
                                widgets::segment_bar(
                                    &p,
                                    bar,
                                    level.clamp(0.0, 1.0),
                                    if level > 0.95 { pal.warn } else { pal.accent },
                                    pal.accent_dim,
                                );
                            }
                            x -= 18.0;
                        }
                        let mut codec = Vec::new();
                        if let Some(v) = &info.video {
                            codec.push(format!("{} {}x{}", v.codec, v.width, v.height));
                            if v.fps > 0.0 {
                                codec.push(format!("{:.3}FPS", v.fps).replace(".000", ""));
                            }
                        }
                        if let Some(a) = &info.audio {
                            codec.push(format!("{} {:.1}K", a.codec, a.sample_rate / 1000.0));
                        }
                        if self.snap.rate != 1.0 {
                            codec.push(format!("{}X", self.snap.rate));
                        }
                        if !codec.is_empty() {
                            let g =
                                p.layout_no_wrap(codec.join("  "), mono(ts.label), pal.text_dim);
                            x -= g.size().x;
                            p.galley(pos2(x, y - g.size().y / 2.0), g, pal.text_dim);
                            x -= 18.0;
                        }
                    }
                    if let Some((word, color)) = state {
                        let g = p.layout_no_wrap(word.to_string(), mono(ts.label), color);
                        let plate = Rect::from_min_size(
                            pos2(x - g.size().x - 12.0, y - ts.row / 2.0 + 2.0),
                            vec2(g.size().x + 12.0, ts.row - 4.0),
                        );
                        p.rect_stroke(
                            plate,
                            egui::CornerRadius::ZERO,
                            Stroke::new(1.0, color),
                            egui::StrokeKind::Inside,
                        );
                        p.galley(pos2(plate.left() + 6.0, y - g.size().y / 2.0), g, color);
                        x = plate.left() - 18.0;
                    }
                    // left: the file name (what the user opened), then the embedded title
                    let left_clip = Rect::from_min_max(st.min, pos2(x - 8.0, st.bottom()));
                    let lp = p.with_clip_rect(left_clip.intersect(p.clip_rect()));
                    // left group, in priority order and only what fits: file name, chapter,
                    // embedded title
                    let name = track.source.label().to_uppercase();
                    let g = lp.layout_no_wrap(name.clone(), mono(ts.data), pal.accent);
                    let mut lx = st.left() + 6.0 + g.size().x;
                    lp.galley(pos2(st.left() + 6.0, y - g.size().y / 2.0), g, pal.accent);
                    let limit = x - 8.0;
                    if let Some(ci) = info.chapter_at(self.snap.time) {
                        let c = &info.chapters[ci];
                        let cg = lp.layout_no_wrap(
                            format!(
                                "//  CH {}/{}  {}",
                                ci + 1,
                                info.chapters.len(),
                                c.title.to_uppercase()
                            ),
                            mono(ts.label),
                            pal.text,
                        );
                        if lx + 14.0 + cg.size().x <= limit {
                            lp.galley(pos2(lx + 14.0, y - cg.size().y / 2.0), cg.clone(), pal.text);
                            lx += 14.0 + cg.size().x;
                        }
                    }
                    if track.title.to_uppercase() != name {
                        let tg = lp.layout_no_wrap(
                            format!("::  {}", track.title.to_uppercase()),
                            mono(ts.label),
                            pal.text_dim,
                        );
                        if lx + 14.0 + tg.size().x <= limit {
                            lp.galley(pos2(lx + 14.0, y - tg.size().y / 2.0), tg, pal.text_dim);
                        }
                    }
                    if drop_hover && showing_picture {
                        // the drop notice replaces the strip so the picture stays clear
                        p.rect_filled(st, egui::CornerRadius::ZERO, pal.bg_deep);
                        p.text(
                            st.center(),
                            Align2::CENTER_CENTER,
                            "DROP // ADD TO QUEUE",
                            mono(ts.data),
                            pal.accent,
                        );
                    }
                    // rule under the strip
                    p.line_segment(
                        [
                            pos2(st.left(), st.bottom() + 2.0),
                            pos2(st.right(), st.bottom() + 2.0),
                        ],
                        Stroke::new(1.0, pal.accent.gamma_multiply(0.25)),
                    );
                }

                // ---- caption band ----
                if let Some(band) = caption_band {
                    p.line_segment(
                        [pos2(band.left(), band.top()), pos2(band.right(), band.top())],
                        Stroke::new(1.0, pal.accent.gamma_multiply(0.25)),
                    );
                    if let Some(text) = self.backend.caption() {
                        let galley = p.layout(
                            text,
                            mono(ts.data + 2.0),
                            pal.text,
                            band.width() - 40.0,
                        );
                        let pos = pos2(
                            band.center().x - galley.size().x / 2.0,
                            band.center().y - galley.size().y / 2.0,
                        );
                        p.galley(pos, galley, pal.text);
                    } else {
                        let name = self
                            .backend
                            .subtitle()
                            .and_then(|i| self.backend.subtitles().get(i).cloned())
                            .unwrap_or_default();
                        p.text(
                            pos2(band.right() - 8.0, band.center().y),
                            Align2::RIGHT_CENTER,
                            format!("CC {}", name.to_uppercase()),
                            mono(ts.small),
                            pal.text_dim,
                        );
                    }
                }

                // ---- audio-only: spectrum below, progress ring and title above ----
                if self.snap.status == Status::Ready && !has_video {
                    fx::scanlines(&p, pic);
                    let spec_h = (pic.height() * 0.42).clamp(60.0, 260.0);
                    let spec = Rect::from_min_max(
                        pos2(pic.left() + 24.0, pic.bottom() - spec_h - 16.0),
                        pos2(pic.right() - 24.0, pic.bottom() - 16.0),
                    );
                    let n = self.spectrum.bands.len();
                    let gap = 3.0;
                    let bw = ((spec.width() - gap * (n as f32 - 1.0)) / n as f32).max(1.0);
                    // baseline + faint grid
                    p.line_segment(
                        [pos2(spec.left(), spec.bottom()), pos2(spec.right(), spec.bottom())],
                        Stroke::new(1.0, pal.accent.gamma_multiply(0.35)),
                    );
                    for k in 1..4 {
                        let gy = spec.bottom() - spec.height() * k as f32 / 4.0;
                        p.line_segment(
                            [pos2(spec.left(), gy), pos2(spec.right(), gy)],
                            Stroke::new(1.0, pal.accent.gamma_multiply(0.08)),
                        );
                    }
                    for (i, (&v, &pk)) in self
                        .spectrum
                        .bands
                        .iter()
                        .zip(self.spectrum.peaks.iter())
                        .enumerate()
                    {
                        let x0 = spec.left() + i as f32 * (bw + gap);
                        let h = (v * spec.height()).max(1.0);
                        let bar = Rect::from_min_max(
                            pos2(x0, spec.bottom() - h),
                            pos2(x0 + bw, spec.bottom()),
                        );
                        // segmented bar: brighter towards the top
                        let seg = 6.0;
                        let mut yy = bar.bottom();
                        while yy > bar.top() {
                            let y0 = (yy - seg + 1.0).max(bar.top());
                            let frac = (spec.bottom() - yy) / spec.height();
                            let col = if frac > 0.85 {
                                pal.warn
                            } else {
                                pal.accent.gamma_multiply(0.45 + 0.55 * frac)
                            };
                            p.rect_filled(
                                Rect::from_min_max(pos2(x0, y0), pos2(x0 + bw, yy)),
                                egui::CornerRadius::ZERO,
                                col,
                            );
                            yy -= seg;
                        }
                        let py = spec.bottom() - pk * spec.height();
                        p.rect_filled(
                            Rect::from_min_max(pos2(x0, py - 2.0), pos2(x0 + bw, py)),
                            egui::CornerRadius::ZERO,
                            pal.text,
                        );
                    }
                    // frequency labels
                    for (frac, label) in [(0.0, "40"), (0.5, "800"), (1.0, "16K")] {
                        let lx = spec.left() + spec.width() * frac;
                        p.text(
                            pos2(lx, spec.bottom() + 4.0),
                            if frac == 0.0 {
                                Align2::LEFT_TOP
                            } else if frac == 1.0 {
                                Align2::RIGHT_TOP
                            } else {
                                Align2::CENTER_TOP
                            },
                            label,
                            mono(ts.small),
                            pal.text_dim,
                        );
                    }
                    let upper = Rect::from_min_max(pic.min, pos2(pic.right(), spec.top() - 8.0));
                    let c = upper.center();
                    let r = (upper.height() * 0.3).clamp(30.0, 90.0);
                    p.circle_stroke(c, r, Stroke::new(1.0, pal.accent.gamma_multiply(0.25)));
                    let frac = self
                        .snap
                        .duration
                        .map(|d| (self.snap.time / d).clamp(0.0, 1.0) as f32)
                        .unwrap_or(0.0);
                    let n = (frac * 96.0).ceil() as usize;
                    if n >= 1 {
                        let pts: Vec<egui::Pos2> = (0..=n)
                            .map(|i| {
                                let a = -std::f32::consts::FRAC_PI_2
                                    + std::f32::consts::TAU * frac * i as f32 / n as f32;
                                c + vec2(r * a.cos(), r * a.sin())
                            })
                            .collect();
                        p.add(egui::Shape::line(pts, Stroke::new(3.0, pal.accent)));
                    }
                    if self.snap.playing {
                        let a = (t * 1.2) as f32 % std::f32::consts::TAU;
                        let d = vec2(a.cos(), a.sin());
                        p.line_segment(
                            [c + d * (r - 8.0), c + d * (r - 2.0)],
                            Stroke::new(1.5, pal.accent),
                        );
                    }
                    let title = track.map(|t| t.title.clone()).unwrap_or_default();
                    fuide::display_text(
                        &p,
                        pos2(c.x, c.y - 8.0),
                        Align2::CENTER_CENTER,
                        title.to_uppercase(),
                        (ts.heading + 4.0).min(r * 0.35),
                        pal.accent,
                    );
                    let sub: Vec<String> = [info.artist.clone(), info.album.clone()]
                        .into_iter()
                        .flatten()
                        .collect();
                    p.text(
                        pos2(c.x, c.y + 14.0),
                        Align2::CENTER_CENTER,
                        if sub.is_empty() {
                            "AUDIO".to_string()
                        } else {
                            sub.join("  //  ").to_uppercase()
                        },
                        mono(ts.label),
                        pal.text_dim,
                    );
                }

                // ---- no picture: plates, brackets, drop notice ----
                if !showing_picture {
                    let plate = |word: &str, color: Color32, hint: &str| {
                        let c = pic.center();
                        fuide::display_text(
                            &p,
                            c - vec2(0.0, 10.0),
                            Align2::CENTER_CENTER,
                            word,
                            ts.title * 1.4,
                            color,
                        );
                        if !hint.is_empty() {
                            p.text(
                                c + vec2(0.0, 18.0),
                                Align2::CENTER_CENTER,
                                hint,
                                mono(ts.label),
                                pal.text_dim,
                            );
                        }
                    };
                    match &self.snap.status {
                        Status::Idle => {
                            fx::scanlines(&p, pic);
                            plate(
                                "NO SIGNAL",
                                pal.text_dim,
                                "CMD+O OPEN FILE  //  CMD+L URL  //  DROP MEDIA HERE  //  TAB PANELS",
                            );
                        }
                        Status::Loading => {
                            fx::scanlines(&p, pic);
                            fx::scan_band(&p, pic, t, pal.accent);
                            plate("ACQUIRING SIGNAL", pal.warn, "");
                        }
                        Status::Failed(e) => {
                            fx::scanlines(&p, pic);
                            plate("SIGNAL LOST", pal.danger, &e.to_uppercase());
                        }
                        Status::Ready if has_video => {
                            // ready but no frame yet (first frame decoding)
                            fx::scan_band(&p, pic, t, pal.accent);
                        }
                        _ => {}
                    }
                    let m = 6.0;
                    let l = 18.0;
                    let bs = Stroke::new(1.0, pal.accent.gamma_multiply(0.7));
                    for (cx, cy, dx, dy) in [
                        (pic.left() + m, pic.top() + m, 1.0, 1.0),
                        (pic.right() - m, pic.top() + m, -1.0, 1.0),
                        (pic.left() + m, pic.bottom() - m, 1.0, -1.0),
                        (pic.right() - m, pic.bottom() - m, -1.0, -1.0),
                    ] {
                        let o = pos2(cx, cy);
                        p.line_segment([o, o + vec2(dx * l, 0.0)], bs);
                        p.line_segment([o, o + vec2(0.0, dy * l)], bs);
                    }
                    if drop_hover {
                        p.rect_filled(
                            pic,
                            egui::CornerRadius::ZERO,
                            pal.accent.gamma_multiply(0.06),
                        );
                        p.rect_stroke(
                            pic.shrink(2.0),
                            egui::CornerRadius::ZERO,
                            Stroke::new(1.5, pal.accent),
                            egui::StrokeKind::Inside,
                        );
                        let g = fuide::display_galley(
                            &p,
                            "DROP // ADD TO QUEUE",
                            ts.heading + 3.0,
                            pal.accent,
                        );
                        let plate =
                            Rect::from_center_size(pic.center(), g.size() + vec2(28.0, 18.0));
                        p.rect_filled(plate, egui::CornerRadius::ZERO, pal.bg_deep);
                        p.rect_stroke(
                            plate,
                            egui::CornerRadius::ZERO,
                            Stroke::new(1.0, pal.accent),
                            egui::StrokeKind::Inside,
                        );
                        p.galley(plate.min + vec2(14.0, 9.0), g, pal.accent);
                    }
                }
            });
    }

    /// Seek bar + transport buttons + loop / speed / volume.
    fn ui_transport(&mut self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let ready = self.snap.status == Status::Ready;
        let duration = self.snap.duration;
        let snap = self.snap.clone();
        let loop_mode = self.loop_mode;
        let has_current = self.current.is_some();
        let has_queue = !self.queue.is_empty();
        let chapters = self.backend.info().chapters.clone();
        let subtitle_label = "cc".to_string();
        let has_subtitles = !self.backend.subtitles().is_empty();
        let subtitle_on = self.backend.subtitle().is_some();
        let mut scrub = self.scrub;
        let tag = if self.panels {
            format!("{}  {}x", loop_mode.label(), snap.rate)
        } else {
            format!("{}  {}x  ::  TAB PANELS", loop_mode.label(), snap.rate)
        };
        let bh = button_h(&ts);
        Panel::new("Transport")
            .tag(tag, pal.text_dim)
            .padding(TRANSPORT_PAD.0, TRANSPORT_PAD.1)
            .show_rect(ui, rect, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                // ---- seek bar --------------------------------------------------------------
                let (bar, resp) = ui.allocate_exact_size(
                    vec2(ui.available_width(), ts.row),
                    if ready && duration.is_some() {
                        Sense::click_and_drag()
                    } else {
                        Sense::hover()
                    },
                );
                fuide::agent::describe(&resp, || {
                    egui::WidgetInfo::labeled(egui::WidgetType::Slider, ready, "SEEK")
                });
                let track = Rect::from_center_size(bar.center(), vec2(bar.width(), 4.0));
                let p = ui.painter();
                p.rect_filled(
                    track,
                    egui::CornerRadius::ZERO,
                    pal.accent.gamma_multiply(0.12),
                );
                if let Some(d) = duration.filter(|d| *d > 0.0) {
                    let frac = |s: f64| ((s / d).clamp(0.0, 1.0) as f32) * bar.width();
                    if let Some(b) = snap.buffered {
                        p.rect_filled(
                            Rect::from_min_max(
                                pos2(bar.left() + frac(snap.time), track.top()),
                                pos2(bar.left() + frac(b), track.bottom()),
                            ),
                            egui::CornerRadius::ZERO,
                            pal.accent.gamma_multiply(0.28),
                        );
                    }
                    // chapter starts as ticks
                    for c in &chapters {
                        if c.start <= 0.0 {
                            continue;
                        }
                        let cx = bar.left() + frac(c.start);
                        p.line_segment(
                            [pos2(cx, track.top() - 4.0), pos2(cx, track.bottom() + 4.0)],
                            Stroke::new(1.0, pal.text_dim),
                        );
                    }
                    let shown = scrub.unwrap_or(snap.time);
                    let x = bar.left() + frac(shown);
                    p.rect_filled(
                        Rect::from_min_max(pos2(bar.left(), track.top()), pos2(x, track.bottom())),
                        egui::CornerRadius::ZERO,
                        pal.accent,
                    );
                    p.rect_filled(
                        Rect::from_center_size(pos2(x, bar.center().y), vec2(3.0, ts.row * 0.7)),
                        egui::CornerRadius::ZERO,
                        pal.accent,
                    );
                    // pointer interaction
                    let at = |pos: egui::Pos2| {
                        ((pos.x - bar.left()) / bar.width()).clamp(0.0, 1.0) as f64 * d
                    };
                    if resp.dragged() || resp.drag_started() {
                        if let Some(pos) = resp.interact_pointer_pos() {
                            scrub = Some(at(pos));
                        }
                    }
                    if resp.drag_stopped() {
                        if let Some(s) = scrub.take() {
                            actions.push(Action::Seek(s));
                        }
                    } else if resp.clicked() {
                        if let Some(pos) = resp.interact_pointer_pos() {
                            actions.push(Action::Seek(at(pos)));
                        }
                    }
                    if let Some(pos) = resp.hover_pos().filter(|_| resp.hovered()) {
                        let s = scrub.unwrap_or_else(|| at(pos));
                        let hx = bar.left() + frac(s);
                        p.text(
                            pos2(hx, bar.top() - 2.0),
                            Align2::CENTER_BOTTOM,
                            fmt_clock_tenths(s),
                            mono(ts.small),
                            pal.text,
                        );
                    }
                } else if ready {
                    // live stream: no duration, the bar just shows activity
                    fx::scan_band(p, track, ui.input(|i| i.time), pal.accent);
                }

                // ---- buttons ---------------------------------------------------------------
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let bsz = vec2(42.0, bh);
                    if widgets::icon_button(ui, bsz, Icon::SkipBack, has_current).clicked() {
                        actions.push(Action::Prev);
                    }
                    let play_icon = if snap.playing {
                        Icon::Pause
                    } else {
                        Icon::Play
                    };
                    if widgets::icon_button(ui, vec2(64.0, bh), play_icon, has_queue).clicked() {
                        actions.push(Action::Toggle);
                    }
                    if widgets::icon_button(ui, bsz, Icon::SkipForward, has_current).clicked() {
                        actions.push(Action::Next);
                    }
                    if widgets::icon_button(ui, bsz, Icon::Stop, has_current).clicked() {
                        actions.push(Action::Stop);
                    }
                    ui.add_space(8.0);
                    let time_text = format!(
                        "{} / {}",
                        fmt_clock(scrub.unwrap_or(snap.time)),
                        duration.map(fmt_clock).unwrap_or_else(|| if ready {
                            "LIVE".into()
                        } else {
                            "--:--".into()
                        })
                    );
                    let tcol = if ready { pal.text } else { pal.text_dim };
                    let tg = ui.painter().layout_no_wrap(time_text, mono(ts.data), tcol);
                    let (tr, _) =
                        ui.allocate_exact_size(vec2(tg.size().x + 8.0, ts.row), Sense::hover());
                    let ty = tr.center().y - tg.size().y / 2.0;
                    ui.painter().galley(pos2(tr.left(), ty), tg, tcol);
                    // the right group only gets the FULL button when the panel is wide enough
                    let wide = ui.available_width() > 420.0;

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        // volume: a small bar, click / drag sets it
                        let (vr, vresp) =
                            ui.allocate_exact_size(vec2(72.0, ts.row), Sense::click_and_drag());
                        fuide::agent::describe(&vresp, || {
                            egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, "VOLUME")
                        });
                        let inner = Rect::from_center_size(vr.center(), vec2(vr.width(), 8.0));
                        widgets::segment_bar(
                            ui.painter(),
                            inner,
                            if snap.muted { 0.0 } else { snap.volume },
                            if snap.muted { pal.text_dim } else { pal.accent },
                            pal.accent_dim,
                        );
                        if vresp.clicked() || vresp.dragged() {
                            if let Some(pos) = vresp.interact_pointer_pos() {
                                let v = ((pos.x - vr.left()) / vr.width()).clamp(0.0, 1.0);
                                actions.push(Action::Volume(v));
                            }
                        }
                        let mut muted = snap.muted;
                        if widgets::toggle_chip(ui, "mute", &mut muted).clicked() {
                            actions.push(Action::ToggleMute);
                        }
                        if widgets::button(ui, vec2(60.0, ts.row), &format!("{}X", snap.rate), true)
                            .clicked()
                        {
                            actions.push(Action::CycleSpeed);
                        }
                        if wide && widgets::button(ui, vec2(52.0, ts.row), "FULL", true).clicked() {
                            actions.push(Action::ToggleFullscreen);
                        }
                        if has_subtitles {
                            let mut on = subtitle_on;
                            if widgets::toggle_chip(ui, &subtitle_label, &mut on).clicked() {
                                actions.push(Action::CycleSubtitle);
                            }
                        }
                        let mut looping = loop_mode != LoopMode::Off;
                        if widgets::toggle_chip(
                            ui,
                            match loop_mode {
                                LoopMode::Off => "loop",
                                LoopMode::One => "loop 1",
                                LoopMode::All => "loop all",
                            },
                            &mut looping,
                        )
                        .clicked()
                        {
                            actions.push(Action::CycleLoop);
                        }
                    });
                });
            });
        self.scrub = scrub;
    }

    /// Inspector: what is known about the playing (or selected) track.
    fn ui_media(&self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let idx = self.focus_index();
        let track = idx.and_then(|i| self.queue.get(i));
        let is_current = idx.is_some() && idx == self.current;
        let info = self.backend.info();
        let tag = match track {
            Some(t) if t.source.is_remote() => "url",
            Some(_) => "file",
            None => "none",
        };
        Panel::new("Media")
            .tag(tag, pal.text_dim)
            .show_rect(ui, rect, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let Some(track) = track else {
                    ui.add(
                        egui::Label::new(
                            RichText::new("NO MEDIA")
                                .font(mono(ts.data + 3.0))
                                .color(pal.text_dim),
                        )
                        .wrap(),
                    );
                    ui.add_space(6.0);
                    widgets::rule(ui);
                    ui.add_space(6.0);
                    self.ui_hints(ui);
                    return;
                };
                ui.add(
                    egui::Label::new(
                        RichText::new(&track.title)
                            .font(mono(ts.data + 3.0))
                            .color(pal.accent),
                    )
                    .wrap(),
                );
                ui.add_space(6.0);
                widgets::rule(ui);
                let dash = "--".to_string();
                if is_current {
                    widgets::readout(ui, "artist", info.artist.as_deref().unwrap_or("--"), None);
                    widgets::readout(ui, "album", info.album.as_deref().unwrap_or("--"), None);
                }
                widgets::readout(
                    ui,
                    "source",
                    if track.source.is_remote() {
                        "NETWORK"
                    } else {
                        "LOCAL FILE"
                    },
                    None,
                );
                let ext = match &track.source {
                    Source::File(p) => p
                        .extension()
                        .map(|e| e.to_string_lossy().to_uppercase())
                        .unwrap_or_else(|| dash.clone()),
                    Source::Url(u) => {
                        let seg = track.source.label();
                        if u.to_lowercase().contains(".m3u8") {
                            "HLS".into()
                        } else {
                            seg.rsplit_once('.')
                                .map(|(_, e)| e.to_uppercase())
                                .unwrap_or_else(|| dash.clone())
                        }
                    }
                };
                widgets::readout(ui, "container", &ext, None);
                widgets::readout(
                    ui,
                    "duration",
                    &track
                        .duration
                        .map(fmt_clock)
                        .unwrap_or_else(|| dash.clone()),
                    None,
                );
                if is_current {
                    ui.add_space(6.0);
                    widgets::rule(ui);
                    match &info.video {
                        Some(v) => {
                            widgets::readout(ui, "video", &v.codec, Some(pal.accent));
                            widgets::readout(
                                ui,
                                "frame",
                                &format!("{}x{}", v.width, v.height),
                                None,
                            );
                            widgets::readout(
                                ui,
                                "rate",
                                &if v.fps > 0.0 {
                                    format!("{:.2} fps", v.fps)
                                } else {
                                    dash.clone()
                                },
                                None,
                            );
                            widgets::readout(ui, "v.bitrate", &fmt_bitrate(v.bitrate), None);
                            widgets::readout(
                                ui,
                                "frames",
                                match self.backend.frame_path() {
                                    "gpu" => "GPU SHARED",
                                    "cpu" => "CPU COPY",
                                    _ => "--",
                                },
                                None,
                            );
                        }
                        None => widgets::readout(ui, "video", "NONE", Some(pal.text_dim)),
                    }
                    match &info.audio {
                        Some(a) => {
                            widgets::readout(ui, "audio", &a.codec, Some(pal.accent));
                            widgets::readout(
                                ui,
                                "sample",
                                &if a.sample_rate > 0.0 {
                                    format!("{:.1} kHz", a.sample_rate / 1000.0)
                                } else {
                                    dash.clone()
                                },
                                None,
                            );
                            widgets::readout(
                                ui,
                                "channels",
                                &match a.channels {
                                    0 => dash.clone(),
                                    1 => "MONO".into(),
                                    2 => "STEREO".into(),
                                    n => format!("{n}"),
                                },
                                None,
                            );
                            widgets::readout(ui, "a.bitrate", &fmt_bitrate(a.bitrate), None);
                        }
                        None => widgets::readout(ui, "audio", "NONE", Some(pal.text_dim)),
                    }
                    ui.add_space(6.0);
                    widgets::rule(ui);
                    widgets::readout(
                        ui,
                        "buffered",
                        &match (self.snap.buffered, self.snap.duration) {
                            (Some(b), Some(d)) => {
                                format!("{} ({:.0}%)", fmt_clock(b), (b / d * 100.0).min(100.0))
                            }
                            (Some(b), None) => fmt_clock(b),
                            _ => dash.clone(),
                        },
                        None,
                    );
                    let subs = self.backend.subtitles();
                    widgets::readout(
                        ui,
                        "subtitles",
                        &match (subs.len(), self.backend.subtitle()) {
                            (0, _) => "NONE".to_string(),
                            (n, None) => format!("OFF ({n} available)"),
                            (n, Some(i)) => {
                                format!(
                                    "{} ({}/{n})",
                                    subs.get(i).cloned().unwrap_or_default(),
                                    i + 1
                                )
                            }
                        },
                        None,
                    );
                    if !info.chapters.is_empty() {
                        ui.add_space(6.0);
                        widgets::rule(ui);
                        let cur = info.chapter_at(self.snap.time);
                        for (i, c) in info.chapters.iter().enumerate().take(8) {
                            let (r, resp) = ui.allocate_exact_size(
                                vec2(ui.available_width(), ts.row - 2.0),
                                Sense::click(),
                            );
                            fuide::agent::describe(&resp, || {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    format!("CHAPTER {} {}", i + 1, c.title),
                                )
                            });
                            let active = cur == Some(i);
                            if active || resp.hovered() {
                                ui.painter().rect_filled(
                                    r,
                                    egui::CornerRadius::ZERO,
                                    pal.accent.gamma_multiply(if active { 0.13 } else { 0.05 }),
                                );
                            }
                            let p = ui.painter();
                            p.text(
                                pos2(r.left() + 4.0, r.center().y),
                                Align2::LEFT_CENTER,
                                fmt_clock(c.start),
                                mono(ts.label),
                                if active { pal.accent } else { pal.text_dim },
                            );
                            p.with_clip_rect(r).text(
                                pos2(r.left() + 52.0, r.center().y),
                                Align2::LEFT_CENTER,
                                &c.title,
                                mono(ts.label),
                                if active { pal.accent } else { pal.text },
                            );
                            if resp.clicked() {
                                actions.push(Action::SeekChapter(i));
                            }
                        }
                        if info.chapters.len() > 8 {
                            widgets::readout(
                                ui,
                                "",
                                &format!("+ {} MORE", info.chapters.len() - 8),
                                Some(pal.text_dim),
                            );
                        }
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
                    "LOCATION",
                    ts.heading,
                    pal.text_dim,
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(track.source.display())
                            .font(mono(ts.label))
                            .color(pal.text_dim),
                    )
                    .wrap(),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let bsz = vec2(72.0, ts.row);
                    if widgets::button(ui, vec2(84.0, ts.row), "PLAY THIS", true).clicked() {
                        if let Some(i) = idx {
                            actions.push(Action::PlayIndex(i));
                        }
                    }
                    if widgets::button(ui, bsz, "COPY", true).clicked() {
                        ui.ctx().copy_text(track.source.display());
                    }
                    if widgets::button_colored(ui, bsz, "REMOVE", true, pal.warn).clicked() {
                        if let Some(i) = idx {
                            actions.push(Action::Remove(i));
                        }
                    }
                });
                ui.add_space(8.0);
                self.ui_hints(ui);
            });
    }

    fn ui_hints(&self, ui: &mut Ui) {
        let pal = palette(ui.ctx());
        let ts = type_scale(ui.ctx());
        let lh = ts.small + 5.0;
        // most useful first: only the lines that fit under the readouts are drawn
        let hints = [
            "KEYS :: SPACE PLAY/PAUSE  S SPEED  L LOOP",
            "LEFT/RIGHT SEEK 5S  +SHIFT 30S",
            "UP/DOWN VOLUME  M MUTE",
            "TAB PANELS  F FULL SCREEN  CLICK PIC PLAY",
            "CMD+O OPEN FILE  CMD+L URL  DROP = QUEUE",
            "ENTER PLAY ROW  BKSP DEQUEUE  DBLCLK PLAY",
            "CMD+UP/DOWN OR DRAG :: REORDER QUEUE",
            "C SUBTITLES  [ ] CHAPTERS",
            "CMD+LEFT/RIGHT PREV/NEXT TRACK",
            "CMD+1..3 PALETTE  CMD+, SETTINGS",
        ];
        let fit = ((ui.available_height() / lh).floor().max(0.0) as usize).min(hints.len());
        if fit == 0 {
            return;
        }
        let (fr, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), lh * fit as f32), Sense::hover());
        let p = ui.painter();
        for (i, h) in hints.iter().take(fit).enumerate() {
            p.text(
                pos2(fr.left(), fr.top() + lh * (i as f32 + 0.5)),
                Align2::LEFT_CENTER,
                *h,
                mono(ts.small),
                pal.text_dim,
            );
        }
    }

    /// Returns `true` when the title chip was clicked (the caller flips `Settings::log_open`).
    /// `open` is the setting (the chip shows `[-]` / `[+]`); `feed` says whether to lay the
    /// lines out — true while the panel is still folding up.
    fn ui_log(&self, ui: &mut Ui, rect: Rect, open: bool, feed: bool) -> bool {
        let pal = palette(ui.ctx());
        let (_, toggled) = Panel::new("Event log")
            .tag(format!("{} events", self.log.len()), pal.text_dim)
            .padding(8.0, 12.0)
            .show_collapsible_rect(ui, rect, open, |ui| {
                if !feed {
                    return; // just the header strip
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
        let tab = open && ctx.input(|i| i.key_pressed(Key::Tab));
        let finished;
        match state {
            DialogState::Open {
                input,
                error,
                focus,
                suggestions,
                suggested_for,
            } => {
                if *suggested_for != *input {
                    *suggestions = if input.contains("://") {
                        Vec::new()
                    } else {
                        fuide::pathinput::complete(input, &self.cwd, true, 6)
                    };
                    *suggested_for = input.clone();
                }
                let resp = Dialog::new("Open URL")
                    .tag("url or path", pal.text_dim)
                    .width(600.0)
                    .show(ctx, open, |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        let field =
                            widgets::text_input(ui, ui.available_width(), input, "path or url");
                        if *focus {
                            field.request_focus();
                            let mut st =
                                egui::TextEdit::load_state(ui.ctx(), field.id).unwrap_or_default();
                            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                                egui::text::CCursor::new(input.chars().count()),
                            )));
                            st.store(ui.ctx(), field.id);
                            *focus = false;
                        }
                        let submit = field.lost_focus() && enter;
                        // egui moves focus away on Tab before widgets run, so the field reports
                        // `lost_focus`; complete and take the focus back
                        let complete = tab && (field.has_focus() || field.lost_focus());
                        if complete {
                            *focus = true;
                        }
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
                                "HTTP(S) URL, OR A LOCAL PATH (~ AND RELATIVE OK, TAB COMPLETES)"
                                    .to_string(),
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
                        let ok = !input.trim().is_empty();
                        let clicked = fuide::dialog::button_row(
                            ui,
                            &[
                                ("CANCEL", pal.text_dim, true),
                                ("ADD TO QUEUE", pal.text, ok),
                                ("PLAY", pal.accent, ok),
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
                } else if clicked == Some(1) {
                    actions.push(Action::ConfirmOpen { play: false });
                } else if submit || clicked == Some(2) {
                    actions.push(Action::ConfirmOpen { play: true });
                } else if complete {
                    actions.push(Action::CompleteOpen);
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
}

/// `M:SS`, or `H:MM:SS` from an hour up.
pub fn fmt_clock(secs: f64) -> String {
    let s = secs.max(0.0).floor() as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// `MM:SS.t` (tenths), `H:MM:SS.t` from an hour up — the HUD timecode.
pub fn fmt_clock_tenths(secs: f64) -> String {
    let secs = secs.max(0.0);
    let s = secs.floor() as u64;
    let tenths = ((secs - s as f64) * 10.0).floor() as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}.{tenths}")
    } else {
        format!("{m:02}:{sec:02}.{tenths}")
    }
}

/// `1.5 Mb/s` / `128 kb/s`, `--` when unknown.
pub fn fmt_bitrate(bps: f32) -> String {
    if bps <= 0.0 {
        "--".into()
    } else if bps >= 1_000_000.0 {
        format!("{:.1} Mb/s", bps / 1_000_000.0)
    } else {
        format!("{:.0} kb/s", bps / 1000.0)
    }
}

#[cfg(test)]
mod e2e;
#[cfg(test)]
mod tests;
