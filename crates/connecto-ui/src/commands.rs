//! What the user can ask for, whether from the menu bar, a shortcut or a click
//! (project-tree-ui R14).
//!
//! A shortcut is taken out of the frame's events before the terminal sees
//! them, so no menu key reaches a PTY.

use connecto_core::ToolKind;
use egui::{Event, Key, Modifiers};

use crate::settings::SettingsTab;

/// Which projects the sidebar lists, when there are groups.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum GroupFilter {
    #[default]
    All,
    Group(String),
    /// The projects in no group.
    Ungrouped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    // File
    /// The New Project dialog: a folder, and optionally a git repository to
    /// clone into it.
    NewProject,
    OpenProject,
    OpenRecent(usize),
    ClearRecents,
    RenameProject,
    CloseProject,
    /// Ask before closing the project shown and deleting its folder.
    DeleteProject,
    // Terminal
    NewTerminal,
    NewTool(ToolKind),
    RenameTerminal,
    CloseTerminal,
    CloseFinishedChildren,
    History,
    RestartTerminal,
    // Agents
    TogglePause,
    StopEverything,
    // View
    ToggleSidebar,
    UtilitySessions,
    Automations,
    AgenticWorks,
    SkillsPlugins,
    Settings,
    /// Check for a newer Connecto now.
    CheckForUpdates,
    /// Settings, showing this section.
    SettingsAt(SettingsTab),
    NextTerminal,
    PreviousTerminal,
    NextNeedingMe,
    GotoProject(usize),
    // Clicks on one row
    Show {
        project: String,
        terminal: String,
    },
    Activate(String),
    SetExpanded(String, bool),
    CloseTerminalAt {
        project: String,
        terminal: String,
    },
    GenerateTitleAt {
        project: String,
        terminal: String,
    },
    CloseFinishedChildrenOf {
        project: String,
        terminal: String,
    },
    RestartAt {
        project: String,
        terminal: String,
    },
    /// Stop the background shells a terminal's agent left running, or only
    /// the task `task`.
    StopBackgroundAt {
        project: String,
        terminal: String,
        task: Option<String>,
    },
    RenameTerminalAt {
        project: String,
        terminal: String,
    },
    RenameProjectAt(String),
    CloseProjectAt(String),
    DeleteProjectAt(String),
    NewTerminalIn(String),
    HistoryOf(String),
    ProjectSettingsAt(String),
    /// The project's Operations dialog.
    OperationsOf(String),
    // Project groups in the sidebar
    /// The name field of a new group.
    NewGroup,
    RenameGroupAt(String),
    DeleteGroupAt(String),
    /// Put a project in a group, or take it out (`None`).
    SetProjectGroup(String, Option<String>),
    /// Show only some projects in the sidebar.
    FilterGroups(GroupFilter),
    /// Fold or unfold a group's projects.
    ToggleGroup(String),
    // Hosts
    AddHost,
    /// `None` is this machine.
    SelectHost(Option<String>),
    ConnectHost(String),
    DisconnectHost(String),
    RenameHostAt(String),
    /// Ask before forgetting a host: the confirm dialog forgets it.
    AskForgetHost(String),
}

/// The menu shortcut a key press stands for, if any. The table is the macOS
/// one; off macOS the press is first read as its macOS chord
/// (`platform::as_mac_chord`: Ctrl+Shift is Cmd).
pub(crate) fn from_key(key: Key, m: Modifiers) -> Option<Command> {
    let m = crate::platform::as_mac_chord(key, m);
    let cmd = m.mac_cmd;
    let plain = !m.shift && !m.alt && !m.ctrl;
    if cmd && m.alt && !m.shift && !m.ctrl {
        return match key {
            Key::Num1 => Some(Command::NewTool(ToolKind::Claude)),
            Key::Num2 => Some(Command::NewTool(ToolKind::Codex)),
            Key::Num3 => Some(Command::NewTool(ToolKind::Shell)),
            Key::Num4 => Some(Command::NewTool(ToolKind::Pi)),
            Key::Num5 => Some(Command::NewTool(ToolKind::Antigravity)),
            Key::Num6 => Some(Command::NewTool(ToolKind::OpenCode)),
            _ => None,
        };
    }
    if m.ctrl && !cmd && !m.alt && key == Key::Tab {
        return Some(if m.shift {
            Command::PreviousTerminal
        } else {
            Command::NextTerminal
        });
    }
    if cmd && m.shift && !m.alt && !m.ctrl {
        return match key {
            Key::W => Some(Command::CloseProject),
            Key::K => Some(Command::CloseFinishedChildren),
            _ => None,
        };
    }
    if !(cmd && plain) {
        return None;
    }
    let digit = [
        Key::Num1,
        Key::Num2,
        Key::Num3,
        Key::Num4,
        Key::Num5,
        Key::Num6,
        Key::Num7,
        Key::Num8,
        Key::Num9,
    ]
    .iter()
    .position(|candidate| *candidate == key);
    if let Some(index) = digit {
        return Some(Command::GotoProject(index));
    }
    Some(match key {
        Key::O => Command::OpenProject,
        Key::T => Command::NewTerminal,
        Key::R => Command::RenameTerminal,
        Key::W => Command::CloseTerminal,
        Key::Y => Command::History,
        Key::J => Command::NextNeedingMe,
        Key::Period => Command::TogglePause,
        Key::Backslash => Command::ToggleSidebar,
        Key::Comma => Command::Settings,
        _ => return None,
    })
}

