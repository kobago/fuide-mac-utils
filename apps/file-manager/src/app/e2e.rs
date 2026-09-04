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

use super::tests::{fixture, goto_state};
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

/// Primary click at a position while holding `modifiers` (kittest's node helper cannot be
/// used here because selected row names also exist as inspector labels).
fn click_modifiers(h: &mut Harness<'static, Explorer>, pos: egui::Pos2, modifiers: Modifiers) {
    h.event(egui::Event::PointerMoved(pos));
    h.event(egui::Event::ModifiersChanged(modifiers));
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers,
        });
    }
    h.event(egui::Event::ModifiersChanged(Modifiers::default()));
    h.run_steps(3);
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
    let sel = h.state().lead.map(|i| h.state().entries[i].name.clone());
    assert_eq!(sel.as_deref(), Some("Music"));
    assert_eq!(
        h.get_by_role_and_label(Role::Button, "Music")
            .accesskit_node()
            .toggled(),
        Some(Toggled::True)
    );
}

#[test]
fn cmd_and_shift_clicks_build_a_multi_selection_and_cmd_a_takes_all() {
    let fx = fixture("e2e-multi");
    let mut h = harness(&fx.root);
    // selection is an index set; sort the names for stable comparisons
    let sel_names = |h: &Harness<'static, Explorer>| -> Vec<String> {
        let s = h.state();
        let mut v: Vec<String> = s
            .selected
            .iter()
            .map(|&i| s.entries[i].name.clone())
            .collect();
        v.sort();
        v
    };

    // rows are queried by role: selected names also appear in the inspector as labels
    let row = |h: &Harness<'static, Explorer>, name: &str| {
        h.get_by_role_and_label(Role::Button, name).rect().center()
    };

    h.get_by_label("A.md").click();
    h.run_steps(2);
    let pos = row(&h, "b.txt");
    click_modifiers(&mut h, pos, Modifiers::COMMAND);
    assert_eq!(sel_names(&h), ["A.md", "b.txt"]);
    assert_eq!(
        h.get_by_role_and_label(Role::Button, "A.md")
            .accesskit_node()
            .toggled(),
        Some(Toggled::True),
        "both rows report as selected"
    );

    // Cmd+click again removes the row from the selection
    let pos = row(&h, "b.txt");
    click_modifiers(&mut h, pos, Modifiers::COMMAND);
    assert_eq!(sel_names(&h), ["A.md"]);

    // Shift+click selects the anchor..target range in visible order (docs .. A.md)
    let pos = row(&h, "A.md");
    click_modifiers(&mut h, pos, Modifiers::default());
    let pos = row(&h, "docs");
    click_modifiers(&mut h, pos, Modifiers::SHIFT);
    assert_eq!(sel_names(&h), ["A.md", "Music", "docs"]);

    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(2);
    assert_eq!(h.state().selected.len(), 4);

    // with several rows selected, the inspector switches to the aggregate view
    h.get_by_label("4 ITEMS SELECTED");
}

#[test]
fn dragging_a_row_onto_a_directory_moves_the_file_there() {
    let fx = fixture("e2e-dnd");
    let mut h = harness(&fx.root);
    let from = h.get_by_label("A.md").rect().center();
    let to = h.get_by_label("docs").rect().center();

    h.drag_at(from);
    h.run_steps(1);
    // several small moves so egui's drag threshold trips before the drop
    for i in 1..=6 {
        let f = i as f32 / 6.0;
        h.hover_at(from.lerp(to, f));
        h.run_steps(1);
    }
    assert!(h.state().drag.is_some(), "row drag is active");
    h.drop_at(to);
    settle(&mut h);

    assert!(h.state().drag.is_none());
    assert!(fx.root.join("docs/A.md").exists(), "file moved into docs");
    assert!(!fx.root.join("A.md").exists());
    assert_eq!(names(&h), ["docs", "Music", "b.txt"]);
    let s = h.state();
    assert!(s
        .log
        .iter()
        .any(|e| e.text.contains("move // A.md -> docs :: done")));
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
    assert!(h.state().selected.is_empty());

    h.key_press(Key::Escape);
    settle(&mut h);
    assert_eq!(h.state().filter, "");
    assert_eq!(names(&h).len(), 4);
}

#[test]
fn dragging_the_log_divider_resizes_the_log_panel_and_saves_the_height() {
    let fx = fixture("e2e-splitter");
    let mut h = harness(&fx.root);
    let conf: PathBuf = fx.root.join("cfg").join("file-manager.conf");
    h.state_mut().persist_settings_to(conf.clone());
    let before = h.state().log_h;
    assert_eq!(before, LOG_H);

    let grip = h.get_by_label("LOG HEIGHT").rect().center();
    h.drag_at(grip);
    h.run_steps(1);
    for i in 1..=6 {
        h.hover_at(grip - egui::vec2(0.0, 10.0 * i as f32));
        h.run_steps(1);
    }
    h.drop_at(grip - egui::vec2(0.0, 60.0));
    h.run_steps(2);
    assert_eq!(h.state().log_h, before + 60.0);
    assert_eq!(
        Settings::load_from(&conf).unwrap().log_height,
        Some(before + 60.0)
    );

    // dragging far down stops at the minimum
    let grip = h.get_by_label("LOG HEIGHT").rect().center();
    h.drag_at(grip);
    h.run_steps(1);
    h.hover_at(grip + egui::vec2(0.0, 500.0));
    h.run_steps(1);
    h.drop_at(grip + egui::vec2(0.0, 500.0));
    h.run_steps(2);
    assert_eq!(h.state().log_h, LOG_MIN);
}

