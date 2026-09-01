//! State-machine tests for the brew console. Two flavours:
//! - pure: brew messages are injected (`Brew::inject`) and no process is started
//! - with `fixtures/fake-brew.sh`: `FUIDE_BREW_BIN` points at a shell script that answers
//!   like brew, so the real worker threads / streaming runner are exercised end to end
//!   without touching Homebrew

use std::path::Path;
use std::sync::Once;
use std::time::{Duration, Instant};

use super::*;
use crate::brew::{parse_info_json, Msg};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");

/// Route every `brew` call to the fake script (process-wide; all tests use the same value).
fn use_fake_brew() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let script = Path::new(FIXTURES).join("fake-brew.sh");
        // SAFETY: set once before any worker thread is spawned; every test sets the same value.
        unsafe { std::env::set_var("FUIDE_BREW_BIN", &script) };
    });
}

fn app() -> (egui::Context, BrewApp) {
    let ctx = egui::Context::default();
    let app = BrewApp::with_context(&ctx, Settings::default());
    (ctx, app)
}

fn fixture_packages() -> Vec<Package> {
    let json = std::fs::read(Path::new(FIXTURES).join("info-installed.json")).unwrap();
    parse_info_json(&json).unwrap()
}

/// Deliver an inventory without running brew.
fn with_inventory(ctx: &egui::Context, app: &mut BrewApp) {
    app.brew.inject(Msg::Inventory(Ok(fixture_packages()), 1.0));
    app.poll(ctx, 0.0);
    app.rebuild_rows();
}

/// Pump brew messages until no fetch / command is in flight.
fn pump(ctx: &egui::Context, app: &mut BrewApp) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        app.poll(ctx, 0.0);
        if !app.brew.fetching() && app.brew.running().is_none() && app.search_pending.is_none() {
            break;
        }
        assert!(Instant::now() < deadline, "brew worker did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    // a finished command re-fetches the inventory; wait for that too
    if app.brew.fetching() {
        pump(ctx, app);
    }
    if app.dirty {
        app.rebuild_rows();
    }
}

