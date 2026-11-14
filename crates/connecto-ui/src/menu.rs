//! The menu bar (project-tree-ui R14): one description of its menus, shown
//! two ways.
//!
//! - macOS: the native menu bar (`macos.rs`, built with `muda`). macOS gives a
//!   menu item's key equivalent to the menu before the key reaches the window,
//!   so a shortcut the menu owns never arrives as an egui key event and never
//!   reaches a terminal. `commands::take` drops the same chords from the
//!   frame's events when a menu is installed, as a second guard, and acts on
//!   them only when it is not.
//! - Elsewhere: a bar drawn at the top of the main window (`bar.rs`, egui). A
//!   native Windows menu would need `unsafe`. Its shortcuts are the chords
//!   `commands::take` reads, shown the way they are typed there.
//!
//! Both are built from [`menus`], so they list the same commands in the same
//! order. The in-window bar leaves out what only the macOS system provides
//! ([`Entry::System`]) and moves the rest of the application menu to the end
//! of File ([`in_window_menus`]).

use connecto_core::{RecentProject, ToolKind};
use egui::Key;

use crate::commands::Command;

#[cfg(not(target_os = "macos"))]
mod bar;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(target_os = "macos"))]
pub(crate) use bar::InWindowBar;
#[cfg(all(test, not(target_os = "macos")))]
pub(crate) use bar::{BAR_HEIGHT, BarHit};
#[cfg(target_os = "macos")]
pub(crate) use macos::NativeMenu;

#[cfg(not(target_os = "macos"))]
pub(crate) struct NativeMenu;

/// Off macOS there is no native menu; the bar in the window stands in for it.
#[cfg(not(target_os = "macos"))]
impl NativeMenu {
    pub(crate) fn install(_ctx: &egui::Context) -> Option<Self> {
        None
    }

    pub(crate) fn poll(&mut self) -> Vec<Command> {
        Vec::new()
    }

    pub(crate) fn sync(&mut self, _recents: &[RecentProject], _paused: bool) {}
}

/// On macOS the menu is the system's, so the window draws none.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub(crate) struct InWindowBar {}

#[cfg(target_os = "macos")]
impl InWindowBar {
    pub(crate) fn draw(
        &mut self,
        _ui: &mut egui::Ui,
        area: egui::Rect,
        _recents: &[RecentProject],
        _paused: bool,
    ) -> BarOut {
        BarOut {
            rest: area,
            commands: Vec::new(),
            closed: false,
        }
    }

    pub(crate) fn blocks_input(&self) -> bool {
        false
    }
}

/// What a pass of the in-window bar did.
pub(crate) struct BarOut {
    /// The window below the bar.
    pub(crate) rest: egui::Rect,
    /// The items clicked.
    pub(crate) commands: Vec<Command>,
    /// A menu that was open closed in this pass.
    pub(crate) closed: bool,
}

/// A shortcut, as the macOS menu has it. Off macOS the same chord is typed
/// with Ctrl+Shift in place of Cmd (`platform::as_mac_chord`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chord {
    Cmd(Key),
    CmdShift(Key),
    CmdAlt(Key),
    Ctrl(Key),
    CtrlShift(Key),
}

impl Chord {
    pub(crate) fn key(self) -> Key {
        match self {
            Self::Cmd(key)
            | Self::CmdShift(key)
            | Self::CmdAlt(key)
            | Self::Ctrl(key)
            | Self::CtrlShift(key) => key,
        }
    }

