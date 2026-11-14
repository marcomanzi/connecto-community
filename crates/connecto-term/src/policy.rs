//! What child-controlled escape sequences may do outside the screen grid.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use alacritty_terminal::event::{Event, EventListener};

use crate::color;
use crate::emulator::Responder;

/// Longest title kept, in characters.
pub const MAX_TITLE_CHARS: usize = 128;

/// Make a child-supplied title safe to show in the UI chrome.
///
/// Removes control characters and the invisible formatting characters that can
/// reorder or hide text, then limits the length.
#[must_use]
pub fn sanitize_title(title: &str) -> String {
    title
        .chars()
        .filter(|c| !c.is_control() && !is_invisible_format(*c))
        .take(MAX_TITLE_CHARS)
        .collect()
}

fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
    )
}

/// State shared between the listener and snapshots.
#[derive(Default)]
pub(crate) struct PolicyState {
    pub(crate) title: Mutex<Option<String>>,
    pub(crate) dropped_clipboard: AtomicU64,
}

/// Applies the escape policy to every event the emulator raises.
pub(crate) struct PolicyListener {
    pub(crate) responder: Responder,
    pub(crate) state: Arc<PolicyState>,
}

impl PolicyListener {
    fn set_title(&self, title: Option<String>) {
        *self
            .state
            .title
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = title;
    }
}

impl EventListener for PolicyListener {
    fn send_event(&self, event: Event) {
        match event {
            // Device status, device attributes and mode reports. The emulator
            // builds these itself; they contain no child-supplied text.
            Event::PtyWrite(reply) => (self.responder)(reply.into_bytes()),
            // TUIs ask for the background colour to pick a light or dark theme.
            Event::ColorRequest(index, format) => {
                (self.responder)(format(color::reported_color(index)).into_bytes());
            }
            Event::Title(title) => {
                let title = sanitize_title(&title);
                self.set_title((!title.is_empty()).then_some(title));
            }
            Event::ResetTitle => self.set_title(None),
            // OSC 52 is off in the emulator config. This is the second layer:
            // a clipboard event is dropped here even if it ever gets raised.
            Event::ClipboardStore(..) | Event::ClipboardLoad(..) => {
                self.state.dropped_clipboard.fetch_add(1, Ordering::Relaxed);
            }
            // Pixel sizes are not needed by any supported TUI. The rest have
            // no effect in this terminal.
            Event::TextAreaSizeRequest(_)
            | Event::MouseCursorDirty
            | Event::CursorBlinkingChange
            | Event::Wakeup
            | Event::Bell
            | Event::Exit
            | Event::ChildExit(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_lose_control_characters() {
        assert_eq!(sanitize_title("a\x1b[31mb\x07\n"), "a[31mb");
    }

    #[test]
    fn titles_lose_bidi_overrides_and_invisible_characters() {
        assert_eq!(sanitize_title("safe\u{202E}gnp.exe\u{200B}"), "safegnp.exe");
        assert_eq!(sanitize_title("a\u{2066}b\u{FEFF}c"), "abc");
    }

    #[test]
    fn titles_are_limited_in_length() {
        let long = "é".repeat(MAX_TITLE_CHARS * 2);
        assert_eq!(sanitize_title(&long).chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn ordinary_titles_survive() {
        assert_eq!(sanitize_title("vim — notes.md"), "vim — notes.md");
    }
}
