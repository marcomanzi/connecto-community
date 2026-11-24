//! The in-window menu bar, off macOS: it lists what the macOS menu lists,
//! each click runs the macOS item's command, and an open menu keeps the
//! frame's keys and clicks from the terminal.

use super::*;
use crate::commands::Command;
use crate::menu::{BAR_HEIGHT, BarHit, Entry, in_window_menus, in_window_shortcut};

type RowState = (String, Option<String>, bool, bool);

fn title(w: &W, name: &str) -> Rect {
    let found = w
        .app
        .bar_hits()
        .iter()
        .find(|(hit, _)| matches!(hit, BarHit::Title(title) if *title == name))
        .map(|(_, rect)| *rect);
    assert!(found.is_some(), "no {name} title: {:?}", w.app.bar_hits());
    found.unwrap()
}

/// The rows drawn in the last pass: label, shortcut, enabled, checked.
fn rows(w: &W) -> Vec<RowState> {
    w.app
        .bar_hits()
        .iter()
        .filter_map(|(hit, _)| match hit {
            BarHit::Row {
                label,
                shortcut,
                enabled,
                checked,
            } => Some((label.clone(), shortcut.clone(), *enabled, *checked)),
            _ => None,
        })
        .collect()
}

fn row(w: &W, label: &str) -> Rect {
    let found = w
        .app
        .bar_hits()
        .iter()
        .find(|(hit, _)| matches!(hit, BarHit::Row { label: l, .. } if l == label))
        .map(|(_, rect)| *rect);
    assert!(found.is_some(), "no {label} row: {:?}", rows(w));
    found.unwrap()
}

fn open(w: &mut W, name: &str) {
    let at = title(w, name).center();
    w.click(at);
    w.frame(vec![]);
}

fn choose(w: &mut W, menu: &str, label: &str) {
    open(w, menu);
    let at = row(w, label).center();
    w.click(at);
    w.frame(vec![]);
}

/// Hover Open Recent until its submenu shows.
fn open_recent(w: &mut W) {
    open(w, "File");
    let at = row(w, "Open Recent").center();
    for _ in 0..4 {
        w.frame(vec![Event::PointerMoved(at)]);
    }
}

fn row_state(label: &str, shortcut: Option<&str>, enabled: bool, checked: bool) -> RowState {
    (
        label.to_owned(),
        shortcut.map(str::to_owned),
        enabled,
        checked,
    )
}

#[test]
fn the_bar_sits_above_the_sidebar_and_the_terminal() {
    let (_quill, _connecto, ws) = two_projects();
    let w = W::new(&ws);
    assert!((w.app.hits.sidebar.min.y - BAR_HEIGHT).abs() < 0.5);
    assert!((w.app.hits.main.min.y - BAR_HEIGHT).abs() < 0.5);
    let titles: Vec<&str> = w
        .app
        .bar_hits()
        .iter()
        .filter_map(|(hit, _)| match hit {
            BarHit::Title(title) => Some(*title),
            _ => None,
        })
        .collect();
    assert_eq!(titles, ["File", "Terminal", "Agents", "View"]);
    for name in titles {
        assert!(title(&w, name).max.y <= BAR_HEIGHT);
    }
    // No band for traffic lights: the collapse button joins History and +,
    // first in the row.
    let collapse = w.rect(&Hit::Collapse);
    let history = w.rect(&Hit::HeaderHistory);
    assert!((collapse.center().y - history.center().y).abs() < 0.5);
    assert!(collapse.max.x <= history.min.x);
    // Nothing is open until a title is clicked.
    assert!(rows(&w).is_empty());
}

#[test]
fn every_menu_lists_the_macos_items_in_order_with_their_windows_shortcuts() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    for menu in in_window_menus() {
        open(&mut w, menu.title);
        let expected: Vec<RowState> = menu
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Item { label, chord, .. } | Entry::Pause { label, chord } => Some((
                    (*label).to_owned(),
                    chord.map(in_window_shortcut),
                    true,
                    false,
                )),
                Entry::Recent { label } => Some(((*label).to_owned(), None, true, false)),
                Entry::Separator | Entry::System(_) => None,
            })
            .collect();
        assert_eq!(rows(&w), expected, "{}", menu.title);
        let separators = w
            .app
            .bar_hits()
            .iter()
            .filter(|(hit, _)| *hit == BarHit::Separator)
            .count();
        let expected_separators = menu
            .entries
            .iter()
            .filter(|entry| **entry == Entry::Separator)
            .count();
        assert_eq!(separators, expected_separators, "{}", menu.title);
        w.frame(esc());
        w.frame(vec![]);
        assert!(rows(&w).is_empty(), "Escape closes {}", menu.title);
    }
    // What a user reads.
    open(&mut w, "File");
    let file = rows(&w);
    for expected in [
        row_state("Open Project…", Some("Ctrl+Shift+O"), true, false),
        row_state("Close Project", Some("Ctrl+Shift+Alt+W"), true, false),
        row_state("Rename Project…", None, true, false),
        row_state("Settings…", Some("Ctrl+Shift+,"), true, false),
    ] {
        assert!(file.contains(&expected), "{expected:?} in {file:?}");
    }
}

