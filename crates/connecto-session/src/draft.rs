//! The user's unsent draft in a terminal's input line, and how Connecto moves
//! it out of the way of a message it types and puts it back.
//!
//! Every byte the user sends to a terminal goes through its pane, so the pane
//! follows the draft from those bytes ([`DraftTracker`]). Only keys whose
//! effect is the same in every version of the tool are followed (characters,
//! pastes, Backspace, Left and Right, Enter). Any other key makes the draft
//! unknown until the next Enter, and an unknown draft is never touched: the
//! message waits instead. The screen is a cross-check ([`decide`]): a draft the
//! screen contradicts is not touched either.
//!
//! Moving a draft uses only keys that never interrupt a turn: Right to its end,
//! then one Backspace per character. Putting it back is a paste (a shell gets
//! the text typed), then Left back to where the cursor was.

use connecto_adapter::{ComposerState, ComposerView, DraftKeys};
use connecto_core::{InputModes, Key, KeyPress, Mods, encode_key, encode_paste};

/// A paste up to this long, on one line, is drawn as typed text by every TUI:
/// the cursor walks it one character at a time.
const SHORT_PASTE: usize = 200;

const ESC: u8 = 0x1b;
const BACKSPACE: u8 = 0x7f;

/// A draft as the tool holds it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Draft {
    chars: Vec<char>,
    /// Per character: part of a paste the tool may show as one placeholder.
    whole: Vec<bool>,
    cursor: usize,
}

impl Draft {
    /// Text Connecto pasted into an empty input line, cursor at its end.
    pub(crate) fn pasted(text: &str) -> Self {
        let mut draft = Self::default();
        let chars: Vec<char> = text.chars().collect();
        draft.insert(&chars, true);
        draft
    }

    pub(crate) fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// The cursor, in characters from the start.
    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    fn insert(&mut self, text: &[char], whole: bool) {
        let at = self.cursor;
        self.chars.splice(at..at, text.iter().copied());
        self.whole
            .splice(at..at, std::iter::repeat_n(whole, text.len()));
        self.cursor += text.len();
    }

    /// The character left of the cursor can be stepped over one at a time.
    fn plain_before(&self) -> bool {
        self.cursor > 0 && !self.whole[self.cursor - 1] && simple(self.chars[self.cursor - 1])
    }

    fn plain_after(&self) -> bool {
        self.cursor < self.chars.len()
            && !self.whole[self.cursor]
            && simple(self.chars[self.cursor])
    }
}

/// What is known of the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Known {
    Exact(Draft),
    /// A key whose effect is not certain was typed since the last Enter.
    Unknown,
}

/// A character the cursor moves over by itself in every tool. One that joins
/// the character before it (an accent, a joiner, a variation or skin tone) is
/// stepped over with it, so a draft holding one is only followed while the
/// cursor does not move.
fn simple(ch: char) -> bool {
    !matches!(u32::from(ch),
        0x0300..=0x036F
        | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF
        | 0x200C..=0x200D
        | 0x20D0..=0x20FF
        | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F
        | 0x1F1E6..=0x1F1FF
        | 0x1F3FB..=0x1F3FF
        | 0xE0020..=0xE007F
        | 0xE0100..=0xE01EF)
}

/// One key or paste in what the user sent.
#[derive(Debug, PartialEq, Eq)]
enum Token {
    Char(char),
    Paste(String),
    Control(u8),
    /// ESC CR: Shift+Enter or Option+Enter.
    AltEnter,
    Left,
    Right,
    Home,
    End,
    /// Mouse buttons and moves, focus reports: nothing typed.
    Ignored,
    /// Anything else, or a sequence cut short.
    Other,
}

