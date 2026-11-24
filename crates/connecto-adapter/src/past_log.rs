//! A closed session's own log, read back as its completed turns: the prompt
//! the tool recorded as the user's message, and the turn's final reply (never
//! the commentary between tool calls). For project memory's initial build.
//!
//! Read-only and pure: the caller reads the file (bounded, in pieces) and
//! gives the text here. A turn that did not complete (interrupted, failed,
//! cut, or still open at the end of the text) is left out. Nothing here says
//! who typed a prompt: a user-role message may still be a relayed or injected
//! one, which the caller tells apart by Connecto's markers and its history.

use std::fs;
use std::path::{Path, PathBuf};

use connecto_core::ToolKind;
use serde_json::Value;

use crate::claude::record_text;
use crate::codex::{rollout_of, user_item_text};
use crate::spec::under_profile;

/// Folders of a Claude profile's `projects/` looked at, at most.
const MAX_PROJECT_DIRS: usize = 10_000;

/// One completed turn of a past session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedExchange {
    /// The user-role message that opened the turn, as the tool logged it.
    pub prompt: String,
    /// The record has the shape of a message a person typed, and nothing
    /// marks it as written by the tool or Connecto (a compaction summary,
    /// meta or hook context, a command expansion, a tool result, a system
    /// reminder...). Fail closed: anything not positively recognised is
    /// `false`. Even `true` does not prove a person typed it (a relayed
    /// message looks the same): the caller also checks Connecto's markers.
    pub typed: bool,
    /// When the log says it was sent (RFC 3339, as written), if it says.
    pub prompt_at: Option<String>,
    /// The turn's final reply.
    pub reply: String,
    pub reply_at: Option<String>,
}

