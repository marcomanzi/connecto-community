//! Antigravity CLI (Google's `agy`, the successor of Gemini CLI). Read from
//! its documentation and from the strings of the 1.5.11 program; no turn of a
//! signed-in `agy` has been run against this yet (docs/specs section 5.8,
//! "Antigravity specifics").
//!
//! Antigravity reads its hooks only from files: `~/.gemini/config/hooks.json`
//! for every workspace, or `<workspace>/.agents/hooks.json` for one. It has
//! no flag to name another file and no variable that moves its configuration
//! folder. So Connecto keeps one named entry, `connecto`, in the user's global
//! file ([`hooks_file`]), and leaves everything else in it as it was. The
//! entry runs Connecto's hook helper for `SessionStart`, `PreInvocation` and
//! `Stop`. Outside a Connecto terminal the helper finds none of its variables
//! and only answers `{}`.
//!
//! The helper turns Antigravity's payloads into the ones Claude Code's hooks
//! send (`connecto-hook antigravity`), so everything after it (the frames,
//! their size limits, the turn tracker) is the path Claude already uses.
//!
//! For the same reason an Antigravity terminal always uses the user's own
//! configuration and sign-in (`~/.gemini`): there is no isolated profile.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use connecto_core::{AgentMode, AuthState, Readiness, ToolEvent};
use serde_json::{Map, Value, json};

use crate::claude::{Claude, prompt_head};
use crate::platform::Flavor;
use crate::quote::{QuoteError, cmd_quote, shell_quote};
use crate::spec::{
    AdapterError, Capabilities, ComposerState, ComposerView, Continuation, DraftKeys, HookCtx,
    LaunchCtx, LaunchSpec, LogMark, LoginCtx, Screen, ToolAdapter, base_env, hook_env,
    under_profile, with_bridge_path,
};

/// The bridge instructions' file in the run folder. The helper reads it at
/// `SessionStart` and hands it to the model as a system message.
const INSTRUCTIONS_FILE: &str = "connecto-antigravity-instructions.md";
/// Where the helper finds the instructions file.
pub const LAUNCH_TEXT_ENV: &str = "CONNECTO_AGY_LAUNCH_TEXT";
/// Set when notices from other terminals are handed to the model.
pub const INBOX_ENV: &str = "CONNECTO_AGY_INBOX";
/// Set on every Antigravity terminal: the helper reports nothing without it,
/// so an `agy` run inside another tool's terminal is not taken for that
/// terminal's tool.
pub const TERMINAL_ENV: &str = "CONNECTO_AGY_TERMINAL";
/// The name of Connecto's entry in `hooks.json`.
pub const HOOK_NAME: &str = "connecto";
/// The events Connecto's entry runs the helper for.
const HOOK_EVENTS: [&str; 3] = ["SessionStart", "PreInvocation", "Stop"];
/// Seconds Antigravity gives one run of the helper. A `Stop` may wait a moment
/// for the transcript to hold the reply.
const HOOK_TIMEOUT_S: u64 = 10;

/// Antigravity's own folder in the user's home: its settings, sign-in data,
/// and the conversations (`brain/<id>/.system_generated/logs/transcript.jsonl`).
#[must_use]
pub fn antigravity_profile(home: &Path) -> PathBuf {
    home.join(".gemini").join("antigravity-cli")
}

/// The global hooks file Connecto keeps its entry in.
#[must_use]
pub fn hooks_path(home: &Path) -> PathBuf {
    home.join(".gemini").join("config").join("hooks.json")
}

/// Why the hooks file was left alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HooksFileError {
    #[error("the hooks file is not a JSON object, so Connecto does not change it")]
    NotAnObject,
    #[error(transparent)]
    Quote(#[from] QuoteError),
}

/// The helper's command for `event`, for the shell Antigravity runs hooks
/// with (`sh -c`; `cmd /c` on Windows).
fn hook_command(flavor: Flavor, hook: &Path, event: &str) -> Result<String, QuoteError> {
    let program = hook.to_string_lossy();
    let program = match flavor {
        Flavor::Unix => shell_quote(&program),
        Flavor::Windows => cmd_quote(&program)?,
    };
    Ok(format!("{program} antigravity --event {event}"))
}

/// Connecto's entry: each event runs the helper once.
fn entry(flavor: Flavor, hook: &Path) -> Result<Value, QuoteError> {
    let mut events = Map::new();
    for event in HOOK_EVENTS {
        events.insert(
            event.to_owned(),
            json!([{
                "type": "command",
                "command": hook_command(flavor, hook, event)?,
                "timeout": HOOK_TIMEOUT_S,
            }]),
        );
    }
    Ok(Value::Object(events))
}

