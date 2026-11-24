//! `connecto-hook antigravity --event NAME`: Antigravity CLI's hooks, turned
//! into the payloads Claude Code's hooks send.
//!
//! Antigravity's hooks are registered once, in the user's global
//! `hooks.json`, so the same command runs for every `agy` on the computer.
//! What makes one a Connecto terminal is its environment: without the hook
//! channel's variables the helper only answers `{}`.
//!
//! Antigravity has no hook for "a prompt was accepted": `PreInvocation` runs
//! before every model call of a turn. A turn's first call is the one whose
//! `initialNumSteps` (the steps the conversation held when the turn started)
//! differs from the last turn's. Its `lastUserInput` is the prompt. And its
//! `Stop` carries no reply: the reply is the conversation's last
//! `PLANNER_RESPONSE` step without tool calls, read from `transcriptPath`.
//!
//! What the helper remembers between runs (the conversation it follows and
//! the last turn it saw) is a small file in the session's temporary folder.
//! A conversation it does not follow (a subagent's) reports nothing, unless
//! it starts while no turn is open: that is the user's `/new` or `/clear`.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The file the bridge instructions are in, set at launch: the helper hands
/// them to the model at `SessionStart`. The adapter sets the same name.
pub const LAUNCH_TEXT_ENV: &str = "CONNECTO_AGY_LAUNCH_TEXT";
/// Set at launch when notices from other terminals are handed to the model.
pub const INBOX_ENV: &str = "CONNECTO_AGY_INBOX";
/// Set on every Antigravity terminal. Connecto's hooks entry is global, and
/// any process started in another terminal (`agy -p` run by a Codex agent)
/// inherits that terminal's hook channel: without this, its `SessionStart`
/// would be reported as the other terminal's and re-pin its session. The
/// adapter sets the same name.
pub const TERMINAL_ENV: &str = "CONNECTO_AGY_TERMINAL";

/// What the helper keeps between runs, per terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// The conversation this terminal follows.
    pub conversation: Option<String>,
    /// `initialNumSteps` of the last turn seen.
    pub steps: Option<u64>,
    /// `lastUserInput` of the last turn seen.
    pub input: Option<String>,
    /// A turn started and has not stopped.
    pub open: bool,
}

impl State {
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return Self::default();
        };
        Self {
            conversation: value["conversation"].as_str().map(str::to_owned),
            steps: value["steps"].as_u64(),
            input: value["input"].as_str().map(str::to_owned),
            open: value["open"].as_bool() == Some(true),
        }
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        json!({
            "conversation": self.conversation,
            "steps": self.steps,
            "input": self.input,
            "open": self.open,
        })
        .to_string()
    }
}

/// What to do with one hook run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Report nothing.
    Nothing,
    /// The session started: report it, and hand the model the instructions.
    Started(Value),
    /// A turn started: report the prompt, and hand the model the inbox.
    Prompt(Value),
    /// A turn ended: report it, once the reply is read from the transcript.
    Stopped {
        payload: Value,
        transcript: Option<String>,
    },
}

fn text<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
    payload.get(field).and_then(Value::as_str)
}

/// The Claude-shaped base of every report: the conversation and its log.
fn base(event: &str, payload: &Value) -> Value {
    let mut out = json!({ "hook_event_name": event });
    if let Some(id) = text(payload, "conversationId") {
        out["session_id"] = json!(id);
    }
    if let Some(path) = text(payload, "transcriptPath") {
        out["transcript_path"] = json!(path);
    }
    out
}

/// Whether `payload` is about the conversation `state` follows. The first
/// conversation seen becomes the one followed.
fn followed(state: &mut State, payload: &Value) -> bool {
    let id = text(payload, "conversationId");
    match (&state.conversation, id) {
        (_, None) => true,
        (None, Some(id)) => {
            state.conversation = Some(id.to_owned());
            true
        }
        (Some(ours), Some(id)) => ours == id,
    }
}

