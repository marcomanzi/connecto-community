//! Which of a project chat's messages answer one prompt, by the rule the
//! phone's call uses (`repliesAfter` in the Flutter app's `call.dart`), on
//! absolute positions: a host keeps answering a request after the history
//! has dropped its oldest messages, and a prompt claimed by one request is
//! never matched by another.
//!
//! A message's position is its place among everything pushed to the history:
//! [`ChatHistory::revision`] counts the pushes, so `messages[i]` is at
//! `revision - messages.len() + i`.

use crate::session::ChatHistory;

/// The position of the history's first message.
#[must_use]
pub fn first_seq(history: &ChatHistory) -> u64 {
    history
        .revision
        .saturating_sub(history.messages.len() as u64)
}

/// The messages at `from` and after, each with its position.
pub fn messages_from(history: &ChatHistory, from: u64) -> impl Iterator<Item = (u64, bool, &str)> {
    let first = first_seq(history);
    history
        .messages
        .iter()
        .enumerate()
        .map(move |(i, (user, text))| (first + i as u64, *user, text.as_str()))
        .filter(move |(seq, _, _)| *seq >= from)
}

/// `text` without the wrapper Claude Code puts around a long paste,
/// `<pasted_content id="x">…</pasted_content id="x">`, which its history
/// keeps; as it is when the two ids differ.
#[must_use]
pub fn unwrap_paste(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("<pasted_content id=\"") else {
        return text;
    };
    let Some((id, body)) = rest.split_once("\">") else {
        return text;
    };
    if id.contains('"') {
        return text;
    }
    let close = format!("</pasted_content id=\"{id}\">");
    body.strip_suffix(close.as_str()).unwrap_or(text)
}

/// `s` as prompts are compared: lower case, letters, digits and
/// apostrophes, single spaces.
#[must_use]
pub fn plain(s: &str) -> String {
    let lowered: String = s
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect();
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether the history's prompt `message` is `sent`, as typed or inside a
/// paste wrapper.
#[must_use]
pub fn same_prompt(message: &str, sent: &str) -> bool {
    plain(unwrap_paste(message)) == plain(sent)
}

/// The position of the first prompt at `from` or after that is `sent`.
#[must_use]
pub fn find_prompt(history: &ChatHistory, from: u64, sent: &str) -> Option<u64> {
    messages_from(history, from)
        .find(|(_, user, text)| *user && same_prompt(text, sent))
        .map(|(seq, _, _)| seq)
}

/// The agent's messages from `from` on, up to the next prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Replies {
    pub messages: Vec<String>,
    /// A prompt came after them: what follows answers someone else.
    pub next_prompt: bool,
}

impl Replies {
    #[must_use]
    pub fn joined(&self) -> Option<String> {
        (!self.messages.is_empty()).then(|| self.messages.join("\n\n"))
    }
}

/// The agent's non-empty messages at `from` and after, up to the next
/// prompt.
#[must_use]
pub fn replies_from(history: &ChatHistory, from: u64) -> Replies {
    let mut out = Replies::default();
    for (_, user, text) in messages_from(history, from) {
        if user {
            out.next_prompt = true;
            break;
        }
        if !text.trim().is_empty() && !is_tagged_report(text) {
            out.messages.push(text.to_owned());
        }
    }
    out
}

/// Whether `message` begins with a board report that names a task
/// (`Task complete [t-1a2b].`, `In coda (t-9).`, as `taskMarker` in the
/// app's `lib/call.dart` reads it). A voice request is no task of the
/// board, so such a report is another task's, sent as the board moves on:
/// never part of its reply (`_forThisCall`).
#[must_use]
pub fn is_tagged_report(message: &str) -> bool {
    const WORDS: [&str; 8] = [
        "task complete",
        "attività completata",
        "compito completato",
        "task failed",
        "compito non riuscito",
        "attività non riuscita",
        "queued",
        "in coda",
    ];
    let rest = message.trim_start_matches(|c: char| c.is_whitespace() || "*_#>".contains(c));
    let lower = rest.to_lowercase();
    let Some(word) = WORDS.iter().find(|w| lower.starts_with(**w)) else {
        return false;
    };
    // `to_lowercase` keeps these words' byte lengths.
    let after = lower[word.len()..].trim_start();
    let Some(open) = after.chars().next().filter(|c| *c == '[' || *c == '(') else {
        return false;
    };
    let inner = &after[open.len_utf8()..];
    let Some(id) = inner.strip_prefix("t-") else {
        return false;
    };
    let len = id
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .count();
    if len == 0 {
        return false;
    }
    let Some(close) = id[len..].chars().next().filter(|c| *c == ']' || *c == ')') else {
        return false;
    };
    // A statement: "Task complete [t-1]?" is not one.
    let tail = id[len + close.len_utf8()..].trim_start();
    tail.is_empty() || tail.starts_with(['.', '!', ':', '*', '_'])
}