fn hooks_file_as(
    flavor: Flavor,
    existing: Option<&str>,
    hook: &Path,
) -> Result<Option<String>, HooksFileError> {
    let mut file = match existing.map(|text| text.trim_start_matches('\u{feff}').trim()) {
        None | Some("") => Map::new(),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(file)) => file,
            _ => return Err(HooksFileError::NotAnObject),
        },
    };
    let wanted = entry(flavor, hook)?;
    if file.get(HOOK_NAME) == Some(&wanted) {
        return Ok(None);
    }
    file.insert(HOOK_NAME.to_owned(), wanted);
    let mut text = serde_json::to_string_pretty(&Value::Object(file)).unwrap_or_default();
    text.push('\n');
    Ok(Some(text))
}

/// The hooks file with Connecto's entry in it, given what the file holds now
/// (`None`: there is no file). `Ok(None)` when it already holds the entry as
/// it should be. Every other entry is kept as it was.
pub fn hooks_file(existing: Option<&str>, hook: &Path) -> Result<Option<String>, HooksFileError> {
    hooks_file_as(Flavor::HOST, existing, hook)
}

/// Antigravity's settings file, where its permission rules are kept.
#[must_use]
pub fn settings_path(home: &Path) -> PathBuf {
    antigravity_profile(home).join("settings.json")
}

/// The rule that lets Antigravity read `folder` and everything in it, and
/// nothing more: a `read_file` allow rule mounts it read-only, also in its
/// terminal sandbox. Antigravity offers no flag for a read-only folder
/// (`--add-dir` makes a workspace folder, which it may write).
#[must_use]
pub fn read_rule(folder: &Path) -> String {
    format!("read_file({})", folder.display())
}

/// The settings file with `rule` in `permissions.allow`, given what the file
/// holds now (`None`: there is no file). `Ok(None)` when the rule is there
/// already. Every other setting and rule is kept as it was. A file, or a
/// `permissions` or `allow`, of another shape is refused and left alone.
pub fn settings_file(existing: Option<&str>, rule: &str) -> Result<Option<String>, HooksFileError> {
    let mut file = match existing.map(|text| text.trim_start_matches('\u{feff}').trim()) {
        None | Some("") => Map::new(),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(file)) => file,
            _ => return Err(HooksFileError::NotAnObject),
        },
    };
    let permissions = file
        .entry("permissions")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or(HooksFileError::NotAnObject)?;
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or(HooksFileError::NotAnObject)?;
    if allow.iter().any(|existing| existing.as_str() == Some(rule)) {
        return Ok(None);
    }
    allow.push(Value::String(rule.to_owned()));
    let mut text = serde_json::to_string_pretty(&Value::Object(file)).unwrap_or_default();
    text.push('\n');
    Ok(Some(text))
}

/// Where Antigravity keeps its state, in the tool's environment: nothing to
/// move it to, so only what every agent gets.
fn tool_env(
    flavor: Flavor,
    home: &Path,
    tmp: &Path,
    inherited: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    base_env(flavor, home, tmp, inherited)
}

/// The Antigravity adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct Antigravity;

impl Antigravity {
    /// [`ToolAdapter::launch`] for `flavor`'s system.
    fn launch_as(flavor: Flavor, ctx: &LaunchCtx) -> LaunchSpec {
        let mut files = Vec::new();
        let mut args: Vec<OsString> = Vec::new();
        if let Some(id) = &ctx.resume {
            args.push(format!("--conversation={id}").into());
        }
        match ctx.agent_mode {
            AgentMode::Bypass => args.push("--dangerously-skip-permissions".into()),
            AgentMode::Auto => args.push("--mode=auto".into()),
            AgentMode::Prompt => {}
        }
        if let Some(effort) = &ctx.effort {
            args.push(format!("--effort={effort}").into());
        }
        if let Some(model) = &ctx.model {
            args.push(format!("--model={model}").into());
        }
        let first = match &ctx.resume {
            Some(_) => ctx.resume_note.as_ref(),
            None => ctx.initial_prompt.as_ref(),
        };
        if let Some(message) = first {
            // In one word: a message may start with `-`.
            args.push(format!("--prompt-interactive={message}").into());
        }
        let mut env = tool_env(flavor, &ctx.home, &ctx.session_tmp, &ctx.inherited);
        env.extend(hook_env(ctx));
        env.push((TERMINAL_ENV.into(), "1".into()));
        // The bridge text and the package instructions, handed over as a
        // system message at `SessionStart`, beside the project's AGENTS.md.
        if let Some(text) = ctx.launch_text() {
            let instructions = ctx.run.join(INSTRUCTIONS_FILE);
            files.push((instructions.clone(), text.into_bytes()));
            env.push((LAUNCH_TEXT_ENV.into(), instructions.into()));
        }
        if ctx.bridge.hooks {
            env.push((INBOX_ENV.into(), "1".into()));
        }
        with_bridge_path(flavor, &mut env, ctx);
        LaunchSpec {
            program: ctx.entry.clone(),
            args,
            cwd: ctx.project.clone(),
            env,
            files,
        }
    }
}

