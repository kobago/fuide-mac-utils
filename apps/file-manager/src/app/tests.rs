//! State-machine tests: `Action`s applied to an `Explorer` built on a bare `egui::Context`,
//! against a throw-away directory. No window, no Finder, no clipboard.
//!
//! The directory loader and file operations run on worker threads (as in the app), so tests
//! pump `poll_loader` / `poll_ops` until idle — the same thing `ui()` does every frame.

use std::path::Path;
use std::time::{Duration, Instant};

use fuide::palette;

use super::*;

pub(super) struct Fixture {
    pub(super) root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `docs/inner.txt`, `Music/`, `b.txt`, `A.md`, `.hidden`
pub(super) fn fixture(name: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "fuide-file-manager-app-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::create_dir_all(root.join("Music")).unwrap();
    std::fs::write(root.join("docs/inner.txt"), "i").unwrap();
    std::fs::write(root.join("b.txt"), "bb").unwrap();
    std::fs::write(root.join("A.md"), "a").unwrap();
    std::fs::write(root.join(".hidden"), "").unwrap();
    Fixture { root }
}

fn app(root: &Path) -> (egui::Context, Explorer) {
    let ctx = egui::Context::default();
    let mut app = Explorer::with_context(&ctx, root.to_path_buf(), Settings::default());
    settle(&ctx, &mut app);
    (ctx, app)
}

/// Pump background work until the loader and file ops are idle, then rebuild the view.
fn settle(ctx: &egui::Context, app: &mut Explorer) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        app.poll_ops(ctx, 0.0);
        app.poll_loader(0.0);
        if app.pending.is_none() && !app.ops.busy() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "loader / file ops did not finish"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    if app.dirty {
        app.rebuild_view();
    }
}

fn names(app: &Explorer) -> Vec<&str> {
    app.view
        .iter()
        .map(|&i| app.entries[i].name.as_str())
        .collect()
}

fn idx_of(app: &Explorer, name: &str) -> usize {
    app.entries
        .iter()
        .position(|e| e.name == name)
        .unwrap_or_else(|| panic!("no entry {name}"))
}

fn log_has(app: &Explorer, needle: &str) -> bool {
    app.log.iter().any(|e| e.text.contains(needle))
}

fn rename_name(app: &mut Explorer) -> &mut String {
    match &mut app.dialog {
        Some(OpenDialog {
            state: DialogState::Rename { name, .. },
            ..
        }) => name,
        _ => panic!("rename dialog is not open"),
    }
}

#[test]
fn lists_directories_first_sorted_case_insensitively_without_hidden_files() {
    let fx = fixture("list");
    let (_ctx, app) = app(&fx.root);
    assert_eq!(names(&app), ["docs", "Music", "A.md", "b.txt"]);
    assert_eq!(app.entries.len(), 5, "hidden entry is read but not shown");
    assert!(app.last_error.is_none());
    assert_eq!(app.history, vec![fx.root.clone()]);
    assert!(log_has(&app, "scan complete :: 5 items (1 hidden)"));
}

#[test]
fn hidden_toggle_and_filter_rebuild_the_view_and_drop_a_filtered_selection() {
    let fx = fixture("filter");
    let (_ctx, mut app) = app(&fx.root);
    app.show_hidden = true;
    app.dirty = true;
    app.rebuild_view();
    assert_eq!(names(&app), ["docs", "Music", ".hidden", "A.md", "b.txt"]);

    app.select_one(Some(idx_of(&app, "b.txt")));
    app.filter = "MD".into(); // case-insensitive
    app.rebuild_view();
    assert_eq!(names(&app), ["A.md"]);
    assert!(
        app.selected.is_empty(),
        "selection outside the view is cleared"
    );
}

