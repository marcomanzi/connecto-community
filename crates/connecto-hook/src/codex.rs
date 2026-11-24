//! What Connecto's Codex `SessionStart` hook hands the model.
//!
//! Codex 0.167 adds a `SessionStart` hook's `additionalContext` as a developer
//! message of its own, beside the configured `developer_instructions` and the
//! project's AGENTS.md, so it is the one channel that adds to them (ADR 0046).
//! The hook runs with a `source` of `startup`, `resume`, `clear`, `compact`
//! or `fork`, and its answer is kept in the thread like any other message.
//!
//! Two texts go this way, in one message, the bridge text first:
//! - the bridge text (how to use the `connecto` command), which names the
//!   terminal and so differs per launch: the launch puts it in
//!   [`BRIDGE_TEXT_ENV`], in Codex's own environment, which a session's
//!   commands cannot change for Codex;
//! - the built-in package's end-of-task status recap, compiled into this
//!   helper; the launch only sets [`RECAP_ENV`] to `1` in a project that has
//!   the package.
//!
//! On `resume` the message is left out only when the most recent one
//! Connecto gave the thread (after its last compaction) is the same, byte for
//! byte; a different one is replaced, with [`REPLACES_NOTE`] in front. Every
//! other source, and a rollout that cannot be read or has no such message
//! near its end, gets the message: an extra copy now and then is better than
//! missing or stale instructions.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{Value, json};

/// Set to `1` by the launch to turn the recap on. The adapter sets the same
/// name.
pub const RECAP_ENV: &str = "CONNECTO_CODEX_RECAP";

/// The bridge text itself, set by the launch when the bridge is on. The
/// adapter sets the same name.
pub const BRIDGE_TEXT_ENV: &str = "CONNECTO_CODEX_BRIDGE_TEXT";

/// How every bridge text starts (the terminal's id follows): how the hook
/// knows a message of Connecto's in the thread.
pub const BRIDGE_TEXT_START: &str = "You are running in terminal \"";

/// The recap, as the built-in package's Claude rule has it.
pub const STATUS_RECAP: &str =
    include_str!("../../connecto-packages/builtin/both/.claude/rules/connecto-status.md");

/// Put in front of the message on a resume that hands it over again.
pub const REPLACES_NOTE: &str =
    "These Connecto instructions replace any given earlier in this conversation.";

/// How much of a rollout's end is read, at most, to find Connecto's latest
/// message. The rollout is read backwards from its end in [`CHUNK`]s.
pub const SCAN_LIMIT: u64 = 8 * 1024 * 1024;
const CHUNK: u64 = 256 * 1024;

/// The kind Codex 0.167 gives a hook's context in a rollout.
const HOOK_CONTEXT_KIND: &str = "hooks.additional_context";

/// What one rollout line says, read backwards.
enum Line {
    /// A message of Connecto's hook, with its text.
    Connecto(String),
    /// A compaction: what came before it is not what the model has.
    Compacted,
    Other,
}

/// A text of Connecto's: it starts as the bridge text, the note, or the recap
/// (a project with the package and the bridge off) does.
fn is_connecto(text: &str) -> bool {
    text.starts_with(BRIDGE_TEXT_START)
        || text.starts_with(REPLACES_NOTE)
        || text.starts_with(STATUS_RECAP.trim())
}

fn classify(line: &[u8]) -> Line {
    let has = |needle: &[u8]| line.windows(needle.len()).any(|w| w == needle);
    if !has(b"\"compacted\"") && !has(HOOK_CONTEXT_KIND.as_bytes()) {
        return Line::Other;
    }
    let Ok(record) = serde_json::from_slice::<Value>(line) else {
        return Line::Other;
    };
    match record["type"].as_str() {
        Some("compacted") => Line::Compacted,
        Some("response_item") => {
            let message = &record["payload"];
            if message["type"] != "message" || message["role"] != "developer" {
                return Line::Other;
            }
            let kinds =
                &message["internal_chat_message_metadata_passthrough"]["content_item_kinds"];
            message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter(|(at, _)| kinds[*at] == HOOK_CONTEXT_KIND)
                .filter_map(|(_, part)| part["text"].as_str())
                .filter(|text| is_connecto(text))
                .last()
                .map_or(Line::Other, |text| Line::Connecto(text.to_owned()))
        }
        _ => Line::Other,
    }
}