/// Split what one write of the user's carried into keys. The UI sends each key
/// and each paste in one write, so a sequence is never split across writes.
fn tokens(bytes: &[u8]) -> Vec<Token> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if let Some(body) = rest.strip_prefix(b"\x1b[200~") {
            match find(body, b"\x1b[201~") {
                Some(end) => {
                    out.push(Token::Paste(
                        String::from_utf8_lossy(&body[..end]).into_owned(),
                    ));
                    i += 6 + end + 6;
                }
                None => {
                    out.push(Token::Other);
                    i = bytes.len();
                }
            }
            continue;
        }
        let byte = rest[0];
        if byte == ESC {
            let (token, len) = escape(rest);
            out.push(token);
            i += len;
            continue;
        }
        if byte < 0x20 || byte == BACKSPACE {
            out.push(Token::Control(byte));
            i += 1;
            continue;
        }
        let len = utf8_len(byte);
        match rest
            .get(..len)
            .and_then(|seq| std::str::from_utf8(seq).ok())
            .and_then(|seq| seq.chars().next())
        {
            Some(ch) => {
                out.push(Token::Char(ch));
                i += len;
            }
            None => {
                out.push(Token::Other);
                i += 1;
            }
        }
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn utf8_len(lead: u8) -> usize {
    match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// The key an escape sequence at the start of `bytes` names, and its length.
fn escape(bytes: &[u8]) -> (Token, usize) {
    match bytes.get(1) {
        // A lone Escape.
        None => (Token::Other, 1),
        Some(b'\r') => (Token::AltEnter, 2),
        Some(b'O') => match bytes.get(2) {
            Some(b'C') => (Token::Right, 3),
            Some(b'D') => (Token::Left, 3),
            Some(b'H') => (Token::Home, 3),
            Some(b'F') => (Token::End, 3),
            Some(_) => (Token::Other, 3),
            None => (Token::Other, 2),
        },
        Some(b'[') => csi(bytes),
        Some(_) => (Token::Other, 2),
    }
}

fn csi(bytes: &[u8]) -> (Token, usize) {
    // X10 mouse: ESC [ M and three bytes.
    if bytes.get(2) == Some(&b'M') {
        let wheel = bytes.get(3).is_some_and(|b| b.wrapping_sub(32) & 64 != 0);
        let token = if wheel { Token::Other } else { Token::Ignored };
        return (token, 6.min(bytes.len()));
    }
    let mut end = 2;
    while end < bytes.len() && (0x20..=0x3F).contains(&bytes[end]) {
        end += 1;
    }
    let Some(&last) = bytes.get(end) else {
        return (Token::Other, bytes.len());
    };
    let params = &bytes[2..end];
    let len = end + 1;
    let token = match (params, last) {
        (b"", b'C') => Token::Right,
        (b"", b'D') => Token::Left,
        (b"", b'H') => Token::Home,
        (b"", b'F') => Token::End,
        // Focus in and out.
        (b"", b'I' | b'O') => Token::Ignored,
        // SGR mouse: a wheel may walk the prompt history; the rest types nothing.
        ([b'<', code @ ..], b'M' | b'm') => {
            let code: u32 = code
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .fold(0, |n, b| n * 10 + u32::from(b - b'0'));
            if code & 64 == 0 {
                Token::Ignored
            } else {
                Token::Other
            }
        }
        _ => Token::Other,
    };
    (token, len)
}

/// Follows a terminal's draft from what the user sends to it.
#[derive(Debug, Clone)]
pub(crate) struct DraftTracker {
    keys: DraftKeys,
    known: Known,
}

impl DraftTracker {
    pub(crate) fn new(keys: DraftKeys) -> Self {
        Self {
            keys,
            known: Known::Exact(Draft::default()),
        }
    }

    pub(crate) fn known(&self) -> &Known {
        &self.known
    }

    /// The input line is empty: a new session, or the screen shows it so.
    pub(crate) fn reset(&mut self) {
        self.known = Known::Exact(Draft::default());
    }

    /// The draft is `draft`: Connecto just put it back.
    pub(crate) fn set(&mut self, draft: Draft) {
        self.known = Known::Exact(draft);
    }

    /// Something changed the line that cannot be followed.
    pub(crate) fn forget(&mut self) {
        self.known = Known::Unknown;
    }

    /// Follow `bytes`, sent by the user.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for token in tokens(bytes) {
            self.apply(token);
        }
    }

    fn apply(&mut self, token: Token) {
        let keys = self.keys;
        let Known::Exact(draft) = &mut self.known else {
            // Unknown until the line is submitted or dropped.
            let ends = matches!(token, Token::Control(b'\r'))
                || (keys.ctrl_c_clears && matches!(token, Token::Control(0x03)));
            if ends {
                self.reset();
            }
            return;
        };
        let known = match token {
            Token::Char(ch) => {
                draft.insert(&[ch], false);
                true
            }
            Token::Paste(text) => {
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                let chars: Vec<char> = text
                    .chars()
                    .filter(|ch| *ch != '\x1b' && *ch != '\x03')
                    .collect();
                // A shell runs each pasted line, and takes a tab as completion.
                let fits = keys.multiline || !chars.iter().any(|ch| ch.is_control());
                if fits {
                    let whole = keys.collapses_pastes
                        && (chars.len() > SHORT_PASTE || chars.contains(&'\n'));
                    draft.insert(&chars, whole);
                }
                fits
            }
            Token::AltEnter if keys.multiline && keys.esc_enter_newline => {
                draft.insert(&['\n'], false);
                true
            }
            // Enter submits, except after a backslash in a TUI, where it may be a newline.
            Token::Control(b'\r') => {
                let continued =
                    keys.multiline && draft.cursor > 0 && draft.chars[draft.cursor - 1] == '\\';
                if !continued {
                    *draft = Draft::default();
                }
                !continued
            }
            Token::Control(0x03) if keys.ctrl_c_clears => {
                *draft = Draft::default();
                true
            }
            Token::Control(BACKSPACE) => {
                if draft.cursor == 0 {
                    true
                } else if draft.plain_before() {
                    draft.cursor -= 1;
                    draft.chars.remove(draft.cursor);
                    draft.whole.remove(draft.cursor);
                    true
                } else {
                    false
                }
            }
            Token::Left => {
                if draft.cursor == 0 {
                    true
                } else if draft.plain_before() {
                    draft.cursor -= 1;
                    true
                } else {
                    false
                }
            }
            Token::Right => {
                if draft.cursor == draft.chars.len() {
                    true
                } else if draft.plain_after() {
                    draft.cursor += 1;
                    true
                } else {
                    false
                }
            }
            // On one line, Home and End (and Ctrl-A, Ctrl-E) mean its ends.
            Token::Home | Token::Control(0x01) if !draft.chars.contains(&'\n') => {
                draft.cursor = 0;
                true
            }
            Token::End | Token::Control(0x05) if !draft.chars.contains(&'\n') => {
                draft.cursor = draft.chars.len();
                true
            }
            Token::Ignored => true,
            _ => false,
        };
        if !known {
            self.forget();
        }
    }
}