#[test]
fn toggle_range_and_select_all_follow_finder_conventions() {
    let fx = fixture("multi");
    let (ctx, mut app) = app(&fx.root);
    // view order: docs, Music, A.md, b.txt
    let (docs, music, amd, btxt) = (
        idx_of(&app, "docs"),
        idx_of(&app, "Music"),
        idx_of(&app, "A.md"),
        idx_of(&app, "b.txt"),
    );

    app.apply(&ctx, Action::Select(Some(amd)), 0.0);
    app.apply(&ctx, Action::SelectToggle(btxt), 0.0);
    assert_eq!(app.selected, BTreeSet::from([amd, btxt]));
    assert_eq!(app.single_selected(), None);

    // Cmd+click again removes the entry
    app.apply(&ctx, Action::SelectToggle(amd), 0.0);
    assert_eq!(app.selected, BTreeSet::from([btxt]));

    // Shift replaces the selection with the anchor..target range in visible order
    app.apply(&ctx, Action::Select(Some(amd)), 0.0);
    app.apply(&ctx, Action::SelectRange(docs), 0.0);
    assert_eq!(app.selected, BTreeSet::from([docs, music, amd]));

    app.apply(&ctx, Action::Select(Some(music)), 0.0);
    app.apply(&ctx, Action::SelectAll, 0.0);
    assert_eq!(app.selected.len(), 4);

    // arrows walk from the lead
    assert_eq!(app.lead, Some(music));
}

#[test]
fn move_to_relocates_selected_paths_and_skips_noops() {
    let fx = fixture("move-to");
    let (ctx, mut app) = app(&fx.root);
    let docs = fx.root.join("docs");
    app.apply(
        &ctx,
        Action::MoveTo {
            dest: docs.clone(),
            paths: vec![
                fx.root.join("A.md"),
                fx.root.join("b.txt"),
                docs.join("inner.txt"), // already there: filtered out
            ],
        },
        0.0,
    );
    settle(&ctx, &mut app);
    assert!(docs.join("A.md").exists() && docs.join("b.txt").exists());
    assert!(!fx.root.join("A.md").exists());
    assert_eq!(names(&app), ["docs", "Music"]);
    assert!(log_has(&app, "move // 2 items -> docs :: done"));

    // moving a directory into itself is filtered out before the worker runs
    app.apply(
        &ctx,
        Action::MoveTo {
            dest: docs.clone(),
            paths: vec![docs.clone()],
        },
        0.0,
    );
    settle(&ctx, &mut app);
    assert!(docs.exists());
    assert!(!log_has(&app, "move // docs"));
}

#[test]
fn confirm_delete_removes_every_selected_entry() {
    let fx = fixture("multi-delete");
    let (ctx, mut app) = app(&fx.root);
    let (amd, btxt) = (idx_of(&app, "A.md"), idx_of(&app, "b.txt"));
    app.apply(&ctx, Action::Select(Some(amd)), 0.0);
    app.apply(&ctx, Action::SelectToggle(btxt), 0.0);
    app.apply(&ctx, Action::OpenDelete(true), 0.0);
    app.apply(&ctx, Action::ConfirmDelete, 0.0);
    settle(&ctx, &mut app);
    assert_eq!(names(&app), ["docs", "Music"]);
    assert!(!fx.root.join("A.md").exists() && !fx.root.join("b.txt").exists());
    assert!(log_has(&app, "delete // 2 items :: done"));
}

#[test]
fn sort_toggles_direction_on_the_same_key_and_keeps_directories_first() {
    let fx = fixture("sort");
    let (ctx, mut app) = app(&fx.root);
    app.apply(&ctx, Action::Sort(SortKey::Name), 0.0);
    assert!(app.sort_desc, "same key again flips the direction");
    settle(&ctx, &mut app);
    assert_eq!(names(&app), ["Music", "docs", "b.txt", "A.md"]);

    app.apply(&ctx, Action::Sort(SortKey::Size), 0.0);
    assert!(!app.sort_desc, "new key starts ascending");
    settle(&ctx, &mut app);
    let n = names(&app);
    assert_eq!(&n[2..], ["A.md", "b.txt"], "files by size: 1 byte before 2");
    assert!(n[..2].contains(&"docs") && n[..2].contains(&"Music"));
}