impl ToolAdapter for Antigravity {
    fn launch(&self, ctx: &LaunchCtx) -> LaunchSpec {
        Self::launch_as(Flavor::HOST, ctx)
    }

    /// Antigravity signs in with a Google account in its own first screen (or
    /// reads `GEMINI_API_KEY`). So "login" is the CLI itself, started in the
    /// run folder, where its sign-in screen comes up.
    fn login(&self, ctx: &LoginCtx) -> LaunchSpec {
        LaunchSpec {
            program: ctx.entry.clone(),
            args: Vec::new(),
            cwd: ctx.run.clone(),
            env: tool_env(Flavor::HOST, &ctx.home, &ctx.session_tmp, &ctx.inherited),
            files: Vec::new(),
        }
    }

    /// Whether `agy` runs at all: it prints its version.
    fn status(&self, ctx: &LoginCtx) -> LaunchSpec {
        LaunchSpec {
            args: vec!["--version".into()],
            cwd: ctx.session_tmp.clone(),
            ..self.login(ctx)
        }
    }

    /// An `agy` that answers is ready to start. Whether it is signed in is
    /// not asked: the sign-in is kept in the system keychain, and a terminal
    /// that is not signed in shows the sign-in screen itself.
    fn parse_status(&self, output: &str) -> AuthState {
        let answered = output.split_whitespace().any(|word| {
            let word = word.trim_start_matches('v');
            word.split('.').count() >= 3 && word.starts_with(|c: char| c.is_ascii_digit())
        });
        if answered {
            AuthState::LoggedIn {
                method: "google".to_owned(),
            }
        } else {
            AuthState::Unknown
        }
    }

    /// The helper sends Claude's hook payloads, with no background tasks.
    fn parse_hook(&self, payload: &[u8], ctx: &HookCtx) -> Result<Vec<ToolEvent>, AdapterError> {
        Claude.parse_hook(payload, ctx)
    }

    /// Antigravity's ready signal is the `SessionStart` hook, not the screen.
    fn readiness(&self, _screen: &str) -> Readiness {
        Readiness::Unknown
    }

    /// Whitespace around a prompt is not counted as a difference: not
    /// measured, and what every other agent does.
    fn ack_form(&self, text: &str) -> String {
        text.trim().to_owned()
    }

    fn send_anyway_text(&self) -> &'static str {
        "Antigravity has not reported that it started. The message may be lost; it would then end Uncertain."
    }

    fn capabilities(&self, _version: &str) -> Capabilities {
        Capabilities {
            immediate_ack: true,
        }
    }

    fn turn_log(&self, reported: &Path, ctx: &HookCtx) -> Option<PathBuf> {
        under_profile(reported, &ctx.profile)
    }

    /// The conversation's `transcript.jsonl`: one step a line. A
    /// `USER_INPUT` step is a prompt, a `PLANNER_RESPONSE` without tool calls
    /// ends the turn, and one with tool calls is the turn at work.
    fn read_log(&self, line: &str, whole: bool) -> LogMark {
        read_step(line, whole)
    }

    /// Antigravity starts turns by itself (a background task that ends), but
    /// the hooks do not say why, so none is taken for a continuation.
    fn continuation(&self, _prompt: &str) -> Option<Continuation> {
        None
    }

    /// Not measured. The draft guard follows plain typing only; a long paste
    /// is shown as a `[Pasted text #N]` placeholder (its changelog).
    fn draft_keys(&self) -> DraftKeys {
        DraftKeys {
            multiline: false,
            esc_enter_newline: false,
            ctrl_c_clears: false,
            collapses_pastes: true,
        }
    }

    /// Antigravity's input box has not been measured, so the screen is not
    /// read for it.
    fn composer(&self, _screen: Screen<'_>) -> Option<ComposerView> {
        None
    }
}