/// The reply to `sent`, a prompt at `from` or after: its position and the
/// agent's messages after it. `None` while the prompt is not there.
#[must_use]
pub fn reply_after(history: &ChatHistory, from: u64, sent: &str) -> Option<(u64, Replies)> {
    let at = find_prompt(history, from, sent)?;
    Some((at, replies_from(history, at + 1)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A history of `messages` after `dropped` older ones fell off.
    fn history(dropped: u64, messages: &[(bool, &str)]) -> ChatHistory {
        ChatHistory {
            revision: dropped + messages.len() as u64,
            messages: messages
                .iter()
                .map(|(u, t)| (*u, (*t).to_owned()))
                .collect(),
        }
    }

    fn joined(h: &ChatHistory, from: u64, sent: &str) -> Option<String> {
        reply_after(h, from, sent).and_then(|(_, r)| r.joined())
    }

    #[test]
    fn finds_a_long_prompt_kept_inside_its_paste_wrapper() {
        let sent = "Check this request.\n\nThe call so far:\nPerson: Ciao";
        let h = history(
            0,
            &[
                (
                    true,
                    &format!(
                        "\n\n<pasted_content id=\"0231\">\n{sent}\n</pasted_content id=\"0231\">\n"
                    ),
                ),
                (false, "QUESTIONS\n1. Quale store?"),
            ],
        );
        assert_eq!(
            joined(&h, 0, sent).as_deref(),
            Some("QUESTIONS\n1. Quale store?")
        );
        let mismatched = "<pasted_content id=\"a\">x</pasted_content id=\"b\">";
        assert_eq!(unwrap_paste(mismatched), mismatched);
    }

    #[test]
    fn takes_the_agents_messages_after_the_new_prompt() {
        let h = history(
            0,
            &[
                (true, "old"),
                (false, "old reply"),
                (true, "Run the tests."),
                (false, "Running."),
                (false, "All green."),
            ],
        );
        assert_eq!(
            joined(&h, 2, "run the tests").as_deref(),
            Some("Running.\n\nAll green.")
        );
    }

    #[test]
    fn is_none_until_the_prompt_and_a_reply_arrive() {
        assert_eq!(joined(&history(0, &[]), 0, "hi"), None);
        assert_eq!(
            joined(&history(0, &[(true, "old"), (true, "hi")]), 1, "hi"),
            None
        );
        let other = history(0, &[(true, "something else"), (false, "reply")]);
        assert_eq!(joined(&other, 0, "hi"), None);
    }

    #[test]
    fn never_answers_with_the_reply_to_an_earlier_identical_prompt() {
        let old = [(true, "run the tests"), (false, "Old result.")];
        assert_eq!(joined(&history(0, &old), 2, "run the tests"), None);
        let h = history(
            0,
            &[
                old[0],
                old[1],
                (true, "run the tests"),
                (false, "New result."),
            ],
        );
        assert_eq!(
            joined(&h, 2, "run the tests").as_deref(),
            Some("New result.")
        );
        // Two requests with the same words: the second looks after the
        // prompt the first claimed, and so never takes its reply.
        let twice = history(
            0,
            &[
                (true, "run the tests"),
                (false, "First."),
                (true, "run the tests"),
                (false, "Second."),
            ],
        );
        let (first, r1) = reply_after(&twice, 0, "run the tests").unwrap();
        let (second, r2) = reply_after(&twice, first + 1, "run the tests").unwrap();
        assert_eq!((first, second), (0, 2));
        assert_eq!(r1.joined().as_deref(), Some("First."));
        assert_eq!(r2.joined().as_deref(), Some("Second."));
    }

    #[test]
    fn stops_at_the_next_prompt() {
        let h = history(
            0,
            &[
                (true, "hi"),
                (false, "hello"),
                (true, "from the desktop"),
                (false, "desktop answer"),
            ],
        );
        let (_, r) = reply_after(&h, 0, "hi").unwrap();
        assert_eq!(r.joined().as_deref(), Some("hello"));
        assert!(r.next_prompt);
    }

    #[test]
    fn keeps_positions_when_the_oldest_messages_were_dropped() {
        // Registered at position 2 (two messages then); since then the
        // history dropped three from its front.
        let h = history(3, &[(true, "hi"), (false, "hello")]);
        assert_eq!(first_seq(&h), 3);
        let (at, r) = reply_after(&h, 2, "hi").unwrap();
        assert_eq!(at, 3);
        assert_eq!(r.joined().as_deref(), Some("hello"));
        // A claimed position is never found again.
        assert_eq!(reply_after(&h, at + 1, "hi"), None);
    }

    #[test]
    fn another_tasks_board_report_is_no_part_of_the_reply() {
        let h = history(
            0,
            &[
                (true, "are the tests green?"),
                (false, "**Task complete [t-1a2b].** Merged the docs."),
                (false, "Yes, all 312 pass."),
                (false, "In coda (t-9): next on the board."),
            ],
        );
        let (_, r) = reply_after(&h, 0, "are the tests green?").unwrap();
        assert_eq!(r.joined().as_deref(), Some("Yes, all 312 pass."));
        // Untagged, questioning, or not at the start: the agent's own words.
        assert!(!is_tagged_report("Task complete."));
        assert!(!is_tagged_report("Task complete [t-1]? Not yet."));
        assert!(!is_tagged_report("The task complete [t-1]."));
        assert!(is_tagged_report("> attività completata [t-x2]"));
    }

    #[test]
    fn plain_ignores_case_punctuation_and_spacing() {
        assert_eq!(plain("  Run   the TESTS!\n"), "run the tests");
        assert_eq!(plain("l'agente è qui"), "l'agente è qui");
    }
}
