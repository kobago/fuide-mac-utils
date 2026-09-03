//! State-machine tests: `Action`s applied to a `PlayerApp` on a bare `egui::Context` with the
//! deterministic `FakeBackend` (ready after two polls, a quarter second per poll).

use super::*;
use crate::player::fake::FakeBackend;

fn app() -> (egui::Context, PlayerApp) {
    let ctx = egui::Context::default();
    let app = PlayerApp::with_context(&ctx, Settings::default(), Box::new(FakeBackend::new()));
    (ctx, app)
}

/// What `ui()` does every frame for the backend: poll, then housekeeping.
fn tick(ctx: &egui::Context, app: &mut PlayerApp, n: usize) {
    for _ in 0..n {
        app.snap = app.backend.poll(ctx);
        app.after_poll(0.0);
    }
}

fn log_has(app: &PlayerApp, needle: &str) -> bool {
    app.log.iter().any(|e| e.text.contains(needle))
}

fn file(name: &str) -> Source {
    Source::File(PathBuf::from(format!("/media/{name}")))
}

fn add(ctx: &egui::Context, app: &mut PlayerApp, names: &[&str], play: bool) {
    app.apply(
        ctx,
        Action::Add {
            sources: names.iter().map(|n| file(n)).collect(),
            play,
        },
        0.0,
    );
}

fn open_input(app: &mut PlayerApp) -> (&mut String, &mut Option<String>, &mut Vec<String>) {
    match &mut app.dialog {
        Some(OpenDialog {
            state:
                DialogState::Open {
                    input,
                    error,
                    suggestions,
                    ..
                },
            ..
        }) => (input, error, suggestions),
        _ => panic!("open dialog is not open"),
    }
}

#[test]
fn adding_plays_the_first_track_when_idle_and_reads_duration_and_title_once_ready() {
    let (ctx, mut app) = app();
    add(&ctx, &mut app, &["a.mp3", "b.mp4"], false);
    assert_eq!(
        app.current,
        Some(0),
        "idle player starts the first added track"
    );
    assert_eq!(app.selected, Some(0));
    assert!(log_has(&app, "queue // a.mp3"));
    assert!(log_has(&app, "play // a.mp3"));
    assert_eq!(app.snap.status, Status::Idle, "not polled yet");

    tick(&ctx, &mut app, 2);
    assert_eq!(app.snap.status, Status::Ready);
    assert!(app.snap.playing);
    assert_eq!(app.queue[0].duration, Some(5.0));
    assert_eq!(app.queue[0].title, "Title of a.mp3");
    assert_eq!(app.queue[1].title, "b.mp4", "untouched until it plays");
    assert!(app.backend.info().video.is_none());

    // adding more while playing only queues (selection unchanged)
    add(&ctx, &mut app, &["c.mp3"], false);
    assert_eq!(app.current, Some(0));
    assert_eq!(app.queue.len(), 3);

    // `play` starts the new one right away
    add(&ctx, &mut app, &["https://host/x.mp4"], true);
    assert_eq!(app.current, Some(3));
    tick(&ctx, &mut app, 2);
    assert!(app.backend.info().video.is_some());
}

#[test]
fn end_of_track_follows_the_loop_mode() {
    let (ctx, mut app) = app();
    add(&ctx, &mut app, &["a.mp3", "b.mp3"], true);
    // 5 s at 0.25 s / poll: ready after 2, ends after 20 more, reported one poll later
    tick(&ctx, &mut app, 30);
    assert_eq!(app.current, Some(1), "loop off: advanced to the next track");
    assert!(log_has(&app, "end of track :: advancing"));
    tick(&ctx, &mut app, 30);
    assert_eq!(app.current, Some(1), "last track: stays");
    assert!(!app.snap.playing);
    assert!(log_has(&app, "end of queue"));

    app.loop_mode = LoopMode::All;
    app.apply(&ctx, Action::PlayIndex(1), 0.0);
    tick(&ctx, &mut app, 30);
    assert_eq!(app.current, Some(0), "loop all wraps around");

    app.loop_mode = LoopMode::One;
    let opened_before = fake(&app).opened.len();
    tick(&ctx, &mut app, 30);
    assert_eq!(app.current, Some(0), "loop one replays the same track");
    assert_eq!(fake(&app).opened.len(), opened_before + 1);
    assert!(app.snap.playing || app.snap.status == Status::Loading);
}

fn fake(app: &PlayerApp) -> &FakeBackend {
    // SAFETY-free downcast: tests always build the app with a `FakeBackend`
    let ptr = &*app.backend as *const dyn Backend as *const FakeBackend;
    unsafe { &*ptr }
}

