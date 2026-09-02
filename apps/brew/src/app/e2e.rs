//! End-to-end: the real `BrewApp` driven through the accessibility tree with `egui_kittest`,
//! against `fixtures/fake-brew.sh` (`FUIDE_BREW_BIN`). Clicks by label, key presses and typed
//! text go in; the tree and the app state are checked. See the file manager's `e2e.rs` for the
//! harness conventions (fixed `run_steps`, embedded settings window).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::accesskit::{Role, Toggled};
use egui::{Key, Modifiers, Vec2};
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::Harness;
use fuide::agent::{Command, Reply};

use super::tests::use_fake_brew;
use super::*;

/// App on the fake brew with the inventory already read (like `new()` does at start-up).
fn harness() -> Harness<'static, BrewApp> {
    use_fake_brew();
    let mut h = Harness::builder()
        .with_size(Vec2::new(1280.0, 800.0))
        .with_step_dt(1.0 / 60.0)
        .build_eframe(|cc| {
            let mut app = BrewApp::with_context(&cc.egui_ctx, Settings::default());
            app.brew.fetch_inventory(cc.egui_ctx.clone());
            app
        });
    pump(&mut h);
    h
}

/// Run frames until no brew worker is in flight and the rows are rebuilt.
fn pump(h: &mut Harness<'static, BrewApp>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        h.run_steps(1);
        let s = h.state();
        if !s.brew.fetching()
            && s.brew.running().is_none()
            && s.search_pending.is_none()
            && !s.dirty
        {
            break;
        }
        assert!(Instant::now() < deadline, "brew worker did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    h.run_steps(1);
}

fn row_names(h: &Harness<'static, BrewApp>) -> Vec<String> {
    let s = h.state();
    s.rows.iter().map(|&i| s.source()[i].name.clone()).collect()
}

fn log_has(h: &Harness<'static, BrewApp>, needle: &str) -> bool {
    h.state().log.iter().any(|e| e.text.contains(needle))
}

#[test]
fn views_switch_by_tab_click_and_by_cmd_number() {
    let mut h = harness();
    assert_eq!(row_names(&h).len(), 5);
    h.get_by_label("INSTALLED  5");

    h.get_by_label("OUTDATED  3").click();
    pump(&mut h);
    assert_eq!(row_names(&h), ["cmake", "iterm2", "ripgrep"]);
    assert_eq!(
        h.get_by_label("OUTDATED  3").accesskit_node().toggled(),
        Some(Toggled::True)
    );

    h.key_press_modifiers(Modifiers::COMMAND, Key::Num3);
    pump(&mut h);
    assert_eq!(row_names(&h), ["iterm2"]);
    h.get_by_label("iterm2");
    assert!(h.query_by_label("jq").is_none());

    // arrow selects the first row; the inspector shows its HOMEPAGE button
    h.key_press(Key::ArrowDown);
    pump(&mut h);
    assert_eq!(h.state().selected_package().unwrap().name, "iterm2");
    h.get_by_label("HOMEPAGE");
}

#[test]
fn upgrade_all_asks_runs_the_fake_brew_and_shows_a_success_card() {
    let mut h = harness();
    h.get_by_label("UPGRADE ALL  2").click();
    h.run_steps(3);
    assert!(matches!(
        h.state().dialog,
        Some(OpenDialog {
            state: DialogState::Confirm(_),
            ..
        })
    ));
    // the dialog's own verb button (the toolbar one carries the count)
    h.get_by_label("UPGRADE ALL").click();
    h.run_steps(2);
    assert!(log_has(&h, "$ brew upgrade"));
    pump(&mut h);
    assert!(
        log_has(&h, "==> upgrade"),
        "streamed output reached the log"
    );

    // SUCCESS card, acknowledged by its button. The card fades in over 0.15 s and its
    // widgets are disabled while invisible (opacity 0), so wait until the button is live.
    let deadline = Instant::now() + Duration::from_secs(5);
    while h
        .query_by_label("ACKNOWLEDGE")
        .is_none_or(|n| n.accesskit_node().is_disabled())
    {
        assert!(Instant::now() < deadline, "no live success card");
        h.run_steps(1);
    }
    assert!(matches!(
        h.state().dialog,
        Some(OpenDialog {
            state: DialogState::Notice { success: true, .. },
            ..
        })
    ));
    h.get_by_label("ACKNOWLEDGE").click();
    h.run_steps(15); // fade-out
    let left = h.state().dialog.as_ref().map(|d| {
        let kind = match &d.state {
            DialogState::Confirm(c) => format!("confirm {}", c.label),
            DialogState::Notice { success, line } => format!("notice {success} {line}"),
        };
        format!("{kind} closing={}", d.closing)
    });
    assert_eq!(left, None, "dialog still open");
    assert_eq!(
        row_names(&h).len(),
        5,
        "inventory re-read after the command"
    );
}

#[test]
fn search_view_takes_a_typed_query_on_enter_and_marks_installed_hits() {
    let mut h = harness();
    h.key_press_modifiers(Modifiers::COMMAND, Key::Num4);
    pump(&mut h);
    assert_eq!(h.state().view, View::Search);
    h.get_by_label("SEARCH");

    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(2);
    h.event(egui::Event::Text("ripgrep".into()));
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(2);
    assert_eq!(h.state().search_query, "ripgrep");
    pump(&mut h);
    assert!(log_has(&h, "search // ripgrep ::"));
    let rg = h
        .state()
        .search_results
        .iter()
        .find(|p| p.name == "ripgrep")
        .expect("hit")
        .clone();
    assert_eq!(rg.installed_version(), "14.0.0");
    assert_eq!(
        h.state().search_results.len(),
        5,
        "4 formulae + 1 cask, no duplicates"
    );
    h.get_by_role_and_label(Role::Button, "ripgrep");
}

#[test]
fn cmd_comma_opens_settings_and_a_palette_click_is_saved() {
    let mut h = harness();
    let dir = std::env::temp_dir().join(format!("fuide-brew-e2e-settings-{}", std::process::id()));
    let conf: PathBuf = dir.join("brew.conf");
    h.state_mut().persist_settings_to(conf.clone());

    h.key_press_modifiers(Modifiers::COMMAND, Key::Comma);
    h.run_steps(2);
    assert!(h.state().settings_win.is_open());
    h.get_by_label("GREEN  PHOSPHOR TERMINAL").click();
    h.run_steps(3);
    assert_eq!(h.state().settings.palette, PaletteKind::Green);
    assert_eq!(
        Settings::load_from(&conf).map(|s| s.palette),
        Some(PaletteKind::Green)
    );
    assert_eq!(
        h.query_all_by_label("CLOSE WINDOW").count(),
        2,
        "main shell + settings shell"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clicking_the_log_title_chip_collapses_the_panel_and_saves() {
    let mut h = harness();
    let dir =
        std::env::temp_dir().join(format!("fuide-brew-e2e-log-toggle-{}", std::process::id()));
    let conf: PathBuf = dir.join("brew.conf");
    h.state_mut().persist_settings_to(conf.clone());
    h.get_by_label("LOG HEIGHT"); // divider is live while open

    h.get_by_label("BREW OUTPUT").click();
    h.run_steps(2);
    assert!(!h.state().settings.log_open);
    assert!(!Settings::load_from(&conf).unwrap().log_open);
    assert!(
        h.query_by_label("LOG HEIGHT").is_none(),
        "no divider while collapsed"
    );

    h.get_by_label("BREW OUTPUT").click();
    h.run_steps(2);
    assert!(h.state().settings.log_open);
    assert!(Settings::load_from(&conf).unwrap().log_open);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------- agent

/// Queue an agent command and step frames until it answers (the cursor glide, the injected
/// click and the settle take a few dozen frames).
fn agent(h: &mut Harness<'static, BrewApp>, cmd: Command) -> Reply {
    let rx = h.state().agent.submit(cmd);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        h.run_steps(1);
        if let Ok(r) = rx.try_recv() {
            return r;
        }
        assert!(Instant::now() < deadline, "the agent did not answer");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn agent_text(h: &mut Harness<'static, BrewApp>, cmd: Command) -> String {
    match agent(h, cmd) {
        Reply::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    }
}

fn click(label: &str) -> Command {
    Command::Click {
        label: label.into(),
        nth: 1,
    }
}

#[test]
fn agent_drives_the_views_and_search() {
    let mut h = harness();
    h.state_mut().agent.enable_without_server();
    h.run_steps(2);

    let t = agent_text(&mut h, Command::Observe);
    assert!(t.contains("view: INSTALLED :: rows: 5"), "{t}");
    assert!(t.contains("[item] INSTALLED  5 (selected)"), "{t}");
    assert!(t.contains("[button] UPGRADE ALL  2"), "{t}");
    assert!(t.contains("[input] FILTER = \"\""), "{t}");

    let t = agent_text(&mut h, click("OUTDATED  3"));
    pump(&mut h);
    assert_eq!(row_names(&h), ["cmake", "iterm2", "ripgrep"]);
    assert!(t.contains("view: OUTDATED"), "{t}");
    assert_eq!(h.state().agent.last_action(), Some("CLICK ▸ OUTDATED  3"));

    // rows are items; clicking one selects it and the inspector shows it
    let t = agent_text(&mut h, click("iterm2"));
    assert!(t.contains("selected: iterm2 (cask)"), "{t}");

    // shortcut + typed filter, like a person would
    agent_text(
        &mut h,
        Command::Key {
            combo: "cmd+1".into(),
            repeat: 1,
        },
    );
    pump(&mut h);
    assert_eq!(h.state().view, View::Installed);
    let t = agent_text(
        &mut h,
        Command::Type {
            text: "rip".into(),
            label: Some("FILTER".into()),
            submit: false,
        },
    );
    pump(&mut h);
    assert_eq!(row_names(&h), ["ripgrep"]);
    assert!(t.contains("[input] FILTER = \"rip\""), "{t}");
}

#[test]
fn agent_is_kept_out_of_confirmations_unless_allowed() {
    let mut h = harness();
    h.state_mut().agent.enable_without_server();
    h.run_steps(2);

    agent_text(&mut h, click("UPGRADE ALL  2"));
    assert!(matches!(
        h.state().dialog,
        Some(OpenDialog {
            state: DialogState::Confirm(_),
            ..
        })
    ));
    // only the dialog is addressable now, and its verb is reserved for the human
    let t = agent_text(&mut h, Command::Observe);
    assert!(t.contains("dialog: CONFIRM"), "{t}");
    assert!(t.contains("a dialog is open"), "{t}");
    assert!(t.contains("[button] UPGRADE ALL (human only)"), "{t}");
    assert!(!t.contains("[item] INSTALLED"), "{t}");
    for cmd in [
        click("UPGRADE ALL"),
        Command::Key {
            combo: "enter".into(),
            repeat: 1,
        },
    ] {
        match agent(&mut h, cmd) {
            Reply::Error(e) => assert!(e.contains("reserved for the human"), "{e}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    assert!(!log_has(&h, "$ brew upgrade"));

    // cancelling is fine
    agent_text(&mut h, click("CANCEL"));
    h.run_steps(15); // fade-out
    assert!(h.state().dialog.is_none());

    // with the setting on, the agent may confirm: the fake brew runs
    h.state_mut().settings.agent_confirm = true;
    agent_text(&mut h, click("UPGRADE ALL  2"));
    let t = agent_text(&mut h, click("UPGRADE ALL"));
    assert!(!t.contains("human only"), "{t}");
    pump(&mut h);
    assert!(log_has(&h, "$ brew upgrade"));
    assert!(log_has(&h, "==> upgrade"));
}