/// What the screen says about Antigravity's input box: nothing yet. Until its
/// start hook comes, it is taken to be starting.
#[must_use]
pub fn antigravity_composer_state(_screen: Screen<'_>) -> ComposerState {
    ComposerState::Booting
}

/// One line of `transcript.jsonl`.
fn read_step(line: &str, whole: bool) -> LogMark {
    if !whole {
        // A cut line cannot say whether it holds tool calls.
        return LogMark::Other;
    }
    let Ok(step) = serde_json::from_str::<Value>(line) else {
        return LogMark::Other;
    };
    let text = |step: &Value| {
        step.get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match step.get("type").and_then(Value::as_str) {
        Some("USER_INPUT") => text(&step).map_or(LogMark::Other, |text| LogMark::Prompt {
            text: prompt_head(&text),
        }),
        Some("PLANNER_RESPONSE") => {
            let calls = step
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty());
            if calls {
                LogMark::Activity
            } else {
                LogMark::FinalReply
            }
        }
        _ => LogMark::Other,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use connecto_core::{EventText, Reply};

    use super::*;
    use crate::spec::tests::{launch_ctx, launch_ctx_windows, login_ctx};

    fn args(spec: &LaunchSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env_of(spec: &LaunchSpec, name: &str) -> Option<String> {
        spec.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string_lossy().into_owned())
    }

    #[test]
    fn a_session_skips_permissions_and_gets_the_instructions_and_the_hook_channel() {
        let ctx = launch_ctx();
        let spec = Antigravity.launch(&ctx);
        assert_eq!(spec.program, PathBuf::from("/store/tool"));
        assert_eq!(spec.cwd, PathBuf::from("/project"));
        assert_eq!(args(&spec), ["--dangerously-skip-permissions"]);
        let instructions = ctx.run.join(INSTRUCTIONS_FILE);
        assert_eq!(
            env_of(&spec, LAUNCH_TEXT_ENV),
            Some(instructions.to_string_lossy().into_owned())
        );
        assert_eq!(env_of(&spec, INBOX_ENV).as_deref(), Some("1"));
        assert_eq!(env_of(&spec, TERMINAL_ENV).as_deref(), Some("1"));
        assert_eq!(
            env_of(&spec, "CONNECTO_HOOK_TOKEN").as_deref(),
            Some("token")
        );
        assert_eq!(env_of(&spec, "HOME").as_deref(), Some("/home"));
        let separator = Flavor::HOST.path_separator();
        assert_eq!(
            env_of(&spec, "PATH"),
            Some(format!("/data/cli/bin{separator}/usr/bin"))
        );
        assert_eq!(
            spec.files,
            [(instructions, b"LAUNCH TEXT".to_vec())],
            "only the instructions, in the run folder"
        );
    }

    #[test]
    fn each_mode_has_its_flag() {
        let mut ctx = launch_ctx();
        ctx.agent_mode = AgentMode::Auto;
        assert_eq!(args(&Antigravity.launch(&ctx)), ["--mode=auto"]);
        ctx.agent_mode = AgentMode::Prompt;
        assert!(args(&Antigravity.launch(&ctx)).is_empty());
    }

    #[test]
    fn a_resume_names_the_conversation_and_takes_the_note_not_the_first_prompt() {
        let mut ctx = launch_ctx();
        ctx.agent_mode = AgentMode::Prompt;
        ctx.initial_prompt = Some("-first".to_owned());
        ctx.resume_note = Some("note".to_owned());
        ctx.model = Some("gemini-4-carbon".to_owned());
        assert_eq!(
            args(&Antigravity.launch(&ctx)),
            ["--model=gemini-4-carbon", "--prompt-interactive=-first"]
        );
        ctx.resume = Some("ec33ebf9-0cba-4100-8142-c61503f6c587".to_owned());
        assert_eq!(
            args(&Antigravity.launch(&ctx)),
            [
                "--conversation=ec33ebf9-0cba-4100-8142-c61503f6c587",
                "--model=gemini-4-carbon",
                "--prompt-interactive=note"
            ]
        );
    }

    #[test]
    fn without_the_bridge_there_are_no_instructions_and_no_inbox() {
        let mut ctx = launch_ctx();
        ctx.bridge.text = None;
        ctx.bridge.hooks = false;
        let spec = Antigravity.launch(&ctx);
        assert!(spec.files.is_empty());
        assert_eq!(env_of(&spec, LAUNCH_TEXT_ENV), None);
        assert_eq!(env_of(&spec, INBOX_ENV), None);
        // Turns are still reported.
        assert!(env_of(&spec, "CONNECTO_HOOK_SOCK").is_some());
        assert_eq!(env_of(&spec, TERMINAL_ENV).as_deref(), Some("1"));
    }

    #[test]
    fn status_asks_the_version_and_an_answer_means_ready() {
        let spec = Antigravity.status(&login_ctx());
        assert_eq!(args(&spec), ["--version"]);
        assert_eq!(spec.cwd, PathBuf::from("/run/tmp"));
        assert_eq!(env_of(&spec, "CONNECTO_HOOK_TOKEN"), None);
        assert!(matches!(
            Antigravity.parse_status("agy version 1.5.11\n"),
            AuthState::LoggedIn { .. }
        ));
        assert_eq!(Antigravity.parse_status(""), AuthState::Unknown);
        assert_eq!(Antigravity.login(&login_ctx()).cwd, PathBuf::from("/run"));
    }

    #[test]
    fn the_hooks_file_gets_connectos_entry_and_keeps_the_others() {
        let hook =
            Path::new("/Users/jane/Library/Application Support/connecto/cli/bin/connecto-hook");
        let fresh = hooks_file_as(Flavor::Unix, None, hook).unwrap().unwrap();
        let value: Value = serde_json::from_str(&fresh).unwrap();
        for event in HOOK_EVENTS {
            let handler = &value[HOOK_NAME][event][0];
            assert_eq!(handler["type"], "command");
            assert_eq!(handler["timeout"], HOOK_TIMEOUT_S);
            assert_eq!(
                handler["command"],
                format!(
                    "'/Users/jane/Library/Application Support/connecto/cli/bin/connecto-hook' antigravity --event {event}"
                )
            );
        }
        // Already as it should be: nothing to write.
        assert_eq!(
            hooks_file_as(Flavor::Unix, Some(&fresh), hook).unwrap(),
            None
        );
        // The user's own hooks stay, a byte order mark is read through, and a
        // stale entry of Connecto's is replaced.
        let theirs = "\u{feff}{\"lint\":{\"PostToolUse\":[]},\"connecto\":{\"Stop\":[]}}";
        let merged = hooks_file_as(Flavor::Unix, Some(theirs), hook)
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["lint"], json!({"PostToolUse": []}));
        assert!(value[HOOK_NAME]["SessionStart"].is_array());
        // A file that is not an object is never touched.
        for broken in ["[1]", "{not json"] {
            assert_eq!(
                hooks_file_as(Flavor::Unix, Some(broken), hook),
                Err(HooksFileError::NotAnObject)
            );
        }
    }

    #[test]
    fn on_windows_the_command_is_for_cmd() {
        let ctx = launch_ctx_windows();
        let file = hooks_file_as(Flavor::Windows, None, &ctx.hook_binary)
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&file).unwrap();
        assert_eq!(
            value[HOOK_NAME]["Stop"][0]["command"],
            r#""C:\Users\Jane O'Neil\AppData\Roaming\connecto\cli\bin\connecto-hook.exe" antigravity --event Stop"#
        );
        let spec = Antigravity::launch_as(Flavor::Windows, &ctx);
        assert_eq!(
            env_of(&spec, "TEMP").as_deref(),
            Some(r"C:\Users\Jane O'Neil\AppData\Roaming\connecto\run\r1\tmp")
        );
        assert!(
            hooks_file_as(
                Flavor::Windows,
                None,
                Path::new(r"C:\a%PATH%\connecto-hook.exe")
            )
            .is_err()
        );
    }

    fn hook_ctx() -> HookCtx {
        HookCtx {
            profile: "/home/.gemini/antigravity-cli".into(),
            hooks: true,
        }
    }

    #[test]
    fn the_helpers_payloads_become_events() {
        let started = br#"{"hook_event_name":"SessionStart","session_id":"c1","transcript_path":"/home/.gemini/antigravity-cli/brain/c1/.system_generated/logs/transcript.jsonl"}"#;
        assert!(matches!(
            Antigravity
                .parse_hook(started, &hook_ctx())
                .unwrap()
                .as_slice(),
            [ToolEvent::Started {
                turn_log: Some(_),
                ..
            }]
        ));
        let prompt = br#"{"hook_event_name":"UserPromptSubmit","prompt":"hello"}"#;
        assert_eq!(
            Antigravity.parse_hook(prompt, &hook_ctx()).unwrap(),
            [ToolEvent::PromptSubmitted {
                prompt: EventText::new("hello".to_owned()),
                turn: None,
            }]
        );
        let stop = br#"{"hook_event_name":"Stop","last_assistant_message":"done"}"#;
        assert!(matches!(
            Antigravity
                .parse_hook(stop, &hook_ctx())
                .unwrap()
                .as_slice(),
            [ToolEvent::TurnEnded {
                reply: Reply::Text(_),
                failure: None,
                ..
            }]
        ));
        let canceled = br#"{"hook_event_name":"StopFailure","error":"user_canceled"}"#;
        assert!(matches!(
            Antigravity
                .parse_hook(canceled, &hook_ctx())
                .unwrap()
                .as_slice(),
            [ToolEvent::TurnEnded {
                reply: Reply::None,
                failure: Some(_),
                ..
            }]
        ));
    }

    #[test]
    fn the_transcript_marks_prompts_final_replies_and_work() {
        assert_eq!(
            Antigravity.read_log(
                r#"{"step_index":0,"type":"USER_INPUT","status":"DONE","content":"say hi"}"#,
                true
            ),
            LogMark::Prompt {
                text: "say hi".to_owned()
            }
        );
        assert_eq!(
            Antigravity.read_log(
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"","tool_calls":[{"name":"run_command","args":{}}]}"#,
                true
            ),
            LogMark::Activity
        );
        let final_reply =
            r#"{"step_index":3,"type":"PLANNER_RESPONSE","content":"hi","tool_calls":[]}"#;
        assert_eq!(Antigravity.read_log(final_reply, true), LogMark::FinalReply);
        assert_eq!(Antigravity.read_log(final_reply, false), LogMark::Other);
        for other in [
            r#"{"type":"SYSTEM_MESSAGE","content":"x"}"#,
            r#"{"type":"GENERIC","status":"DONE"}"#,
            "not json",
        ] {
            assert_eq!(Antigravity.read_log(other, true), LogMark::Other, "{other}");
        }
    }

    #[test]
    fn only_a_transcript_under_antigravitys_folder_is_tailed() {
        assert!(
            Antigravity
                .turn_log(
                    Path::new("/home/.gemini/antigravity-cli/brain/c/.system_generated/logs/transcript.jsonl"),
                    &hook_ctx()
                )
                .is_some()
        );
        assert!(
            Antigravity
                .turn_log(Path::new("/etc/passwd"), &hook_ctx())
                .is_none()
        );
    }

    #[test]
    fn the_attachments_rule_is_added_once_beside_the_users_settings() {
        let rule = read_rule(Path::new("/data/chat-attachments"));
        assert_eq!(rule, "read_file(/data/chat-attachments)");
        let fresh: Value =
            serde_json::from_str(&settings_file(None, &rule).unwrap().unwrap()).unwrap();
        assert_eq!(fresh, json!({"permissions": {"allow": [rule]}}));

        let users = r#"{"model": "x", "permissions": {"allow": ["command(git)"], "deny": ["command(sudo)"]}}"#;
        let text = settings_file(Some(users), &rule).unwrap().unwrap();
        let kept: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(kept["model"], "x");
        assert_eq!(kept["permissions"]["deny"], json!(["command(sudo)"]));
        assert_eq!(kept["permissions"]["allow"], json!(["command(git)", rule]));
        assert_eq!(settings_file(Some(&text), &rule), Ok(None), "already there");

        for odd in [
            "[1]",
            r#"{"permissions": 3}"#,
            r#"{"permissions": {"allow": "x"}}"#,
        ] {
            assert_eq!(
                settings_file(Some(odd), &rule),
                Err(HooksFileError::NotAnObject),
                "{odd}"
            );
        }
        assert_eq!(
            settings_path(Path::new("/home")),
            PathBuf::from("/home/.gemini/antigravity-cli/settings.json")
        );
    }

    #[test]
    fn the_profile_and_the_hooks_file_are_in_the_users_gemini_folder() {
        assert_eq!(
            antigravity_profile(Path::new("/home")),
            PathBuf::from("/home/.gemini/antigravity-cli")
        );
        assert_eq!(
            hooks_path(Path::new("/home")),
            PathBuf::from("/home/.gemini/config/hooks.json")
        );
    }
}
