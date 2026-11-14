//! Codex. Checked against 0.166.0.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use connecto_core::{AgentMode, AuthState, EventText, Readiness, Reply, ToolEvent};
use serde_json::Value;

use crate::platform::Flavor;
use crate::quote::{QuoteError, powershell_quote, shell_quote};
use crate::spec::{
    AdapterError, Capabilities, ComposerState, Continuation, HookCtx, LaunchCtx, LaunchSpec,
    LogMark, LoginCtx, ToolAdapter, base_env, hook_env, under_profile, with_bridge_path,
};

/// Stops a filled profile folder from costing unbounded time per event.
const MAX_ROLLOUT_ENTRIES: usize = 50_000;
const ROLLOUT_DEPTH: u8 = 4;

/// A TOML basic string.
fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if u32::from(ch) < 0x20 || ch == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", u32::from(ch)));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn tool_env(
    flavor: Flavor,
    home: &Path,
    tmp: &Path,
    profile: &Path,
    system_config: bool,
    inherited: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let mut env = base_env(flavor, home, tmp, inherited);
    if !system_config {
        env.push(("CODEX_HOME".into(), profile.into()));
    }
    env
}

/// A turn id is compared with other ids, never with paths.
fn safe_turn_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A thread ID is compared with file names, so it must not carry a path.
fn safe_thread_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn find_rollout(dir: &Path, suffix: &str, depth: u8, budget: &mut usize) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth > 0 && find_rollout(&entry.path(), suffix, depth - 1, budget) {
                return true;
            }
        } else if kind.is_file() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(suffix) {
                return true;
            }
        }
    }
    false
}

/// Hidden title threads have no rollout file, which is how they are told apart.
fn has_rollout(profile: &Path, thread_id: &str) -> bool {
    let mut budget = MAX_ROLLOUT_ENTRIES;
    find_rollout(
        &profile.join("sessions"),
        &format!("-{thread_id}.jsonl"),
        ROLLOUT_DEPTH,
        &mut budget,
    )
}

/// The events Codex gets a hook for. `Stop` and `Interrupt` are not added:
/// `notify` stays the turn end, which keeps their unproven interplay small.
const HOOK_EVENTS: [&str; 2] = ["SessionStart", "UserPromptSubmit"];

/// The command Codex runs for `event`. On macOS Codex runs it with `sh -c`;
/// on Windows with `pwsh.exe -NoProfile -Command` (or Windows PowerShell),
/// where a quoted path alone is a string, not a command, so `&` calls it
/// (checked on Codex 0.167.2).
fn hook_command(flavor: Flavor, helper: &str, event: &str) -> Result<String, QuoteError> {
    let context = if event == "UserPromptSubmit" {
        " --context"
    } else {
        ""
    };
    Ok(match flavor {
        Flavor::Unix => format!(
            "{} codex-hook --event {event}{context}",
            shell_quote(helper)
        ),
        Flavor::Windows => format!(
            "& {} codex-hook --event {event}{context}",
            powershell_quote(helper)?
        ),
    })
}

fn hook_config_as(flavor: Flavor, helper: &Path) -> Result<Vec<String>, QuoteError> {
    HOOK_EVENTS
        .iter()
        .map(|event| {
            let command = hook_command(flavor, &helper.to_string_lossy(), event)?;
            Ok(format!(
                "hooks.{event}=[{{hooks=[{{type=\"command\",command={}}}]}}]",
                toml_string(&command)
            ))
        })
        .collect()
}

/// The `-c` values that register the bridge's hooks, for this helper. Preflight
/// passes the same values to `codex app-server` to read their trust hashes, so
/// the definitions here are the ones that get trusted.
///
/// # Errors
///
/// On Windows, a helper path PowerShell cannot take literally (a control
/// character or `"`). No hook is registered then. It never fails on macOS.
pub fn hook_config(helper: &Path) -> Result<Vec<String>, QuoteError> {
    hook_config_as(Flavor::HOST, helper)
}

/// Where Codex says a hook given with `-c` came from. The hook keys are built
/// from this one value.
///
/// Windows: checked on Codex 0.167.2 with `CODEX_HOME` on `C:`, from working
/// folders on `C:`, `D:` and `B:`. Whether the drive letter follows
/// `CODEX_HOME` or the system drive is **unverified**, so a listed key is
/// matched with any drive letter ([`hook_key_matches`]) and the trust value is
/// built from the key Codex listed.
const fn session_flags_source(flavor: Flavor) -> &'static str {
    match flavor {
        Flavor::Unix => "/<session-flags>/config.toml",
        Flavor::Windows => r"C:\<session-flags>\config.toml",
    }
}

