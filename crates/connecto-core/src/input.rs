//! Translation of key presses and pastes into the bytes a terminal sends.
//!
//! Encoding follows xterm's legacy sequences. The kitty keyboard protocol is
//! deliberately not spoken: the emulator rejects it, so applications fall back
//! to these sequences.

/// Terminal modes that change what input bytes mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputModes {
    /// DECCKM: arrows send SS3 rather than CSI.
    pub app_cursor: bool,
    /// Pastes are wrapped in `ESC [ 200 ~` and `ESC [ 201 ~`.
    pub bracketed_paste: bool,
    /// The alternate screen is active, so there is no scrollback to view.
    pub alt_screen: bool,
    /// Mouse reporting: button presses (DECSET 1000).
    pub mouse_click: bool,
    /// Mouse reporting: button-held motion (1002).
    pub mouse_drag: bool,
    /// Mouse reporting: all motion (1003).
    pub mouse_motion: bool,
    /// SGR mouse encoding (1006).
    pub mouse_sgr: bool,
    /// UTF-8 mouse encoding (1005).
    pub mouse_utf8: bool,
    /// Focus in and out reports (1004).
    pub focus_reporting: bool,
}

/// Modifier keys, excluding the macOS Command key, which never reaches a PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    /// xterm's modifier parameter, or `None` when no modifier is held.
    fn param(self) -> Option<u8> {
        let bits = u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl);
        (bits != 0).then_some(bits + 1)
    }
}

/// A key that produces a terminal sequence by itself or with Control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Tab,
    Backspace,
    Escape,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Function key F1 to F12.
    F(u8),
    /// A lowercase ASCII letter.
    Letter(char),
    /// An ASCII punctuation key, as typed without Shift.
    Symbol(char),
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    const fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
        }
    }
}

/// What the mouse did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press(MouseButton),
    Release(MouseButton),
    /// Moved with a button held.
    Drag(MouseButton),
    /// Moved with no button held.
    Move,
    WheelUp,
    WheelDown,
}

/// A mouse event at a cell of the screen (0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub col: u16,
    pub row: u16,
    pub mods: Mods,
}

impl InputModes {
    /// Whether the program asked for any mouse reporting.
    #[must_use]
    pub const fn mouse_reporting(&self) -> bool {
        self.mouse_click || self.mouse_drag || self.mouse_motion
    }
}

/// The bytes a program that turned mouse reporting on expects for `event`, or
/// `None` when its tracking level does not include the event (a plain move
/// when only clicks were asked for, say). SGR encoding when the program asked
/// for it, otherwise the legacy X10 form, which cannot name cells past 222.
#[must_use]
pub fn encode_mouse(event: MouseEvent, modes: InputModes) -> Option<Vec<u8>> {
    if !modes.mouse_reporting() {
        return None;
    }
    let wanted = match event.kind {
        MouseKind::Press(_) | MouseKind::Release(_) | MouseKind::WheelUp | MouseKind::WheelDown => {
            true
        }
        MouseKind::Drag(_) => modes.mouse_drag || modes.mouse_motion,
        MouseKind::Move => modes.mouse_motion,
    };
    if !wanted {
        return None;
    }
    let mut code = match event.kind {
        MouseKind::Press(b) | MouseKind::Release(b) => b.code(),
        MouseKind::Drag(b) => b.code() + 32,
        MouseKind::Move => 3 + 32,
        MouseKind::WheelUp => 64,
        MouseKind::WheelDown => 65,
    };
    code += 4 * u8::from(event.mods.shift)
        + 8 * u8::from(event.mods.alt)
        + 16 * u8::from(event.mods.ctrl);
    let (x, y) = (u32::from(event.col) + 1, u32::from(event.row) + 1);
    if modes.mouse_sgr {
        let end = if matches!(event.kind, MouseKind::Release(_)) {
            'm'
        } else {
            'M'
        };
        return Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes());
    }
    // X10: a release has no button, and every value is offset by 32.
    if x > 222 || y > 222 {
        return None;
    }
    let code = if matches!(event.kind, MouseKind::Release(_)) {
        3 + (code & !3)
    } else {
        code
    };
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32 + code,
        u8::try_from(32 + x).ok()?,
        u8::try_from(32 + y).ok()?,
    ])
}