#[test]
fn clicking_an_item_runs_the_same_command_as_the_macos_menu() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    choose(&mut w, "Agents", "Stop Everything");
    assert_eq!(ws.stopped.load(Ordering::Relaxed), 1);
    assert!(rows(&w).is_empty(), "the click closes the menu");

    choose(&mut w, "Agents", "Pause All Agents");
    assert!(ws.pause.is_paused());
    open(&mut w, "Agents");
    assert!(
        rows(&w).contains(&row_state(
            "Pause All Agents",
            Some("Ctrl+Shift+."),
            true,
            true
        )),
        "checked while paused: {:?}",
        rows(&w)
    );
    let at = row(&w, "Pause All Agents").center();
    w.click(at);
    w.frame(vec![]);
    assert!(!ws.pause.is_paused());

    choose(&mut w, "View", "Utility Sessions");
    assert!(w.app.utility_open());
    choose(&mut w, "View", "Utility Sessions");
    assert!(!w.app.utility_open());

    choose(&mut w, "Terminal", "Skills & Plugins…");
    assert!(w.app.skills_open());
    choose(&mut w, "Terminal", "Skills & Plugins…");
    assert!(!w.app.skills_open());

    choose(&mut w, "Terminal", "Automations…");
    assert!(w.app.automations_open());
    choose(&mut w, "Terminal", "Automations…");
    assert!(!w.app.automations_open());

    choose(&mut w, "File", "Settings…");
    assert!(w.app.settings_open());
    choose(&mut w, "File", "Settings…");
    assert!(!w.app.settings_open());

    choose(&mut w, "View", "Project 2");
    assert_eq!(w.shown(), Some(("connecto".into(), "claude".into())));

    choose(&mut w, "File", "Rename Project…");
    assert!(w.app.is_renaming());
    w.frame(esc());
    w.frame(vec![]);
    assert!(!w.app.is_renaming());

    choose(&mut w, "Terminal", "Close Terminal");
    assert_eq!(*connecto.terminals.closes.lock().unwrap(), ["claude"]);
    assert!(!quill.typed_anywhere() && !connecto.typed_anywhere());
}

#[test]
fn open_recent_is_disabled_when_empty_and_lists_the_recents() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_recent(&mut w);
    assert!(
        rows(&w).contains(&row_state("No Recent Projects", None, false, false)),
        "{:?}",
        rows(&w)
    );
    // A disabled row does nothing.
    let at = row(&w, "No Recent Projects").center();
    w.click(at);
    w.frame(vec![]);
    assert_eq!(ws.cleared.load(Ordering::Relaxed), 0);
    assert!(ws.opened.lock().unwrap().is_empty());
    w.frame(esc());
    w.frame(vec![]);

    ws.edit(|view| {
        view.recents = vec![
            RecentProject {
                path: "/work/quill".into(),
                name: "quill".into(),
            },
            RecentProject {
                path: "/work/older".into(),
                name: "older".into(),
            },
        ];
    });
    w.frame(vec![]);
    open_recent(&mut w);
    let recent: Vec<RowState> = rows(&w)
        .into_iter()
        .skip_while(|(label, ..)| label != "quill")
        .take(3)
        .collect();
    assert_eq!(
        recent,
        [
            row_state("quill", None, true, false),
            row_state("older", None, true, false),
            row_state("Clear Recent", None, true, false),
        ]
    );
    let at = row(&w, "older").center();
    w.frame(vec![Event::PointerMoved(at)]);
    w.click(at);
    w.frame(vec![]);
    assert_eq!(
        ws.opened.lock().unwrap().last(),
        Some(&PathBuf::from("/work/older"))
    );
    assert!(rows(&w).is_empty(), "the click closes both menus");

    open_recent(&mut w);
    let at = row(&w, "Clear Recent").center();
    w.frame(vec![Event::PointerMoved(at)]);
    w.click(at);
    w.frame(vec![]);
    assert_eq!(ws.cleared.load(Ordering::Relaxed), 1);
}

#[test]
fn an_open_menu_keeps_keys_and_clicks_from_the_terminal() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.type_text("a");
    assert_eq!(quill.slot("claude-1").typed(), b"a");
    open(&mut w, "Terminal");
    assert!(!rows(&w).is_empty());
    w.type_text("b");
    w.key(Key::ArrowDown, Modifiers::NONE);
    w.frame(esc());
    w.frame(vec![]);
    assert!(rows(&w).is_empty(), "Escape closes the menu");
    assert_eq!(
        quill.slot("claude-1").typed(),
        b"a",
        "nothing typed while the menu was open reached the terminal"
    );
    // A click outside closes the menu and chooses nothing.
    open(&mut w, "Agents");
    let outside = w.app.hits.content.center();
    w.click(outside);
    w.frame(vec![]);
    assert!(rows(&w).is_empty(), "a click outside closes the menu");
    assert_eq!(ws.stopped.load(Ordering::Relaxed), 0);
    assert!(!ws.pause.is_paused());
    // Afterwards the terminal has the keyboard again.
    w.frame(vec![]);
    w.type_text("c");
    assert_eq!(quill.slot("claude-1").typed(), b"ac");
    assert!(!connecto.typed_anywhere());
}

#[test]
fn a_menu_click_goes_through_the_native_menus_queue() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.app.queue_for_tests(Command::NewTerminal);
    w.frame(vec![]);
    assert!(w.app.chooser_open());
    w.frame(esc());
    w.frame(vec![]);
    assert!(!w.app.chooser_open());
    choose(&mut w, "Terminal", "New Terminal…");
    assert!(w.app.chooser_open());
}