    /// The modifiers egui reports for this chord on macOS.
    #[cfg(test)]
    pub(crate) fn mac_modifiers(self) -> egui::Modifiers {
        let cmd = egui::Modifiers {
            mac_cmd: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        match self {
            Self::Cmd(_) => cmd,
            Self::CmdShift(_) => egui::Modifiers { shift: true, ..cmd },
            Self::CmdAlt(_) => egui::Modifiers { alt: true, ..cmd },
            Self::Ctrl(_) => egui::Modifiers::CTRL,
            Self::CtrlShift(_) => egui::Modifiers {
                shift: true,
                ..egui::Modifiers::CTRL
            },
        }
    }
}

/// An item only the macOS system provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum System {
    About,
    Hide,
    HideOthers,
    ShowAll,
    Quit,
    Minimize,
    Maximize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Entry {
    Item {
        label: &'static str,
        command: Command,
        chord: Option<Chord>,
    },
    /// Pause All Agents: a check item, checked while agents are paused.
    Pause {
        label: &'static str,
        chord: Option<Chord>,
    },
    /// The Open Recent submenu, filled from the recent projects
    /// ([`recent_entries`]).
    Recent {
        label: &'static str,
    },
    Separator,
    System(System),
}

/// What a top-level menu is to macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The application menu, named after the app.
    App,
    /// The menu macOS lists the windows in.
    Window,
    Plain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Menu {
    pub(crate) title: &'static str,
    pub(crate) role: Role,
    pub(crate) entries: Vec<Entry>,
}

fn item(label: &'static str, command: Command, chord: Option<Chord>) -> Entry {
    Entry::Item {
        label,
        command,
        chord,
    }
}

/// The menu bar, as macOS shows it, left to right and top to bottom.
pub(crate) fn menus() -> Vec<Menu> {
    use Chord::{Cmd, CmdAlt, CmdShift, Ctrl, CtrlShift};
    use Entry::{Separator, System as Sys};

    let digits = [
        Key::Num1,
        Key::Num2,
        Key::Num3,
        Key::Num4,
        Key::Num5,
        Key::Num6,
        Key::Num7,
        Key::Num8,
        Key::Num9,
    ];
    const PROJECT_LABELS: [&str; 9] = [
        "Project 1",
        "Project 2",
        "Project 3",
        "Project 4",
        "Project 5",
        "Project 6",
        "Project 7",
        "Project 8",
        "Project 9",
    ];
    let mut view = vec![
        item(
            "Toggle Sidebar",
            Command::ToggleSidebar,
            Some(Cmd(Key::Backslash)),
        ),
        Separator,
        item("Next Terminal", Command::NextTerminal, Some(Ctrl(Key::Tab))),
        item(
            "Previous Terminal",
            Command::PreviousTerminal,
            Some(CtrlShift(Key::Tab)),
        ),
        item("Next Needing Me", Command::NextNeedingMe, Some(Cmd(Key::J))),
        item("Utility Sessions", Command::UtilitySessions, None),
        Separator,
    ];
    view.extend(
        digits
            .iter()
            .zip(PROJECT_LABELS)
            .enumerate()
            .map(|(index, (key, label))| item(label, Command::GotoProject(index), Some(Cmd(*key)))),
    );

    vec![
        // The application menu has no Edit items on purpose: Copy and Paste
        // reach the terminal as key events, and an Edit menu would take them
        // for the first responder, which has no `copy:`.
        Menu {
            title: "Connecto",
            role: Role::App,
            entries: vec![
                Sys(System::About),
                Separator,
                item("Settings…", Command::Settings, Some(Cmd(Key::Comma))),
                item("Check for Updates…", Command::CheckForUpdates, None),
                Separator,
                Sys(System::Hide),
                Sys(System::HideOthers),
                Sys(System::ShowAll),
                Separator,
                Sys(System::Quit),
            ],
        },
        Menu {
            title: "File",
            role: Role::Plain,
            entries: vec![
                item("Open Project…", Command::OpenProject, Some(Cmd(Key::O))),
                Entry::Recent {
                    label: "Open Recent",
                },
                Separator,
                item("Rename Project…", Command::RenameProject, None),
                item(
                    "Close Project",
                    Command::CloseProject,
                    Some(CmdShift(Key::W)),
                ),
            ],
        },
        Menu {
            title: "Terminal",
            role: Role::Plain,
            entries: vec![
                item("New Terminal…", Command::NewTerminal, Some(Cmd(Key::T))),
                item(
                    "New Claude",
                    Command::NewTool(ToolKind::Claude),
                    Some(CmdAlt(Key::Num1)),
                ),
                item(
                    "New Codex",
                    Command::NewTool(ToolKind::Codex),
                    Some(CmdAlt(Key::Num2)),
                ),
                item(
                    "New Shell",
                    Command::NewTool(ToolKind::Shell),
                    Some(CmdAlt(Key::Num3)),
                ),
                Separator,
                item(
                    "Rename Terminal",
                    Command::RenameTerminal,
                    Some(Cmd(Key::R)),
                ),
                item("Close Terminal", Command::CloseTerminal, Some(Cmd(Key::W))),
                item(
                    "Close Finished Children",
                    Command::CloseFinishedChildren,
                    Some(CmdShift(Key::K)),
                ),
                Separator,
                item("History…", Command::History, Some(Cmd(Key::Y))),
                item("Restart Terminal", Command::RestartTerminal, None),
                Separator,
                item("Automations…", Command::Automations, None),
                item("Skills & Plugins…", Command::SkillsPlugins, None),
            ],
        },
        Menu {
            title: "Agents",
            role: Role::Plain,
            entries: vec![
                Entry::Pause {
                    label: "Pause All Agents",
                    chord: Some(Cmd(Key::Period)),
                },
                item("Stop Everything", Command::StopEverything, None),
            ],
        },
        Menu {
            title: "View",
            role: Role::Plain,
            entries: view,
        },
        Menu {
            title: "Window",
            role: Role::Window,
            entries: vec![Sys(System::Minimize), Sys(System::Maximize)],
        },
    ]
}

/// The menus a bar inside the window shows: [`menus`] without the items only
/// the macOS system provides. What is left of the application menu
/// (Settings) goes to the end of File, where Windows apps keep it, and a menu
/// left empty (Window) goes.
#[cfg(any(test, not(target_os = "macos")))]
pub(crate) fn in_window_menus() -> Vec<Menu> {
    let mut app_items = Vec::new();
    let mut out: Vec<Menu> = Vec::new();
    for mut menu in menus() {
        menu.entries
            .retain(|entry| !matches!(entry, Entry::System(_)));
        if menu.role == Role::App {
            app_items.append(&mut menu.entries);
        } else {
            out.push(menu);
        }
    }
    if let Some(file) = out.iter_mut().find(|menu| menu.title == "File") {
        file.entries.push(Entry::Separator);
        file.entries.append(&mut app_items);
    }
    for menu in &mut out {
        menu.entries = tidy_separators(std::mem::take(&mut menu.entries));
    }
    out.retain(|menu| !menu.entries.is_empty());
    out
}

/// No separator first, last or next to another.
#[cfg(any(test, not(target_os = "macos")))]
fn tidy_separators(entries: Vec<Entry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(entries.len());
    for entry in entries {
        let separator = entry == Entry::Separator;
        if separator && out.last().is_none_or(|last| *last == Entry::Separator) {
            continue;
        }
        out.push(entry);
    }
    if out.last() == Some(&Entry::Separator) {
        out.pop();
    }
    out
}

/// The label of the disabled item an empty Open Recent shows.
pub(crate) const NO_RECENTS: &str = "No Recent Projects";
pub(crate) const CLEAR_RECENT: &str = "Clear Recent";

/// One row of Open Recent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecentEntry<'a> {
    /// No recent projects: a disabled row.
    Empty,
    Project {
        index: usize,
        name: &'a str,
    },
    Separator,
    Clear,
}