/// The most recent message Connecto's hook gave the thread whose rollout is
/// at `path`, when the model still has it: found within the last
/// [`SCAN_LIMIT`] bytes, read backwards, with no compaction after it. `None`
/// otherwise (no such message there, a compaction first, an unreadable file).
#[must_use]
pub fn latest_connecto_message(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let floor = len.saturating_sub(SCAN_LIMIT);
    let mut end = len;
    // The start of a line whose beginning is in an earlier chunk.
    let mut rest: Vec<u8> = Vec::new();
    while end > floor {
        let start = end.saturating_sub(CHUNK).max(floor);
        let mut buf = vec![0; usize::try_from(end - start).ok()?];
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut buf).ok()?;
        buf.extend_from_slice(&rest);
        let mut lines: Vec<&[u8]> = buf.split(|byte| *byte == b'\n').collect();
        // The first piece may be the end of a longer line, unless the file
        // starts here.
        let head = if start > 0 { lines.remove(0) } else { &[] };
        for line in lines.iter().rev() {
            match classify(line) {
                Line::Connecto(text) => return Some(text),
                Line::Compacted => return None,
                Line::Other => {}
            }
        }
        if start == 0 {
            return None;
        }
        rest = head.to_vec();
        end = start;
    }
    None
}

/// One `SessionStart`, as Codex and the launch describe it.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionStart<'a> {
    /// The value of [`RECAP_ENV`].
    pub recap_flag: Option<&'a str>,
    /// The value of [`BRIDGE_TEXT_ENV`].
    pub bridge: Option<&'a str>,
    /// The payload's `source`.
    pub source: Option<&'a str>,
    /// On `resume`: the thread's latest message from Connecto
    /// ([`latest_connecto_message`]).
    pub latest: Option<&'a str>,
}

impl SessionStart<'_> {
    /// The message: the bridge text, then the recap. `None` when there is
    /// neither.
    fn message(&self) -> Option<String> {
        let parts: Vec<&str> = [
            self.bridge.map(str::trim).filter(|text| !text.is_empty()),
            (self.recap_flag == Some("1")).then(|| STATUS_RECAP.trim()),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// The hook's answer, `None` when there is nothing to hand over.
    #[must_use]
    pub fn output(&self) -> Option<Vec<u8>> {
        let mut text = self.message()?;
        if self.source == Some("resume") {
            let latest = self.latest.map(|latest| {
                latest
                    .strip_prefix(REPLACES_NOTE)
                    .map_or(latest, |rest| rest.trim_start_matches('\n'))
            });
            if latest == Some(text.as_str()) {
                return None;
            }
            text = format!("{REPLACES_NOTE}\n\n{text}");
        }
        let value = json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": text,
            }
        });
        serde_json::to_vec(&value).ok()
    }
}