#[test]
fn next_prev_stop_and_toggle() {
    let (ctx, mut app) = app();
    add(&ctx, &mut app, &["a.mp3", "b.mp3", "c.mp3"], true);
    tick(&ctx, &mut app, 2);
    app.apply(&ctx, Action::Next, 0.0);
    assert_eq!(app.current, Some(1));
    tick(&ctx, &mut app, 2);

    // Prev within the first 3 s goes back a track; later it restarts the current one
    app.apply(&ctx, Action::Prev, 0.0);
    assert_eq!(app.current, Some(0));
    tick(&ctx, &mut app, 2 + 16); // ~4 s in
    assert!(app.snap.time > 3.0);
    app.apply(&ctx, Action::Prev, 0.0);
    assert_eq!(app.current, Some(0));
    assert_eq!(app.backend.time(), 0.0);

    // Next at the end with loop off: nothing happens
    app.apply(&ctx, Action::PlayIndex(2), 0.0);
    app.apply(&ctx, Action::Next, 0.0);
    assert_eq!(app.current, Some(2));
    assert!(log_has(&app, "end of queue"));

    // Stop pauses and rewinds; Toggle resumes
    tick(&ctx, &mut app, 6);
    assert!(app.backend.time() > 0.5);
    app.apply(&ctx, Action::Stop, 0.0);
    assert!(!app.backend.is_playing());
    assert_eq!(app.backend.time(), 0.0);
    app.apply(&ctx, Action::Toggle, 0.0);
    assert!(app.backend.is_playing());
    app.apply(&ctx, Action::Toggle, 0.0);
    assert!(!app.backend.is_playing());

    // Toggle with nothing loaded plays the selection
    app.apply(&ctx, Action::Clear, 0.0);
    add(&ctx, &mut app, &["d.mp3", "e.mp3"], false);
    assert_eq!(app.current, Some(0), "empty queue: the added track plays");
    app.apply(&ctx, Action::Remove(0), 0.0);
    assert_eq!(app.current, None, "removing the playing track unloads it");
    assert_eq!(app.snap.status, Status::Idle);
    assert_eq!(app.selected, Some(0), "selection moves to the next row");
    app.apply(&ctx, Action::Toggle, 0.0);
    assert_eq!(app.current, Some(0));
    assert_eq!(app.queue[0].title, "e.mp3");
}

#[test]
fn remove_and_clear_keep_indices_consistent() {
    let (ctx, mut app) = app();
    add(&ctx, &mut app, &["a.mp3", "b.mp3", "c.mp3"], true);
    app.apply(&ctx, Action::PlayIndex(2), 0.0);
    app.apply(&ctx, Action::Select(Some(1)), 0.0);
    app.apply(&ctx, Action::Remove(0), 0.0);
    assert_eq!(app.current, Some(1), "current shifts down");
    assert_eq!(app.selected, Some(0));
    assert_eq!(app.queue.len(), 2);
    assert!(log_has(&app, "dequeue // a.mp3"));

    app.apply(&ctx, Action::Remove(5), 0.0); // out of range: ignored
    assert_eq!(app.queue.len(), 2);

    app.apply(&ctx, Action::Clear, 0.0);
    assert!(app.queue.is_empty());
    assert_eq!(app.current, None);
    assert_eq!(app.selected, None);
    assert!(log_has(&app, "queue // cleared 2 tracks"));
    app.apply(&ctx, Action::Clear, 0.0); // empty: no second log line
    assert_eq!(
        app.log
            .iter()
            .filter(|e| e.text.contains("cleared"))
            .count(),
        1
    );
}

#[test]
fn volume_mute_speed_and_loop_controls() {
    let (ctx, mut app) = app();
    add(&ctx, &mut app, &["a.mp3"], true);
    tick(&ctx, &mut app, 2);
    app.apply(&ctx, Action::Volume(0.3), 0.0);
    assert!((app.backend.volume() - 0.3).abs() < 1e-6);
    app.apply(&ctx, Action::Volume(1.7), 0.0);
    assert_eq!(app.backend.volume(), 1.0, "clamped");
    app.apply(&ctx, Action::ToggleMute, 0.0);
    assert!(app.backend.muted());
    app.apply(&ctx, Action::Volume(0.5), 0.0);
    assert!(!app.backend.muted(), "setting a volume unmutes");

    assert_eq!(app.backend.speed(), 1.0);
    app.apply(&ctx, Action::CycleSpeed, 0.0);
    assert_eq!(app.backend.speed(), 1.25);
    for _ in 0..4 {
        app.apply(&ctx, Action::CycleSpeed, 0.0);
    }
    assert_eq!(app.backend.speed(), 1.0, "cycles back around");

    assert_eq!(app.loop_mode, LoopMode::Off);
    app.apply(&ctx, Action::CycleLoop, 0.0);
    assert_eq!(app.loop_mode, LoopMode::All);
    app.apply(&ctx, Action::CycleLoop, 0.0);
    assert_eq!(app.loop_mode, LoopMode::One);
    app.apply(&ctx, Action::CycleLoop, 0.0);
    assert_eq!(app.loop_mode, LoopMode::Off);

    // seeking
    app.apply(&ctx, Action::Seek(4.0), 0.0);
    assert_eq!(app.backend.time(), 4.0);
    app.apply(&ctx, Action::SeekBy(-1.5), 0.0);
    assert_eq!(app.backend.time(), 2.5);
    app.apply(&ctx, Action::SeekBy(100.0), 0.0);
    assert_eq!(app.backend.time(), 5.0, "clamped to the duration");
}