impl RecentEntry<'_> {
    /// What clicking the row asks for; `None` for a row that cannot be
    /// clicked.
    #[cfg(any(test, not(target_os = "macos")))]
    pub(crate) fn command(&self) -> Option<Command> {
        match self {
            Self::Project { index, .. } => Some(Command::OpenRecent(*index)),
            Self::Clear => Some(Command::ClearRecents),
            Self::Empty | Self::Separator => None,
        }
    }
}

/// Open Recent's rows: the recent projects, then Clear Recent.
pub(crate) fn recent_entries(recents: &[RecentProject]) -> Vec<RecentEntry<'_>> {
    if recents.is_empty() {
        return vec![RecentEntry::Empty];
    }
    let mut out: Vec<RecentEntry<'_>> = recents
        .iter()
        .enumerate()
        .map(|(index, recent)| RecentEntry::Project {
            index,
            name: recent.name.as_str(),
        })
        .collect();
    out.push(RecentEntry::Separator);
    out.push(RecentEntry::Clear);
    out
}

/// The shortcut text the in-window bar shows for `chord`: the keys typed off
/// macOS, the way `platform::kbd!` writes them.
#[cfg(not(target_os = "macos"))]
pub(crate) fn in_window_shortcut(chord: Chord) -> String {
    let key = match chord.key() {
        Key::Comma => ",".to_owned(),
        Key::Period => ".".to_owned(),
        Key::Backslash => "\\".to_owned(),
        other => other.name().to_owned(),
    };
    let modifiers = match chord {
        Chord::Cmd(_) => "Ctrl+Shift+",
        Chord::CmdShift(_) | Chord::CmdAlt(_) => "Ctrl+Shift+Alt+",
        Chord::Ctrl(_) => "Ctrl+",
        Chord::CtrlShift(_) => "Ctrl+Shift+",
    };
    format!("{modifiers}{key}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::from_key;

    fn commands_of(menus: &[Menu]) -> Vec<Command> {
        menus
            .iter()
            .flat_map(|menu| menu.entries.iter())
            .filter_map(|entry| match entry {
                Entry::Item { command, .. } => Some(command.clone()),
                Entry::Pause { .. } => Some(Command::TogglePause),
                Entry::Recent { .. } => Some(Command::ClearRecents),
                Entry::Separator | Entry::System(_) => None,
            })
            .collect()
    }

    #[test]
    fn every_macos_chord_is_the_shortcut_for_its_command() {
        for menu in menus() {
            for entry in &menu.entries {
                let (command, chord) = match entry {
                    Entry::Item {
                        command,
                        chord: Some(chord),
                        ..
                    } => (command.clone(), *chord),
                    Entry::Pause {
                        chord: Some(chord), ..
                    } => (Command::TogglePause, *chord),
                    _ => continue,
                };
                assert_eq!(
                    from_key(chord.key(), chord.mac_modifiers()),
                    Some(command.clone()),
                    "{chord:?} for {command:?}"
                );
            }
        }
    }

    #[test]
    fn the_in_window_bar_has_every_command_of_the_macos_menu_in_order() {
        let mac = commands_of(&menus());
        let bar = commands_of(&in_window_menus());
        assert_eq!(bar.len(), mac.len());
        // Settings moves from the application menu to the end of File; the
        // rest keeps its order.
        let mut moved = mac.clone();
        let settings = moved.iter().position(|c| *c == Command::Settings);
        assert_eq!(settings, Some(0), "Settings heads the macOS list");
        assert_eq!(moved[1], Command::CheckForUpdates, "then Check for Updates");
        moved.drain(0..2);
        let after_file = moved
            .iter()
            .position(|c| *c == Command::CloseProject)
            .map(|at| at + 1);
        assert_eq!(after_file, Some(4));
        moved.insert(4, Command::Settings);
        moved.insert(5, Command::CheckForUpdates);
        assert_eq!(bar, moved);
        for command in [
            Command::ClearRecents,
            Command::RenameProject,
            Command::RestartTerminal,
            Command::StopEverything,
            Command::UtilitySessions,
            Command::Automations,
            Command::SkillsPlugins,
        ] {
            assert!(bar.contains(&command), "{command:?}");
        }
    }

    #[test]
    fn the_in_window_bar_leaves_out_only_the_macos_system_items() {
        let bar = in_window_menus();
        let titles: Vec<&str> = bar.iter().map(|menu| menu.title).collect();
        assert_eq!(titles, ["File", "Terminal", "Agents", "View"]);
        for menu in &bar {
            assert!(
                !menu
                    .entries
                    .iter()
                    .any(|entry| matches!(entry, Entry::System(_))),
                "{}",
                menu.title
            );
            assert_ne!(menu.entries.first(), Some(&Entry::Separator));
            assert_ne!(menu.entries.last(), Some(&Entry::Separator));
            assert!(
                !menu
                    .entries
                    .windows(2)
                    .any(|pair| pair[0] == Entry::Separator && pair[1] == Entry::Separator)
            );
        }
        // Every macOS menu's own entries, system items aside, are there in
        // the same order with the same separators.
        let mac = menus();
        for menu in &bar {
            let source = mac.iter().find(|m| m.title == menu.title);
            assert!(source.is_some(), "{} is not a macOS menu", menu.title);
            let Some(source) = source else { continue };
            let expected: Vec<&Entry> = source
                .entries
                .iter()
                .filter(|entry| !matches!(entry, Entry::System(_)))
                .collect();
            let shown: Vec<&Entry> = menu.entries.iter().take(expected.len()).collect();
            assert_eq!(shown, expected, "{}", menu.title);
        }
    }

    #[test]
    fn open_recent_lists_the_projects_then_clear() {
        assert_eq!(recent_entries(&[]), [RecentEntry::Empty]);
        assert_eq!(RecentEntry::Empty.command(), None);
        let recents = [
            RecentProject {
                name: "orbit".into(),
                path: "/work/orbit".into(),
            },
            RecentProject {
                name: "connecto".into(),
                path: "/work/connecto".into(),
            },
        ];
        let rows = recent_entries(&recents);
        assert_eq!(
            rows,
            [
                RecentEntry::Project {
                    index: 0,
                    name: "orbit"
                },
                RecentEntry::Project {
                    index: 1,
                    name: "connecto"
                },
                RecentEntry::Separator,
                RecentEntry::Clear,
            ]
        );
        assert_eq!(rows[1].command(), Some(Command::OpenRecent(1)));
        assert_eq!(rows[3].command(), Some(Command::ClearRecents));
    }

    /// Every character the bar draws, and the sidebar's no-project hint, is
    /// in the fonts the window draws with off macOS (egui's own proportional
    /// fonts). The full-width ＋ the hint had is not.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn every_glyph_the_bar_and_the_hint_draw_is_in_the_fonts() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx);
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        let font = egui::FontId::proportional(12.5);
        let mut texts: Vec<String> = vec![
            crate::platform::NO_PROJECT_HINT.to_owned(),
            crate::platform::OPEN_PROJECT_INVITE.to_owned(),
            NO_RECENTS.to_owned(),
            CLEAR_RECENT.to_owned(),
        ];
        for menu in in_window_menus() {
            texts.push(menu.title.to_owned());
            for entry in &menu.entries {
                match entry {
                    Entry::Item { label, chord, .. } | Entry::Pause { label, chord } => {
                        texts.push((*label).to_owned());
                        texts.extend(chord.map(in_window_shortcut));
                    }
                    Entry::Recent { label } => texts.push((*label).to_owned()),
                    Entry::Separator | Entry::System(_) => {}
                }
            }
        }
        ctx.fonts_mut(|fonts| {
            for text in &texts {
                assert!(fonts.has_glyphs(&font, text), "{text}");
            }
            assert!(!fonts.has_glyph(&font, '＋'));
        });
    }

    /// The text the bar shows is the chord that works there: typed as
    /// egui reports it off macOS, it is the item's command.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_shortcut_shown_in_the_window_is_the_one_that_works() {
        let mut seen = 0;
        for menu in in_window_menus() {
            for entry in &menu.entries {
                let (command, chord) = match entry {
                    Entry::Item {
                        command,
                        chord: Some(chord),
                        ..
                    } => (command.clone(), *chord),
                    Entry::Pause {
                        chord: Some(chord), ..
                    } => (Command::TogglePause, *chord),
                    _ => continue,
                };
                // Read the text back as the press egui reports off macOS,
                // where Ctrl also sets `command`.
                let text = in_window_shortcut(chord);
                let mut key_name = text.as_str();
                let mut held = |prefix: &str| {
                    let rest = key_name.strip_prefix(prefix);
                    if let Some(rest) = rest {
                        key_name = rest;
                    }
                    rest.is_some()
                };
                let ctrl = held("Ctrl+");
                let modifiers = egui::Modifiers {
                    ctrl,
                    command: ctrl,
                    shift: held("Shift+"),
                    alt: held("Alt+"),
                    mac_cmd: false,
                };
                let key = match key_name {
                    "," => Key::Comma,
                    "." => Key::Period,
                    "\\" => Key::Backslash,
                    name => Key::from_name(name).unwrap_or(Key::F35),
                };
                assert_eq!(key, chord.key(), "{text}");
                assert_eq!(
                    from_key(key, modifiers),
                    Some(command.clone()),
                    "{text} for {command:?}"
                );
                seen += 1;
            }
        }
        assert_eq!(seen, 25);
        assert_eq!(
            in_window_shortcut(Chord::Cmd(Key::O)),
            crate::platform::kbd!("O")
        );
        assert_eq!(
            in_window_shortcut(Chord::CmdAlt(Key::Num1)),
            crate::platform::kbd!(alt "1")
        );
    }
}
