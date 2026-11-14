//! Translating egui input events into the bytes a terminal sends.

use connecto_core::{InputModes, Key, KeyPress, Mods, encode_key, encode_paste};
use egui::{Event, ImeEvent, Key as EguiKey, Modifiers};

/// The text a terminal app inserts when files are dropped on it: each path
/// quoted for the shell (`platform::quote_dropped_path`: a backslash before
/// every character a Unix shell would read as syntax; PowerShell quoting on
/// Windows), paths separated by spaces, and one space after the last so the
/// user can keep typing. A path with a control character (a newline cannot be
/// escaped safely) is left out. `None` when nothing is left.
pub(crate) fn dropped_paths_text(paths: &[std::path::PathBuf]) -> Option<String> {
    let mut out = String::new();
    for path in paths {
        let path = path.to_string_lossy();
        if path.chars().any(char::is_control) {
            continue;
        }
        crate::platform::quote_dropped_path(&path, &mut out);
        out.push(' ');
    }
    (!out.is_empty()).then_some(out)
}

/// Bytes for an input event, or `None` when it sends nothing to the child.
pub(crate) fn translate(event: &Event, modes: InputModes) -> Option<Vec<u8>> {
    match event {
        Event::Text(text) | Event::Ime(ImeEvent::Commit(text)) => Some(text.clone().into_bytes()),
        Event::Paste(text) => Some(encode_paste(text, modes)),
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } if !crate::platform::app_owns(modifiers) => {
            let press = KeyPress {
                key: map_key(*key)?,
                mods: map_mods(*modifiers),
            };
            encode_key(press, modes)
        }
        _ => None,
    }
}

fn map_mods(modifiers: Modifiers) -> Mods {
    Mods {
        shift: modifiers.shift,
        alt: modifiers.alt,
        ctrl: modifiers.ctrl,
    }
}

fn map_key(key: EguiKey) -> Option<Key> {
    Some(match key {
        EguiKey::Enter => Key::Enter,
        EguiKey::Tab => Key::Tab,
        EguiKey::Backspace => Key::Backspace,
        EguiKey::Escape => Key::Escape,
        EguiKey::Space => Key::Space,
        EguiKey::ArrowUp => Key::Up,
        EguiKey::ArrowDown => Key::Down,
        EguiKey::ArrowLeft => Key::Left,
        EguiKey::ArrowRight => Key::Right,
        EguiKey::Home => Key::Home,
        EguiKey::End => Key::End,
        EguiKey::PageUp => Key::PageUp,
        EguiKey::PageDown => Key::PageDown,
        EguiKey::Insert => Key::Insert,
        EguiKey::Delete => Key::Delete,
        EguiKey::OpenBracket => Key::Symbol('['),
        EguiKey::CloseBracket => Key::Symbol(']'),
        EguiKey::Backslash => Key::Symbol('\\'),
        EguiKey::Slash => Key::Symbol('/'),
        EguiKey::Minus => Key::Symbol('-'),
        other => return map_letter(other).or_else(|| map_function(other)),
    })
}

fn map_letter(key: EguiKey) -> Option<Key> {
    const LETTERS: [(EguiKey, char); 26] = [
        (EguiKey::A, 'a'),
        (EguiKey::B, 'b'),
        (EguiKey::C, 'c'),
        (EguiKey::D, 'd'),
        (EguiKey::E, 'e'),
        (EguiKey::F, 'f'),
        (EguiKey::G, 'g'),
        (EguiKey::H, 'h'),
        (EguiKey::I, 'i'),
        (EguiKey::J, 'j'),
        (EguiKey::K, 'k'),
        (EguiKey::L, 'l'),
        (EguiKey::M, 'm'),
        (EguiKey::N, 'n'),
        (EguiKey::O, 'o'),
        (EguiKey::P, 'p'),
        (EguiKey::Q, 'q'),
        (EguiKey::R, 'r'),
        (EguiKey::S, 's'),
        (EguiKey::T, 't'),
        (EguiKey::U, 'u'),
        (EguiKey::V, 'v'),
        (EguiKey::W, 'w'),
        (EguiKey::X, 'x'),
        (EguiKey::Y, 'y'),
        (EguiKey::Z, 'z'),
    ];
    LETTERS
        .iter()
        .find(|(candidate, _)| *candidate == key)
        .map(|(_, letter)| Key::Letter(*letter))
}