#[test]
fn history_back_forward_up_and_truncation() {
    let fx = fixture("history");
    let (ctx, mut app) = app(&fx.root);
    let docs = fx.root.join("docs");
    let music = fx.root.join("Music");

    app.apply(&ctx, Action::Navigate(docs.clone()), 0.0);
    settle(&ctx, &mut app);
    assert_eq!(app.cwd, docs);
    assert_eq!(names(&app), ["inner.txt"]);
    assert_eq!((app.history.len(), app.hist_pos), (2, 1));

    app.apply(&ctx, Action::Back, 0.0);
    settle(&ctx, &mut app);
    assert_eq!((app.cwd.clone(), app.hist_pos), (fx.root.clone(), 0));
    app.apply(&ctx, Action::Back, 0.0);
    assert_eq!(app.hist_pos, 0, "back at the start is a no-op");

    app.apply(&ctx, Action::Forward, 0.0);
    settle(&ctx, &mut app);
    assert_eq!((app.cwd.clone(), app.hist_pos), (docs.clone(), 1));

    // navigating from the middle of the history drops the forward part
    app.apply(&ctx, Action::Back, 0.0);
    app.apply(&ctx, Action::Navigate(music.clone()), 0.0);
    settle(&ctx, &mut app);
    assert_eq!(app.history, vec![fx.root.clone(), music.clone()]);
    app.apply(&ctx, Action::Forward, 0.0);
    assert_eq!(app.hist_pos, 1, "nothing to go forward to");

    app.apply(&ctx, Action::Up, 0.0);
    settle(&ctx, &mut app);
    assert_eq!(app.cwd, fx.root);
    assert_eq!(app.history.len(), 3);

    // same directory again does not grow the history
    app.apply(&ctx, Action::Navigate(fx.root.clone()), 0.0);
    assert_eq!(app.history.len(), 3);

    // activating a directory entry navigates into it
    let i = idx_of(&app, "docs");
    app.apply(&ctx, Action::Activate(i), 0.0);
    settle(&ctx, &mut app);
    assert_eq!(app.cwd, docs);
}

#[test]
fn rename_dialog_rejects_conflicts_then_renames_and_reselects_the_entry() {
    let fx = fixture("rename");
    let (ctx, mut app) = app(&fx.root);
    let i = idx_of(&app, "b.txt");
    app.apply(&ctx, Action::OpenRename(i), 0.0);
    assert_eq!(rename_name(&mut app), "b.txt");

    *rename_name(&mut app) = "A.md".into();
    app.apply(&ctx, Action::ConfirmRename, 0.0);
    match &app.dialog {
        Some(OpenDialog {
            state: DialogState::Rename { error, .. },
            closing: false,
        }) => assert!(error.is_some(), "conflict is shown in the dialog"),
        _ => panic!("dialog should stay open with an error"),
    }

    *rename_name(&mut app) = "c.txt".into();
    app.apply(&ctx, Action::ConfirmRename, 0.0);
    assert!(app.dialog.as_ref().unwrap().closing);
    settle(&ctx, &mut app);
    assert_eq!(names(&app), ["docs", "Music", "A.md", "c.txt"]);
    assert_eq!(app.single_selected(), Some(idx_of(&app, "c.txt")));
    assert!(fx.root.join("c.txt").exists() && !fx.root.join("b.txt").exists());
    assert!(log_has(&app, "rename // b.txt -> c.txt"));
}