/// The action for one run of `event` with Antigravity's `payload`, updating
/// `state`.
pub fn action(event: &str, payload: &Value, state: &mut State) -> Action {
    match event {
        "SessionStart" => {
            // A new conversation while a turn runs is a subagent's.
            let id = text(payload, "conversationId");
            let ours = state.conversation.as_deref() == id;
            if !ours && state.open && state.conversation.is_some() {
                return Action::Nothing;
            }
            *state = State {
                conversation: id.map(str::to_owned),
                ..State::default()
            };
            Action::Started(base("SessionStart", payload))
        }
        "PreInvocation" => {
            if !followed(state, payload) {
                return Action::Nothing;
            }
            let steps = payload.get("initialNumSteps").and_then(Value::as_u64);
            if steps.is_some() && steps == state.steps {
                // A later model call of the same turn.
                return Action::Nothing;
            }
            let input = text(payload, "lastUserInput").map(str::to_owned);
            let repeated = state.steps.is_some() && input == state.input;
            state.steps = steps;
            state.open = true;
            match input {
                // A turn Antigravity started by itself (a background task
                // that ended) still shows the last prompt: no new prompt.
                Some(_) if repeated => Action::Nothing,
                Some(prompt) => {
                    state.input = Some(prompt.clone());
                    let mut out = base("UserPromptSubmit", payload);
                    out["prompt"] = json!(prompt);
                    Action::Prompt(out)
                }
                None => Action::Nothing,
            }
        }
        "Stop" => {
            if !followed(state, payload) {
                return Action::Nothing;
            }
            state.open = false;
            let reason = text(payload, "terminationReason").unwrap_or("");
            let continued = payload
                .get("continuationActive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let failure = match reason {
                "USER_CANCELED" => Some("user_canceled".to_owned()),
                "ERROR" => Some("error".to_owned()),
                "MAX_INVOCATIONS" | "MAX_FORCED_INVOCATIONS" | "MAX_TOKEN_BUDGET_EXCEEDED" => {
                    Some(reason.to_lowercase())
                }
                _ => None,
            };
            if let Some(error) = failure {
                let mut out = base("StopFailure", payload);
                out["error"] = json!(error);
                return Action::Stopped {
                    payload: out,
                    transcript: None,
                };
            }
            let mut out = base("Stop", payload);
            if continued {
                out["stop_hook_active"] = json!(true);
            }
            Action::Stopped {
                payload: out,
                transcript: text(payload, "transcriptPath").map(str::to_owned),
            }
        }
        _ => Action::Nothing,
    }
}

/// What the transcript says about the turn's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The final reply.
    Reply(String),
    /// There is one, but the compact transcript cut it.
    Cut,
    /// The newest steps are not a finished turn (not written yet).
    NotYet,
}

/// The reply at the end of `text`, the tail of a `transcript.jsonl`.
#[must_use]
pub fn final_reply(text: &str) -> Lookup {
    final_step(text).map_or(Lookup::NotYet, |step| lookup_of(&step))
}

/// The step that ends the turn at the end of `text`: the last
/// `PLANNER_RESPONSE` without tool calls, or `None` while the newest steps
/// are not a finished turn.
fn final_step(text: &str) -> Option<Value> {
    for line in text.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        // A line still being written, or the cut start of the tail.
        let step = serde_json::from_str::<Value>(line).ok()?;
        match step.get("type").and_then(Value::as_str) {
            // Hooks insert these mid-turn; they do not end it.
            Some("SYSTEM_MESSAGE" | "EPHEMERAL_MESSAGE") => {}
            Some("PLANNER_RESPONSE") => {
                let calls = step
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty());
                return (!calls).then_some(step);
            }
            _ => return None,
        }
    }
    None
}