fn map_function(key: EguiKey) -> Option<Key> {
    const KEYS: [EguiKey; 12] = [
        EguiKey::F1,
        EguiKey::F2,
        EguiKey::F3,
        EguiKey::F4,
        EguiKey::F5,
        EguiKey::F6,
        EguiKey::F7,
        EguiKey::F8,
        EguiKey::F9,
        EguiKey::F10,
        EguiKey::F11,
        EguiKey::F12,
    ];
    let index = KEYS.iter().position(|candidate| *candidate == key)?;
    u8::try_from(index + 1).ok().map(Key::F)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event(key: EguiKey, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn translated(event: &Event) -> Option<Vec<u8>> {
        translate(event, InputModes::default())
    }

    #[test]
    fn text_is_sent_as_utf8() {
        assert_eq!(translated(&Event::Text("é".into())), Some("é".into()));
    }

    #[test]
    fn control_c_is_an_interrupt_not_a_copy() {
        assert_eq!(
            translated(&key_event(EguiKey::C, Modifiers::CTRL)),
            Some(vec![3])
        );
    }

    #[test]
    fn command_shortcuts_never_reach_the_child() {
        assert_eq!(translated(&key_event(EguiKey::Q, Modifiers::MAC_CMD)), None);
        // `command` is Cmd only on macOS; elsewhere egui sets it for Ctrl.
        #[cfg(target_os = "macos")]
        assert_eq!(
            translated(&key_event(EguiKey::ArrowUp, Modifiers::COMMAND)),
            None
        );
    }

    /// Off macOS egui reports Ctrl as `command` too; Ctrl keys still reach
    /// the shell.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn control_c_as_reported_off_macos_is_an_interrupt() {
        let ctrl = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        };
        assert_eq!(translated(&key_event(EguiKey::C, ctrl)), Some(vec![3]));
    }

    #[test]
    fn key_releases_send_nothing() {
        let event = Event::Key {
            key: EguiKey::Enter,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(translated(&event), None);
    }

    #[test]
    fn a_typed_letter_is_not_sent_twice() {
        assert_eq!(translated(&key_event(EguiKey::A, Modifiers::NONE)), None);
    }

    #[test]
    fn arrows_respect_application_cursor_mode() {
        let modes = InputModes {
            app_cursor: true,
            ..InputModes::default()
        };
        let event = key_event(EguiKey::ArrowUp, Modifiers::NONE);
        assert_eq!(translate(&event, modes), Some(b"\x1bOA".to_vec()));
    }

    #[cfg(not(windows))]
    #[test]
    fn dropped_paths_are_escaped_and_space_separated() {
        use std::path::PathBuf;
        let text = dropped_paths_text(&[
            PathBuf::from("/tmp/a b/it's (1).png"),
            PathBuf::from("/tmp/plain.txt"),
            PathBuf::from("/tmp/bad\nname"),
        ]);
        assert_eq!(
            text.as_deref(),
            Some("/tmp/a\\ b/it\\'s\\ \\(1\\).png /tmp/plain.txt ")
        );
        assert_eq!(dropped_paths_text(&[]), None);
    }

    #[cfg(windows)]
    #[test]
    fn dropped_paths_are_quoted_for_powershell_and_space_separated() {
        use std::path::PathBuf;
        let text = dropped_paths_text(&[
            PathBuf::from(r"C:\tmp\a b\it's (1).png"),
            PathBuf::from(r"C:\tmp\plain.txt"),
            PathBuf::from("C:\\tmp\\bad\nname"),
        ]);
        assert_eq!(
            text.as_deref(),
            Some(r"'C:\tmp\a b\it''s (1).png' C:\tmp\plain.txt ")
        );
        assert_eq!(dropped_paths_text(&[]), None);
    }

    #[test]
    fn paste_respects_bracketed_mode() {
        let modes = InputModes {
            bracketed_paste: true,
            ..InputModes::default()
        };
        let bytes = translate(&Event::Paste("x".into()), modes);
        assert_eq!(bytes, Some(b"\x1b[200~x\x1b[201~".to_vec()));
    }

    #[test]
    fn every_function_key_maps() {
        assert_eq!(map_key(EguiKey::F1), Some(Key::F(1)));
        assert_eq!(map_key(EguiKey::F12), Some(Key::F(12)));
        assert_eq!(map_key(EguiKey::F13), None);
    }

    #[test]
    fn every_letter_maps() {
        assert_eq!(map_key(EguiKey::A), Some(Key::Letter('a')));
        assert_eq!(map_key(EguiKey::Z), Some(Key::Letter('z')));
    }
}
