//! End-to-end: the real `PlayerApp` (its `eframe::App::ui`, every frame) driven through the
//! accessibility tree with `egui_kittest`, on the `FakeBackend` (AVFoundation needs the main
//! thread's run loop, which the test harness does not run — see `tests/engine.rs`).

use egui::accesskit::{Role, Toggled};
use egui::{Key, Modifiers, Vec2};
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::Harness;

use super::*;
use crate::player::fake::FakeBackend;

/// The app opens picture-only; the tests that click panel buttons show the panels first.
fn harness_with_panels() -> Harness<'static, PlayerApp> {
    let mut h = harness();
    h.key_press(Key::Tab);
    h.run_steps(2);
    assert!(h.state().panels);
    h
}

fn harness() -> Harness<'static, PlayerApp> {
    let mut h = Harness::builder()
        .with_size(Vec2::new(1280.0, 800.0))
        .with_step_dt(1.0 / 60.0)
        .build_eframe(move |cc| {
            PlayerApp::with_context(
                &cc.egui_ctx,
                Settings::default(),
                Box::new(FakeBackend::new()),
            )
        });
    h.run_steps(2);
    h
}

#[test]
fn cmd_l_queues_a_url_and_the_transport_controls_it() {
    let mut h = harness_with_panels();
    assert!(h.state().queue.is_empty());
    assert_eq!(h.state().snap.status, Status::Idle);

    // Cmd+L (the in-app dialog; Cmd+O would be the macOS file panel) -> type a URL -> Enter
    h.key_press_modifiers(Modifiers::COMMAND, Key::L);
    h.run_steps(3);
    assert_eq!(
        h.state().loop_mode,
        LoopMode::Off,
        "the L in Cmd+L must not toggle loop"
    );
    h.event(egui::Event::Text(
        "https://media.example.com/clips/demo.mp4".into(),
    ));
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(20); // dialog fade + two backend polls
    assert!(h.state().dialog.is_none());
    assert_eq!(h.state().current, Some(0));
    assert!(h.state().snap.playing);
    assert_eq!(h.state().snap.status, Status::Ready);
    // the queue row carries the embedded title once the fake "loaded" it
    h.get_by_role_and_label(Role::Button, "Title of demo.mp4");

    // Space pauses, the PLAY button (was PAUSE) resumes
    h.key_press(Key::Space);
    h.run_steps(2);
    assert!(!h.state().snap.playing);
    h.get_by_label("PLAY").click();
    h.run_steps(2);
    assert!(h.state().snap.playing);
    h.get_by_label("PAUSE");

    // arrows seek, M mutes, S changes the speed
    let before = h.state().snap.time;
    h.key_press(Key::ArrowRight);
    h.run_steps(2);
    assert!(h.state().snap.time >= before + 4.0);
    h.key_press(Key::M);
    h.run_steps(2);
    assert!(h.state().snap.muted);
    h.key_press(Key::S);
    h.run_steps(2);
    assert_eq!(h.state().snap.rate, 1.25);

    // STOP rewinds and pauses
    h.get_by_label("STOP").click();
    h.run_steps(2);
    assert_eq!(h.state().snap.time, 0.0);
    assert!(!h.state().snap.playing);
}

