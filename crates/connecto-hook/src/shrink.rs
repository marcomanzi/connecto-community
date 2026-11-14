//! Making an over-limit hook payload fit a frame (section 5.9).
//!
//! Lifecycle facts must survive even when text does not, so the helper removes
//! text in a fixed order and names what it dropped in `connecto_dropped`.

use serde_json::{Map, Value, json};

use crate::MAX_FRAME;

/// Largest payload the helper reads. The rest is drained and ignored.
pub const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

/// The first part of a prompt that survives.
const PROMPT_KEEP: usize = 64 * 1024;
/// `input-messages` entries kept, each cut to this many bytes.
const INPUT_KEEP: usize = 16;
const INPUT_ENTRY: usize = 4 * 1024;

/// Which frame is being sent, for the incomplete form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind<'a> {
    /// A hook frame, named by the `--event` argument. `None` for the older
    /// form without one.
    Hook(Option<&'a str>),
    /// Codex `notify`.
    Notify,
}

fn size(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

fn cut(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn drop_field(object: &mut Map<String, Value>, field: &str, dropped: &mut Vec<String>) {
    if object.remove(field).is_some() {
        dropped.push(field.to_owned());
    }
}

/// What to send when nothing can be recovered. `None` when the event cannot be
/// named, which is how an older helper form behaves too: the frame is dropped.
#[must_use]
pub fn incomplete(kind: Kind<'_>) -> Option<Vec<u8>> {
    let value = match kind {
        Kind::Notify => json!({ "type": "connecto-incomplete" }),
        Kind::Hook(Some(event)) => json!({ "hook_event_name": event, "connecto_incomplete": true }),
        Kind::Hook(None) => return None,
    };
    serde_json::to_vec(&value).ok()
}

/// The payload to send for `payload`, given room for `token_len` token bytes.
/// `None` means there is nothing worth sending.
#[must_use]
pub fn fit(kind: Kind<'_>, payload: &[u8], token_len: usize, complete: bool) -> Option<Vec<u8>> {
    let room = MAX_FRAME.saturating_sub(1 + token_len);
    // The older form has no event name to put in an incomplete frame, so a
    // payload that does not parse goes as it is and is refused and counted
    // there, as it always was.
    if kind == Kind::Hook(None) && complete && payload.len() <= room {
        return Some(payload.to_vec());
    }
    if complete && payload.len() <= room && serde_json::from_slice::<Value>(payload).is_ok() {
        return Some(payload.to_vec());
    }
    if !complete {
        return incomplete(kind);
    }
    let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(payload) else {
        return incomplete(kind);
    };
    let mut dropped: Vec<String> = Vec::new();
    let fits = |object: &Map<String, Value>| size(&Value::Object(object.clone())) <= room;
    match kind {
        Kind::Hook(Some("UserPromptSubmit")) | Kind::Hook(None) => {
            if let Some(Value::String(prompt)) = object.get_mut("prompt")
                && prompt.len() > PROMPT_KEEP
            {
                *prompt = cut(prompt, PROMPT_KEEP).to_owned();
                dropped.push("prompt".to_owned());
            }
        }
        Kind::Hook(Some(_)) => {
            drop_field(&mut object, "last_assistant_message", &mut dropped);
        }
        Kind::Notify => {
            if let Some(Value::Array(inputs)) = object.get_mut("input-messages") {
                let total = inputs.len();
                let keep_from = total.saturating_sub(INPUT_KEEP);
                if keep_from > 0 {
                    inputs.drain(..keep_from);
                    dropped.push("input-messages".to_owned());
                }
                for entry in inputs.iter_mut() {
                    if let Value::String(text) = entry
                        && text.len() > INPUT_ENTRY
                    {
                        *text = cut(text, INPUT_ENTRY).to_owned();
                        if !dropped.iter().any(|name| name == "input-messages") {
                            dropped.push("input-messages".to_owned());
                        }
                    }
                }
                if !dropped.is_empty() {
                    object.insert("connecto_input_total".to_owned(), json!(total));
                }
            }
            if !fits(&object) {
                drop_field(&mut object, "last-assistant-message", &mut dropped);
            }
        }
    }
    if !dropped.is_empty() {
        object.insert("connecto_dropped".to_owned(), json!(dropped));
    }
    if !fits(&object) {
        return incomplete(kind);
    }
    serde_json::to_vec(&Value::Object(object)).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn a_payload_that_fits_is_sent_unchanged() {
        let payload = br#"{"hook_event_name":"Stop"}"#;
        assert_eq!(
            fit(Kind::Hook(Some("Stop")), payload, 64, true).unwrap(),
            payload
        );
    }

    #[test]
    fn an_over_limit_stop_keeps_its_background_tasks() {
        let payload = json!({
            "hook_event_name": "Stop", "session_id": "s", "transcript_path": "/t",
            "background_tasks": [{"id": "b1"}],
            "last_assistant_message": "x".repeat(MAX_FRAME),
        });
        let sent = parse(
            &fit(
                Kind::Hook(Some("Stop")),
                &serde_json::to_vec(&payload).unwrap(),
                64,
                true,
            )
            .unwrap(),
        );
        assert!(sent.get("last_assistant_message").is_none());
        assert_eq!(sent["background_tasks"], json!([{"id": "b1"}]));
        assert_eq!(sent["session_id"], "s");
        assert_eq!(sent["connecto_dropped"], json!(["last_assistant_message"]));
    }

    /// T29: old entries go first and the original length is recorded.
    #[test]
    fn a_notify_loses_old_inputs_first_and_records_the_total() {
        let inputs: Vec<String> = (0..40)
            .map(|n| format!("input {n} {}", "x".repeat(8000)))
            .collect();
        let payload = json!({
            "type": "agent-turn-complete", "thread-id": "t", "turn-id": "u",
            "input-messages": inputs, "last-assistant-message": "reply",
        });
        let sent = parse(
            &fit(
                Kind::Notify,
                &serde_json::to_vec(&payload).unwrap(),
                64,
                true,
            )
            .unwrap(),
        );
        assert_eq!(sent["connecto_input_total"], 40);
        let kept = sent["input-messages"].as_array().unwrap();
        assert_eq!(kept.len(), 16);
        assert!(kept[15].as_str().unwrap().starts_with("input 39"));
        assert!(
            kept.iter()
                .all(|entry| entry.as_str().unwrap().len() <= INPUT_ENTRY)
        );
        assert_eq!(
            sent["last-assistant-message"], "reply",
            "the reply is removed last"
        );
        assert_eq!(sent["turn-id"], "u");
    }

    #[test]
    fn a_huge_reply_is_removed_from_a_notify_after_the_inputs() {
        let payload = json!({
            "type": "agent-turn-complete", "thread-id": "t", "turn-id": "u",
            "input-messages": ["a"], "last-assistant-message": "r".repeat(MAX_FRAME),
        });
        let sent = parse(
            &fit(
                Kind::Notify,
                &serde_json::to_vec(&payload).unwrap(),
                64,
                true,
            )
            .unwrap(),
        );
        assert!(sent.get("last-assistant-message").is_none());
        assert_eq!(sent["thread-id"], "t");
    }

    #[test]
    fn a_long_prompt_keeps_its_head_so_the_header_survives() {
        let prompt = format!("[connecto from a, request r1.]\n{}", "p".repeat(MAX_FRAME));
        let payload =
            json!({ "hook_event_name": "UserPromptSubmit", "prompt": prompt, "turn_id": "t" });
        let sent = parse(
            &fit(
                Kind::Hook(Some("UserPromptSubmit")),
                &serde_json::to_vec(&payload).unwrap(),
                64,
                true,
            )
            .unwrap(),
        );
        let kept = sent["prompt"].as_str().unwrap();
        assert_eq!(kept.len(), PROMPT_KEEP);
        assert!(kept.starts_with("[connecto from a, request r1.]"));
        assert_eq!(sent["turn_id"], "t");
    }

    #[test]
    fn what_cannot_be_read_becomes_an_incomplete_frame_named_by_the_event() {
        let sent = fit(Kind::Hook(Some("Stop")), b"not json at all", 64, true).unwrap();
        assert_eq!(
            parse(&sent),
            json!({ "hook_event_name": "Stop", "connecto_incomplete": true })
        );
        let sent = fit(Kind::Notify, b"{{{", 64, false).unwrap();
        assert_eq!(parse(&sent), json!({ "type": "connecto-incomplete" }));
        // The older form without `--event` cannot name the event: it goes as it
        // is when it fits, and is refused and counted by the server.
        assert_eq!(
            fit(Kind::Hook(None), b"{{{", 64, true),
            Some(b"{{{".to_vec())
        );
    }
}