/// What a turn's final step says about its reply.
fn lookup_of(step: &Value) -> Lookup {
    let cut = step
        .get("truncated_fields")
        .and_then(Value::as_array)
        .is_some_and(|fields| fields.iter().any(|f| f.as_str() == Some("content")));
    if cut {
        return Lookup::Cut;
    }
    step.get("content")
        .and_then(Value::as_str)
        .map_or(Lookup::NotYet, |reply| Lookup::Reply(reply.to_owned()))
}

/// The tail of a transcript worth reading for its last step.
const TAIL: u64 = 4 * 1024 * 1024;
/// The same for the full transcript, whose last step also holds the
/// turn's whole thinking.
const FULL_TAIL: u64 = 16 * 1024 * 1024;
/// The compact transcript Antigravity names in its hooks. It cuts long
/// fields to about 4 KB and lists them in `truncated_fields`.
const TRANSCRIPT: &str = "transcript.jsonl";
/// The uncut transcript Antigravity writes next to it.
const FULL_TRANSCRIPT: &str = "transcript_full.jsonl";

/// The last [`TAIL`] bytes of the transcript at `path`. Only a file named
/// `transcript.jsonl` is read.
#[must_use]
pub fn read_tail(path: &Path) -> Option<String> {
    if path.file_name()? != TRANSCRIPT {
        return None;
    }
    tail(path, TAIL)
}

fn tail(path: &Path, size: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(size);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(size).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The compact transcript of the conversation whose transcript a hook named.
/// Antigravity 1.5 names `transcript_full.jsonl` in `Stop` (and
/// `transcript.jsonl` elsewhere): either leads to the same pair of files.
/// Any other file is not a transcript.
fn compact_transcript(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    (name == TRANSCRIPT || name == FULL_TRANSCRIPT).then(|| path.with_file_name(TRANSCRIPT))
}

/// The reply of the turn that ended, from the transcript at `path` (a
/// `transcript.jsonl` or `transcript_full.jsonl`). It is read from the
/// compact `transcript.jsonl`; a reply that one cut is read from the
/// `transcript_full.jsonl` beside it; while that one does not hold it yet,
/// the answer stays [`Lookup::Cut`].
#[must_use]
pub fn transcript_reply(path: &Path) -> Lookup {
    let Some(path) = compact_transcript(path) else {
        return Lookup::NotYet;
    };
    let path = path.as_path();
    let Some(step) = read_tail(path).and_then(|text| final_step(&text)) else {
        return Lookup::NotYet;
    };
    let found = lookup_of(&step);
    if found != Lookup::Cut {
        return found;
    }
    // Only the same step: the full transcript may still end at an earlier
    // turn's reply.
    let index = |step: &Value| step.get("step_index").and_then(Value::as_u64);
    let Some(wanted) = index(&step) else {
        return Lookup::Cut;
    };
    match tail(&path.with_file_name(FULL_TRANSCRIPT), FULL_TAIL)
        .and_then(|text| final_step(&text))
        .filter(|full| index(full) == Some(wanted))
        .map(|full| lookup_of(&full))
    {
        Some(Lookup::Reply(reply)) => Lookup::Reply(reply),
        _ => Lookup::Cut,
    }
}

/// Put the reply `lookup` found into a `Stop` payload: the text, or the mark
/// that it was dropped (Claude's shape for a reply over the frame limit).
pub fn with_reply(payload: &mut Value, lookup: Lookup) {
    match lookup {
        Lookup::Reply(reply) => payload["last_assistant_message"] = json!(reply),
        Lookup::Cut => payload["connecto_dropped"] = json!(["last_assistant_message"]),
        Lookup::NotYet => {}
    }
}

/// What the helper prints for Antigravity: steps to add to the conversation,
/// or `{}`. Antigravity reads a hook's answer as JSON, always.
#[must_use]
pub fn answer(system_message: Option<&str>) -> String {
    match system_message
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(text) => json!({
            "injectSteps": [{ "systemMessage": { "systemMessage": text } }]
        })
        .to_string(),
        None => "{}".to_owned(),
    }
}