/// The hook's answer for a new thread: the bridge text (`bridge`, the value
/// of [`BRIDGE_TEXT_ENV`]), then the recap when `flag` (the value of
/// [`RECAP_ENV`]) turns it on. `None` when there is neither.
#[must_use]
pub fn session_start_output(flag: Option<&str>, bridge: Option<&str>) -> Option<Vec<u8>> {
    SessionStart {
        recap_flag: flag,
        bridge,
        ..SessionStart::default()
    }
    .output()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn context(out: &[u8]) -> String {
        let value: serde_json::Value = serde_json::from_slice(out).unwrap();
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "SessionStart");
        value["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    const BRIDGE: &str =
        "You are running in terminal \"codex-1\" of Connecto.\n`connecto list` — ünïcode \\ $HOME";
    const MEMORY_ON: &str =
        "You are running in terminal \"codex-1\" of Connecto.\nThis project has a project memory.";

    fn both(bridge: &str) -> String {
        format!("{bridge}\n\n{}", STATUS_RECAP.trim())
    }

    /// A rollout line as Codex 0.167 records a hook's context.
    fn hook_line(text: &str) -> String {
        json!({"timestamp": "t", "type": "response_item", "payload": {
            "type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": text}],
            "internal_chat_message_metadata_passthrough": {
                "content_item_kinds": [HOOK_CONTEXT_KIND]}}})
        .to_string()
    }

    fn other_line(text: &str) -> String {
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": text}]}})
        .to_string()
    }

    fn compacted_line() -> String {
        json!({"type": "compacted", "payload": {"message": "summary", "replacement_history": []}})
            .to_string()
    }

    fn latest_in(lines: &[String]) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        latest_connecto_message(&path)
    }

    fn answer(source: &str, bridge: &str, latest: Option<&str>) -> Option<String> {
        SessionStart {
            recap_flag: Some("1"),
            bridge: Some(bridge),
            source: Some(source),
            latest,
        }
        .output()
        .map(|out| context(&out))
    }

    #[test]
    fn the_flag_turns_on_the_compiled_recap_as_codexs_session_start_answer() {
        let text = context(&session_start_output(Some("1"), None).unwrap());
        assert_eq!(text, STATUS_RECAP.trim());
        assert!(text.contains("**Waiting on user**"));
        for off in [None, Some(""), Some("0"), Some("/some/file")] {
            assert_eq!(session_start_output(off, None), None, "{off:?}");
        }
    }

    /// The bridge text comes first, whole, with the recap after it when the
    /// project has the package; an empty one counts as none.
    #[test]
    fn the_bridge_text_comes_before_the_recap() {
        let text = context(&session_start_output(Some("1"), Some(BRIDGE)).unwrap());
        assert_eq!(text, both(BRIDGE));
        let text = context(&session_start_output(None, Some(BRIDGE)).unwrap());
        assert_eq!(text, BRIDGE);
        assert_eq!(session_start_output(None, Some(" \n")), None);
    }

    /// A resume leaves the message out only when the thread's latest one from
    /// Connecto is the same, byte for byte (with or without the note in
    /// front); otherwise it is handed over with the note. Every other source
    /// gets it as it is.
    #[test]
    fn a_resume_skips_only_a_byte_identical_latest_message() {
        let same = both(BRIDGE);
        let noted = format!("{REPLACES_NOTE}\n\n{same}");
        assert_eq!(answer("resume", BRIDGE, Some(&same)), None);
        assert_eq!(answer("resume", BRIDGE, Some(&noted)), None);
        // Different, missing, or only almost the same: handed over again.
        let changed = both(MEMORY_ON);
        for latest in [Some(changed.as_str()), None, Some(BRIDGE), Some(&same[1..])] {
            assert_eq!(
                answer("resume", BRIDGE, latest),
                Some(noted.clone()),
                "{latest:?}"
            );
        }
        for source in ["startup", "clear", "compact", "fork"] {
            assert_eq!(answer(source, BRIDGE, Some(&same)), Some(same.clone()));
        }
    }

    /// Project memory turned on, then off: the resume after it hands over the
    /// text without memory, although an older message with that very text is
    /// still in the thread; the next resume adds nothing.
    #[test]
    fn a_capability_turned_off_is_not_hidden_by_an_older_identical_text() {
        let base = both(BRIDGE);
        let lines = vec![
            hook_line(&base),
            other_line("hi"),
            hook_line(&format!("{REPLACES_NOTE}\n\n{}", both(MEMORY_ON))),
            other_line("again"),
        ];
        let latest = latest_in(&lines);
        let text = answer("resume", BRIDGE, latest.as_deref()).unwrap();
        assert_eq!(text, format!("{REPLACES_NOTE}\n\n{base}"));
        let mut lines = lines;
        lines.push(hook_line(&text));
        assert_eq!(answer("resume", BRIDGE, latest_in(&lines).as_deref()), None);
    }

    /// Only Connecto's own hook messages count, only after the last
    /// compaction, and only near the end of the rollout.
    #[test]
    fn the_latest_message_is_read_backwards_and_bounded() {
        let base = both(BRIDGE);
        assert_eq!(
            latest_in(&[hook_line(&base), other_line("hi")]).as_deref(),
            Some(base.as_str())
        );
        // Another hook's context, or the same text as a user message, is not one.
        let user_hook = hook_line("some other hook's context");
        assert_eq!(latest_in(&[user_hook]), None);
        assert_eq!(latest_in(&[other_line(&base)]), None);
        // A compaction after it: the model no longer has it.
        assert_eq!(latest_in(&[hook_line(&base), compacted_line()]), None);
        assert_eq!(
            latest_in(&[compacted_line(), hook_line(&base)]).as_deref(),
            Some(base.as_str())
        );
        // Lines across chunk borders are put back together.
        let filler = other_line(&"x".repeat(usize::try_from(CHUNK).unwrap() + 17));
        assert_eq!(
            latest_in(&[hook_line(&base), filler.clone(), filler.clone()]).as_deref(),
            Some(base.as_str())
        );
        // Further back than the limit: not found (the message is handed over).
        let big = other_line(&"y".repeat(usize::try_from(SCAN_LIMIT).unwrap()));
        assert_eq!(latest_in(&[hook_line(&base), big]), None);
        assert_eq!(
            latest_connecto_message(Path::new("/nonexistent/rollout")),
            None
        );
    }
}