#[test]
fn a_failing_item_is_reported_and_unloaded() {
    let (ctx, mut app) = app();
    add(&ctx, &mut app, &["missing.mp4"], true);
    tick(&ctx, &mut app, 2);
    assert_eq!(app.current, None);
    assert!(log_has(
        &app,
        "play // missing.mp4 :: failed :: no such file"
    ));
    assert_eq!(
        app.error_queue.front().map(String::as_str),
        Some("play // missing.mp4")
    );
    assert_eq!(app.queue.len(), 1, "the track stays queued");
}

#[test]
fn open_dialog_queues_urls_and_paths_completes_and_rejects_bad_input() {
    let (ctx, mut app) = app();
    app.apply(&ctx, Action::OpenDialog, 0.0);
    *open_input(&mut app).0 = "https://example.com/stream/live.m3u8".into();
    app.apply(&ctx, Action::ConfirmOpen { play: true }, 0.0);
    assert!(app.dialog.as_ref().is_some_and(|d| d.closing));
    assert_eq!(
        app.queue[0].source,
        Source::Url("https://example.com/stream/live.m3u8".into())
    );
    assert_eq!(app.queue[0].title, "live.m3u8");
    assert_eq!(app.current, Some(0));
    app.dialog = None;

    // a local file must exist
    app.apply(&ctx, Action::OpenDialog, 0.0);
    *open_input(&mut app).0 = "/definitely/not/here.mp4".into();
    app.apply(&ctx, Action::ConfirmOpen { play: false }, 0.0);
    assert!(app.dialog.as_ref().is_some_and(|d| !d.closing));
    assert_eq!(open_input(&mut app).1.as_deref(), Some("no such file"));
    assert_eq!(app.queue.len(), 1);

    // an existing file, queued without playing
    let dir = std::env::temp_dir().join(format!("fuide-player-open-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("song.mp3"), "x").unwrap();
    std::fs::write(dir.join("sound.wav"), "x").unwrap();
    *open_input(&mut app).0 = dir.join("song.mp3").display().to_string();
    app.apply(&ctx, Action::ConfirmOpen { play: false }, 0.0);
    assert_eq!(app.queue.len(), 2);
    assert_eq!(app.current, Some(0), "queued only");
    app.dialog = None;

    // Tab completion (suggestions are computed by the dialog UI; set them as it would)
    app.apply(&ctx, Action::OpenDialog, 0.0);
    let prefix = format!("{}/so", dir.display());
    let cwd = app.cwd.clone();
    {
        let (input, _, suggestions) = open_input(&mut app);
        *input = prefix.clone();
        *suggestions = fuide::pathinput::complete(&prefix, &cwd, true, 6);
        assert_eq!(suggestions.len(), 2);
    }
    app.apply(&ctx, Action::CompleteOpen, 0.0);
    assert_eq!(
        *open_input(&mut app).0,
        format!("{}/so", dir.display()),
        "common prefix only"
    );
    {
        let (input, _, suggestions) = open_input(&mut app);
        input.push('n');
        *suggestions = fuide::pathinput::complete(input, &cwd, true, 6);
    }
    app.apply(&ctx, Action::CompleteOpen, 0.0);
    assert_eq!(
        *open_input(&mut app).0,
        dir.join("song.mp3").display().to_string()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clock_and_bitrate_formatting() {
    assert_eq!(fmt_clock(0.0), "0:00");
    assert_eq!(fmt_clock(65.9), "1:05");
    assert_eq!(fmt_clock(3725.0), "1:02:05");
    assert_eq!(fmt_clock_tenths(65.94), "01:05.9");
    assert_eq!(fmt_clock_tenths(3600.0), "1:00:00.0");
    assert_eq!(fmt_bitrate(0.0), "--");
    assert_eq!(fmt_bitrate(128_000.0), "128 kb/s");
    assert_eq!(fmt_bitrate(2_500_000.0), "2.5 Mb/s");
}

#[test]
fn panels_are_hidden_by_default_and_tab_toggles_them() {
    let (ctx, mut app) = app();
    assert!(!app.panels, "picture only by default");
    app.apply(&ctx, Action::TogglePanels, 0.0);
    assert!(app.panels);
    assert!(log_has(&app, "view // panels shown"));
    app.apply(&ctx, Action::TogglePanels, 0.0);
    assert!(!app.panels);
    assert!(log_has(&app, "view // picture only"));
}

#[test]
fn open_files_falls_back_to_the_in_app_dialog_without_the_native_picker() {
    let (ctx, mut app) = app();
    assert!(!app.native_open, "tests never show the macOS dialog");
    app.apply(&ctx, Action::OpenFiles, 0.0);
    assert!(matches!(
        &app.dialog,
        Some(OpenDialog {
            state: DialogState::Open { .. },
            closing: false
        })
    ));
}

#[test]
fn move_reorders_the_queue_and_follows_current_and_selection() {
    let (ctx, mut app) = app();
    add(&ctx, &mut app, &["a.mp3", "b.mp3", "c.mp3", "d.mp3"], true);
    app.apply(&ctx, Action::Select(Some(3)), 0.0); // current 0 (a), selected 3 (d)
    app.apply(&ctx, Action::Move { from: 0, to: 2 }, 0.0);
    let names: Vec<&str> = app.queue.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(names, ["b.mp3", "c.mp3", "a.mp3", "d.mp3"]);
    assert_eq!(app.current, Some(2), "the playing track moved with its row");
    assert_eq!(app.selected, Some(3));
    app.apply(&ctx, Action::Move { from: 3, to: 0 }, 0.0);
    let names: Vec<&str> = app.queue.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(names, ["d.mp3", "b.mp3", "c.mp3", "a.mp3"]);
    assert_eq!(app.current, Some(3));
    assert_eq!(app.selected, Some(0));
    assert!(log_has(&app, "queue // moved 4 -> 1"));
    // no-ops and out-of-range moves are ignored
    app.apply(&ctx, Action::Move { from: 1, to: 1 }, 0.0);
    app.apply(&ctx, Action::Move { from: 9, to: 0 }, 0.0);
    assert_eq!(app.queue[1].title, "b.mp3");
}

#[test]
fn subtitles_cycle_and_chapters_navigate() {
    let (ctx, mut app) = app();
    add(&ctx, &mut app, &["movie.mp4"], true);
    tick(&ctx, &mut app, 2);
    assert_eq!(app.backend.subtitles(), ["English", "日本語"]);
    assert_eq!(app.backend.subtitle(), None);
    app.apply(&ctx, Action::CycleSubtitle, 0.0);
    assert_eq!(app.backend.subtitle(), Some(0));
    assert!(log_has(&app, "subtitles // English"));
    app.apply(&ctx, Action::CycleSubtitle, 0.0);
    assert_eq!(app.backend.subtitle(), Some(1));
    app.apply(&ctx, Action::CycleSubtitle, 0.0);
    assert_eq!(app.backend.subtitle(), None, "wraps to off");
    app.apply(&ctx, Action::SelectSubtitle(Some(1)), 0.0);
    tick(&ctx, &mut app, 6); // 1.5 s in: the fake emits a caption from 1 s
    assert_eq!(
        app.backend.caption().as_deref(),
        Some("日本語 caption at 1s")
    );

    // chapters: Intro 0-3, Middle 3-7, End 7-10
    let info = app.backend.info().clone();
    assert_eq!(info.chapters.len(), 3);
    assert_eq!(info.chapter_at(0.5), Some(0));
    assert_eq!(info.chapter_at(3.0), Some(1));
    assert_eq!(info.chapter_at(10.0), Some(2), "the very end still counts");
    app.apply(&ctx, Action::NextChapter, 0.0);
    assert_eq!(app.backend.time(), 3.0);
    app.apply(&ctx, Action::NextChapter, 0.0);
    assert_eq!(app.backend.time(), 7.0);
    app.apply(&ctx, Action::NextChapter, 0.0);
    assert_eq!(app.backend.time(), 7.0, "no chapter after the last");
    app.apply(&ctx, Action::PrevChapter, 0.0);
    assert_eq!(
        app.backend.time(),
        3.0,
        "within 3 s of a start: previous chapter"
    );
    tick(&ctx, &mut app, 16); // 4 s further into Middle
    app.apply(&ctx, Action::PrevChapter, 0.0);
    assert_eq!(app.backend.time(), 3.0, "deep into a chapter: its start");
    app.apply(&ctx, Action::SeekChapter(2), 0.0);
    assert_eq!(app.backend.time(), 7.0);
    assert!(log_has(&app, "chapter // 3 End"));

    // audio-only items have neither
    add(&ctx, &mut app, &["song.mp3"], true);
    tick(&ctx, &mut app, 2);
    assert!(app.backend.subtitles().is_empty());
    app.apply(&ctx, Action::CycleSubtitle, 0.0);
    assert!(log_has(&app, "subtitles // none in this item"));
}
