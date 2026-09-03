//! State-machine tests: `apply` and `ingest` on the fake source, no UI.

use super::*;
use crate::sys::fake::FakeSource;

fn app() -> (egui::Context, MonitorApp) {
    let ctx = egui::Context::default();
    let app = MonitorApp::with_context(&ctx, Settings::default(), Box::new(FakeSource::new()));
    (ctx, app)
}

/// Feed `n` fake snapshots straight into the app (bypassing the sampler thread).
fn feed(app: &mut MonitorApp, n: usize) {
    let mut src = FakeSource::new();
    for _ in 0..n {
        let s = src.sample();
        app.ingest(s, 0.0);
    }
}

fn log_has(app: &MonitorApp, needle: &str) -> bool {
    app.log.iter().any(|e| e.text.contains(needle))
}

fn names(app: &MonitorApp) -> Vec<String> {
    let snap = app.snap.as_ref().unwrap();
    app.view
        .iter()
        .map(|&i| snap.procs[i].name.clone())
        .collect()
}

#[test]
fn cpu_tab_sorts_by_cpu_descending_by_default() {
    let (_, mut app) = app();
    feed(&mut app, 2);
    assert_eq!(app.tab, Tab::Cpu);
    let n = names(&app);
    assert_eq!(n[0], "rustc", "busiest first: {n:?}");
    assert_eq!(n[1], "cargo");
    assert!(log_has(&app, "scan // 10 processes"));
}

#[test]
fn tabs_reset_the_sort_to_their_key_metric() {
    let (ctx, mut app) = app();
    feed(&mut app, 2);
    app.apply(&ctx, Action::Tab(Tab::Memory), 0.0);
    assert_eq!(names(&app)[0], "rustc"); // 3.1 GB
    app.apply(&ctx, Action::Tab(Tab::Network), 0.0);
    assert_eq!(names(&app)[0], "Safari");
    app.apply(&ctx, Action::Tab(Tab::Disk), 0.0);
    assert_eq!(names(&app)[0], "cargo");
    app.apply(&ctx, Action::Tab(Tab::Energy), 0.0);
    assert_eq!(names(&app)[0], "rustc");
    assert!(log_has(&app, "view // energy"));
}

#[test]
fn filter_and_mine_only_narrow_the_view_and_selection_follows_the_pid() {
    let (ctx, mut app) = app();
    feed(&mut app, 1);
    let total = app.view.len();
    app.apply(&ctx, Action::ToggleMine, 0.0);
    assert_eq!(
        app.view.len(),
        total - 2,
        "launchd and kernel_task are root's"
    );
    app.apply(&ctx, Action::ToggleMine, 0.0);

    app.filter = "saf".into();
    app.rebuild();
    assert_eq!(names(&app), vec!["Safari"]);
    app.apply(&ctx, Action::Select(Some(0)), 0.0);
    assert_eq!(app.selected_pid, Some(1201));
    app.apply(&ctx, Action::ClearFilter, 0.0);
    assert_eq!(app.view.len(), total);
    // still selected, at its new row
    let row = app.table.selected.unwrap();
    assert_eq!(names(&app)[row], "Safari");
    // pid filter
    app.filter = "2023".into();
    app.rebuild();
    assert_eq!(names(&app), vec!["rustc"]);
}

#[test]
fn quit_needs_confirmation_and_reports_the_outcome() {
    let (ctx, mut app) = app();
    feed(&mut app, 1);
    // nothing selected: no dialog
    app.apply(&ctx, Action::RequestKill { force: false }, 0.0);
    assert!(app.dialog.is_none());

    app.selected_pid = Some(1340);
    app.apply(&ctx, Action::RequestKill { force: false }, 0.0);
    assert!(matches!(
        &app.dialog,
        Some(OpenDialog { state: DialogState::Confirm(c), closing: false }) if c.name == "Music" && !c.force && !c.foreign
    ));
    assert_eq!(app.agent_blocked(), vec!["QUIT".to_string()]);
    app.apply(&ctx, Action::ConfirmDialog, 0.0);
    assert!(log_has(&app, "quit // Music [1340] :: SIGTERM sent"));
    assert!(matches!(
        &app.dialog,
        Some(OpenDialog { closing: true, .. })
    ));

    // root's process: the fake refuses like the kernel would
    app.dialog = None;
    app.selected_pid = Some(1);
    app.apply(&ctx, Action::RequestKill { force: true }, 0.0);
    assert!(matches!(
        &app.dialog,
        Some(OpenDialog { state: DialogState::Confirm(c), .. }) if c.foreign && c.force
    ));
    assert_eq!(app.agent_blocked(), vec!["FORCE QUIT".to_string()]);
    app.apply(&ctx, Action::ConfirmDialog, 0.0);
    assert!(log_has(
        &app,
        "force quit // launchd [1] :: failed :: not permitted"
    ));
    assert_eq!(app.error_queue.len(), 1, "an ERROR card is queued");
}

#[test]
fn history_and_pressure_log() {
    let (_, mut app) = app();
    feed(&mut app, 5);
    assert_eq!(app.hist.cpu.len(), 4, "the first sample has no rates");
    assert!(app.hist.mem.last() > 0.5 && app.hist.mem.last() < 0.7);
    assert!(
        !log_has(&app, "pressure"),
        "normal pressure at start is not news"
    );
}

#[test]
fn interval_chips_change_the_sampler() {
    let (ctx, mut app) = app();
    app.apply(&ctx, Action::Interval(0), 0.0);
    assert_eq!(app.interval(), 1.0);
    assert!(log_has(&app, "sampling // every 1 s"));
    app.apply(&ctx, Action::Interval(9), 0.0);
    assert_eq!(app.interval(), 5.0);
}

#[test]
fn formatting() {
    assert_eq!(fmt_bytes(0), "0 B");
    assert_eq!(fmt_bytes(999), "999 B");
    assert_eq!(fmt_bytes(1_234), "1.23 KB");
    assert_eq!(fmt_bytes(12_345_678), "12.3 MB");
    assert_eq!(fmt_bytes(123_456_789_012), "123 GB");
    assert_eq!(fmt_rate(0.4), "0 B/s");
    assert_eq!(fmt_rate(2_500_000.0), "2.50 MB/s");
    assert_eq!(fmt_cpu_time(3.5), "0:03.50");
    assert_eq!(fmt_cpu_time(3725.25), "1:02:05.25");
    assert_eq!(fmt_span(59.0), "00:00:59");
    assert_eq!(fmt_span(3.0 * 86400.0 + 4000.0), "3d 01:06");
}

#[test]
fn agent_state_names_the_tab_selection_and_top_rows() {
    let (ctx, mut app) = app();
    feed(&mut app, 2);
    app.apply(&ctx, Action::Select(Some(0)), 0.0);
    let s = app.agent_state();
    assert!(s.contains("tab: cpu"), "{s}");
    assert!(s.contains("selected: rustc [2023]"), "{s}");
    assert!(
        s.contains("kernel_task [88] root: cpu ~"),
        "ps-sourced rows are marked: {s}"
    );
}