#[test]
fn permanent_delete_removes_the_file_and_reloads() {
    let fx = fixture("delete");
    let (ctx, mut app) = app(&fx.root);
    let i = idx_of(&app, "A.md");
    app.apply(&ctx, Action::Select(Some(i)), 0.0);
    app.apply(&ctx, Action::OpenDelete(true), 0.0);
    app.apply(&ctx, Action::ConfirmDelete, 0.0);
    assert!(app.dialog.as_ref().unwrap().closing);
    app.apply(&ctx, Action::ConfirmDelete, 0.0); // double-confirm while closing is ignored
    settle(&ctx, &mut app);
    assert_eq!(names(&app), ["docs", "Music", "b.txt"]);
    assert!(!fx.root.join("A.md").exists());
    assert!(log_has(&app, "delete // A.md :: done"));
    assert!(app.error_queue.is_empty());
}

#[test]
fn unreadable_directory_sets_the_link_error_and_queues_an_error_card() {
    use std::os::unix::fs::PermissionsExt;
    // root can read anything; the assertion would not hold
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let fx = fixture("locked");
    let locked = fx.root.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let (ctx, mut app) = app(&fx.root);
    app.apply(&ctx, Action::Navigate(locked.clone()), 0.0);
    settle(&ctx, &mut app);
    assert!(app.last_error.is_some());
    assert!(app.entries.is_empty());
    assert_eq!(
        app.error_queue.front().map(String::as_str),
        Some("access denied // locked")
    );

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn palette_shortcut_applies_the_theme_and_settings_are_saved_where_asked() {
    let fx = fixture("settings");
    let (ctx, mut app) = app(&fx.root);
    let conf = fx.root.join("cfg").join("file-manager.conf");
    app.persist_settings_to(conf.clone());

    app.apply(&ctx, Action::Palette(PaletteKind::Green), 1.0);
    assert_eq!(app.settings.palette, PaletteKind::Green);
    assert_eq!(palette(&ctx).accent, Palette::green().accent);
    assert!(log_has(
        &app,
        "settings // palette green :: square :: normal"
    ));
    assert_eq!(
        Settings::load_from(&conf).map(|s| s.palette),
        Some(PaletteKind::Green)
    );

    app.settings.chamfer = true;
    app.settings_changed(2.0);
    assert!(Settings::load_from(&conf).unwrap().chamfer);
}

#[test]
fn copy_and_paste_duplicate_the_selection_with_finder_style_names() {
    let fx = fixture("copy-paste");
    let (ctx, mut app) = app(&fx.root);

    // nothing selected: Cmd+C is a no-op
    app.apply(&ctx, Action::Copy, 0.0);
    assert!(app.clipboard.is_none());

    let b = idx_of(&app, "b.txt");
    app.apply(&ctx, Action::Select(Some(b)), 0.0);
    app.apply(&ctx, Action::Copy, 0.0);
    assert!(matches!(&app.clipboard, Some(c) if !c.cut && c.paths == vec![fx.root.join("b.txt")]));
    assert!(log_has(&app, "copy // b.txt"));

    // pasting into the same directory duplicates as `b copy.txt`, which gets selected
    app.apply(&ctx, Action::Paste(None), 0.0);
    settle(&ctx, &mut app);
    assert_eq!(
        names(&app),
        ["docs", "Music", "A.md", "b copy.txt", "b.txt"]
    );
    assert_eq!(
        std::fs::read_to_string(fx.root.join("b copy.txt")).unwrap(),
        "bb"
    );
    assert_eq!(app.lead, Some(idx_of(&app, "b copy.txt")));
    assert!(log_has(&app, "paste // b.txt -> "));
    assert!(log_has(&app, ":: done"));

    // a copy stays on the clipboard: pasting again yields `b copy 2.txt`
    app.apply(&ctx, Action::Paste(None), 0.0);
    settle(&ctx, &mut app);
    assert!(fx.root.join("b copy 2.txt").exists());
    assert!(app.clipboard.is_some());

    // pasting elsewhere keeps the original name; the source is untouched
    app.apply(&ctx, Action::Navigate(fx.root.join("docs")), 0.0);
    settle(&ctx, &mut app);
    app.apply(&ctx, Action::Paste(None), 0.0);
    settle(&ctx, &mut app);
    assert!(fx.root.join("docs/b.txt").exists() && fx.root.join("b.txt").exists());
    assert_eq!(names(&app), ["b.txt", "inner.txt"]);
}

#[test]
fn cut_and_paste_move_the_selection_and_empty_the_clipboard() {
    let fx = fixture("cut-paste");
    let (ctx, mut app) = app(&fx.root);
    let a = idx_of(&app, "A.md");
    let music = idx_of(&app, "Music");
    app.apply(&ctx, Action::Select(Some(a)), 0.0);
    app.apply(&ctx, Action::SelectToggle(music), 0.0);
    app.apply(&ctx, Action::Cut, 0.0);
    assert!(matches!(&app.clipboard, Some(c) if c.cut && c.paths.len() == 2));
    assert!(log_has(&app, "cut // 2 items"));

    // pasting where the files already live does nothing (but still consumes the cut)
    app.apply(&ctx, Action::Paste(None), 0.0);
    settle(&ctx, &mut app);
    assert!(app.clipboard.is_none());
    assert_eq!(names(&app), ["docs", "Music", "A.md", "b.txt"]);

    app.apply(&ctx, Action::Select(Some(idx_of(&app, "A.md"))), 0.0);
    app.apply(&ctx, Action::SelectToggle(idx_of(&app, "Music")), 0.0);
    app.apply(&ctx, Action::Cut, 0.0);
    app.apply(&ctx, Action::Navigate(fx.root.join("docs")), 0.0);
    settle(&ctx, &mut app);
    app.apply(&ctx, Action::Paste(None), 0.0);
    settle(&ctx, &mut app);
    assert_eq!(names(&app), ["Music", "A.md", "inner.txt"]);
    assert!(!fx.root.join("A.md").exists() && !fx.root.join("Music").exists());
    assert!(app.clipboard.is_none(), "a cut is one-shot");
    assert!(log_has(&app, "move // 2 items -> docs :: done"));

    // a collision on a move is reported instead of overwriting
    std::fs::write(fx.root.join("A.md"), "other").unwrap();
    app.apply(&ctx, Action::Select(Some(idx_of(&app, "A.md"))), 0.0);
    app.apply(&ctx, Action::Cut, 0.0);
    app.apply(&ctx, Action::Navigate(fx.root.clone()), 0.0);
    settle(&ctx, &mut app);
    app.apply(&ctx, Action::Paste(None), 0.0);
    settle(&ctx, &mut app);
    assert!(log_has(&app, "A.md: 'A.md' already exists here"));
    assert_eq!(
        std::fs::read_to_string(fx.root.join("A.md")).unwrap(),
        "other"
    );
    assert!(fx.root.join("docs/A.md").exists());
}

#[test]
fn cmd_c_writes_the_paths_as_text_and_a_foreign_paste_supersedes_the_files() {
    let fx = fixture("clipboard-os");
    let (ctx, mut app) = app(&fx.root);
    app.apply(&ctx, Action::Select(Some(idx_of(&app, "b.txt"))), 0.0);
    app.apply(&ctx, Action::SelectToggle(idx_of(&app, "A.md")), 0.0);
    app.apply(&ctx, Action::Copy, 0.0);
    // one path per line (entry order); this is also what egui hands to the OS clipboard
    let expect = app.clipboard.as_ref().unwrap().text.clone();
    let mut lines: Vec<&str> = expect.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            fx.root.join("A.md").display().to_string(),
            fx.root.join("b.txt").display().to_string()
        ]
    );
    assert!(ctx.output(|o| o
        .commands
        .iter()
        .any(|c| matches!(c, egui::OutputCommand::CopyText(t) if *t == expect))));

    // Cmd+V while the OS clipboard still holds our text pastes the files
    app.apply(&ctx, Action::Paste(Some(expect.clone())), 0.0);
    settle(&ctx, &mut app);
    assert!(fx.root.join("b copy.txt").exists() && fx.root.join("A copy.md").exists());

    // text copied elsewhere in the meantime: nothing is pasted and the files are forgotten
    app.apply(
        &ctx,
        Action::Paste(Some("hello from another app".into())),
        0.0,
    );
    settle(&ctx, &mut app);
    assert!(app.clipboard.is_none());
    assert!(!fx.root.join("b copy 2.txt").exists());
    assert!(log_has(&app, "paste // clipboard was replaced elsewhere"));
}