/// What to do with the user's draft before Connecto types into the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The line is empty: type as always.
    Plain,
    /// The screen shows the line empty though the draft said otherwise: type as
    /// always, and follow the line from empty.
    PlainReset,
    /// Move this draft out of the way, type, then put it back.
    Protect(Draft),
    /// The screen may not have drawn the user's last keys yet: look again.
    Wait,
    /// The draft cannot be moved safely: do not type now.
    Unsure,
}

/// Decide from what the user typed (`known`), what the screen shows of the
/// input box (`None` when it cannot be read), whether the tool is in a turn,
/// and whether the screen has had time to draw the user's last keys.
pub(crate) fn decide(
    known: &Known,
    screen: Option<&ComposerView>,
    busy: bool,
    settled: bool,
) -> Verdict {
    let later = |verdict| if settled { verdict } else { Verdict::Wait };
    let view = screen.filter(|view| view.state != ComposerState::Booting);
    match (known, view) {
        (Known::Exact(draft), None) if draft.is_empty() => Verdict::Plain,
        (Known::Exact(draft), None) => Verdict::Protect(draft.clone()),
        (Known::Unknown, None) => Verdict::Unsure,
        (Known::Exact(draft), Some(view)) => match view.state {
            ComposerState::Occupied if draft.is_empty() => {
                // A turn takes messages as it always has; an idle box with text
                // nobody typed here (a prompt put back after an interrupt, a
                // menu) is not typed into.
                if busy {
                    Verdict::Plain
                } else {
                    later(Verdict::Unsure)
                }
            }
            ComposerState::Occupied if agrees(draft, &view.first_line) => {
                Verdict::Protect(draft.clone())
            }
            ComposerState::Occupied => later(Verdict::Unsure),
            _ if draft.is_empty() => Verdict::Plain,
            _ => later(Verdict::PlainReset),
        },
        (Known::Unknown, Some(view)) => match view.state {
            ComposerState::Occupied => later(Verdict::Unsure),
            _ => later(Verdict::PlainReset),
        },
    }
}

