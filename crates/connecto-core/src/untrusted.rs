//! A marker for data that came out of a session.

use std::fmt;

/// Text an agent or its output produced. It is data and never an instruction.
/// There is no `Display`, and `Debug` hides the content, so it cannot reach a
/// log by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Untrusted<T>(T);

impl<T> Untrusted<T> {
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// The raw value. The caller takes responsibility for treating it as data.
    #[must_use]
    pub const fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Untrusted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Untrusted(..)")
    }
}

/// Text after redaction. Only `connecto-redact` should construct this: holding
/// one is the claim that the pipeline ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    #[must_use]
    pub const fn assume(value: T) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(&self) -> &T {
        &self.0
    }
}

/// Largest text an event may carry. 256 queued events of a full 256 KiB frame
/// would be 64 MiB against a 48 MiB session budget.
pub const MAX_EVENT_TEXT: usize = 64 * 1024;

/// Largest reply a turn end carries: the hook frame limit. A reply that fits a
/// frame is held whole, so a long reply is never returned as `ok` with its end
/// missing (section 5.9).
pub const MAX_REPLY_TEXT: usize = 256 * 1024;

/// Text from a hook event, capped when parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventText {
    text: Untrusted<String>,
    truncated: bool,
}

impl EventText {
    #[must_use]
    pub fn new(text: String) -> Self {
        Self::capped(text, MAX_EVENT_TEXT)
    }

    /// As [`EventText::new`] with the larger cap that a turn-end reply gets.
    #[must_use]
    pub fn reply(text: String) -> Self {
        Self::capped(text, MAX_REPLY_TEXT)
    }

    fn capped(mut text: String, cap: usize) -> Self {
        let truncated = text.len() > cap;
        if truncated {
            let mut end = cap;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        Self {
            text: Untrusted::new(text),
            truncated,
        }
    }

    #[must_use]
    pub const fn expose(&self) -> &String {
        self.text.expose()
    }

    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_shows_the_content() {
        let value = Untrusted::new("secret".to_owned());
        assert_eq!(format!("{value:?}"), "Untrusted(..)");
        assert_eq!(value.expose(), "secret");
    }

    #[test]
    fn event_text_is_capped_on_a_character_boundary_and_marked() {
        let text = EventText::new("é".repeat(MAX_EVENT_TEXT));
        assert!(text.truncated());
        assert!(text.expose().len() <= MAX_EVENT_TEXT);
        assert!(!EventText::new("short".to_owned()).truncated());
        let exact = EventText::new("a".repeat(MAX_EVENT_TEXT));
        assert!(!exact.truncated());
    }
}