#[test]
fn clicking_the_log_title_chip_collapses_the_panel_and_saves() {
    let fx = fixture("e2e-log-toggle");
    let mut h = harness(&fx.root);
    let conf: PathBuf = fx.root.join("cfg").join("file-manager.conf");
    h.state_mut().persist_settings_to(conf.clone());
    assert!(h.state().settings.log_open);
    h.get_by_label("LOG HEIGHT"); // divider is live while open

    h.get_by_label("EVENT LOG").click();
    h.run_steps(2);
    assert!(!h.state().settings.log_open);
    assert!(!Settings::load_from(&conf).unwrap().log_open);
    assert_eq!(
        h.get_by_label("EVENT LOG").accesskit_node().toggled(),
        Some(Toggled::False)
    );
    assert!(
        h.query_by_label("LOG HEIGHT").is_none(),
        "no divider while collapsed"
    );

    h.get_by_label("EVENT LOG").click();
    h.run_steps(2);
    assert!(h.state().settings.log_open);
    assert!(Settings::load_from(&conf).unwrap().log_open);
    h.get_by_label("LOG HEIGHT");
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

    // window transparency: OPAQUE paints the ground at full alpha and is saved
    assert!(palette(&h.ctx).bg_deep.a() < 255);
    h.get_by_label("OPAQUE").click();
    h.run_steps(3);
    assert!(!h.state().settings.transparent);
    assert_eq!(palette(&h.ctx).bg_deep.a(), 255);
    assert!(!Settings::load_from(&conf).unwrap().transparent);
    h.get_by_label("TRANSLUCENT").click();
    h.run_steps(3);
    assert!(palette(&h.ctx).bg_deep.a() < 255);
    assert!(Settings::load_from(&conf).unwrap().transparent);

    h.key_press(Key::Escape);
    h.run_steps(3);
    assert!(!h.state().settings_win.is_open());
    assert!(
        h.query_by_label("CHAMFER").is_none(),
        "settings window is gone"
    );
}

#[test]
fn cmd_c_and_cmd_v_duplicate_a_row_and_cmd_x_moves_it() {
    let fx = fixture("e2e-clipboard");
    let mut h = harness(&fx.root);

    // egui-winit turns Cmd+C / X / V into these events (no key press reaches the app); the
    // paste event carries whatever text the OS clipboard holds, here the path Cmd+C wrote
    h.get_by_label("b.txt").click();
    h.run_steps(2);
    h.event(egui::Event::Copy);
    h.run_steps(2);
    assert!(h.state().clipboard.as_ref().is_some_and(|c| !c.cut));
    let os_text = fx.root.join("b.txt").display().to_string();
    assert_eq!(h.state().clipboard.as_ref().unwrap().text, os_text);
    h.event(egui::Event::Paste(os_text));
    settle(&mut h);
    assert_eq!(names(&h), ["docs", "Music", "A.md", "b copy.txt", "b.txt"]);
    assert_eq!(
        h.get_by_role_and_label(Role::Button, "b copy.txt")
            .accesskit_node()
            .toggled(),
        Some(Toggled::True)
    );

    // Cmd+X on A.md, Enter into docs, Cmd+V: the file moved and the clipboard is empty
    h.get_by_label("A.md").click();
    h.run_steps(2);
    h.event(egui::Event::Cut);
    h.run_steps(2);
    assert!(h.state().clipboard.as_ref().is_some_and(|c| c.cut));
    let os_text = h.state().clipboard.as_ref().unwrap().text.clone();
    h.get_by_label("docs").click();
    h.run_steps(2);
    h.key_press(Key::Enter);
    settle(&mut h);
    h.event(egui::Event::Paste(os_text));
    settle(&mut h);
    assert_eq!(names(&h), ["A.md", "inner.txt"]);
    assert!(!fx.root.join("A.md").exists());
    assert!(h.state().clipboard.is_none());
}

#[test]
fn cmd_shift_g_opens_the_go_to_dialog_tab_completes_and_enter_navigates() {
    let fx = fixture("e2e-goto");
    let mut h = harness(&fx.root);

    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::G);
    h.run_steps(3);
    let prefilled = format!("{}/", fx.root.display());
    assert_eq!(*goto_state(h.state_mut()).0, prefilled);

    // the field has focus with the cursor at the end: typing appends, Tab completes `do` -> `docs/`
    h.event(egui::Event::Text("do".into()));
    h.run_steps(2);
    h.key_press(Key::Tab);
    h.run_steps(3);
    assert_eq!(*goto_state(h.state_mut()).0, format!("{prefilled}docs/"));

    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(h.state().cwd, fx.root.join("docs"));
    h.run_steps(20); // dialog fade-out
    assert!(h.state().dialog.is_none());
    h.get_by_label("inner.txt");

    // clicking the current breadcrumb also opens it; Escape closes without moving
    h.get_by_label("DOCS (go to path)").click();
    h.run_steps(3);
    assert!(h.state().dialog.is_some());
    h.key_press(Key::Escape);
    h.run_steps(20);
    assert!(h.state().dialog.is_none());
    assert_eq!(h.state().cwd, fx.root.join("docs"));
}