/// Whether the first row the screen draws can be the start of `draft`. Spaces
/// are left out (a tool wraps lines and draws tabs as spaces), and so is the
/// draft from its first long paste on (drawn as a placeholder).
fn agrees(draft: &Draft, first_line: &str) -> bool {
    let typed: String = draft
        .chars
        .iter()
        .zip(&draft.whole)
        .take_while(|(_, whole)| !**whole)
        .map(|(ch, _)| *ch)
        .take_while(|ch| *ch != '\n')
        .filter(|ch| !ch.is_whitespace())
        .collect();
    let drawn: String = first_line
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    typed.starts_with(&drawn) || drawn.starts_with(&typed)
}

fn key(key: Key, modes: InputModes) -> Vec<u8> {
    encode_key(
        KeyPress {
            key,
            mods: Mods::default(),
        },
        modes,
    )
    .unwrap_or_default()
}

/// The keys that empty the input line holding `draft`: Right to its end, then
/// one Backspace per character. A placeholder is never longer than the paste
/// it stands for, so the count is always enough, and a key past the end of the
/// line does nothing.
pub(crate) fn clear_keys(draft: &Draft, modes: InputModes) -> Vec<u8> {
    let right = key(Key::Right, modes);
    let mut bytes = right.repeat(draft.chars.len() - draft.cursor);
    bytes.extend(std::iter::repeat_n(BACKSPACE, draft.chars.len()));
    bytes
}