/// Poll until `done` holds (for fire-and-forget workers such as `fetch_system`).
fn wait_until(ctx: &egui::Context, app: &mut BrewApp, done: impl Fn(&BrewApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(app) {
        assert!(Instant::now() < deadline, "condition not met in time");
        std::thread::sleep(Duration::from_millis(5));
        app.poll(ctx, 0.0);
    }
}

fn row_names(app: &BrewApp) -> Vec<&str> {
    app.rows
        .iter()
        .map(|&i| app.source()[i].name.as_str())
        .collect()
}

fn log_has(app: &BrewApp, needle: &str) -> bool {
    app.log.iter().any(|e| e.text.contains(needle))
}

fn confirm(app: &BrewApp) -> &Confirm {
    match &app.dialog {
        Some(OpenDialog {
            state: DialogState::Confirm(c),
            ..
        }) => c,
        _ => panic!("no confirm dialog"),
    }
}

// ------------------------------------------------------------------ pure

#[test]
fn inventory_feeds_the_views_and_the_filter_matches_name_or_description() {
    let (ctx, mut app) = app();
    with_inventory(&ctx, &mut app);
    assert_eq!(
        row_names(&app),
        ["cmake", "iterm2", "jq", "pcre2", "ripgrep"]
    );
    assert!(log_has(
        &app,
        "inventory :: 4 formulae, 1 casks, 3 outdated"
    ));

    app.apply(&ctx, Action::SetView(View::Outdated), 0.0);
    app.rebuild_rows();
    assert_eq!(row_names(&app), ["cmake", "iterm2", "ripgrep"]);

    app.apply(&ctx, Action::SetView(View::Casks), 0.0);
    app.rebuild_rows();
    assert_eq!(row_names(&app), ["iterm2"]);

    app.apply(&ctx, Action::SetView(View::Installed), 0.0);
    app.filter = "JSON processor".into();
    app.rebuild_rows();
    assert_eq!(
        row_names(&app),
        ["jq"],
        "description matches, case-insensitive"
    );
}

#[test]
fn sorting_by_status_keeps_the_selection_on_the_same_package() {
    let (ctx, mut app) = app();
    with_inventory(&ctx, &mut app);
    let jq_row = row_names(&app).iter().position(|n| *n == "jq").unwrap();
    app.apply(&ctx, Action::Select(Some(jq_row)), 0.0);
    assert_eq!(app.selected_package().unwrap().name, "jq");

    app.table.sort_col = 4; // STATUS: Outdated < Pinned < Current < Dependency
    app.rebuild_rows();
    assert_eq!(
        row_names(&app),
        ["iterm2", "ripgrep", "cmake", "jq", "pcre2"]
    );
    assert_eq!(app.selected_package().unwrap().name, "jq");

    app.table.sort_desc = true;
    app.rebuild_rows();
    assert_eq!(row_names(&app)[0], "pcre2");
    assert_eq!(app.selected_package().unwrap().name, "jq");
}

#[test]
fn mutating_actions_open_a_confirmation_with_the_exact_brew_arguments() {
    let (ctx, mut app) = app();
    with_inventory(&ctx, &mut app);

    app.apply(&ctx, Action::Upgrade("ripgrep".into(), Kind::Formula), 0.0);
    let c = confirm(&app);
    assert_eq!(c.args, ["upgrade", "--formula", "ripgrep"]);
    assert!(!c.danger);
    assert_eq!(c.label, "upgrade // ripgrep");

    app.apply(&ctx, Action::Uninstall("iterm2".into(), Kind::Cask), 0.0);
    let c = confirm(&app);
    assert_eq!(c.args, ["uninstall", "--cask", "iterm2"]);
    assert!(c.danger, "uninstall is the destructive one");

    app.apply(&ctx, Action::Install("fd".into(), Kind::Formula), 0.0);
    assert_eq!(confirm(&app).args, ["install", "--formula", "fd"]);

    app.apply(&ctx, Action::UpgradeAll, 0.0);
    let c = confirm(&app);
    assert_eq!(c.args, ["upgrade"]);
    assert_eq!(c.line, "2 outdated packages", "pinned cmake is not counted");

    app.apply(&ctx, Action::CloseDialog, 0.0);
    assert!(app.dialog.as_ref().unwrap().closing);
}

#[test]
fn search_needs_at_least_two_characters() {
    let (ctx, mut app) = app();
    app.search_query = " r ".into();
    app.apply(&ctx, Action::Search, 0.0);
    assert!(app.search_pending.is_none());
    assert!(!log_has(&app, "search //"));
}

#[test]
fn a_failed_inventory_marks_the_link_and_queues_an_error_card() {
    let (ctx, mut app) = app();
    app.brew
        .inject(Msg::Inventory(Err("brew not found".into()), 0.0));
    app.poll(&ctx, 0.0);
    assert_eq!(app.last_error.as_deref(), Some("brew not found"));
    assert_eq!(
        app.notice_queue.front(),
        Some(&(false, "inventory".to_string()))
    );
}

// ------------------------------------------------------------------ fake brew

#[test]
fn refresh_reads_the_inventory_and_system_info_from_brew() {
    use_fake_brew();
    let (ctx, mut app) = app();
    app.apply(&ctx, Action::Refresh, 0.0);
    pump(&ctx, &mut app);
    assert_eq!(row_names(&app).len(), 5);
    wait_until(&ctx, &mut app, |a| !a.system.version.is_empty());
    assert_eq!(app.system.version, "4.9.9");
    assert_eq!(
        app.system.prefix,
        PathBuf::from("/tmp/fuide-fake-brew-prefix")
    );
    assert!(app.last_error.is_none());
}

#[test]
fn confirming_runs_brew_streams_its_output_and_reports_success_then_refetches() {
    use_fake_brew();
    let (ctx, mut app) = app();
    with_inventory(&ctx, &mut app);
    app.apply(&ctx, Action::Upgrade("ripgrep".into(), Kind::Formula), 0.0);
    app.apply(&ctx, Action::ConfirmDialog, 0.0);
    assert!(app.dialog.as_ref().unwrap().closing);
    assert_eq!(app.brew.running(), Some("upgrade // ripgrep"));
    assert!(log_has(&app, "$ brew upgrade --formula ripgrep"));

    // a second command while one runs is refused with an error card
    app.apply(&ctx, Action::Update, 0.0);
    assert_eq!(
        app.notice_queue.back(),
        Some(&(false, "update".to_string()))
    );

    pump(&ctx, &mut app);
    assert!(
        log_has(&app, "==> upgrade --formula ripgrep"),
        "stdout streamed into the log"
    );
    assert!(log_has(
        &app,
        "upgrade // ripgrep :: brew upgrade --formula ripgrep done in"
    ));
    assert!(app
        .notice_queue
        .contains(&(true, "upgrade // ripgrep".to_string())));
    assert_eq!(
        row_names(&app).len(),
        5,
        "inventory re-read after the command"
    );
}

#[test]
fn a_failing_command_reports_an_error_card_with_the_stderr_line_in_the_log() {
    use_fake_brew();
    let (ctx, mut app) = app();
    app.apply(&ctx, Action::Uninstall("boom".into(), Kind::Formula), 0.0);
    app.apply(&ctx, Action::ConfirmDialog, 0.0);
    pump(&ctx, &mut app);
    assert!(app
        .notice_queue
        .contains(&(false, "uninstall // boom".to_string())));
    assert!(app
        .log
        .iter()
        .any(|e| e.text == "Error: boom" && matches!(e.level, Level::Danger)));
    assert!(log_has(&app, "uninstall // boom :: failed :: exit code 1"));
}

#[test]
fn search_results_are_marked_with_local_install_state() {
    use_fake_brew();
    let (ctx, mut app) = app();
    with_inventory(&ctx, &mut app);
    app.apply(&ctx, Action::SetView(View::Search), 0.0);
    app.search_query = "ripgrep".into();
    app.apply(&ctx, Action::Search, 0.0);
    assert_eq!(app.search_pending.as_deref(), Some("ripgrep"));
    pump(&ctx, &mut app);
    assert!(app.search_pending.is_none());
    let rg = app
        .search_results
        .iter()
        .find(|p| p.name == "ripgrep")
        .expect("search hit");
    assert_eq!(
        rg.installed_version(),
        "14.0.0",
        "local install state copied onto the hit"
    );
    assert!(rg.outdated);
    assert!(log_has(&app, "search // ripgrep ::"));
    assert!(!row_names(&app).is_empty());
}

#[test]
fn settings_changes_are_saved_where_asked() {
    let (_ctx, mut app) = app();
    let dir = std::env::temp_dir().join(format!("fuide-brew-app-settings-{}", std::process::id()));
    let conf = dir.join("brew.conf");
    app.persist_settings_to(conf.clone());
    app.settings.compact = true;
    app.settings_changed(1.0);
    assert!(Settings::load_from(&conf).unwrap().compact);
    assert!(log_has(
        &app,
        "settings // palette cyan :: square :: compact"
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