/// Take every shortcut press out of `events` and return what they asked for.
/// The key releases go too, so nothing of a menu key is left.
pub(crate) fn take(events: &mut Vec<Event>) -> Vec<Command> {
    let mut found = Vec::new();
    let mut released = Vec::new();
    events.retain(|event| {
        let Event::Key {
            key,
            physical_key,
            pressed,
            modifiers,
            repeat,
        } = event
        else {
            return true;
        };
        if *pressed {
            let shortcut = crate::platform::shortcut_key(*key, *physical_key);
            let Some(command) = from_key(shortcut, *modifiers) else {
                return true;
            };
            // A held key repeats the press, not the command.
            if !*repeat {
                found.push(command);
            }
            released.push(*key);
            false
        } else {
            !released.contains(key)
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> Modifiers {
        Modifiers {
            mac_cmd: true,
            command: true,
            ..Modifiers::NONE
        }
    }

    #[test]
    fn every_documented_shortcut_maps() {
        let shift = |m: Modifiers| Modifiers { shift: true, ..m };
        let alt = |m: Modifiers| Modifiers { alt: true, ..m };
        let ctrl = Modifiers::CTRL;
        let cases = [
            (Key::O, cmd(), Command::OpenProject),
            (Key::W, shift(cmd()), Command::CloseProject),
            (Key::T, cmd(), Command::NewTerminal),
            (Key::Num1, alt(cmd()), Command::NewTool(ToolKind::Claude)),
            (Key::Num2, alt(cmd()), Command::NewTool(ToolKind::Codex)),
            (Key::Num3, alt(cmd()), Command::NewTool(ToolKind::Shell)),
            (Key::Num4, alt(cmd()), Command::NewTool(ToolKind::Pi)),
            (
                Key::Num5,
                alt(cmd()),
                Command::NewTool(ToolKind::Antigravity),
            ),
            (Key::Num6, alt(cmd()), Command::NewTool(ToolKind::OpenCode)),
            (Key::R, cmd(), Command::RenameTerminal),
            (Key::W, cmd(), Command::CloseTerminal),
            (Key::K, shift(cmd()), Command::CloseFinishedChildren),
            (Key::Y, cmd(), Command::History),
            (Key::Period, cmd(), Command::TogglePause),
            (Key::Backslash, cmd(), Command::ToggleSidebar),
            (Key::Comma, cmd(), Command::Settings),
            (Key::Tab, ctrl, Command::NextTerminal),
            (Key::Tab, shift(ctrl), Command::PreviousTerminal),
            (Key::J, cmd(), Command::NextNeedingMe),
            (Key::Num1, cmd(), Command::GotoProject(0)),
            (Key::Num9, cmd(), Command::GotoProject(8)),
        ];
        for (key, modifiers, expected) in cases {
            assert_eq!(
                from_key(key, modifiers),
                Some(expected.clone()),
                "{expected:?}"
            );
        }
    }

    #[test]
    fn plain_keys_and_the_terminals_own_chords_are_not_shortcuts() {
        assert_eq!(from_key(Key::T, Modifiers::NONE), None);
        assert_eq!(
            from_key(Key::W, Modifiers::CTRL),
            None,
            "ctrl-w is the shell's"
        );
        assert_eq!(
            from_key(Key::A, cmd()),
            None,
            "select all stays with the pane"
        );
        assert_eq!(from_key(Key::Tab, Modifiers::NONE), None);
        assert_eq!(from_key(Key::Num1, Modifiers::ALT), None);
    }

    /// Off macOS the chords are Ctrl+Shift (+Alt), as egui reports them there
    /// (`command` set with Ctrl); plain Ctrl stays the shell's.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn ctrl_shift_chords_map_off_macos() {
        let ctrl = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        };
        let ctrl_shift = Modifiers {
            shift: true,
            ..ctrl
        };
        let all = Modifiers {
            alt: true,
            ..ctrl_shift
        };
        let cases = [
            (Key::T, ctrl_shift, Some(Command::NewTerminal)),
            (Key::O, ctrl_shift, Some(Command::OpenProject)),
            (Key::W, ctrl_shift, Some(Command::CloseTerminal)),
            (Key::W, all, Some(Command::CloseProject)),
            (Key::K, all, Some(Command::CloseFinishedChildren)),
            (Key::Num1, all, Some(Command::NewTool(ToolKind::Claude))),
            (Key::Num2, ctrl_shift, Some(Command::GotoProject(1))),
            (Key::Comma, ctrl_shift, Some(Command::Settings)),
            (Key::Tab, ctrl, Some(Command::NextTerminal)),
            (Key::Tab, ctrl_shift, Some(Command::PreviousTerminal)),
            (Key::W, ctrl, None),
            (Key::T, ctrl, None),
            (Key::A, ctrl_shift, None),
        ];
        for (key, modifiers, expected) in cases {
            assert_eq!(from_key(key, modifiers), expected, "{key:?} {modifiers:?}");
        }
        // Ctrl+Shift+1 arrives as `!` with the digit as the physical key.
        let mut events = vec![Event::Key {
            key: Key::Exclamationmark,
            physical_key: Some(Key::Num1),
            pressed: true,
            repeat: false,
            modifiers: ctrl_shift,
        }];
        assert_eq!(take(&mut events), [Command::GotoProject(0)]);
        assert!(events.is_empty());
    }

    #[test]
    fn taking_shortcuts_leaves_the_rest_in_order() {
        let key = |key, pressed, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        };
        let mut events = vec![
            Event::Text("a".into()),
            key(Key::T, true, cmd()),
            key(Key::T, false, cmd()),
            key(Key::Tab, true, Modifiers::NONE),
            key(Key::Tab, false, Modifiers::NONE),
        ];
        assert_eq!(take(&mut events), [Command::NewTerminal]);
        assert_eq!(events.len(), 3);
    }
}