#[test]
fn queue_rows_select_play_and_dequeue() {
    let mut h = harness_with_panels();
    let sources = vec![
        Source::File("/m/one.mp3".into()),
        Source::File("/m/two.mp3".into()),
        Source::File("/m/three.mp3".into()),
    ];
    let ctx = h.ctx.clone();
    h.state_mut().apply(
        &ctx,
        Action::Add {
            sources,
            play: false,
        },
        0.0,
    );
    h.run_steps(4);
    assert_eq!(h.state().current, Some(0));

    // click selects (toggled), double-click plays
    h.get_by_label("three.mp3").click();
    h.run_steps(2);
    assert_eq!(h.state().selected, Some(2));
    assert_eq!(
        h.get_by_role_and_label(Role::Button, "three.mp3")
            .accesskit_node()
            .toggled(),
        Some(Toggled::True)
    );
    h.key_press(Key::Enter);
    h.run_steps(4);
    assert_eq!(h.state().current, Some(2));

    // PREVIOUS within 3 s goes back a track; NEXT at the end stays
    h.get_by_label("PREVIOUS").click();
    h.run_steps(2);
    assert_eq!(h.state().current, Some(1));
    h.get_by_label("NEXT").click();
    h.run_steps(2);
    assert_eq!(h.state().current, Some(2));

    // Cmd+Up moves the selected row up (three.mp3 is playing and selected at index 2)
    h.key_press_modifiers(Modifiers::COMMAND, Key::ArrowUp);
    h.run_steps(2);
    assert_eq!(h.state().queue[1].title, "Title of three.mp3");
    assert_eq!(h.state().current, Some(1));
    h.key_press_modifiers(Modifiers::COMMAND, Key::ArrowDown);
    h.run_steps(2);
    assert_eq!(h.state().queue[2].title, "Title of three.mp3");
    assert_eq!(h.state().current, Some(2));

    // Backspace removes the selected row
    h.get_by_label("Title of two.mp3").click();
    h.run_steps(2);
    h.key_press(Key::Backspace);
    h.run_steps(2);
    assert_eq!(h.state().queue.len(), 2);
    assert_eq!(
        h.state().current,
        Some(1),
        "current index followed the shift"
    );

    // CLEAR empties everything
    h.get_by_label("CLEAR").click();
    h.run_steps(2);
    assert!(h.state().queue.is_empty());
    assert_eq!(h.state().current, None);
    assert_eq!(h.state().snap.status, Status::Idle);
}

/// Headless wgpu renders of the three screen states (`UPDATE_SNAPSHOTS=true cargo test -p
/// fuide-player` to regenerate `tests/snapshots/*.png`). The fake backend's "video" is a flat
/// frame; the layout, HUD and transport are what the pictures check.
#[test]
fn snapshots_idle_video_and_audio() {
    let mut h = harness();
    h.run_steps(3);
    h.snapshot("player_theater_idle");
    h.key_press(Key::Tab);
    h.run_steps(3);
    h.snapshot("player_idle");

    let ctx = h.ctx.clone();
    h.state_mut().apply(
        &ctx,
        Action::Add {
            sources: vec![
                Source::Url("https://media.example.com/clips/orbital-pass.mp4".into()),
                Source::File("/m/second-track.mp3".into()),
            ],
            play: true,
        },
        0.0,
    );
    h.run_steps(12); // ready + ~2.5 s in
    assert!(h.state().backend.frame().is_some());
    h.snapshot("player_video");

    // subtitles on: a caption band under the picture carries the text
    h.state_mut()
        .apply(&ctx, Action::SelectSubtitle(Some(0)), 0.0);
    h.run_steps(3);
    assert!(h.state().backend.caption().is_some());
    h.snapshot("player_captions");
    h.state_mut().apply(&ctx, Action::SelectSubtitle(None), 0.0);
    h.run_steps(2);

    // picture only: the HUD strip above the frame, the transport under it, nothing on it
    h.key_press(Key::Tab);
    h.run_steps(3);
    h.snapshot("player_theater_video");
    h.key_press(Key::Tab);
    h.run_steps(3);

    h.state_mut().apply(&ctx, Action::Next, 0.0);
    h.run_steps(10);
    assert!(h.state().backend.info().video.is_none());
    h.snapshot("player_audio");
}

#[test]
fn theater_mode_keeps_the_transport_and_hides_the_side_panels() {
    let mut h = harness();
    assert!(!h.state().panels);
    // the transport is always there; the queue / media panels are not
    h.get_by_label("VOLUME");
    assert!(h.query_by_label("CLEAR").is_none());
    assert!(h.query_by_label("PLAY THIS").is_none());

    let ctx = h.ctx.clone();
    h.state_mut().apply(
        &ctx,
        Action::Add {
            sources: vec![Source::File("/m/movie.mp4".into())],
            play: true,
        },
        0.0,
    );
    h.run_steps(4);
    assert!(h.state().snap.playing);
    h.get_by_label("VOLUME");
    // a click on the picture pauses, another (not a double click) resumes
    h.get_by_label("SCREEN (click = play/pause)").click();
    h.run_steps(2);
    assert!(!h.state().snap.playing);
    h.run_steps(30); // past egui's double-click window
    h.get_by_label("SCREEN (click = play/pause)").click();
    h.run_steps(2);
    assert!(h.state().snap.playing);
    // Tab shows everything
    h.key_press(Key::Tab);
    h.run_steps(2);
    h.get_by_label("CLEAR");
    h.get_by_label("PLAY THIS");
}