/// The bytes that put `draft` back into an empty input line, and the draft as
/// the tool then holds it. A leading `!` is typed, so it opens the shell mode
/// it opened when the user typed it (a pasted one does not); the rest is
/// pasted. Other leading characters are pasted: a typed `?` would open help. The cursor goes back where it was when the text after it is a
/// short single line (a long one may be drawn as a placeholder), and to the
/// end otherwise.
pub(crate) fn restore_keys(draft: &Draft, keys: DraftKeys, modes: InputModes) -> (Vec<u8>, Draft) {
    let paste = |chars: &[char]| -> Vec<u8> {
        let text: String = chars.iter().collect();
        if modes.bracketed_paste {
            encode_paste(&text, modes)
        } else if keys.multiline {
            // No bracket to hold a newline: send it as Shift+Enter.
            text.replace('\n', "\x1b\r").into_bytes()
        } else {
            text.into_bytes()
        }
    };
    let (before, after) = draft.chars.split_at(draft.cursor);
    let mut bytes = Vec::new();
    let mut now = Draft::default();
    let mut rest = before;
    if keys.multiline
        && let Some((first, tail)) = before.split_first()
        && *first == '!'
    {
        bytes.push(*first as u8);
        now.insert(&[*first], false);
        rest = tail;
    }
    for part in [rest, after] {
        if part.is_empty() {
            continue;
        }
        bytes.extend(paste(part));
        let whole = keys.collapses_pastes && (part.len() > SHORT_PASTE || part.contains(&'\n'));
        now.insert(part, whole);
    }
    let walk_back =
        !after.is_empty() && !now.whole[now.chars.len() - 1] && after.iter().all(|ch| simple(*ch));
    if walk_back {
        bytes.extend(key(Key::Left, modes).repeat(after.len()));
        now.cursor = before.len();
    }
    (bytes, now)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    const TUI: DraftKeys = DraftKeys {
        multiline: true,
        esc_enter_newline: true,
        ctrl_c_clears: false,
        collapses_pastes: true,
    };
    const SHELL: DraftKeys = DraftKeys {
        multiline: false,
        esc_enter_newline: false,
        ctrl_c_clears: true,
        collapses_pastes: false,
    };
    const BRACKETED: InputModes = InputModes {
        app_cursor: false,
        bracketed_paste: true,
        alt_screen: false,
        mouse_click: false,
        mouse_drag: false,
        mouse_motion: false,
        mouse_sgr: false,
        mouse_utf8: false,
        focus_reporting: false,
    };

    fn followed(keys: DraftKeys, writes: &[&[u8]]) -> Known {
        let mut tracker = DraftTracker::new(keys);
        for write in writes {
            tracker.feed(write);
        }
        tracker.known().clone()
    }

    fn exact(known: &Known) -> &Draft {
        match known {
            Known::Exact(draft) => draft,
            Known::Unknown => panic!("the draft is unknown"),
        }
    }

    fn typed(text: &str, cursor: usize) -> Draft {
        let chars: Vec<char> = text.chars().collect();
        Draft {
            whole: vec![false; chars.len()],
            chars,
            cursor,
        }
    }

    fn view(state: ComposerState, first_line: &str) -> ComposerView {
        ComposerView {
            state,
            first_line: first_line.to_owned(),
            rest: connecto_adapter::BoxRows::NotRead,
        }
    }

    #[test]
    fn typed_text_edits_and_moves_are_followed() {
        let known = followed(
            TUI,
            &[b"hello", b"\x7f", b"\x1b[D", b"\x1b[D", b"X", b"\x1bOC"],
        );
        let draft = exact(&known);
        assert_eq!(draft.text(), "heXll");
        assert_eq!(draft.cursor(), 4);
        let known = followed(TUI, &["héllo wörld".as_bytes(), b"\x01", b"> "]);
        assert_eq!(exact(&known).text(), "> héllo wörld");
        assert_eq!(exact(&known).cursor(), 2);
    }

    #[test]
    fn enter_submits_and_shift_enter_is_a_newline() {
        let known = followed(TUI, &[b"one", b"\x1b\r", b"two"]);
        assert_eq!(exact(&known).text(), "one\ntwo");
        let known = followed(TUI, &[b"one", b"\r", b"t"]);
        assert_eq!(exact(&known).text(), "t");
        // A backslash before Enter may make it a newline: not followed.
        assert_eq!(followed(TUI, &[b"one\\", b"\r"]), Known::Unknown);
        // Home on a line of several rows means different things per tool.
        assert_eq!(
            followed(TUI, &[b"a", b"\x1b\r", b"b", b"\x1b[H"]),
            Known::Unknown
        );
    }

    #[test]
    fn a_paste_is_followed_whole_and_a_long_one_is_a_placeholder() {
        let known = followed(TUI, &[b"see ", b"\x1b[200~line 1\rline 2\x1b[201~", b"!"]);
        let draft = exact(&known);
        assert_eq!(draft.text(), "see line 1\nline 2!");
        // Stepping back over the placeholder is not followed.
        assert_eq!(
            followed(TUI, &[b"\x1b[200~a\rb\x1b[201~", b"\x1b[D", b"\x1b[D"]),
            Known::Unknown
        );
        // A short paste is drawn as typed text.
        let known = followed(TUI, &[b"\x1b[200~short\x1b[201~", b"\x1b[D"]);
        assert_eq!(exact(&known).cursor(), 4);
    }

    #[test]
    fn keys_with_uncertain_effects_make_the_draft_unknown_until_enter() {
        for key in [
            &b"\t"[..],
            b"\x1b[A",
            b"\x1b[B",
            b"\x1b",
            b"\x17",
            b"\x0b",
            b"\x15",
            b"\x03",
            b"\x16",
            b"\x1b[1;5D",
            b"\x1bb",
        ] {
            assert_eq!(followed(TUI, &[b"draft", key]), Known::Unknown, "{key:?}");
        }
        let known = followed(TUI, &[b"draft", b"\t", b"more", b"\r", b"new"]);
        assert_eq!(exact(&known).text(), "new");
        // Focus reports and clicks type nothing; a wheel may walk the history.
        let known = followed(TUI, &[b"ab", b"\x1b[I", b"\x1b[<0;3;4M", b"\x1b[<0;3;4m"]);
        assert_eq!(exact(&known).text(), "ab");
        assert_eq!(followed(TUI, &[b"ab", b"\x1b[<64;3;4M"]), Known::Unknown);
        // An accent joined to the letter before it is stepped over with it.
        assert_eq!(
            followed(TUI, &["e\u{301}".as_bytes(), b"\x7f"]),
            Known::Unknown
        );
    }

    #[test]
    fn a_shell_line_ends_at_ctrl_c_and_a_pasted_newline_is_not_followed() {
        let known = followed(SHELL, &[b"ls -la", b"\x03", b"pwd"]);
        assert_eq!(exact(&known).text(), "pwd");
        assert_eq!(
            followed(SHELL, &[b"\x1b[200~a\rb\x1b[201~"]),
            Known::Unknown
        );
        assert_eq!(
            followed(SHELL, &[b"\x1b[200~a\tb\x1b[201~"]),
            Known::Unknown
        );
        assert_eq!(followed(SHELL, &[b"x", b"\x1b\r"]), Known::Unknown);
    }

    #[test]
    fn an_empty_line_is_typed_into_as_always() {
        let empty = Known::Exact(Draft::default());
        assert_eq!(decide(&empty, None, false, false), Verdict::Plain);
        let idle = view(ComposerState::Empty, "");
        assert_eq!(decide(&empty, Some(&idle), false, false), Verdict::Plain);
        let working = view(ComposerState::Busy, "");
        assert_eq!(decide(&empty, Some(&working), true, false), Verdict::Plain);
    }

    #[test]
    fn a_draft_the_screen_agrees_with_is_moved() {
        let draft = typed("fix the bug in main.rs", 7);
        let known = Known::Exact(draft.clone());
        let shown = view(ComposerState::Occupied, "fix the bug in");
        assert_eq!(
            decide(&known, Some(&shown), false, false),
            Verdict::Protect(draft.clone())
        );
        // A screen that cannot be read: what was typed decides.
        assert_eq!(
            decide(&known, None, false, false),
            Verdict::Protect(draft.clone())
        );
        // The screen has not drawn the last keys yet.
        let behind = view(ComposerState::Empty, "");
        assert_eq!(decide(&known, Some(&behind), false, false), Verdict::Wait);
        assert_eq!(
            decide(&known, Some(&behind), false, true),
            Verdict::PlainReset
        );
        // Something else is in the box.
        let other = view(ComposerState::Occupied, "Do you want to proceed?");
        assert_eq!(decide(&known, Some(&other), false, true), Verdict::Unsure);
    }

    #[test]
    fn a_multi_line_draft_with_a_paste_agrees_with_its_placeholder() {
        let known = followed(
            TUI,
            &[
                b"look: ",
                b"\x1b[200~a\rb\rc\x1b[201~",
                b"\x1b\r",
                b"thanks",
            ],
        );
        let shown = view(ComposerState::Occupied, "look: [Pasted text #1 +2 lines]");
        assert!(matches!(
            decide(&known, Some(&shown), false, true),
            Verdict::Protect(_)
        ));
    }

    #[test]
    fn an_unknown_draft_or_text_nobody_typed_is_never_touched() {
        assert_eq!(decide(&Known::Unknown, None, false, true), Verdict::Unsure);
        let shown = view(ComposerState::Occupied, "recalled");
        assert_eq!(
            decide(&Known::Unknown, Some(&shown), true, true),
            Verdict::Unsure
        );
        let empty = Known::Exact(Draft::default());
        assert_eq!(decide(&empty, Some(&shown), false, true), Verdict::Unsure);
        assert_eq!(decide(&empty, Some(&shown), false, false), Verdict::Wait);
        // A turn takes the message as it always has.
        assert_eq!(decide(&empty, Some(&shown), true, true), Verdict::Plain);
        // The screen shows the box empty: the draft is gone.
        let idle = view(ComposerState::Empty, "");
        assert_eq!(
            decide(&Known::Unknown, Some(&idle), false, true),
            Verdict::PlainReset
        );
    }

    #[test]
    fn clearing_walks_to_the_end_and_deletes_every_character() {
        let bytes = clear_keys(&typed("abcd", 1), BRACKETED);
        assert_eq!(bytes, b"\x1b[C\x1b[C\x1b[C\x7f\x7f\x7f\x7f");
        let app = InputModes {
            app_cursor: true,
            ..BRACKETED
        };
        assert_eq!(clear_keys(&typed("ab", 1), app), b"\x1bOC\x7f\x7f");
    }

    #[test]
    fn restoring_pastes_the_draft_and_walks_the_cursor_back() {
        let (bytes, now) = restore_keys(&typed("hello world", 6), TUI, BRACKETED);
        assert_eq!(
            bytes,
            b"\x1b[200~hello \x1b[201~\x1b[200~world\x1b[201~\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D"
        );
        assert_eq!(now.text(), "hello world");
        assert_eq!(now.cursor(), 6);
        // At the end: no walk back.
        let (bytes, now) = restore_keys(&typed("!ls", 3), TUI, BRACKETED);
        assert_eq!(bytes, b"!\x1b[200~ls\x1b[201~");
        assert_eq!(now.cursor(), 3);
    }

    #[test]
    fn restoring_a_multi_line_draft_keeps_its_lines_and_ends_at_the_end() {
        let draft = exact(&followed(
            TUI,
            &[b"one", b"\x1b\r", b"two", b"\x1b\r", b"three", b"\x1b[D"],
        ))
        .clone();
        assert_eq!(draft.cursor(), 12);
        let (bytes, now) = restore_keys(&draft, TUI, BRACKETED);
        assert_eq!(
            bytes,
            b"\x1b[200~one\rtwo\rthre\x1b[201~\x1b[200~e\x1b[201~\x1b[D"
        );
        assert_eq!(now.text(), "one\ntwo\nthree");
        assert_eq!(now.cursor(), 12);
        // The text after the cursor spans lines: the cursor ends at the end.
        let draft = typed("a\nb", 0);
        let (_, now) = restore_keys(&draft, TUI, BRACKETED);
        assert_eq!(now.cursor(), 3);
    }

    #[test]
    fn a_shell_line_is_typed_back() {
        let (bytes, now) = restore_keys(&typed("git status", 3), SHELL, InputModes::default());
        assert_eq!(
            bytes,
            b"git status\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D".to_vec()
        );
        assert_eq!(now.cursor(), 3);
    }

    #[test]
    fn clearing_then_restoring_gives_the_same_draft_back() {
        // The user's keys, then Connecto's: the line ends as it started.
        let mut tracker = DraftTracker::new(TUI);
        tracker.feed(b"keep this");
        tracker.feed(b"\x1b[D\x1b[D");
        let draft = exact(tracker.known()).clone();
        tracker.feed(&clear_keys(&draft, BRACKETED));
        assert_eq!(exact(tracker.known()).text(), "");
        let (bytes, now) = restore_keys(&draft, TUI, BRACKETED);
        tracker.feed(&bytes);
        assert_eq!(exact(tracker.known()), &draft);
        assert_eq!(now.text(), draft.text());
        assert_eq!(now.cursor(), draft.cursor());
    }
}