pub(super) fn goto_state(
    app: &mut Explorer,
) -> (&mut String, &mut Option<String>, &mut Vec<String>) {
    match &mut app.dialog {
        Some(OpenDialog {
            state:
                DialogState::GoTo {
                    path,
                    error,
                    suggestions,
                    ..
                },
            ..
        }) => (path, error, suggestions),
        _ => panic!("go-to dialog is not open"),
    }
}

#[test]
fn go_to_dialog_navigates_to_typed_paths_selects_files_and_completes() {
    let fx = fixture("goto");
    let (ctx, mut app) = app(&fx.root);

    // opens prefilled with the current directory and a trailing slash
    app.apply(&ctx, Action::OpenGoTo, 0.0);
    assert_eq!(*goto_state(&mut app).0, format!("{}/", fx.root.display()));

    // a relative directory
    *goto_state(&mut app).0 = "docs".into();
    app.apply(&ctx, Action::ConfirmGoTo, 0.0);
    settle(&ctx, &mut app);
    assert_eq!(app.cwd, fx.root.join("docs"));
    assert!(app.dialog.as_ref().is_some_and(|d| d.closing));
    assert!(log_has(&app, "goto // "));
    app.dialog = None;

    // a file: stay in its directory and select it (no reload happens, so immediately)
    app.apply(&ctx, Action::OpenGoTo, 0.0);
    *goto_state(&mut app).0 = "inner.txt".into();
    app.apply(&ctx, Action::ConfirmGoTo, 0.0);
    settle(&ctx, &mut app);
    assert_eq!(app.cwd, fx.root.join("docs"));
    assert_eq!(app.lead, Some(idx_of(&app, "inner.txt")));
    app.dialog = None;

    // a file elsewhere: navigate to its parent, then select it after the load
    app.apply(&ctx, Action::OpenGoTo, 0.0);
    *goto_state(&mut app).0 = "../b.txt".into();
    app.apply(&ctx, Action::ConfirmGoTo, 0.0);
    settle(&ctx, &mut app);
    assert_eq!(app.cwd, fx.root);
    assert_eq!(app.lead, Some(idx_of(&app, "b.txt")));
    assert_eq!(app.history.len(), 3, "typed navigation is in the history");
    app.dialog = None;

    // a missing path is rejected in place
    app.apply(&ctx, Action::OpenGoTo, 0.0);
    *goto_state(&mut app).0 = "nowhere".into();
    app.apply(&ctx, Action::ConfirmGoTo, 0.0);
    assert!(app.dialog.as_ref().is_some_and(|d| !d.closing));
    assert_eq!(goto_state(&mut app).1.as_deref(), Some("no such path"));
    assert_eq!(app.cwd, fx.root);

    // Tab: a single completion is taken whole, several give their common prefix
    // (the suggestions themselves are computed by the dialog UI each frame)
    let (path, _, suggestions) = goto_state(&mut app);
    *path = "Mu".into();
    *suggestions = fs::complete_goto("Mu", &fx.root, 6);
    app.apply(&ctx, Action::CompleteGoTo, 0.0);
    assert_eq!(*goto_state(&mut app).0, "Music/");
    let (path, _, suggestions) = goto_state(&mut app);
    *path = "d".into();
    *suggestions = vec!["docs/".into(), "documents/".into()];
    app.apply(&ctx, Action::CompleteGoTo, 0.0);
    assert_eq!(*goto_state(&mut app).0, "doc");
}