/// A key press with its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPress {
    pub key: Key,
    pub mods: Mods,
}

const ESC: u8 = 0x1b;

/// Bytes for a key press, or `None` when the key sends nothing by itself.
///
/// Printable text is not handled here: it arrives separately as text events,
/// so a plain letter or symbol yields `None` and cannot be typed twice.
#[must_use]
pub fn encode_key(press: KeyPress, modes: InputModes) -> Option<Vec<u8>> {
    let KeyPress { key, mods } = press;
    match key {
        // Shift+Enter sends ESC CR like Option+Enter, so Claude Code and Codex
        // insert a newline instead of submitting.
        Key::Enter if mods.shift => Some(vec![ESC, b'\r']),
        Key::Enter => Some(with_alt(mods, b'\r')),
        Key::Tab if mods.shift => Some(b"\x1b[Z".to_vec()),
        Key::Tab => Some(with_alt(mods, b'\t')),
        Key::Backspace if mods.ctrl => Some(with_alt(mods, 0x08)),
        Key::Backspace => Some(with_alt(mods, 0x7f)),
        Key::Escape => Some(with_alt(mods, ESC)),
        Key::Space if mods.ctrl => Some(with_alt(mods, 0)),
        Key::Space => None,
        Key::Up => Some(cursor_key(b'A', mods, modes)),
        Key::Down => Some(cursor_key(b'B', mods, modes)),
        Key::Right => Some(cursor_key(b'C', mods, modes)),
        Key::Left => Some(cursor_key(b'D', mods, modes)),
        Key::Home => Some(cursor_key(b'H', mods, modes)),
        Key::End => Some(cursor_key(b'F', mods, modes)),
        Key::Insert => Some(tilde_key(2, mods)),
        Key::Delete => Some(tilde_key(3, mods)),
        Key::PageUp => Some(tilde_key(5, mods)),
        Key::PageDown => Some(tilde_key(6, mods)),
        Key::F(n) => function_key(n, mods),
        Key::Letter(c) if mods.ctrl && c.is_ascii_lowercase() => {
            Some(with_alt(mods, c as u8 - b'a' + 1))
        }
        Key::Symbol(c) if mods.ctrl => control_symbol(c).map(|byte| with_alt(mods, byte)),
        Key::Letter(_) | Key::Symbol(_) => None,
    }
}