/// The notices in what the bridge answered a drain with: the JSON Claude's
/// prompt hook wants, around the text.
#[must_use]
pub fn inbox_text(printed: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(printed).ok()?;
    value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .map(str::to_owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn pre(conversation: &str, steps: u64, input: &str) -> Value {
        json!({
            "conversationId": conversation,
            "transcriptPath": "/h/.gemini/antigravity-cli/brain/c1/.system_generated/logs/transcript.jsonl",
            "hookEventName": "PreInvocation",
            "invocationNum": 0,
            "initialNumSteps": steps,
            "lastUserInput": input,
        })
    }

    #[test]
    fn a_session_start_is_claudes_with_the_conversation_and_its_transcript() {
        let mut state = State::default();
        let payload = json!({"conversationId": "c1", "transcriptPath": "/t/transcript.jsonl"});
        assert_eq!(
            action("SessionStart", &payload, &mut state),
            Action::Started(json!({
                "hook_event_name": "SessionStart",
                "session_id": "c1",
                "transcript_path": "/t/transcript.jsonl",
            }))
        );
        assert_eq!(state.conversation.as_deref(), Some("c1"));
    }

    #[test]
    fn only_the_first_model_call_of_a_turn_is_its_prompt() {
        let mut state = State::default();
        let Action::Prompt(first) = action("PreInvocation", &pre("c1", 0, "hello"), &mut state)
        else {
            panic!("no prompt");
        };
        assert_eq!(first["hook_event_name"], "UserPromptSubmit");
        assert_eq!(first["prompt"], "hello");
        assert_eq!(first["session_id"], "c1");
        assert!(state.open);
        // The same turn calls the model again after a tool.
        assert_eq!(
            action("PreInvocation", &pre("c1", 0, "hello"), &mut state),
            Action::Nothing
        );
        action("Stop", &json!({"conversationId": "c1"}), &mut state);
        // The next turn, even with the same words.
        assert!(matches!(
            action("PreInvocation", &pre("c1", 4, "again"), &mut state),
            Action::Prompt(_)
        ));
    }

    #[test]
    fn a_turn_antigravity_starts_by_itself_is_no_new_prompt() {
        let mut state = State::default();
        action("PreInvocation", &pre("c1", 0, "hello"), &mut state);
        action("Stop", &json!({"conversationId": "c1"}), &mut state);
        assert_eq!(
            action("PreInvocation", &pre("c1", 6, "hello"), &mut state),
            Action::Nothing
        );
        assert!(state.open);
    }

    #[test]
    fn another_conversation_reports_nothing_unless_it_starts_while_idle() {
        let mut state = State::default();
        action("PreInvocation", &pre("c1", 0, "hello"), &mut state);
        // A subagent's conversation, while the turn runs.
        let sub = json!({"conversationId": "sub"});
        assert_eq!(action("SessionStart", &sub, &mut state), Action::Nothing);
        assert_eq!(
            action("PreInvocation", &pre("sub", 0, "task"), &mut state),
            Action::Nothing
        );
        assert_eq!(action("Stop", &sub, &mut state), Action::Nothing);
        assert!(state.open);
        action("Stop", &json!({"conversationId": "c1"}), &mut state);
        // `/new` while idle: the new conversation is followed from now on.
        let fresh = json!({"conversationId": "c2"});
        assert!(matches!(
            action("SessionStart", &fresh, &mut state),
            Action::Started(_)
        ));
        assert!(matches!(
            action("PreInvocation", &pre("c2", 0, "hello"), &mut state),
            Action::Prompt(_)
        ));
    }

    #[test]
    fn a_stop_ends_the_turn_and_one_that_failed_says_why() {
        let mut state = State::default();
        let stop = json!({
            "conversationId": "c1",
            "transcriptPath": "/t/transcript.jsonl",
            "terminationReason": "NO_TOOL_CALL",
            "fullyIdle": true,
        });
        assert_eq!(
            action("Stop", &stop, &mut state),
            Action::Stopped {
                payload: json!({
                    "hook_event_name": "Stop",
                    "session_id": "c1",
                    "transcript_path": "/t/transcript.jsonl",
                }),
                transcript: Some("/t/transcript.jsonl".to_owned()),
            }
        );
        for (reason, error) in [
            ("USER_CANCELED", "user_canceled"),
            ("ERROR", "error"),
            ("MAX_INVOCATIONS", "max_invocations"),
        ] {
            let failed = json!({"conversationId": "c1", "terminationReason": reason});
            let Action::Stopped {
                payload,
                transcript,
            } = action("Stop", &failed, &mut state)
            else {
                panic!("{reason}");
            };
            assert_eq!(payload["hook_event_name"], "StopFailure");
            assert_eq!(payload["error"], error);
            assert_eq!(transcript, None);
        }
        let continued = json!({"conversationId": "c1", "continuationActive": true});
        let Action::Stopped { payload, .. } = action("Stop", &continued, &mut state) else {
            panic!("no stop");
        };
        assert_eq!(payload["stop_hook_active"], true);
    }

    #[test]
    fn the_reply_is_the_last_planner_response_without_tool_calls() {
        let log = [
            r#"{"step_index":0,"type":"USER_INPUT","content":"hi"}"#,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"","tool_calls":[{"name":"run_command"}]}"#,
            r#"{"step_index":2,"type":"GENERIC","status":"DONE"}"#,
            r#"{"step_index":3,"type":"PLANNER_RESPONSE","content":"Hello!","tool_calls":[]}"#,
            r#"{"step_index":4,"type":"SYSTEM_MESSAGE","content":"x"}"#,
        ]
        .join("\n");
        assert_eq!(final_reply(&log), Lookup::Reply("Hello!".to_owned()));
        // Mid-turn, or the reply not written yet.
        let mid = log.lines().take(3).collect::<Vec<_>>().join("\n");
        assert_eq!(final_reply(&mid), Lookup::NotYet);
        assert_eq!(final_reply(""), Lookup::NotYet);
        assert_eq!(final_reply("{\"type\":\"PLANNER_RESP"), Lookup::NotYet);
        let cut = r#"{"type":"PLANNER_RESPONSE","content":"Hel","truncated_fields":["content"]}"#;
        assert_eq!(final_reply(cut), Lookup::Cut);
    }

    #[test]
    fn a_reply_goes_into_the_payload_as_claudes_would() {
        let mut payload = json!({"hook_event_name": "Stop"});
        with_reply(&mut payload, Lookup::Reply("done".to_owned()));
        assert_eq!(payload["last_assistant_message"], "done");
        let mut payload = json!({"hook_event_name": "Stop"});
        with_reply(&mut payload, Lookup::Cut);
        assert_eq!(
            payload["connecto_dropped"],
            json!(["last_assistant_message"])
        );
    }

    #[test]
    fn the_answer_is_always_json() {
        assert_eq!(answer(None), "{}");
        assert_eq!(answer(Some("  ")), "{}");
        let value: Value = serde_json::from_str(&answer(Some("notices"))).unwrap();
        assert_eq!(
            value["injectSteps"][0]["systemMessage"]["systemMessage"],
            "notices"
        );
        assert_eq!(
            inbox_text(br#"{"hookSpecificOutput":{"additionalContext":"n"}}"#).as_deref(),
            Some("n")
        );
        assert_eq!(inbox_text(b""), None);
    }

    #[test]
    fn the_state_survives_a_round_trip() {
        let state = State {
            conversation: Some("c1".to_owned()),
            steps: Some(4),
            input: Some("hi".to_owned()),
            open: true,
        };
        assert_eq!(State::parse(&state.to_json()), state);
        assert_eq!(State::parse("garbage"), State::default());
    }

    /// The last step of a turn whose reply is long, as Antigravity 2026-11
    /// writes it: the compact `transcript.jsonl` cuts `content` and
    /// `thinking` to about 4 KB and lists them; `transcript_full.jsonl`
    /// holds the whole reply (here a JSON answer for an Agentic Works job).
    #[test]
    #[cfg(unix)]
    fn a_reply_the_compact_transcript_cut_is_read_from_the_full_one() {
        let reply = format!(
            r#"{{"teams":[{{"id":"t1","label":"{}"}}]}}"#,
            "x".repeat(30_000)
        );
        let step = |content: &str, thinking: &str, cut: bool| {
            let mut step = json!({
                "step_index": 9, "source": "MODEL", "type": "PLANNER_RESPONSE",
                "status": "DONE", "created_at": "2026-11-22T18:25:41Z",
                "model": "gemini-4-hydrogen-high", "input_tokens": 22945,
                "cache_read_tokens": 18919, "output_tokens": 48725,
                "content": content, "thinking": thinking,
            });
            if cut {
                step["truncated_fields"] = json!(["content", "thinking"]);
            }
            step.to_string()
        };
        let head = [
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-22T18:25:36Z","content":"<USER_REQUEST>\nanalyse\n</USER_REQUEST>"}"#,
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T18:25:36Z","model":"gemini-4-hydrogen-high","thinking":"t","tool_calls":[{"name":"read_file","args":{"Path":"/w/0/segment.mp4"}}]}"#,
            r#"{"step_index":4,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-22T18:25:40Z","content":"Created At: 2026-11-22T19:25:40+01:00"}"#,
        ]
        .join("\n");
        let dir = tempfile::tempdir().unwrap();
        let compact = dir.path().join("transcript.jsonl");
        let full = dir.path().join("transcript_full.jsonl");
        let thinking = "y".repeat(50_000);
        fs::write(
            &compact,
            format!(
                "{head}\n{}\n",
                step(&reply[..4113], &thinking[..4120], true)
            ),
        )
        .unwrap();
        // The full transcript not written yet: the reply is still cut.
        assert_eq!(transcript_reply(&compact), Lookup::Cut);
        // The full transcript one turn behind, ending at the last turn's
        // reply: that older answer is not this turn's.
        let earlier = r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T18:20:00Z","model":"gemini-4-hydrogen-high","content":"{\"teams\":[]}"}"#;
        fs::write(&full, format!("{earlier}\n")).unwrap();
        assert_eq!(transcript_reply(&compact), Lookup::Cut);
        fs::write(
            &full,
            format!("{head}\n{}\n", step(&reply, &thinking, false)),
        )
        .unwrap();
        assert_eq!(transcript_reply(&compact), Lookup::Reply(reply.clone()));
        // A short reply is read from the compact transcript alone.
        fs::remove_file(&full).unwrap();
        fs::write(&compact, format!("{head}\n{}\n", step("{}", "t", false))).unwrap();
        assert_eq!(transcript_reply(&compact), Lookup::Reply("{}".to_owned()));
        // Named by its full transcript, the turn is read the same way.
        assert_eq!(transcript_reply(&full), Lookup::Reply("{}".to_owned()));
        // Any other file is no transcript.
        assert_eq!(
            transcript_reply(&dir.path().join("notes.jsonl")),
            Lookup::NotYet
        );
    }

    /// An Agentic Works job, synthetic but shaped like a real one: Antigravity
    /// 1.5.14's `Stop` names `transcript_full.jsonl`, not `transcript.jsonl`,
    /// and both files already hold the whole turn when the hook runs (measured
    /// with a probe `Stop` hook). The reply, 29 KB of JSON, is cut in the
    /// compact transcript and whole in the full one.
    #[test]
    #[cfg(unix)]
    fn a_stop_that_names_the_full_transcript_gets_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("brain/5e7a9b31/.system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let reply = format!(
            r#"{{"aisles":[{{"id":"a1","label":"Aisle 4"}}],"observations":[{}],"limitations":[]}}"#,
            vec![r#"{"id":"o1","category":"shelves","value":"185","unit":"mm"}"#; 520].join(",")
        );
        assert!(reply.len() > 29_000);
        let thinking = "Careful review of each camera clip. ".repeat(1_000);
        let segment = |n: u32, quote: bool| {
            let path = format!("/w/.connecto/works/7-0a1b2c3d-4e5/{n}/clip.mp4");
            if quote { format!("\"{path}\"") } else { path }
        };
        let steps = |compact: bool| {
            let cut = |text: &str, at: usize| {
                if compact {
                    text[..at].to_owned()
                } else {
                    text.to_owned()
                }
            };
            let request = format!(
                "<USER_REQUEST>\nYou inspect one warehouse camera clip.\n{}</USER_REQUEST>",
                "r".repeat(19_000)
            );
            let mut user = json!({"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT",
                "status": "DONE", "created_at": "2026-01-15T09:00:00Z", "content": cut(&request, 4119)});
            let mut last = json!({"step_index": 10, "source": "MODEL", "type": "PLANNER_RESPONSE",
                "status": "DONE", "created_at": "2026-01-15T09:00:18Z", "model": "gemini-4-hydrogen-high",
                "input_tokens": 8000, "cache_read_tokens": 32000, "output_tokens": 36000,
                "content": cut(&reply, 4114), "thinking": cut(&thinking, 4107)});
            if compact {
                user["truncated_fields"] = json!(["content"]);
                last["truncated_fields"] = json!(["content", "thinking"]);
            }
            let mut lines = vec![
                user,
                json!({"step_index": 1, "source": "SYSTEM_SDK", "type": "SYSTEM_MESSAGE",
                    "status": "DONE", "created_at": "2026-01-15T09:00:00Z", "content": "The following is a <SYSTEM_MESSAGE>"}),
                json!({"step_index": 3, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE",
                    "created_at": "2026-01-15T09:00:00Z", "model": "gemini-4-hydrogen-high", "thinking": "t",
                    "tool_calls": [{"name": "read_file", "args": {"Path": segment(0, compact)}}]}),
            ];
            for index in [4, 8, 9] {
                lines.push(json!({"step_index": index, "source": "MODEL", "type": "GENERIC", "status": "DONE",
                    "created_at": "2026-01-15T09:00:18Z", "content": "Created At: 2026-01-15T10:00:18+01:00",
                    "media": [{"mime_type": "video/mp4", "uri": format!("file:///m/step_{index}_media_0.mp4")}]}));
            }
            lines.push(last);
            lines
                .iter()
                .map(|line| format!("{line}\n"))
                .collect::<String>()
        };
        fs::write(logs.join("transcript.jsonl"), steps(true)).unwrap();
        fs::write(logs.join("transcript_full.jsonl"), steps(false)).unwrap();
        let full = logs.join("transcript_full.jsonl");
        let stop = json!({
            "artifactDirectoryPath": dir.path().join("brain/5e7a9b31"),
            "continuationActive": false, "conversationId": "5e7a9b31", "error": "",
            "executionNum": 0, "fullyIdle": true, "hookEventName": "Stop",
            "modelName": "gemini-4-hydrogen-high", "terminationReason": "NO_TOOL_CALL",
            "transcriptPath": full,
        });
        let mut state = State::default();
        let Action::Stopped {
            payload,
            transcript: Some(transcript),
        } = action("Stop", &stop, &mut state)
        else {
            panic!("no stop with a transcript");
        };
        let mut payload = payload;
        with_reply(&mut payload, transcript_reply(Path::new(&transcript)));
        assert_eq!(payload["last_assistant_message"], json!(reply));
        assert!(payload.get("connecto_dropped").is_none());
    }

    #[test]
    fn only_a_transcript_file_is_read() {
        assert_eq!(read_tail(Path::new("/etc/passwd")), None);
    }
}
