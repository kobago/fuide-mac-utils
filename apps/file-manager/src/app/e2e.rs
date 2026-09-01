//! End-to-end: the real `Explorer` (its `eframe::App::ui`, every frame) driven through the
//! accessibility tree with `egui_kittest` — clicks by label, key presses, typed text — and
//! checked through both the tree and the app state. No window is opened; the settings
//! window becomes an embedded `egui::Window` (egui's fallback when a backend has no viewports).
//!
//! The shell animates and asks for a repaint every frame, so tests advance a fixed number of
//! frames (`run_steps`) rather than waiting for quiescence.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::accesskit::{Role, Toggled};
use egui::{Key, Modifiers, Vec2};
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::Harness;
use fuide::palette;

use super::tests::fixture;
use super::*;

fn harness(root: &Path) -> Harness<'static, Explorer> {
    let root = root.to_path_buf();
    let mut h = Harness::builder()
        .with_size(Vec2::new(1280.0, 800.0))
        .with_step_dt(1.0 / 60.0)
        .build_eframe(move |cc| Explorer::with_context(&cc.egui_ctx, root, Settings::default()));
    settle(&mut h);
    h
}

/// Run frames until the loader / file ops are idle and the view is rebuilt.
fn settle(h: &mut Harness<'static, Explorer>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        h.run_steps(1);
        let s = h.state();
        if s.pending.is_none() && !s.ops.busy() && !s.dirty {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "loader / file ops did not finish"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    h.run_steps(1);
}

fn names(h: &Harness<'static, Explorer>) -> Vec<String> {
    let s = h.state();
    s.view.iter().map(|&i| s.entries[i].name.clone()).collect()
}

#[test]
fn rows_select_by_click_and_the_keyboard_walks_the_tree() {
    let fx = fixture("e2e-nav");
    let mut h = harness(&fx.root);
    assert_eq!(names(&h), ["docs", "Music", "A.md", "b.txt"]);

    // click a row, Enter opens the directory (once selected, the inspector also shows the
    // name as a plain label, so rows are asked for by role)
    h.get_by_label("docs").click();
    h.run_steps(2);
    assert_eq!(
        h.get_by_role_and_label(Role::Button, "docs")
            .accesskit_node()
            .toggled(),
        Some(Toggled::True)
    );
    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(h.state().cwd, fx.root.join("docs"));
    h.get_by_label("inner.txt");

    // Backspace goes up, Cmd+[ goes back into docs, Cmd+] forward again
    h.key_press(Key::Backspace);
    settle(&mut h);
    assert_eq!(h.state().cwd, fx.root);
    h.key_press_modifiers(Modifiers::COMMAND, Key::OpenBracket);
    settle(&mut h);
    assert_eq!(h.state().cwd, fx.root.join("docs"));
    h.key_press_modifiers(Modifiers::COMMAND, Key::CloseBracket);
    settle(&mut h);
    assert_eq!(h.state().cwd, fx.root);

    // arrow keys move the selection through the visible rows
    h.key_press(Key::ArrowDown);
    h.run_steps(2);
    h.key_press(Key::ArrowDown);
    h.run_steps(2);
    let sel = h
        .state()
        .selected
        .map(|i| h.state().entries[i].name.clone());
    assert_eq!(sel.as_deref(), Some("Music"));
    assert_eq!(
        h.get_by_role_and_label(Role::Button, "Music")
            .accesskit_node()
            .toggled(),
        Some(Toggled::True)
    );
}

#[test]
fn cmd_f_focuses_the_filter_typing_narrows_the_list_and_escape_clears_it() {
    let fx = fixture("e2e-filter");
    let mut h = harness(&fx.root);

    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(2);
    h.event(egui::Event::Text("mD".into()));
    settle(&mut h);
    assert_eq!(h.state().filter, "mD");
    assert_eq!(names(&h), ["A.md"]);
    h.get_by_label("A.md");
    assert!(
        h.query_by_label("b.txt").is_none(),
        "filtered rows are gone from the tree"
    );

    // the text field owns the keyboard: arrows must not move the selection
    h.key_press(Key::ArrowDown);
    h.run_steps(2);
    assert_eq!(h.state().selected, None);

    h.key_press(Key::Escape);
    settle(&mut h);
    assert_eq!(h.state().filter, "");
    assert_eq!(names(&h).len(), 4);
}

#[test]
fn the_gear_opens_settings_and_a_palette_click_restyles_and_saves() {
    let fx = fixture("e2e-settings");
    let mut h = harness(&fx.root);
    let conf: PathBuf = fx.root.join("cfg").join("file-manager.conf");
    h.state_mut().persist_settings_to(conf.clone());

    h.get_by_label("SETTINGS").click();
    h.run_steps(2);
    assert!(h.state().settings_win.is_open());
    h.get_by_label("AMBER  INDUSTRIAL / REACTOR").click();
    h.run_steps(3);
    assert_eq!(h.state().settings.palette, PaletteKind::Amber);
    assert_eq!(palette(&h.ctx).accent, Palette::amber().accent);
    assert_eq!(
        Settings::load_from(&conf).map(|s| s.palette),
        Some(PaletteKind::Amber)
    );
    assert_eq!(
        h.get_by_label("AMBER  INDUSTRIAL / REACTOR")
            .accesskit_node()
            .toggled(),
        Some(Toggled::True)
    );

    h.get_by_label("CHAMFER").click();
    h.run_steps(3);
    assert!(Settings::load_from(&conf).unwrap().chamfer);

    h.key_press(Key::Escape);
    h.run_steps(3);
    assert!(!h.state().settings_win.is_open());
    assert!(
        h.query_by_label("CHAMFER").is_none(),
        "settings window is gone"
    );
}