/// `SessionStart` → `session_start`, as Codex names events in hook keys.
fn snake_case(event: &str) -> String {
    let mut out = String::with_capacity(event.len() + 4);
    for (index, ch) in event.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn hook_keys(flavor: Flavor) -> [String; 2] {
    HOOK_EVENTS.map(|event| format!("{}:{}:0:0", session_flags_source(flavor), snake_case(event)))
}

/// The hook keys Codex reports for [`hook_config`], in the order of
/// [`HOOK_EVENTS`]. A key names where the definition came from.
#[must_use]
pub fn expected_hook_keys() -> [String; 2] {
    hook_keys(Flavor::HOST)
}

fn key_matches(flavor: Flavor, expected: &str, listed: &str) -> bool {
    if expected == listed {
        return true;
    }
    match flavor {
        Flavor::Unix => false,
        // Any drive letter, the rest exact: `X:\<session-flags>\…`.
        Flavor::Windows => {
            let drive = |key: &str| {
                let bytes = key.as_bytes();
                bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
            };
            drive(expected)
                && drive(listed)
                && expected
                    .get(1..)
                    .is_some_and(|rest| Some(rest) == listed.get(1..))
        }
    }
}

/// Whether `listed`, a key from `codex app-server`'s `hooks/list`, is the hook
/// [`expected_hook_keys`] names as `expected`. On macOS they are equal; on
/// Windows the drive letter may differ (see [`session_flags_source`]).
#[must_use]
pub fn hook_key_matches(expected: &str, listed: &str) -> bool {
    key_matches(Flavor::HOST, expected, listed)
}

/// A TOML key for `hooks.state`. macOS: a basic string, as it always was.
/// Windows: a literal string, so the backslashes stay as they are (checked on
/// Codex 0.167.2: a basic string with single backslashes stops Codex).
fn trust_key(flavor: Flavor, key: &str) -> Result<String, String> {
    match flavor {
        Flavor::Unix => Ok(format!(
            "\"{}\"",
            key.replace('\\', "\\\\").replace('"', "\\\"")
        )),
        Flavor::Windows => {
            if key.chars().any(|ch| ch == '\'' || ch.is_control()) {
                return Err(format!("the hook key {key:?} cannot be a TOML literal key"));
            }
            Ok(format!("'{key}'"))
        }
    }
}

fn trust_state_as(flavor: Flavor, listed: &[(&str, Option<&str>)]) -> Result<String, String> {
    let mut entries = Vec::new();
    for expected in hook_keys(flavor) {
        let (key, hash) = listed
            .iter()
            .find(|(key, _)| key_matches(flavor, &expected, key))
            .ok_or_else(|| format!("hooks/list did not list {expected}"))?;
        let hash = hash
            .filter(|hash| {
                hash.starts_with("sha256:")
                    && hash
                        .chars()
                        .all(|ch| ch.is_ascii_graphic() && ch != '"' && ch != '\\')
            })
            .ok_or_else(|| format!("hooks/list gave no hash for {expected}"))?;
        entries.push(format!(
            "{}={{trusted_hash=\"{hash}\"}}",
            trust_key(flavor, key)?
        ));
    }
    Ok(format!("hooks.state={{{}}}", entries.join(",")))
}

/// The `hooks.state={…}` value that trusts the bridge's hooks, from what
/// `codex app-server`'s `hooks/list` reported: each listed hook's `key` and
/// `currentHash`. Every expected hook must be listed with a `sha256:` hash.
/// The keys are the ones Codex listed, so they are right whatever drive
/// Windows puts in them.
///
/// # Errors
///
/// Why there is no trust value: a hook not listed, a missing or odd hash, or a
/// key that cannot be written as TOML.
pub fn hook_trust_state(listed: &[(&str, Option<&str>)]) -> Result<String, String> {
    trust_state_as(Flavor::HOST, listed)
}

/// What Codex draws in an empty composer.
const COMPOSER_PLACEHOLDER: &str = "Ask Codex to do anything";

/// What the live screen says about Codex's composer. Measured on 0.166.0 in its
/// default and `--no-alt-screen` layouts. The rule is anchored at the bottom: the
/// last `›` line is the composer. The footer's "? for shortcuts" hint is not used,
/// because the bypass layout Connecto launches has none, and it goes while text is
/// typed.
///
/// An older `›` echo higher up does not count. A draft, a slash menu, and a list
/// choice such as an approval's `1. Yes` are `Occupied`. A line below the composer
/// that is not footer, or a composer with no footer, is `Booting`: not ready, and
/// not a prompt either. The status row directly above an empty composer says
/// `Busy` when Codex is working.
pub fn codex_composer_state(screen: &str) -> ComposerState {
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let Some(at) = lines.iter().rposition(|line| line.starts_with('›')) else {
        return ComposerState::Booting;
    };
    let typed = lines[at].trim_start_matches('›').trim();
    if typed.is_empty() || typed == COMPOSER_PLACEHOLDER {
        // An empty composer.
    } else if COMPOSER_PLACEHOLDER.starts_with(typed) {
        // The placeholder is still being drawn.
        return ComposerState::Booting;
    } else {
        return ComposerState::Occupied;
    }
    let below = &lines[at + 1..];
    if below.is_empty() || !below.iter().all(|line| is_footer_line(line)) {
        return ComposerState::Booting;
    }
    let status_above = at.checked_sub(1).map(|row| lines[row]).unwrap_or("");
    // The status row has the shape `• Working (4s • esc to interrupt)`; a line
    // of output that merely mentions the key does not.
    if status_above.contains("esc to interrupt") && status_above.contains('(') {
        return ComposerState::Busy;
    }
    ComposerState::Empty
}

/// A line of Codex's footer: the hint row, or the status line that names the
/// model and the folder (`model · ~/folder`, with an optional thread name).
fn is_footer_line(line: &str) -> bool {
    line.contains("? for shortcuts") || line.contains(" · ")
}

/// The Codex adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct Codex;

impl Codex {
    /// [`ToolAdapter::launch`] for `flavor`'s system.
    fn launch_as(flavor: Flavor, ctx: &LaunchCtx) -> LaunchSpec {
        // On the command line, so an agent editing its own config.toml cannot
        // remove it.
        let notify = format!(
            "notify=[{},\"codex\"]",
            toml_string(&ctx.hook_binary.to_string_lossy())
        );
        let mut args: Vec<OsString> = Vec::new();
        // `codex resume <id>` takes the same options as the plain command.
        if let Some(id) = &ctx.resume {
            args.extend(["resume".into(), id.clone().into()]);
        }
        args.push("--no-daemon".into());
        // The main screen, not the alternate one: Connecto keeps the scrollback,
        // and a wheel turned into arrow keys would walk the prompt history. The
        // flag overrides `tui.alternate_screen` for this run; nothing is written
        // to the user's config. Every TUI start comes through here (new,
        // resumed, automation-started); login and status are other commands.
        args.push("--no-alt-screen".into());
        // How the tool asks before it acts is the user's choice (setup).
        match ctx.agent_mode {
            AgentMode::Bypass => args.push("--dangerously-bypass-approvals-and-sandbox".into()),
            AgentMode::Auto => args.push("--approve-for-me".into()),
            AgentMode::Prompt => {}
        }
        args.extend::<[OsString; 6]>([
            "-C".into(),
            ctx.project.clone().into(),
            "-c".into(),
            "check_for_update_on_startup=false".into(),
            "-c".into(),
            notify.into(),
        ]);
        if let Some(model) = &ctx.model {
            args.push("-c".into());
            args.push(format!("model={}", toml_string(model)).into());
        }
        #[cfg(feature = "test-endpoints")]
        if let Some(url) = &ctx.fake_model {
            // The default model offers the fake endpoint no tools at all.
            let provider = format!(
                "model_providers.fake={{name=\"fake\",base_url={},wire_api=\"responses\",env_key=\"FAKE_KEY\"}}",
                toml_string(&format!("{url}/v1"))
            );
            for value in [
                provider.as_str(),
                "model_provider=\"fake\"",
                "model=\"gpt-fake\"",
            ] {
                args.push("-c".into());
                args.push(value.into());
            }
        }
        // Hooks are never registered without a trust state to go with them.
        // The trust state was read for these same values, so a helper path
        // they cannot be built for has none either.
        if ctx.bridge.hooks
            && let Some(trust) = &ctx.bridge.trust
            && let Ok(values) = hook_config_as(flavor, &ctx.hook_binary)
        {
            for value in values {
                args.push("-c".into());
                args.push(value.into());
            }
            args.push("-c".into());
            args.push(trust.into());
        }
        if let Some(text) = &ctx.bridge.text {
            args.push("-c".into());
            args.push(format!("developer_instructions={}", toml_string(text)).into());
        }
        if let (Some(_), Some(note)) = (&ctx.resume, &ctx.resume_note) {
            args.push("--".into());
            args.push(note.into());
        }
        if ctx.resume.is_none()
            && let Some(prompt) = &ctx.initial_prompt
        {
            args.push("--".into());
            args.push(prompt.into());
        }
        let mut env = tool_env(
            flavor,
            &ctx.home,
            &ctx.session_tmp,
            &ctx.profile,
            ctx.system_config,
            &ctx.inherited,
        );
        env.extend(hook_env(ctx));
        with_bridge_path(flavor, &mut env, ctx);
        #[cfg(feature = "test-endpoints")]
        if ctx.fake_model.is_some() {
            env.push(("FAKE_KEY".into(), "fake-local-model".into()));
        }
        LaunchSpec {
            program: ctx.entry.clone(),
            args,
            cwd: ctx.project.clone(),
            env,
            files: Vec::new(),
        }
    }
}

impl ToolAdapter for Codex {
    fn launch(&self, ctx: &LaunchCtx) -> LaunchSpec {
        Self::launch_as(Flavor::HOST, ctx)
    }

    fn login(&self, ctx: &LoginCtx) -> LaunchSpec {
        LaunchSpec {
            program: ctx.entry.clone(),
            args: vec!["login".into()],
            cwd: ctx.run.clone(),
            env: tool_env(
                Flavor::HOST,
                &ctx.home,
                &ctx.session_tmp,
                &ctx.profile,
                ctx.system_config,
                &ctx.inherited,
            ),
            files: Vec::new(),
        }
    }

    fn status(&self, ctx: &LoginCtx) -> LaunchSpec {
        LaunchSpec {
            args: vec!["login".into(), "status".into()],
            cwd: ctx.session_tmp.clone(),
            ..self.login(ctx)
        }
    }

    fn parse_status(&self, output: &str) -> AuthState {
        let line = output.lines().map(str::trim).find(|l| !l.is_empty());
        match line {
            Some("Not logged in") => AuthState::LoggedOut,
            Some(line) => {
                line.strip_prefix("Logged in using ")
                    .map_or(AuthState::Unknown, |method| AuthState::LoggedIn {
                        method: match method {
                            "ChatGPT" => "chatgpt",
                            _ => "other",
                        }
                        .to_owned(),
                    })
            }
            None => AuthState::Unknown,
        }
    }

    fn parse_hook(&self, payload: &[u8], ctx: &HookCtx) -> Result<Vec<ToolEvent>, AdapterError> {
        let value: Value = serde_json::from_slice(payload).map_err(|_| AdapterError::NotJson)?;
        if let Some(name) = value.get("hook_event_name").and_then(Value::as_str) {
            if value.get("connecto_incomplete").and_then(Value::as_bool) == Some(true) {
                return Ok(Vec::new());
            }
            let text = |field: &str| {
                value
                    .get(field)
                    .and_then(Value::as_str)
                    .map(|text| EventText::new(text.to_owned()))
            };
            return Ok(match name {
                "SessionStart" => vec![ToolEvent::Started {
                    tool_session: text("session_id")
                        .ok_or(AdapterError::MissingField("session_id"))?,
                    turn_log: text("transcript_path"),
                }],
                "UserPromptSubmit" => vec![ToolEvent::PromptSubmitted {
                    prompt: text("prompt").ok_or(AdapterError::MissingField("prompt"))?,
                    turn: text("turn_id"),
                }],
                _ => Vec::new(),
            });
        }
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(AdapterError::MissingField("type"))?;
        if kind == "connecto-incomplete" {
            return Ok(vec![ToolEvent::TurnEnded {
                input: Vec::new(),
                input_total: 0,
                reply: Reply::None,
                lifecycle_known: false,
                background: Vec::new(),
                thread: None,
                turn: None,
            }]);
        }
        if kind != "agent-turn-complete" {
            return Ok(Vec::new());
        }
        let thread = value
            .get("thread-id")
            .and_then(Value::as_str)
            .ok_or(AdapterError::MissingField("thread-id"))?;
        // Without hooks nothing pins the main thread, so a hidden title thread
        // is told apart by having no rollout file. With them, the session
        // layer compares the thread with the pinned one.
        if !safe_thread_id(thread) || (!ctx.hooks && !has_rollout(&ctx.profile, thread)) {
            return Ok(Vec::new());
        }
        let input: Vec<EventText> = value
            .get("input-messages")
            .and_then(Value::as_array)
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|text| EventText::new(text.to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let input_total = value
            .get("connecto_input_total")
            .and_then(Value::as_u64)
            .and_then(|total| usize::try_from(total).ok())
            .unwrap_or(input.len())
            .max(input.len());
        let dropped = value
            .get("connecto_dropped")
            .and_then(Value::as_array)
            .is_some_and(|names| {
                names
                    .iter()
                    .any(|name| name.as_str() == Some("last-assistant-message"))
            });
        let reply = match value.get("last-assistant-message").and_then(Value::as_str) {
            Some(text) => {
                let text = EventText::reply(text.to_owned());
                if text.truncated() {
                    Reply::Dropped
                } else {
                    Reply::Text(text)
                }
            }
            None if dropped => Reply::Dropped,
            None => Reply::None,
        };
        let id = |field: &str, safe: fn(&str) -> bool| {
            value
                .get(field)
                .and_then(Value::as_str)
                .filter(|id| safe(id))
                .map(|id| EventText::new(id.to_owned()))
        };
        Ok(vec![ToolEvent::TurnEnded {
            input,
            input_total,
            reply,
            lifecycle_known: true,
            background: Vec::new(),
            thread: id("thread-id", safe_thread_id),
            turn: id("turn-id", safe_turn_id),
        }])
    }

    fn readiness(&self, screen: &str) -> Readiness {
        if codex_composer_state(screen) == ComposerState::Empty {
            Readiness::Ready
        } else {
            Readiness::NotReady
        }
    }

    /// Measured on 0.166.0: all whitespace at the end is dropped, including Unicode spaces.
    /// Nothing inside the text changes.
    fn ack_form(&self, text: &str) -> String {
        text.trim_end().to_owned()
    }

    fn send_anyway_text(&self) -> &'static str {
        "Codex may still be starting, or has a draft in its composer. A draft joins this message and Enter sends both."
    }

    fn capabilities(&self, _version: &str) -> Capabilities {
        Capabilities {
            immediate_ack: false,
        }
    }

    fn turn_log(&self, reported: &Path, ctx: &HookCtx) -> Option<PathBuf> {
        under_profile(reported, &ctx.profile)
    }

    fn read_log(&self, line: &str, whole: bool) -> LogMark {
        if !whole {
            // The rollouts measured put `type` and `turn_id` before the long
            // `last_agent_message`, so the start is enough to see an end.
            let turn = head_turn_id(line);
            return if line.contains(r#""type":"task_complete""#) {
                LogMark::TurnComplete { turn, reply: None }
            } else if line.contains(r#""type":"turn_aborted""#) {
                LogMark::Interrupt { turn }
            } else if line.contains(r#""type":"task_started""#) {
                LogMark::TurnStarted { turn }
            } else {
                LogMark::Other
            };
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return LogMark::Other;
        };
        if value.get("type").and_then(Value::as_str) != Some("event_msg") {
            return LogMark::Other;
        }
        let payload = value.get("payload");
        let turn = payload
            .and_then(|p| p.get("turn_id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        match payload.and_then(|p| p.get("type")).and_then(Value::as_str) {
            Some("task_complete") => LogMark::TurnComplete {
                turn,
                reply: payload
                    .and_then(|p| p.get("last_agent_message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            Some("turn_aborted") => LogMark::Interrupt { turn },
            Some("task_started") => LogMark::TurnStarted { turn },
            _ => LogMark::Other,
        }
    }

    fn continuation(&self, _prompt: &str) -> Option<Continuation> {
        None
    }
}

/// The `turn_id` in the start of a line, if it sits there.
fn head_turn_id(line: &str) -> Option<String> {
    let (_, after) = line.split_once(r#""turn_id":""#)?;
    let id: String = after.chars().take_while(|ch| *ch != '"').collect();
    safe_turn_id(&id).then_some(id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::spec::tests::{launch_ctx, login_ctx};

    #[test]
    fn the_ack_form_removes_only_what_codex_was_measured_rewriting() {
        assert_eq!(Codex.ack_form("end \n\u{3000}"), "end");
        assert_eq!(Codex.ack_form("a\tb  \n\n\nend"), "a\tb  \n\n\nend");
        assert_ne!(Codex.ack_form("one body"), Codex.ack_form("other body"));
    }

    #[test]
    fn system_config_leaves_codex_home_unset_but_keeps_the_hooks() {
        let has = |spec: &LaunchSpec, name: &str| spec.env.iter().any(|(key, _)| key == name);
        let mut ctx = launch_ctx();
        ctx.system_config = true;
        let spec = Codex.launch(&ctx);
        assert!(!has(&spec, "CODEX_HOME"));
        assert!(has(&spec, "CONNECTO_HOOK_TOKEN"));
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a.starts_with("hooks.state=")));
        assert!(
            args.iter()
                .any(|a| a.starts_with("hooks.UserPromptSubmit="))
        );
        let mut login = login_ctx();
        login.system_config = true;
        assert!(!has(&Codex.login(&login), "CODEX_HOME"));
    }

    const THREAD: &str = "01a1a641-e9d8-79a0-8164-6bd15f72cb16";
    const TITLE_THREAD: &str = "01a1a642-0dff-7f31-bf9a-f329fb42fdc8";

    struct Profile(PathBuf);

    impl Profile {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("mg-codex-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let day = root.join("sessions/2026/11/04");
            fs::create_dir_all(&day).unwrap();
            fs::write(
                day.join(format!("rollout-2026-11-04T12-00-00-{THREAD}.jsonl")),
                "{}",
            )
            .unwrap();
            Self(root)
        }

        fn ctx(&self) -> HookCtx {
            HookCtx {
                profile: self.0.clone(),
                hooks: false,
            }
        }
    }

    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn notify(thread: &str, inputs: &str) -> Vec<u8> {
        format!(
            r#"{{"type":"agent-turn-complete","thread-id":"{thread}","turn-id":"t",
            "client":"codex-tui","input-messages":{inputs},
            "last-assistant-message":"reply"}}"#
        )
        .into_bytes()
    }

    #[test]
    fn a_session_gets_every_flag_the_design_names() {
        let mut ctx = launch_ctx();
        ctx.bridge.hooks = false;
        ctx.bridge.text = None;
        let spec = Codex.launch(&ctx);
        let args: Vec<_> = spec.args.iter().map(|a| a.to_string_lossy()).collect();
        assert_eq!(
            args,
            [
                "--no-daemon",
                "--no-alt-screen",
                "--dangerously-bypass-approvals-and-sandbox",
                "-C",
                "/project",
                "-c",
                "check_for_update_on_startup=false",
                "-c",
                r#"notify=["/data/cli/bin/connecto-hook","codex"]"#,
            ]
        );
        assert!(spec.env.iter().any(|(name, _)| name == "CODEX_HOME"));
        assert!(
            spec.env
                .iter()
                .any(|(name, _)| name == "CONNECTO_HOOK_TOKEN")
        );
    }

    /// Every Codex Connecto starts runs with full power (A11, the long form of
    /// `--yolo`), however it is launched, and no `-c` value sets an approval
    /// policy, a sandbox mode or a profile that could lower it.
    fn args(spec: &LaunchSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The setup choice maps to exactly one set of flags.
    #[test]
    fn a_model_is_passed_only_when_the_terminal_was_added_with_one() {
        let mut ctx = launch_ctx();
        assert!(
            !args(&Codex.launch(&ctx))
                .iter()
                .any(|w| w.starts_with("model="))
        );
        ctx.model = Some("gpt-6.1-sol".to_owned());
        let words = args(&Codex.launch(&ctx));
        assert!(
            words
                .windows(2)
                .any(|w| w == ["-c", "model=\"gpt-6.1-sol\""])
        );
    }

    #[test]
    fn the_launch_mode_picks_the_approval_flags() {
        let words = |mode: AgentMode| {
            let mut ctx = launch_ctx();
            ctx.agent_mode = mode;
            args(&Codex.launch(&ctx))
        };
        let bypass = words(AgentMode::Bypass);
        assert!(
            bypass
                .iter()
                .any(|w| w == "--dangerously-bypass-approvals-and-sandbox")
        );
        assert!(!bypass.iter().any(|w| w == "--approve-for-me"));
        let auto = words(AgentMode::Auto);
        assert!(auto.iter().any(|w| w == "--approve-for-me"));
        assert!(!auto.iter().any(|w| w.contains("bypass-approvals")));
        let prompt = words(AgentMode::Prompt);
        assert!(
            !prompt
                .iter()
                .any(|w| w == "--approve-for-me" || w.contains("bypass-approvals"))
        );
        // The rest of the command line is the same in every mode, and it never
        // sets an approval policy or a sandbox of its own.
        for w in [&bypass, &auto, &prompt] {
            assert!(w.iter().any(|a| a == "--no-daemon"));
            assert!(w.iter().any(|a| a == "--no-alt-screen"));
            assert!(w.iter().any(|a| a == "-C"));
            assert!(
                !w.iter()
                    .any(|a| a == "--ask-for-approval" || a == "--sandbox")
            );
            assert!(
                !w.iter()
                    .any(|a| a.starts_with("approval_policy") || a.starts_with("sandbox_mode"))
            );
        }
        // A resumed conversation takes the same flags after `resume <id>`.
        let mut ctx = launch_ctx();
        ctx.agent_mode = AgentMode::Auto;
        ctx.resume = Some("abc".to_owned());
        let resumed = args(&Codex.launch(&ctx));
        assert_eq!(&resumed[..2], ["resume", "abc"]);
        assert!(resumed.iter().any(|w| w == "--approve-for-me"));
    }

    #[test]
    fn every_launch_runs_with_approvals_and_sandbox_bypassed_and_nothing_lowers_it() {
        for (hooks, text, trust) in [
            (true, true, true),
            (false, false, false),
            (true, false, false),
        ] {
            let mut ctx = launch_ctx();
            ctx.bridge.hooks = hooks;
            ctx.bridge.text = text.then(|| "T".to_owned());
            ctx.bridge.trust = trust.then(|| "hooks.state={}".to_owned());
            let words: Vec<String> = Codex
                .launch(&ctx)
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                words
                    .iter()
                    .filter(|w| *w == "--dangerously-bypass-approvals-and-sandbox")
                    .count(),
                1,
                "{words:?}"
            );
            for word in &words {
                for lowering in [
                    "approval_policy",
                    "sandbox_mode",
                    "profile=",
                    "--ask-for-approval",
                    "--sandbox",
                    "--full-auto",
                    "--dangerously-bypass-hook-trust",
                ] {
                    assert!(!word.contains(lowering), "{lowering} in {word}");
                }
            }
        }
    }

    fn launch_words(ctx: &LaunchCtx) -> Vec<String> {
        Codex
            .launch(ctx)
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_resumed_launch_is_the_resume_subcommand_with_the_same_options() {
        let mut ctx = launch_ctx();
        assert!(!launch_words(&ctx).iter().any(|arg| arg == "resume"));
        ctx.resume = Some("0199-abc".to_owned());
        let args = launch_words(&ctx);
        assert_eq!(args[..2], ["resume", "0199-abc"]);
        assert!(args.iter().any(|arg| arg == "--no-daemon"));
        assert!(args.iter().any(|arg| arg == "--no-alt-screen"));
        assert!(
            args.iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox")
        );
    }

    /// T1: the setting existed for the sandbox, and went with it (R11).
    #[test]
    fn a_resume_note_is_the_last_argument_of_a_resumed_conversation_only() {
        let strings = |spec: &LaunchSpec| -> Vec<String> {
            spec.args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let mut ctx = crate::spec::tests::launch_ctx();
        ctx.resume_note = Some("re-read the guide".to_owned());
        assert!(
            !strings(&Codex.launch(&ctx))
                .iter()
                .any(|a| a.contains("re-read"))
        );
        ctx.resume = Some("abc".to_owned());
        let args = strings(&Codex.launch(&ctx));
        assert_eq!(&args[..2], ["resume", "abc"]);
        assert_eq!(&args[args.len() - 2..], ["--", "re-read the guide"]);
    }

    #[test]
    fn the_first_prompt_is_the_last_argument_of_a_new_conversation_only() {
        let strings = |spec: &LaunchSpec| -> Vec<String> {
            spec.args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let mut ctx = crate::spec::tests::launch_ctx();
        ctx.initial_prompt = Some("you are authoring".to_owned());
        let args = strings(&Codex.launch(&ctx));
        assert_eq!(&args[args.len() - 2..], ["--", "you are authoring"]);
        ctx.resume = Some("abc".to_owned());
        let resumed = strings(&Codex.launch(&ctx));
        assert_eq!(&resumed[..2], ["resume", "abc"]);
        assert!(!resumed.iter().any(|a| a.contains("authoring")));
    }

    #[test]
    fn the_launch_no_longer_pins_unified_exec_tty() {
        let spec = Codex.launch(&launch_ctx());
        assert!(
            !spec
                .args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("unified_exec_tty"))
        );
    }

    #[test]
    fn toml_strings_are_escaped() {
        assert_eq!(toml_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(toml_string("a\nb"), r#""a\u000ab""#);
    }

    #[test]
    fn login_has_no_bypass_flag_and_no_hook_channel_and_runs_in_the_run_folder() {
        let spec = Codex.login(&login_ctx());
        assert_eq!(spec.args, [OsString::from("login")]);
        assert_eq!(spec.cwd.to_str(), Some("/run"));
        assert!(
            !spec
                .env
                .iter()
                .any(|(name, _)| { name.to_string_lossy().starts_with("CONNECTO_") })
        );
    }

    #[test]
    fn status_runs_in_the_session_temp_folder() {
        let spec = Codex.status(&login_ctx());
        assert_eq!(spec.cwd.to_str(), Some("/run/tmp"));
    }

    #[test]
    fn status_output_keeps_only_state_and_method() {
        assert_eq!(Codex.parse_status("Not logged in\n"), AuthState::LoggedOut);
        assert_eq!(
            Codex.parse_status("Logged in using ChatGPT\n"),
            AuthState::LoggedIn {
                method: "chatgpt".to_owned()
            }
        );
        assert_eq!(
            Codex.parse_status("Logged in using an API key - sk-abc\n"),
            AuthState::LoggedIn {
                method: "other".to_owned()
            }
        );
        assert_eq!(Codex.parse_status("Error: boom"), AuthState::Unknown);
        assert_eq!(Codex.parse_status(""), AuthState::Unknown);
    }

    #[test]
    fn a_turn_on_a_real_thread_is_reported_with_its_inputs_ids_and_reply() {
        let profile = Profile::new("real");
        let events = Codex
            .parse_hook(&notify(THREAD, r#"["first","second"]"#), &profile.ctx())
            .unwrap();
        assert_eq!(
            events,
            [ToolEvent::TurnEnded {
                input: vec![
                    EventText::new("first".to_owned()),
                    EventText::new("second".to_owned())
                ],
                input_total: 2,
                reply: Reply::Text(EventText::new("reply".to_owned())),
                lifecycle_known: true,
                background: Vec::new(),
                thread: Some(EventText::new(THREAD.to_owned())),
                turn: Some(EventText::new("t".to_owned())),
            }]
        );
    }

    /// With hooks the pinned thread decides, so no rollout walk is made.
    #[test]
    fn with_hooks_a_thread_needs_no_rollout_file_and_is_left_to_the_pin() {
        let profile = Profile::new("pinned");
        let ctx = HookCtx {
            hooks: true,
            ..profile.ctx()
        };
        let events = Codex
            .parse_hook(&notify(TITLE_THREAD, r#"["x"]"#), &ctx)
            .unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn a_shrunk_notify_keeps_its_total_and_marks_a_dropped_reply() {
        let profile = Profile::new("shrunk");
        let json = format!(
            r#"{{"type":"agent-turn-complete","thread-id":"{THREAD}","turn-id":"t",
            "input-messages":["a","b"],"connecto_input_total":40,
            "connecto_dropped":["input-messages","last-assistant-message"]}}"#
        );
        let events = Codex.parse_hook(json.as_bytes(), &profile.ctx()).unwrap();
        let [
            ToolEvent::TurnEnded {
                input_total,
                reply,
                input,
                ..
            },
        ] = &events[..]
        else {
            panic!("{events:?}");
        };
        assert_eq!((*input_total, input.len()), (40, 2));
        assert_eq!(*reply, Reply::Dropped);
    }

    #[test]
    fn an_incomplete_notify_has_no_known_lifecycle() {
        let profile = Profile::new("incomplete");
        let events = Codex
            .parse_hook(br#"{"type":"connecto-incomplete"}"#, &profile.ctx())
            .unwrap();
        assert!(matches!(
            &events[..],
            [ToolEvent::TurnEnded {
                lifecycle_known: false,
                ..
            }]
        ));
    }

    #[test]
    fn codex_hooks_carry_the_session_and_the_turn() {
        let profile = Profile::new("hooks");
        let parse = |json: &str| Codex.parse_hook(json.as_bytes(), &profile.ctx()).unwrap();
        assert_eq!(
            parse(
                r#"{"hook_event_name":"SessionStart","session_id":"th","transcript_path":"/p/r.jsonl"}"#
            ),
            [ToolEvent::Started {
                tool_session: EventText::new("th".to_owned()),
                turn_log: Some(EventText::new("/p/r.jsonl".to_owned())),
            }]
        );
        assert_eq!(
            parse(r#"{"hook_event_name":"UserPromptSubmit","prompt":"hi","turn_id":"tu"}"#),
            [ToolEvent::PromptSubmitted {
                prompt: EventText::new("hi".to_owned()),
                turn: Some(EventText::new("tu".to_owned())),
            }]
        );
    }

    #[test]
    fn a_task_started_mark_names_its_turn_whole_and_from_a_cut_line() {
        let started = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"tu9"}}"#;
        assert_eq!(
            Codex.read_log(started, true),
            LogMark::TurnStarted {
                turn: Some("tu9".into())
            }
        );
        assert_eq!(
            Codex.read_log(&started[..started.len() - 3], false),
            LogMark::TurnStarted {
                turn: Some("tu9".into())
            }
        );
    }

    #[test]
    fn the_rollout_marks_are_read_whole_and_from_a_cut_line() {
        let complete = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"tu1","last_agent_message":"all done"}}"#;
        assert_eq!(
            Codex.read_log(complete, true),
            LogMark::TurnComplete {
                turn: Some("tu1".into()),
                reply: Some("all done".into())
            }
        );
        let cut = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"tu2","last_agent_mess"#;
        assert_eq!(
            Codex.read_log(cut, false),
            LogMark::TurnComplete {
                turn: Some("tu2".into()),
                reply: None
            }
        );
        assert_eq!(
            Codex.read_log(
                r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"tu3"}}"#,
                true
            ),
            LogMark::Interrupt {
                turn: Some("tu3".into())
            }
        );
        assert_eq!(
            Codex.read_log(r#"{"type":"response_item"}"#, true),
            LogMark::Other
        );
    }

    #[test]
    fn the_launch_registers_trusted_hooks_and_the_launch_text_and_never_hooks_without_trust() {
        let spec = Codex.launch(&launch_ctx());
        let args: Vec<_> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a.starts_with("hooks.SessionStart=")));
        assert!(
            args.iter()
                .any(|a| a.contains("codex-hook --event UserPromptSubmit --context"))
        );
        assert!(args.iter().any(|a| a.starts_with("hooks.state=")));
        assert!(
            args.iter()
                .any(|a| a == r#"developer_instructions="LAUNCH TEXT""#)
        );
        let mut ctx = launch_ctx();
        ctx.bridge.trust = None;
        let args: Vec<_> = Codex
            .launch(&ctx)
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args.iter().any(|a| a.starts_with("hooks.")),
            "hooks without trust"
        );
        ctx.bridge.hooks = false;
        ctx.bridge.text = None;
        let args: Vec<_> = Codex
            .launch(&ctx)
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("hooks.") || a.starts_with("developer_instructions"))
        );
    }

    #[test]
    fn a_hidden_title_thread_is_ignored() {
        let profile = Profile::new("title");
        let events = Codex
            .parse_hook(
                &notify(TITLE_THREAD, r#"["Generate a title"]"#),
                &profile.ctx(),
            )
            .unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn a_thread_id_cannot_walk_the_file_system() {
        let profile = Profile::new("walk");
        for id in ["../../x", "a/b", "", "*"] {
            let events = Codex.parse_hook(&notify(id, "[]"), &profile.ctx()).unwrap();
            assert!(events.is_empty(), "{id}");
        }
    }

    #[test]
    fn other_notification_types_are_ignored() {
        let profile = Profile::new("other");
        let events = Codex
            .parse_hook(br#"{"type":"something-new"}"#, &profile.ctx())
            .unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn readiness_needs_an_empty_composer_at_the_bottom_with_its_footer() {
        assert_eq!(
            Codex.readiness("› Ask Codex to do anything\n\n  model · ~/p\n  ? for shortcuts"),
            Readiness::Ready
        );
        // A footer without the hint is enough: the bypass layout has none.
        assert_eq!(
            Codex.readiness("› Ask Codex to do anything\n  GPT-6 medium · ~/p · backend\n"),
            Readiness::Ready
        );
        assert_eq!(Codex.readiness("  › Ask Codex\n"), Readiness::NotReady);
        assert_eq!(Codex.readiness("Booting\n"), Readiness::NotReady);
        // A composer with no footer under it is still drawing.
        assert_eq!(
            Codex.readiness("› Ask Codex to do anything\n"),
            Readiness::NotReady
        );
    }

    /// Screens captured from the real Codex 0.166.0 (in `tests/fixtures/codex`),
    /// with the state each one must read as.
    const SCREENS: &[(&str, ComposerState)] = &[
        ("reported_error_screen.txt", ComposerState::Empty),
        ("fresh_default.txt", ComposerState::Empty),
        ("fresh_inline_yolo.txt", ComposerState::Empty),
        ("named_inline_yolo.txt", ComposerState::Empty),
        ("named_alt_after_rename.txt", ComposerState::Empty),
        ("named_narrow_80.txt", ComposerState::Empty),
        ("after_interrupt_inline.txt", ComposerState::Empty),
        ("worked_inline.txt", ComposerState::Empty),
        ("inline_after_typing_backspace.txt", ComposerState::Empty),
        ("working_inline.txt", ComposerState::Busy),
        ("slash_popup_inline.txt", ComposerState::Occupied),
        ("draft_rename_alt.txt", ComposerState::Occupied),
        ("approval_prompt_inline.txt", ComposerState::Occupied),
        // A footer with no "·" is not recognised: not ready, which is the
        // fail-safe side.
        ("footer_without_dot.txt", ComposerState::Booting),
    ];

    #[test]
    fn captured_screens_read_as_their_composer_state() {
        for (file, want) in SCREENS {
            let path = format!("{}/tests/fixtures/codex/{file}", env!("CARGO_MANIFEST_DIR"));
            let screen = std::fs::read_to_string(&path).unwrap();
            assert_eq!(codex_composer_state(&screen), *want, "{file}");
            let ready = *want == ComposerState::Empty;
            let readiness = if ready {
                Readiness::Ready
            } else {
                Readiness::NotReady
            };
            assert_eq!(Codex.readiness(&screen), readiness, "{file}");
        }
    }

    #[test]
    fn an_esc_hint_in_the_transcript_does_not_make_an_idle_composer_busy() {
        // The phrase is in output above the composer, not on the status row
        // directly above it.
        let idle = "› explain it\n• Press esc to interrupt a turn at any time\n\n› Ask Codex to do anything\n  GPT-6 · ~/p\n";
        assert_eq!(codex_composer_state(idle), ComposerState::Empty);
        // On the status row directly above the composer it is busy.
        let busy = "› explain it\n• Working (4s • esc to interrupt)\n› Ask Codex to do anything\n  GPT-6 · ~/p\n";
        assert_eq!(codex_composer_state(busy), ComposerState::Busy);
    }

    #[test]
    fn a_stale_prompt_echo_above_the_composer_does_not_fool_the_check() {
        // An old `›` echo higher up, then output, then the live composer.
        let screen =
            "› earlier question\n• answer text\n\n› Ask Codex to do anything\n  GPT-6 · ~/p\n";
        assert_eq!(Codex.readiness(screen), Readiness::Ready);
        // The same echo as the last `›` line, with no composer under it.
        let echo_last = "› Ask Codex to do anything\n• answer text\n  GPT-6 · ~/p\n";
        assert_eq!(Codex.readiness(echo_last), Readiness::NotReady);
    }

    #[test]
    fn capabilities_do_not_depend_on_the_version() {
        for version in ["0.166.0", "0.200.0", "", "not a version"] {
            assert!(!Codex.capabilities(version).immediate_ack, "{version}");
        }
    }

    const WINDOWS_HELPER: &str =
        r"C:\Users\Jane O'Neil\AppData\Roaming\connecto\cli\bin\connecto-hook.exe";

    /// The macOS shape, on every host: `sh` quoting, as it always was.
    #[test]
    fn unix_hook_commands_are_sh_words() {
        assert_eq!(
            hook_config_as(Flavor::Unix, Path::new("/data/cli/bin/connecto-hook")).unwrap(),
            [
                r#"hooks.SessionStart=[{hooks=[{type="command",command="'/data/cli/bin/connecto-hook' codex-hook --event SessionStart"}]}]"#,
                r#"hooks.UserPromptSubmit=[{hooks=[{type="command",command="'/data/cli/bin/connecto-hook' codex-hook --event UserPromptSubmit --context"}]}]"#,
            ]
        );
    }

    /// The Windows shape, on every host: Codex runs the command with
    /// PowerShell, so the helper is called with `&` and quoted with single
    /// quotes only, a `'` written twice (checked on Codex 0.167.2).
    #[test]
    fn windows_hook_commands_are_powershell_calls() {
        assert_eq!(
            hook_command(Flavor::Windows, WINDOWS_HELPER, "SessionStart").unwrap(),
            r"& 'C:\Users\Jane O''Neil\AppData\Roaming\connecto\cli\bin\connecto-hook.exe' codex-hook --event SessionStart"
        );
        assert_eq!(
            hook_config_as(Flavor::Windows, Path::new(WINDOWS_HELPER)).unwrap(),
            [
                r#"hooks.SessionStart=[{hooks=[{type="command",command="& 'C:\\Users\\Jane O''Neil\\AppData\\Roaming\\connecto\\cli\\bin\\connecto-hook.exe' codex-hook --event SessionStart"}]}]"#,
                r#"hooks.UserPromptSubmit=[{hooks=[{type="command",command="& 'C:\\Users\\Jane O''Neil\\AppData\\Roaming\\connecto\\cli\\bin\\connecto-hook.exe' codex-hook --event UserPromptSubmit --context"}]}]"#,
            ]
        );
        // Nothing PowerShell would read as code survives unquoted.
        assert_eq!(
            hook_command(Flavor::Windows, r"C:\a $x `y & z; w\h.exe", "SessionStart").unwrap(),
            r"& 'C:\a $x `y & z; w\h.exe' codex-hook --event SessionStart"
        );
    }

    /// A helper path PowerShell cannot take literally gets no hooks at all,
    /// and the launch goes on without them.
    #[test]
    fn a_helper_powershell_cannot_quote_gets_no_hooks() {
        let bad = "C:\\a\nb\\connecto-hook.exe";
        assert!(hook_config_as(Flavor::Windows, Path::new(bad)).is_err());
        assert!(hook_config_as(Flavor::Unix, Path::new(bad)).is_ok());
        let mut ctx = crate::spec::tests::launch_ctx_windows();
        ctx.hook_binary = bad.into();
        let words = args(&Codex::launch_as(Flavor::Windows, &ctx));
        assert!(!words.iter().any(|w| w.starts_with("hooks.")), "{words:?}");
        assert!(words.iter().any(|w| w.starts_with("notify=")));
    }

    #[test]
    fn hook_keys_follow_the_flavor() {
        assert_eq!(
            hook_keys(Flavor::Unix),
            [
                "/<session-flags>/config.toml:session_start:0:0",
                "/<session-flags>/config.toml:user_prompt_submit:0:0",
            ]
        );
        assert_eq!(
            hook_keys(Flavor::Windows),
            [
                r"C:\<session-flags>\config.toml:session_start:0:0",
                r"C:\<session-flags>\config.toml:user_prompt_submit:0:0",
            ]
        );
        assert_eq!(expected_hook_keys(), hook_keys(Flavor::HOST));
    }

    #[test]
    fn a_listed_key_matches_exactly_or_on_windows_with_any_drive() {
        let [start, prompt] = hook_keys(Flavor::Unix);
        assert!(key_matches(Flavor::Unix, &start, &start));
        assert!(!key_matches(Flavor::Unix, &start, &prompt));
        assert!(!key_matches(Flavor::Unix, &start, &format!("/x{start}")));
        let [start, prompt] = hook_keys(Flavor::Windows);
        assert!(key_matches(Flavor::Windows, &start, &start));
        for drive in ["D", "B", "c"] {
            let listed = format!("{drive}{}", &start[1..]);
            assert!(key_matches(Flavor::Windows, &start, &listed), "{listed}");
        }
        for other in [
            prompt.clone(),
            format!("CC{}", &start[1..]),
            format!("1{}", &start[1..]),
            start.replace(r"\<session-flags>", r"\other"),
            String::new(),
            "C".to_owned(),
        ] {
            assert!(!key_matches(Flavor::Windows, &start, &other), "{other}");
        }
    }

    /// The macOS shape, byte for byte what the session layer built before.
    #[test]
    fn the_unix_trust_value_has_basic_string_keys() {
        let [start, prompt] = hook_keys(Flavor::Unix);
        let listed = [
            ("/other:x:0:0", Some("sha256:000")),
            (start.as_str(), Some("sha256:abc1")),
            (prompt.as_str(), Some("sha256:abc2")),
        ];
        assert_eq!(
            trust_state_as(Flavor::Unix, &listed).unwrap(),
            r#"hooks.state={"/<session-flags>/config.toml:session_start:0:0"={trusted_hash="sha256:abc1"},"/<session-flags>/config.toml:user_prompt_submit:0:0"={trusted_hash="sha256:abc2"}}"#
        );
    }

    /// The Windows shape: TOML literal keys, and the key Codex listed, drive
    /// letter included (checked on Codex 0.167.2).
    #[test]
    fn the_windows_trust_value_has_literal_keys_as_codex_listed_them() {
        let listed = [
            (
                r"D:\<session-flags>\config.toml:user_prompt_submit:0:0",
                Some("sha256:abc2"),
            ),
            (
                r"D:\<session-flags>\config.toml:session_start:0:0",
                Some("sha256:abc1"),
            ),
        ];
        assert_eq!(
            trust_state_as(Flavor::Windows, &listed).unwrap(),
            r#"hooks.state={'D:\<session-flags>\config.toml:session_start:0:0'={trusted_hash="sha256:abc1"},'D:\<session-flags>\config.toml:user_prompt_submit:0:0'={trusted_hash="sha256:abc2"}}"#
        );
        assert_eq!(
            trust_key(Flavor::Windows, "it's").unwrap_err(),
            r#"the hook key "it's" cannot be a TOML literal key"#
        );
        assert!(trust_key(Flavor::Windows, "a\nb").is_err());
    }

    #[test]
    fn a_trust_value_needs_every_hook_with_a_sha256_hash() {
        for flavor in [Flavor::Unix, Flavor::Windows] {
            let [start, prompt] = hook_keys(flavor);
            let only_start = [(start.as_str(), Some("sha256:a"))];
            assert!(
                trust_state_as(flavor, &only_start)
                    .unwrap_err()
                    .contains("did not list")
            );
            for hash in [
                None,
                Some("md5:a"),
                Some("sha256:a\"b"),
                Some("sha256:a\\b"),
                Some("sha256:a b"),
            ] {
                let listed = [(start.as_str(), Some("sha256:a")), (prompt.as_str(), hash)];
                assert!(
                    trust_state_as(flavor, &listed)
                        .unwrap_err()
                        .contains("gave no hash"),
                    "{hash:?}"
                );
            }
        }
    }

    /// The whole Windows launch, on every host.
    #[test]
    fn a_windows_launch_registers_powershell_hooks_and_a_windows_environment() {
        let ctx = crate::spec::tests::launch_ctx_windows();
        let spec = Codex::launch_as(Flavor::Windows, &ctx);
        let words = args(&spec);
        assert!(
            words
                .windows(2)
                .any(|w| w == ["-C".to_owned(), r"C:\Users\Jane O'Neil\proj".to_owned()])
        );
        assert!(words.iter().any(|w| w
            == r#"notify=["C:\\Users\\Jane O'Neil\\AppData\\Roaming\\connecto\\cli\\bin\\connecto-hook.exe","codex"]"#));
        let config = hook_config_as(Flavor::Windows, &ctx.hook_binary).unwrap();
        for value in &config {
            assert!(words.contains(value), "{value}");
        }
        assert!(words.iter().any(|w| w.starts_with("hooks.state=")));
        let get = |name: &str| {
            spec.env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(
            get("PATH").as_deref(),
            Some(
                r"C:\Users\Jane O'Neil\AppData\Roaming\connecto\cli\bin;C:\WINDOWS\system32;C:\WINDOWS"
            )
        );
        assert_eq!(
            get("CODEX_HOME"),
            Some(ctx.profile.to_string_lossy().into_owned())
        );
        assert_eq!(
            get("TEMP"),
            Some(ctx.session_tmp.to_string_lossy().into_owned())
        );
        assert_eq!(get("TMPPREFIX"), None);
        assert_eq!(
            get("CONNECTO_HOOK_SOCK").as_deref(),
            Some(r"\\.\pipe\connecto-hook-0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            get("CONNECTO_BRIDGE_SOCK").as_deref(),
            Some(r"\\.\pipe\connecto-bridge-0123456789abcdef0123456789abcdef")
        );
    }

    /// The host's own shape through the public API.
    #[test]
    fn the_public_api_uses_the_hosts_flavor() {
        let ctx = launch_ctx();
        assert_eq!(Codex.launch(&ctx), Codex::launch_as(Flavor::HOST, &ctx));
        assert_eq!(
            hook_config(Path::new("/h")).unwrap(),
            hook_config_as(Flavor::HOST, Path::new("/h")).unwrap()
        );
    }
}