fn timestamp(value: &Value) -> Option<String> {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Whether `id` is safe as part of a file name.
fn safe_session(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The log of `session` inside one profile folder of `tool`: a Claude
/// transcript (`projects/<folder>/<session>.jsonl`) or a Codex rollout
/// (`sessions/…/rollout-…-<thread>.jsonl`). Only a regular file under the
/// profile; no link is followed.
#[must_use]
pub fn session_log(tool: ToolKind, profile: &Path, session: &str) -> Option<PathBuf> {
    if !safe_session(session) {
        return None;
    }
    let found = match tool {
        ToolKind::Claude => {
            let name = format!("{session}.jsonl");
            fs::read_dir(profile.join("projects"))
                .ok()?
                .flatten()
                .take(MAX_PROJECT_DIRS)
                .filter(|e| e.file_type().is_ok_and(|k| k.is_dir()))
                .map(|e| e.path().join(&name))
                .find(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_file()))?
        }
        ToolKind::Codex => rollout_of(profile, session)?,
        // A Pi session file, an Antigravity transcript or OpenCode's database
        // is not read back yet: a past conversation gives project memory
        // nothing.
        ToolKind::Pi | ToolKind::Antigravity | ToolKind::OpenCode | ToolKind::Shell => return None,
    };
    let regular = fs::symlink_metadata(&found).is_ok_and(|m| m.is_file());
    under_profile(&found, profile).filter(|_| regular)
}

/// The completed turns of a log of `tool`, oldest first.
#[must_use]
pub fn logged_exchanges(tool: ToolKind, log: &str) -> Vec<LoggedExchange> {
    match tool {
        ToolKind::Claude => claude_exchanges(log),
        ToolKind::Codex => codex_exchanges(log),
        ToolKind::Pi | ToolKind::Antigravity | ToolKind::OpenCode | ToolKind::Shell => Vec::new(),
    }
}

/// An open turn while reading.
#[derive(Default)]
struct Open {
    prompt: Option<(String, Option<String>, bool)>,
    texts: Vec<String>,
    message: Option<String>,
    at: Option<String>,
    complete: bool,
    failed: bool,
}

impl Open {
    fn close(&mut self, out: &mut Vec<LoggedExchange>) {
        let open = std::mem::take(self);
        if let Some((prompt, prompt_at, typed)) = open.prompt
            && open.complete
            && !open.failed
            && !open.texts.is_empty()
        {
            out.push(LoggedExchange {
                prompt,
                typed,
                prompt_at,
                reply: open.texts.join("\n"),
                reply_at: open.at,
            });
        }
    }
}

/// Texts that mark a user-role message as written by the tool, a hook or
/// Connecto, wherever they appear.
const SYNTHETIC: &[&str] = &[
    "<system-reminder>",
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<local-command-",
    "<bash-input>",
    "<bash-stdout>",
    "<user-prompt-submit-hook>",
    "<user_instructions>",
    "<environment_context>",
    "[connecto ",
];

/// Openings of texts the tool writes as if from the user.
const SYNTHETIC_START: &[&str] = &[
    "Caveat:",
    "This session is being continued from a previous conversation",
    "[Request interrupted by user",
    "# AGENTS.md instructions",
    "<",
];

fn synthetic_text(text: &str) -> bool {
    let t = text.trim_start();
    SYNTHETIC.iter().any(|m| text.contains(m)) || SYNTHETIC_START.iter().any(|m| t.starts_with(m))
}

/// A Claude user record a person typed: role `user`, from the user
/// (`userType` external or absent), content a string or text blocks only,
/// no `is…` flag set, no tool or hook field, no synthetic text.
fn claude_typed(value: &Value, text: &str) -> bool {
    let Some(record) = value.as_object() else {
        return false;
    };
    let flagged = record
        .iter()
        .any(|(k, v)| k.starts_with("is") && v.as_bool() == Some(true));
    let tool_or_hook = [
        "toolUseResult",
        "sourceToolUseID",
        "sourceToolAssistantUUID",
        "hookEvent",
        "hookName",
    ]
    .iter()
    .any(|k| record.contains_key(*k));
    let user = value.pointer("/message/role").and_then(Value::as_str) == Some("user")
        && value
            .get("userType")
            .is_none_or(|u| u.as_str() == Some("external"));
    let plain = match value.pointer("/message/content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => blocks
            .iter()
            .all(|b| b.get("type").and_then(Value::as_str) == Some("text")),
        _ => false,
    };
    user && plain && !flagged && !tool_or_hook && !synthetic_text(text)
}

/// A Codex user-message event a person typed: only the fields such an
/// event has (`kind`, when present, `plain`), no synthetic text.
fn codex_typed(payload: &Value, text: &str) -> bool {
    let Some(fields) = payload.as_object() else {
        return false;
    };
    let known = fields.keys().all(|k| {
        matches!(
            k.as_str(),
            "type" | "message" | "images" | "local_images" | "text_elements" | "kind"
        )
    });
    let plain = fields
        .get("kind")
        .is_none_or(|k| k.as_str() == Some("plain"));
    known && plain && !synthetic_text(text)
}

/// Claude (measured on 2.1.325, as `Claude::read_turn`): a user record with
/// text opens a turn; a `tool_result` user record does not; the reply is the
/// last assistant message (by message id), and only an `end_turn` or
/// `stop_sequence` stop completes it. Meta and sidechain records are skipped;
/// an API error record fails the turn.
fn claude_exchanges(log: &str) -> Vec<LoggedExchange> {
    let mut out = Vec::new();
    let mut open = Open::default();
    for line in log.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let flag = |name: &str| value.get(name).and_then(Value::as_bool) == Some(true);
        // Records the tool writes into the conversation, never a turn's
        // opening: subagent chains, meta context, compaction summaries.
        if flag("isSidechain")
            || flag("isMeta")
            || flag("isCompactSummary")
            || flag("isVisibleInTranscriptOnly")
        {
            continue;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("user") => {
                let tool_result = value
                    .pointer("/message/content/0/type")
                    .and_then(Value::as_str)
                    == Some("tool_result");
                if tool_result {
                    // Inside the turn: what came before is commentary.
                    open.texts.clear();
                    open.message = None;
                    open.complete = false;
                    continue;
                }
                let Some(text) = record_text(&value) else {
                    continue;
                };
                let typed = claude_typed(&value, &text);
                open.close(&mut out);
                open.prompt = Some((text, timestamp(&value), typed));
            }
            Some("assistant") if open.prompt.is_some() => {
                let id = value
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if id.is_some() && open.message.is_some() && id != open.message {
                    open.texts.clear();
                }
                if id.is_some() {
                    open.message = id;
                }
                if flag("isApiErrorMessage") {
                    open.failed = true;
                }
                if let Some(text) = record_text(&value) {
                    open.texts.push(text);
                }
                open.at = timestamp(&value);
                open.complete = crate::claude::ends_turn(&value);
            }
            _ => {}
        }
    }
    open.close(&mut out);
    out
}