/// Bytes for pasted text under the current modes.
///
/// Line endings become carriage returns, as a typed Enter would be. With
/// bracketed paste on, escape and interrupt bytes are removed so that pasted
/// content cannot end the bracket early and run as keystrokes.
#[must_use]
pub fn encode_paste(text: &str, modes: InputModes) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if !modes.bracketed_paste {
        return normalized.into_bytes();
    }
    let mut bytes = Vec::with_capacity(normalized.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend(normalized.bytes().filter(|b| *b != ESC && *b != 0x03));
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

fn with_alt(mods: Mods, byte: u8) -> Vec<u8> {
    if mods.alt {
        vec![ESC, byte]
    } else {
        vec![byte]
    }
}

fn cursor_key(final_byte: u8, mods: Mods, modes: InputModes) -> Vec<u8> {
    match mods.param() {
        Some(param) => format!("\x1b[1;{param}{}", final_byte as char).into_bytes(),
        None if modes.app_cursor => vec![ESC, b'O', final_byte],
        None => vec![ESC, b'[', final_byte],
    }
}

fn tilde_key(code: u8, mods: Mods) -> Vec<u8> {
    match mods.param() {
        Some(param) => format!("\x1b[{code};{param}~").into_bytes(),
        None => format!("\x1b[{code}~").into_bytes(),
    }
}

fn function_key(n: u8, mods: Mods) -> Option<Vec<u8>> {
    let tilde_code = match n {
        1..=4 => {
            let final_byte = (b'P' + n - 1) as char;
            return Some(match mods.param() {
                Some(param) => format!("\x1b[1;{param}{final_byte}").into_bytes(),
                None => format!("\x1bO{final_byte}").into_bytes(),
            });
        }
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };
    Some(tilde_key(tilde_code, mods))
}

fn control_symbol(c: char) -> Option<u8> {
    match c {
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '/' | '-' => Some(0x1f),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        shift: false,
        alt: false,
        ctrl: false,
    };
    const CTRL: Mods = Mods {
        shift: false,
        alt: false,
        ctrl: true,
    };
    const SHIFT: Mods = Mods {
        shift: true,
        alt: false,
        ctrl: false,
    };
    const ALT: Mods = Mods {
        shift: false,
        alt: true,
        ctrl: false,
    };

    fn enc(key: Key, mods: Mods) -> Option<Vec<u8>> {
        encode_key(KeyPress { key, mods }, InputModes::default())
    }

    #[test]
    fn arrows_follow_application_cursor_mode() {
        let press = KeyPress {
            key: Key::Up,
            mods: NONE,
        };
        assert_eq!(
            encode_key(press, InputModes::default()),
            Some(b"\x1b[A".to_vec())
        );
        let app = InputModes {
            app_cursor: true,
            ..InputModes::default()
        };
        assert_eq!(encode_key(press, app), Some(b"\x1bOA".to_vec()));
    }

    fn mouse(kind: MouseKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            col,
            row,
            mods: Mods::default(),
        }
    }

    #[test]
    fn sgr_mouse_reports_presses_releases_drags_and_wheels() {
        let modes = InputModes {
            mouse_drag: true,
            mouse_sgr: true,
            ..InputModes::default()
        };
        let enc =
            |k, c, r| encode_mouse(mouse(k, c, r), modes).map(|b| String::from_utf8(b).unwrap());
        assert_eq!(
            enc(MouseKind::Press(MouseButton::Left), 0, 0).as_deref(),
            Some("\x1b[<0;1;1M")
        );
        assert_eq!(
            enc(MouseKind::Release(MouseButton::Left), 4, 2).as_deref(),
            Some("\x1b[<0;5;3m")
        );
        assert_eq!(
            enc(MouseKind::Press(MouseButton::Right), 9, 9).as_deref(),
            Some("\x1b[<2;10;10M")
        );
        assert_eq!(
            enc(MouseKind::Drag(MouseButton::Left), 1, 1).as_deref(),
            Some("\x1b[<32;2;2M")
        );
        assert_eq!(
            enc(MouseKind::WheelUp, 0, 0).as_deref(),
            Some("\x1b[<64;1;1M")
        );
        assert_eq!(
            enc(MouseKind::WheelDown, 0, 0).as_deref(),
            Some("\x1b[<65;1;1M")
        );
        // Plain motion is only reported at the all-motion level.
        assert_eq!(enc(MouseKind::Move, 0, 0), None);
        let all = InputModes {
            mouse_motion: true,
            mouse_sgr: true,
            ..InputModes::default()
        };
        assert_eq!(
            encode_mouse(mouse(MouseKind::Move, 2, 3), all).unwrap(),
            b"\x1b[<35;3;4M"
        );
    }

    #[test]
    fn modifiers_add_to_the_button_code_and_clicks_only_mode_skips_drags() {
        let click = InputModes {
            mouse_click: true,
            mouse_sgr: true,
            ..InputModes::default()
        };
        let event = MouseEvent {
            kind: MouseKind::Press(MouseButton::Left),
            col: 0,
            row: 0,
            mods: Mods {
                shift: true,
                alt: false,
                ctrl: true,
            },
        };
        assert_eq!(encode_mouse(event, click).unwrap(), b"\x1b[<20;1;1M");
        assert_eq!(
            encode_mouse(mouse(MouseKind::Drag(MouseButton::Left), 1, 1), click),
            None
        );
    }

    #[test]
    fn legacy_mouse_encoding_and_its_limit() {
        let modes = InputModes {
            mouse_click: true,
            ..InputModes::default()
        };
        assert_eq!(
            encode_mouse(mouse(MouseKind::Press(MouseButton::Left), 0, 0), modes).unwrap(),
            [0x1b, b'[', b'M', 32, 33, 33]
        );
        // A release carries no button.
        assert_eq!(
            encode_mouse(mouse(MouseKind::Release(MouseButton::Right), 0, 0), modes).unwrap(),
            [0x1b, b'[', b'M', 35, 33, 33]
        );
        assert_eq!(
            encode_mouse(mouse(MouseKind::Press(MouseButton::Left), 300, 0), modes),
            None
        );
    }

    #[test]
    fn nothing_is_reported_when_the_program_did_not_ask() {
        assert_eq!(
            encode_mouse(
                mouse(MouseKind::Press(MouseButton::Left), 0, 0),
                InputModes::default()
            ),
            None
        );
    }

    #[test]
    fn modified_arrows_ignore_application_cursor_mode() {
        let app = InputModes {
            app_cursor: true,
            ..InputModes::default()
        };
        let press = KeyPress {
            key: Key::Left,
            mods: CTRL,
        };
        assert_eq!(encode_key(press, app), Some(b"\x1b[1;5D".to_vec()));
    }

    #[test]
    fn control_letters_map_to_c0() {
        assert_eq!(enc(Key::Letter('a'), CTRL), Some(vec![1]));
        assert_eq!(enc(Key::Letter('c'), CTRL), Some(vec![3]));
        assert_eq!(enc(Key::Letter('z'), CTRL), Some(vec![26]));
    }

    #[test]
    fn plain_text_keys_send_nothing_because_text_events_carry_them() {
        assert_eq!(enc(Key::Letter('a'), NONE), None);
        assert_eq!(enc(Key::Letter('a'), SHIFT), None);
        assert_eq!(enc(Key::Space, NONE), None);
        assert_eq!(enc(Key::Symbol('['), NONE), None);
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(Key::Enter, NONE), Some(b"\r".to_vec()));
        assert_eq!(enc(Key::Enter, ALT), Some(b"\x1b\r".to_vec()));
        assert_eq!(enc(Key::Enter, SHIFT), Some(b"\x1b\r".to_vec()));
        assert_eq!(enc(Key::Backspace, NONE), Some(vec![0x7f]));
        assert_eq!(enc(Key::Backspace, ALT), Some(vec![0x1b, 0x7f]));
        assert_eq!(enc(Key::Tab, SHIFT), Some(b"\x1b[Z".to_vec()));
        assert_eq!(enc(Key::Delete, NONE), Some(b"\x1b[3~".to_vec()));
        assert_eq!(enc(Key::PageUp, SHIFT), Some(b"\x1b[5;2~".to_vec()));
    }

    #[test]
    fn control_symbols() {
        assert_eq!(enc(Key::Symbol('['), CTRL), Some(vec![0x1b]));
        assert_eq!(enc(Key::Space, CTRL), Some(vec![0]));
        assert_eq!(enc(Key::Symbol('.'), CTRL), None);
    }

    #[test]
    fn function_keys() {
        assert_eq!(enc(Key::F(1), NONE), Some(b"\x1bOP".to_vec()));
        assert_eq!(enc(Key::F(4), SHIFT), Some(b"\x1b[1;2S".to_vec()));
        assert_eq!(enc(Key::F(5), NONE), Some(b"\x1b[15~".to_vec()));
        assert_eq!(enc(Key::F(12), NONE), Some(b"\x1b[24~".to_vec()));
        assert_eq!(enc(Key::F(13), NONE), None);
    }

    #[test]
    fn paste_without_brackets_normalizes_newlines() {
        let bytes = encode_paste("a\nb\r\nc", InputModes::default());
        assert_eq!(bytes, b"a\rb\rc");
    }

    #[test]
    fn bracketed_paste_cannot_terminate_its_own_bracket() {
        let modes = InputModes {
            bracketed_paste: true,
            ..InputModes::default()
        };
        let bytes = encode_paste("x\x1b[201~rm -rf\x03", modes);
        assert_eq!(bytes, b"\x1b[200~x[201~rm -rf\x1b[201~");
    }
}
