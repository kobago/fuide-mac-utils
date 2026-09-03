//! End-to-end: the real `MonitorApp` (its `eframe::App::ui`, every frame) driven through the
//! accessibility tree with `egui_kittest`, on the fake source. The sampler thread still runs;
//! `settle` pumps frames until its first snapshot has arrived.

use std::time::{Duration, Instant};

use egui::accesskit::Toggled;
use egui::{Key, Modifiers, Vec2};
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::Harness;

use super::*;
use crate::sys::fake::FakeSource;

/// Frames (at 60 fps) that let a dialog fade or the log fold settle.
const FOLD_STEPS: usize = 24;

fn harness() -> Harness<'static, MonitorApp> {
    let mut h = Harness::builder()
        .with_size(Vec2::new(1280.0, 800.0))
        .with_step_dt(1.0 / 60.0)
        .build_eframe(move |cc| {
            MonitorApp::with_context(
                &cc.egui_ctx,
                Settings::default(),
                Box::new(FakeSource::new()),
            )
        });
    settle(&mut h);
    h
}

/// Runs frames until the sampler's first snapshot is in.
fn settle(h: &mut Harness<'static, MonitorApp>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.state().snap.is_none() {
        h.run_steps(1);
        assert!(Instant::now() < deadline, "no snapshot from the sampler");
        std::thread::sleep(Duration::from_millis(2));
    }
    h.run_steps(2);
}

/// The dialog's button of that name: the toolbar has QUIT / FORCE QUIT too, so take the last
/// match (the modal is on top, hence later in the tree). Waits out the fade-in, during which
/// the dialog's widgets are disabled.
fn dialog_button<'a>(
    h: &'a mut Harness<'static, MonitorApp>,
    label: &'a str,
) -> egui_kittest::Node<'a> {
    for _ in 0..30 {
        let enabled = h
            .query_all_by_label(label)
            .last()
            .is_some_and(|n| !n.accesskit_node().is_disabled());
        if enabled {
            break;
        }
        h.run_steps(1);
    }
    let n = h.query_all_by_label(label).last().expect("dialog button");
    assert!(
        !n.accesskit_node().is_disabled(),
        "{label} never became enabled"
    );
    n
}

/// The tab of that name (a column header may share the label; only tabs carry a toggle state).
fn tab_toggled(h: &Harness<'static, MonitorApp>, label: &str) -> Option<Toggled> {
    h.query_all_by_label(label)
        .find_map(|n| n.accesskit_node().toggled())
}

#[test]
fn tabs_switch_columns_and_report_selection() {
    let mut h = harness();
    assert_eq!(tab_toggled(&h, "CPU"), Some(Toggled::True));
    h.get_by_label("% CPU"); // a CPU column header
    h.get_by_label("MEMORY").click();
    h.run_steps(2);
    assert_eq!(h.state().tab, Tab::Memory);
    assert_eq!(tab_toggled(&h, "MEMORY"), Some(Toggled::True));
    assert_eq!(tab_toggled(&h, "CPU"), Some(Toggled::False));
    assert!(h.query_by_label("% CPU").is_none());
    h.get_by_label("RESIDENT");
    h.key_press_modifiers(Modifiers::COMMAND, Key::Num5);
    h.run_steps(2);
    assert_eq!(h.state().tab, Tab::Network);
    h.get_by_label("TOTAL IN");
}

#[test]
fn rows_select_by_name_and_the_verbs_light_up() {
    let mut h = harness();
    assert!(h.get_by_label("QUIT").accesskit_node().is_disabled());
    h.get_by_label("Safari").click();
    h.run_steps(2);
    assert_eq!(h.state().selected_pid, Some(1201));
    assert_eq!(
        h.get_by_label("Safari").accesskit_node().toggled(),
        Some(Toggled::True)
    );
    assert!(!h.get_by_label("QUIT").accesskit_node().is_disabled());
    // arrow keys move the selection
    h.key_press(Key::ArrowDown);
    h.run_steps(2);
    assert_ne!(h.state().selected_pid, Some(1201));
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert_eq!(h.state().selected_pid, None);
}