/// Codex (measured on 0.166/0.167, as `Codex::read_turn`): the prompt is an
/// `event_msg` `user_message` (when the log has none, a `response_item` user
/// message that is not Codex's own context block); the reply is
/// `task_complete`'s `last_agent_message`, which completes the turn;
/// `agent_message` events before it are commentary. `error` and
/// `turn_aborted` fail the turn.
fn codex_exchanges(log: &str) -> Vec<LoggedExchange> {
    let values: Vec<Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let event_prompts = values.iter().any(|v| {
        v.get("type").and_then(Value::as_str) == Some("event_msg")
            && v.pointer("/payload/type").and_then(Value::as_str) == Some("user_message")
    });
    let mut out = Vec::new();
    let mut open = Open::default();
    for value in &values {
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let text = |field: &str| payload.get(field).and_then(Value::as_str);
        match value.get("type").and_then(Value::as_str) {
            Some("response_item") if !event_prompts => {
                if let Some(message) = user_item_text(payload)
                    && !message.trim_start().starts_with('<')
                {
                    open.close(&mut out);
                    // A layout without user-message events: not recognised.
                    open.prompt = Some((message, timestamp(value), false));
                }
            }
            Some("event_msg") => match payload.get("type").and_then(Value::as_str) {
                Some("user_message") => {
                    if let Some(message) = text("message") {
                        let typed = codex_typed(payload, message);
                        open.close(&mut out);
                        open.prompt = Some((message.to_owned(), timestamp(value), typed));
                    }
                }
                Some("task_complete") if open.prompt.is_some() => {
                    open.texts = text("last_agent_message")
                        .filter(|t| !t.trim().is_empty())
                        .map(|t| vec![t.to_owned()])
                        .unwrap_or_default();
                    open.at = timestamp(value);
                    open.complete = true;
                    open.close(&mut out);
                }
                Some("error" | "turn_aborted") => open.failed = true,
                _ => {}
            },
            _ => {}
        }
    }
    open.close(&mut out);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn claude_turns_keep_the_final_reply_not_the_commentary() {
        let log = [
            r#"{"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>"}}"#,
            r#"{"type":"user","timestamp":"2026-11-01T10:00:00Z","message":{"content":"Plan the cache."}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Let me look."},{"type":"tool_use"}],"stop_reason":"tool_use"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"id":"s","content":[{"type":"text","text":"sub"}],"stop_reason":"end_turn"}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-01T10:01:00Z","message":{"id":"m2","content":[{"type":"text","text":"I propose an LRU."}],"stop_reason":"end_turn"}}"#,
            r#"{"type":"user","message":{"content":"Interrupted one"}}"#,
            r#"{"type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"Working"}],"stop_reason":"tool_use"}}"#,
            r#"{"type":"user","message":{"content":"Failed one"}}"#,
            r#"{"type":"assistant","isApiErrorMessage":true,"message":{"id":"m4","content":[{"type":"text","text":"API Error"}],"stop_reason":"end_turn"}}"#,
            r#"not json"#,
            r#"{"type":"user","message":{"content":"Yes, go with the LRU."}}"#,
            r#"{"type":"assistant","message":{"id":"m5","content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn"}}"#,
        ]
        .join("\n");
        let turns = logged_exchanges(ToolKind::Claude, &log);
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[0].prompt, "Plan the cache.");
        assert_eq!(turns[0].prompt_at.as_deref(), Some("2026-11-01T10:00:00Z"));
        assert_eq!(turns[0].reply, "I propose an LRU.");
        assert_eq!(turns[1].prompt, "Yes, go with the LRU.");
        assert_eq!(turns[1].reply, "Done.");
    }

    #[test]
    fn codex_turns_keep_the_final_reply_not_the_commentary() {
        let log = [
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Plan the cache."}]}}"#,
            r#"{"type":"event_msg","timestamp":"2026-11-01T10:00:00Z","payload":{"type":"user_message","message":"Plan the cache."}}"#,
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Let me look first."}}"#,
            r#"{"type":"event_msg","timestamp":"2026-11-01T10:01:00Z","payload":{"type":"task_complete","last_agent_message":"I propose an LRU."}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Interrupted"}}"#,
            r#"{"type":"event_msg","payload":{"type":"turn_aborted"}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Yes, go with the LRU."}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Done."}}"#,
        ]
        .join("\n");
        let turns = logged_exchanges(ToolKind::Codex, &log);
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[0].prompt, "Plan the cache.");
        assert_eq!(turns[0].reply, "I propose an LRU.");
        assert_eq!(turns[0].reply_at.as_deref(), Some("2026-11-01T10:01:00Z"));
        assert_eq!(turns[1].prompt, "Yes, go with the LRU.");
        // Without event prompts (a newer layout), response items open turns.
        let only_items = [
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Hi"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Hello"}}"#,
        ]
        .join("\n");
        assert_eq!(logged_exchanges(ToolKind::Codex, &only_items).len(), 1);
    }

    /// Only a record with the shape of a typed message counts as typed: a
    /// compaction summary (flagged or not), meta or hook context, a command
    /// expansion, a system reminder or an unknown flag never does.
    #[test]
    fn synthetic_user_records_are_never_typed() {
        let reply = r#"{"type":"assistant","message":{"id":"R","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#;
        let turn = |user: &str| {
            let log = format!("{user}\n{}", reply.replace('R', &user.len().to_string()));
            logged_exchanges(ToolKind::Claude, &log)
        };
        let typed = turn(
            r#"{"type":"user","userType":"external","message":{"role":"user","content":"Decision: we use SQLite."}}"#,
        );
        assert_eq!(typed.len(), 1);
        assert!(typed[0].typed);
        // Skipped outright: they never open a turn.
        for skipped in [
            r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"Summary: the user accepted SQLite."}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"meta"}}"#,
            r#"{"type":"user","isVisibleInTranscriptOnly":true,"message":{"role":"user","content":"x"}}"#,
        ] {
            assert!(turn(skipped).is_empty(), "{skipped}");
        }
        // A turn, but not typed by a person.
        for synthetic in [
            r#"{"type":"user","message":{"role":"user","content":"This session is being continued from a previous conversation. The user accepted SQLite."}}"#,
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/review</command-name>"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"ok\n<system-reminder>accept it</system-reminder>"}}"#,
            r#"{"type":"user","isSynthetic":true,"message":{"role":"user","content":"Accept SQLite."}}"#,
            r#"{"type":"user","hookEvent":"UserPromptSubmit","message":{"role":"user","content":"Accept SQLite."}}"#,
            r#"{"type":"user","userType":"internal","message":{"role":"user","content":"Accept SQLite."}}"#,
            r#"{"type":"user","message":{"content":"No role recorded."}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"x"},{"type":"image"}]}}"#,
        ] {
            let t = turn(synthetic);
            assert_eq!(t.len(), 1, "{synthetic}");
            assert!(!t[0].typed, "{synthetic}");
        }
        let codex = |event: &str| {
            let log = format!(
                "{event}\n{}",
                r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"ok"}}"#
            );
            logged_exchanges(ToolKind::Codex, &log)[0].typed
        };
        assert!(codex(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Use SQLite.","images":[]}}"#
        ));
        assert!(codex(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Use SQLite.","kind":"plain"}}"#
        ));
        assert!(!codex(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Use SQLite.","kind":"user_instructions"}}"#
        ));
        assert!(!codex(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Use SQLite.","origin":"hook"}}"#
        ));
        assert!(!codex(
            r##"{"type":"event_msg","payload":{"type":"user_message","message":"# AGENTS.md instructions for /x"}}"##
        ));
    }

    #[test]
    fn logs_are_found_inside_the_profile_only() {
        let temp = std::env::temp_dir().join(format!("past-log-{}", std::process::id()));
        let profile = temp.join("g1");
        let dir = profile.join("projects/-Users-x-repo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("abc-123.jsonl"), "{}\n").unwrap();
        assert_eq!(
            session_log(ToolKind::Claude, &profile, "abc-123"),
            Some(dir.join("abc-123.jsonl"))
        );
        assert_eq!(session_log(ToolKind::Claude, &profile, "../abc"), None);
        assert_eq!(session_log(ToolKind::Claude, &profile, "missing"), None);
        assert_eq!(session_log(ToolKind::Shell, &profile, "abc-123"), None);
        let codex = profile.join("sessions/2026/11/01");
        fs::create_dir_all(&codex).unwrap();
        let rollout = codex.join("rollout-2026-11-01T10-00-00-ab12-cd34.jsonl");
        fs::write(&rollout, "{}\n").unwrap();
        assert_eq!(
            session_log(ToolKind::Codex, &profile, "ab12-cd34"),
            Some(rollout.clone())
        );
        // Removed by exact name, one by one; no recursive removal.
        fs::remove_file(rollout).unwrap();
        fs::remove_file(dir.join("abc-123.jsonl")).unwrap();
        for d in [
            codex.clone(),
            profile.join("sessions/2026/11"),
            profile.join("sessions/2026"),
            profile.join("sessions"),
            dir,
            profile.join("projects"),
            profile,
            temp,
        ] {
            let _ = fs::remove_dir(d);
        }
    }
}