#[test]
fn filter_narrows_the_table_and_escape_clears_it() {
    let mut h = harness();
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(1);
    h.event(egui::Event::Text("music".into()));
    h.run_steps(2);
    assert_eq!(h.state().view.len(), 1);
    h.get_by_label("Music");
    assert!(h.query_by_label("Safari").is_none());
    h.key_press(Key::Escape);
    h.run_steps(3);
    assert!(h.state().filter.is_empty());
    assert!(h.state().view.len() > 1);
}

#[test]
fn quit_dialog_confirms_and_the_process_goes_away() {
    let mut h = harness();
    h.get_by_label("Music").click();
    h.run_steps(2);
    h.get_by_label("QUIT").click();
    h.run_steps(2);
    // the dialog: CANCEL / QUIT, Enter confirms
    dialog_button(&mut h, "CANCEL").click();
    h.run_steps(FOLD_STEPS);
    assert!(h.state().dialog.is_none());
    assert!(h.state().log.iter().all(|e| !e.text.contains("SIGTERM")));

    h.get_by_label("FORCE QUIT").click();
    h.run_steps(2);
    dialog_button(&mut h, "FORCE QUIT");
    h.key_press(Key::Enter);
    h.run_steps(2);
    assert!(h.state().log.iter().any(|e| e
        .text
        .contains("force quit // Music [1340] :: SIGKILL sent")));
    // the sampler re-samples right after a kill: Music is gone from the fake's list
    let deadline = Instant::now() + Duration::from_secs(5);
    while h
        .state()
        .snap
        .as_ref()
        .unwrap()
        .procs
        .iter()
        .any(|p| p.pid == 1340)
    {
        h.run_steps(1);
        assert!(Instant::now() < deadline, "Music still listed");
        std::thread::sleep(Duration::from_millis(2));
    }
    h.run_steps(FOLD_STEPS);
    assert!(h.query_by_label("Music").is_none());
    assert_eq!(
        h.state().selected_pid,
        None,
        "selection dropped with the process"
    );
}

#[test]
fn refusing_the_kernel_shows_an_error_card() {
    let mut h = harness();
    h.get_by_label("launchd").click();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::ALT, Key::Q);
    h.run_steps(2);
    dialog_button(&mut h, "QUIT").click();
    h.run_steps(FOLD_STEPS);
    // the ERROR card replaced the confirm
    dialog_button(&mut h, "ACKNOWLEDGE").click();
    h.run_steps(FOLD_STEPS);
    assert!(h.state().dialog.is_none());
    assert!(h
        .state()
        .log
        .iter()
        .any(|e| e.text.contains("not permitted")));
}

/// Headless wgpu renders of the five tabs and the quit dialog (`UPDATE_SNAPSHOTS=true cargo
/// test -p fuide-activity-monitor` to regenerate `tests/snapshots/*.png`).
#[test]
fn snapshots_of_every_tab() {
    let mut h = harness();
    // a few samples so the graphs have a line
    let ctx = h.ctx.clone();
    let mut src = FakeSource::new();
    for _ in 0..40 {
        let s = src.sample();
        h.state_mut().ingest(s, 0.0);
    }
    h.get_by_label("rustc").click();
    h.run_steps(3);
    h.snapshot("monitor_cpu");
    for (tab, name) in [
        (Tab::Memory, "monitor_memory"),
        (Tab::Energy, "monitor_energy"),
        (Tab::Disk, "monitor_disk"),
        (Tab::Network, "monitor_network"),
    ] {
        h.state_mut().apply(&ctx, Action::Tab(tab), 0.0);
        h.run_steps(3);
        h.snapshot(name);
    }
    h.state_mut().apply(&ctx, Action::Tab(Tab::Cpu), 0.0);
    h.state_mut()
        .apply(&ctx, Action::RequestKill { force: true }, 0.0);
    h.run_steps(FOLD_STEPS);
    h.snapshot("monitor_force_quit");
}
