//! Claude Code. Checked against 2.1.318.

use std::ffi::OsString;

use std::path::{Path, PathBuf};

use connecto_core::{AgentMode, AuthState, EventText, Readiness, Reply, ToolEvent};
use serde_json::{Value, json};

use crate::platform::Flavor;
use crate::quote::shell_quote;
use crate::spec::{
    AdapterError, Capabilities, ComposerState, Continuation, HookCtx, LaunchCtx, LaunchSpec,
    LogMark, LoginCtx, ToolAdapter, base_env, hook_env, under_profile, windows_var,
    with_bridge_path,
};
use crate::statusline::windows_statuslines;

const HOOKS: [&str; 4] = ["SessionStart", "UserPromptSubmit", "Stop", "Notification"];
/// Where Claude reads settings, skills and agents from: the user's profile, and
/// the project's `.claude/` and `.claude/settings.local.json`. Checked against
/// 2.1.318 without a model call: with `project` a project skill and agent show
/// in the session's init message, and without it they do not.
const SETTING_SOURCES: &str = "user,project,local";
const SETTINGS_FILE: &str = "claude-settings.json";

/// The values the bridge depends on, as the settings file repeats them.
fn bridge_env_pairs(env: &[(OsString, OsString)]) -> Vec<(String, String)> {
    [
        "CONNECTO_HOOK_SOCK",
        "CONNECTO_HOOK_TOKEN",
        "CONNECTO_BRIDGE_SOCK",
        "CONNECTO_TERMINAL",
        "PATH",
    ]
    .iter()
    .filter_map(|name| {
        let value = env.iter().find(|(key, _)| key == name)?.1.to_string_lossy();
        Some(((*name).to_owned(), value.into_owned()))
    })
    .collect()
}

/// The hook for `name`. With the bridge on, it names its event, and
/// `UserPromptSubmit` also asks for the inbox (`--context`). Without it, the
/// older form is kept: it forwards and prints nothing.
///
/// macOS: a shell command, which Claude runs with `sh -c`. Windows: the exec
/// form (`command` is the program, `args` its arguments), which Claude starts
/// directly, with no shell and each argument as written. The shell form would
/// run through Git Bash, or through PowerShell without it, where a quoted path
/// is printed instead of run (checked on Claude Code 2.1.322).
fn hook_entry(flavor: Flavor, hook_binary: &str, name: &str, bridge_hooks: bool) -> Value {
    let context = name == "UserPromptSubmit";
    match flavor {
        Flavor::Unix => {
            let command = if bridge_hooks {
                let context = if context { " --context" } else { "" };
                format!(
                    "{} claude --event {name}{context}",
                    shell_quote(hook_binary)
                )
            } else {
                format!("{} claude", shell_quote(hook_binary))
            };
            json!({ "type": "command", "command": command })
        }
        Flavor::Windows => {
            let mut args = vec!["claude"];
            if bridge_hooks {
                args.extend(["--event", name]);
                if context {
                    args.push("--context");
                }
            }
            json!({ "type": "command", "command": hook_binary, "args": args })
        }
    }
}

/// One hook per event, the bridge's environment, and `extra` (top-level keys
/// only Windows adds: the status lines).
fn settings(
    flavor: Flavor,
    hook_binary: &str,
    bridge_hooks: bool,
    env: &[(String, String)],
    extra: serde_json::Map<String, Value>,
) -> Vec<u8> {
    let hooks: serde_json::Map<String, Value> = HOOKS
        .iter()
        .map(|name| {
            let hook = hook_entry(flavor, hook_binary, name, bridge_hooks);
            ((*name).to_owned(), json!([{ "hooks": [hook] }]))
        })
        .collect();
    // R15, as decided after the first manual run: other sessions still cannot
    // message a Connecto terminal (`crossSessionInbound`), but Claude's own
    // `SendMessage` is no longer denied outward, so a skill can message its
    // subagents. Those sends do not pass the bridge's audit or Pause.
    // `--settings` outranks the project's settings for `env` (checked on 2.1.318),
    // so the values the bridge needs are set here too: a project that sets
    // `env.CONNECTO_BRIDGE_SOCK` cannot point the `connecto` command, or the hook
    // helper, somewhere else.
    let env: serde_json::Map<String, Value> = env
        .iter()
        .map(|(name, value)| (name.clone(), json!(value)))
        .collect();
    let mut value = json!({
        "hooks": hooks,
        "crossSessionInbound": "refuse",
        "env": env,
    });
    if let Some(object) = value.as_object_mut() {
        for (key, setting) in extra {
            object.entry(key).or_insert(setting);
        }
    }
    value.to_string().into_bytes()
}

/// What a project's own Claude settings do to Connecto, found by reading them
/// before launch. Two things cannot be protected, because no source outranks
/// them (checked on 2.1.318): `disableAllHooks: true` silences Connecto's hooks,
/// and `permissions.disableBypassPermissionsMode: "disable"` turns the bypass
/// flag off. They are reported, not hidden.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectSettingsConflicts {
    /// Why Connecto's hooks will not run, if the project disables them.
    pub hooks_disabled: Option<String>,
    /// Why the terminal will not run with full power, if the project says so.
    pub bypass_disabled: Option<String>,
}

/// Read `.claude/settings.json` and `.claude/settings.local.json` in `project`.
#[must_use]
pub fn project_settings_conflicts(project: &Path) -> ProjectSettingsConflicts {
    let mut found = ProjectSettingsConflicts::default();
    for name in ["settings.json", "settings.local.json"] {
        let path = project.join(".claude").join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let shown = format!(".claude/{name}");
        if value.get("disableAllHooks").and_then(Value::as_bool) == Some(true)
            && found.hooks_disabled.is_none()
        {
            found.hooks_disabled = Some(format!("{shown} sets disableAllHooks"));
        }
        if value
            .pointer("/permissions/disableBypassPermissionsMode")
            .and_then(Value::as_str)
            == Some("disable")
            && found.bypass_disabled.is_none()
        {
            found.bypass_disabled = Some(format!(
                "{shown} sets permissions.disableBypassPermissionsMode"
            ));
        }
    }
    found
}

/// The file in a profile folder that names the folder its sign-in is kept under.
pub const SECURE_STORAGE_FILE: &str = ".secure-storage-dir";

/// Where this profile's sign-in is kept, if it is not under its own path.
fn secure_storage_dir(profile: &std::path::Path) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string(profile.join(SECURE_STORAGE_FILE)).ok()?;
    let path = std::path::PathBuf::from(text.trim());
    path.is_absolute().then_some(path)
}

fn tool_env(
    flavor: Flavor,
    home: &std::path::Path,
    tmp: &std::path::Path,
    profile: &std::path::Path,
    system_config: bool,
    inherited: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let mut env = base_env(flavor, home, tmp, inherited);
    if !system_config {
        env.push(("CLAUDE_CONFIG_DIR".into(), profile.into()));
        // Claude keeps its sign-in in the Keychain under the config folder's path,
        // so a folder that moved is signed out. A profile that moved says where its
        // sign-in still is, and Claude is told to look there.
        if let Some(dir) = secure_storage_dir(profile) {
            env.push(("CLAUDE_SECURESTORAGE_CONFIG_DIR".into(), dir.into()));
        }
    }
    env.push(("CLAUDE_CODE_TMPDIR".into(), tmp.into()));
    env.push(("DISABLE_UPDATES".into(), "1".into()));
    env.push(("DISABLE_AUTOUPDATER".into(), "1".into()));
    env
}

fn text_field_reply(value: &Value, field: &str) -> Option<EventText> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(|text| EventText::reply(text.to_owned()))
}

/// Ids of the background work a `Stop` lists. A task may be an object with an
/// `id`, or a bare id.
fn background_ids(value: &Value) -> Vec<EventText> {
    value
        .get("background_tasks")
        .and_then(Value::as_array)
        .map(|tasks| {
            tasks
                .iter()
                .filter_map(|task| {
                    task.get("id")
                        .or(Some(task))
                        .and_then(Value::as_str)
                        .map(|id| EventText::new(id.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn text_field(value: &Value, field: &'static str) -> Result<EventText, AdapterError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(|text| EventText::new(text.to_owned()))
        .ok_or(AdapterError::MissingField(field))
}

/// The footer rows Claude draws under its composer, by the mode they name.
const CLAUDE_FOOTER: [&str; 7] = [
    "? for shortcuts",
    "shift+tab to cycle",
    "bypass permissions on",
    "auto mode on",
    "manual mode on",
    "plan mode on",
    "accept edits on",
];

/// A rule drawn across the input box: the box has one above the composer line
/// and one below it.
fn is_rule(line: &str) -> bool {
    line.chars().count() >= 4 && line.chars().all(|c| c == '─')
}

/// A footer row: a mode the tool names under the composer.
fn is_footer_row(line: &str) -> bool {
    CLAUDE_FOOTER.iter().any(|mode| line.contains(mode))
}

/// What the live screen says about Claude's composer. Measured on 2.1.322 in
/// bypass, auto, default and manual modes, and in this repository's own
/// configuration (a custom status line and a usage row below the footer).
///
/// The composer is the last `❯` line drawn, anchored at the bottom: an older
/// `❯` echo higher up does not count. Its text is the typed draft, or the
/// dimmed placeholder `Try "…"`. The rows between it and the box's bottom rule
/// are the rest of a draft: any such row makes the composer `Occupied`, so a
/// multi-line draft with an empty first line is not ready. The rows after the
/// rule are the footer and any status rows the user configured; they are not
/// the draft. Two things the text cannot show: a typed line in the exact shape of
/// the placeholder reads as `Empty`, and a menu or prompt answered by the same
/// `❯` line reads as `Occupied`.
pub fn claude_composer_state(screen: &str) -> ComposerState {
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let Some(at) = lines.iter().rposition(|line| line.starts_with('❯')) else {
        return ComposerState::Booting;
    };
    let typed = lines[at].trim_start_matches('❯').trim();
    let placeholder = typed.starts_with("Try \"") && typed.ends_with('"');
    if !(typed.is_empty() || placeholder) {
        return ComposerState::Occupied;
    }
    let below = &lines[at + 1..];
    match below.iter().position(|line| is_rule(line)) {
        // The rows between the composer and the bottom rule are draft text.
        Some(rule) if rule > 0 => return ComposerState::Occupied,
        Some(_) => {}
        // No rule: only footer and warning rows may follow the composer.
        None => {
            if below
                .iter()
                .any(|line| !is_footer_row(line) && !line.starts_with('⚠'))
            {
                return ComposerState::Occupied;
            }
        }
    }
    if !below.iter().any(|line| is_footer_row(line)) {
        return ComposerState::Booting;
    }
    if below.iter().any(|line| line.contains("esc to interrupt")) {
        return ComposerState::Busy;
    }
    ComposerState::Empty
}

/// The Claude Code adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct Claude;

impl Claude {
    /// [`ToolAdapter::launch`] for `flavor`'s system.
    fn launch_as(flavor: Flavor, ctx: &LaunchCtx) -> LaunchSpec {
        let settings_path = ctx.run.join(SETTINGS_FILE);
        let mut args: Vec<OsString> = Vec::new();
        if let Some(utility) = &ctx.utility {
            // A utility session: no bypass flag, no tools, nothing from the
            // project's own settings, and the model and effort asked for.
            args.extend([
                "--settings".into(),
                settings_path.clone().into(),
                "--setting-sources".into(),
                "user".into(),
                "--strict-mcp-config".into(),
                "--tools".into(),
                "".into(),
                "--model".into(),
                utility.model.clone().into(),
                "--effort".into(),
                utility.effort.clone().into(),
            ]);
        } else {
            // How the tool asks before it acts is the user's choice (setup).
            match ctx.agent_mode {
                AgentMode::Bypass => args.push("--dangerously-skip-permissions".into()),
                AgentMode::Auto => args.extend(["--permission-mode".into(), "auto".into()]),
                AgentMode::Prompt => {}
            }
            args.extend([
                "--settings".into(),
                settings_path.clone().into(),
                "--setting-sources".into(),
                SETTING_SOURCES.into(),
                "--strict-mcp-config".into(),
            ]);
            if let Some(model) = &ctx.model {
                args.extend(["--model".into(), model.into()]);
            }
        }
        if let (Some(name), None) = (&ctx.session_name, &ctx.utility) {
            args.extend(["--name".into(), name.into()]);
        }
        // A resumed conversation keeps the id it had; a new one gets ours.
        match &ctx.resume {
            Some(id) => args.extend(["--resume".into(), id.clone().into()]),
            None => args.extend(["--session-id".into(), ctx.session_id.clone().into()]),
        }
        if let Some(text) = &ctx.bridge.text {
            args.push("--append-system-prompt".into());
            args.push(text.into());
        }
        if let (Some(_), Some(note), None) = (&ctx.resume, &ctx.resume_note, &ctx.utility) {
            args.push("--".into());
            args.push(note.into());
        }
        // The first message of a new conversation. `--` ends the options, so
        // nothing in the text can be read as one.
        if ctx.resume.is_none()
            && ctx.utility.is_none()
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
        if let Some(url) = &ctx.fake_model {
            env.push(("ANTHROPIC_BASE_URL".into(), url.into()));
            env.push(("ANTHROPIC_AUTH_TOKEN".into(), "fake-local-model".into()));
        }
        // Windows: the PowerShell twins of the builtin status lines (see
        // `statusline`). A utility session reads no project settings.
        let extra = match flavor {
            Flavor::Windows if ctx.utility.is_none() => windows_statuslines(
                &ctx.project,
                windows_var(&ctx.inherited, "SystemRoot").and_then(std::ffi::OsStr::to_str),
            ),
            _ => serde_json::Map::new(),
        };
        let settings_file = settings(
            flavor,
            &ctx.hook_binary.to_string_lossy(),
            ctx.bridge.hooks,
            &bridge_env_pairs(&env),
            extra,
        );
        LaunchSpec {
            program: ctx.entry.clone(),
            args,
            cwd: ctx.project.clone(),
            env,
            files: vec![(settings_path, settings_file)],
        }
    }
}

impl ToolAdapter for Claude {
    fn launch(&self, ctx: &LaunchCtx) -> LaunchSpec {
        Self::launch_as(Flavor::HOST, ctx)
    }

    fn login(&self, ctx: &LoginCtx) -> LaunchSpec {
        LaunchSpec {
            program: ctx.entry.clone(),
            args: vec!["auth".into(), "login".into()],
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
            args: vec!["auth".into(), "status".into()],
            cwd: ctx.session_tmp.clone(),
            ..self.login(ctx)
        }
    }

    fn parse_status(&self, output: &str) -> AuthState {
        let Ok(value) = serde_json::from_str::<Value>(output) else {
            return AuthState::Unknown;
        };
        match value.get("loggedIn").and_then(Value::as_bool) {
            Some(true) => {
                let method = value.get("authMethod").and_then(Value::as_str);
                let safe = method.filter(|method| {
                    method.len() <= 32
                        && method
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                });
                AuthState::LoggedIn {
                    method: safe.unwrap_or("other").to_owned(),
                }
            }
            Some(false) => AuthState::LoggedOut,
            None => AuthState::Unknown,
        }
    }

    fn parse_hook(&self, payload: &[u8], _ctx: &HookCtx) -> Result<Vec<ToolEvent>, AdapterError> {
        let value: Value = serde_json::from_slice(payload).map_err(|_| AdapterError::NotJson)?;
        let name = value
            .get("hook_event_name")
            .and_then(Value::as_str)
            .ok_or(AdapterError::MissingField("hook_event_name"))?;
        let incomplete = value.get("connecto_incomplete").and_then(Value::as_bool) == Some(true);
        Ok(match name {
            "SessionStart" if !incomplete => vec![ToolEvent::Started {
                tool_session: text_field(&value, "session_id")?,
                turn_log: text_field(&value, "transcript_path").ok(),
            }],
            "UserPromptSubmit" if !incomplete => vec![ToolEvent::PromptSubmitted {
                prompt: text_field(&value, "prompt")?,
                turn: None,
            }],
            "Stop" => {
                let dropped = value
                    .get("connecto_dropped")
                    .and_then(Value::as_array)
                    .is_some_and(|names| {
                        names
                            .iter()
                            .any(|name| name.as_str() == Some("last_assistant_message"))
                    });
                let reply = match text_field_reply(&value, "last_assistant_message") {
                    Some(text) if text.truncated() => Reply::Dropped,
                    Some(text) => Reply::Text(text),
                    None if dropped => Reply::Dropped,
                    None => Reply::None,
                };
                vec![ToolEvent::TurnEnded {
                    input: Vec::new(),
                    input_total: 0,
                    reply,
                    lifecycle_known: !incomplete,
                    background: background_ids(&value),
                    thread: None,
                    turn: None,
                }]
            }
            "Notification" if !incomplete => {
                // An idle Claude would otherwise wake its watchers.
                let kind = value.get("notification_type").and_then(Value::as_str);
                if matches!(kind, Some("idle_prompt" | "auth_success")) {
                    Vec::new()
                } else {
                    vec![ToolEvent::NeedsUser]
                }
            }
            _ => Vec::new(),
        })
    }

    /// Claude's ready signal is the `SessionStart` hook, not the screen.
    fn readiness(&self, _screen: &str) -> Readiness {
        Readiness::Unknown
    }

    /// Measured on 2.1.318: a tab becomes four spaces at any column, and
    /// whitespace at the end is sometimes dropped (`x\n` is, `x\n\n` is not).
    fn ack_form(&self, text: &str) -> String {
        text.replace('\t', "    ").trim_end().to_owned()
    }

    fn send_anyway_text(&self) -> &'static str {
        "Claude has not reported that it started. The message may be lost; it would then end Uncertain."
    }

    fn capabilities(&self, _version: &str) -> Capabilities {
        Capabilities {
            immediate_ack: true,
        }
    }

    fn turn_log(&self, reported: &Path, ctx: &HookCtx) -> Option<PathBuf> {
        under_profile(reported, &ctx.profile)
    }

    fn read_log(&self, line: &str, whole: bool) -> LogMark {
        if !whole {
            // Only the start of a very long line is given: it can still be a
            // record that ends a turn or an interrupt, but its text is skipped.
            return if line.contains(r#""stop_reason":"end_turn""#) {
                LogMark::FinalReply
            } else {
                LogMark::Other
            };
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return LogMark::Other;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let stop = value
                    .pointer("/message/stop_reason")
                    .and_then(Value::as_str);
                if stop == Some("end_turn") {
                    LogMark::FinalReply
                } else {
                    LogMark::Other
                }
            }
            Some("user") => {
                let content = value.pointer("/message/content");
                let text = match content {
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(Value::Array(blocks)) => {
                        let texts: Vec<&str> = blocks
                            .iter()
                            .filter(|block| {
                                block.get("type").and_then(Value::as_str) == Some("text")
                            })
                            .filter_map(|block| block.get("text").and_then(Value::as_str))
                            .collect();
                        // A tool result is not a prompt.
                        (!texts.is_empty()).then(|| texts.join("\n"))
                    }
                    _ => None,
                };
                match text {
                    Some(text) if text.starts_with("[Request interrupted by user") => {
                        LogMark::Interrupt { turn: None }
                    }
                    Some(text) => LogMark::Prompt {
                        text: prompt_head(&text),
                    },
                    None => LogMark::Other,
                }
            }
            // A prompt the tool took while a turn was running (a message from
            // another agent, a background task's notification) is logged as an
            // attachment, not as a user record. Its hook fires all the same, so
            // without this mark the turn's end could never place it.
            Some("attachment")
                if value.pointer("/attachment/type").and_then(Value::as_str)
                    == Some("queued_command") =>
            {
                value
                    .pointer("/attachment/prompt")
                    .and_then(Value::as_str)
                    .map_or(LogMark::Other, |text| LogMark::Prompt {
                        text: prompt_head(text),
                    })
            }
            _ => LogMark::Other,
        }
    }

    fn continuation(&self, prompt: &str) -> Option<Continuation> {
        let rest = prompt.trim_start().strip_prefix("<task-notification>")?;
        let task = rest
            .split_once("<task-id>")
            .and_then(|(_, after)| after.split_once("</task-id>"))
            .map(|(id, _)| id.trim().to_owned())
            .filter(|id| !id.is_empty() && id.len() <= 128);
        Some(Continuation { task })
    }
}

/// The start of a prompt, enough to match a hook's prompt to its log record.
pub(crate) fn prompt_head(text: &str) -> String {
    text.chars().take(256).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    /// Screens captured from Claude Code 2.1.322 with Connecto's flags (in
    /// `tests/fixtures/claude`), with the state each one must read as.
    const SCREENS: &[(&str, ComposerState)] = &[
        ("fresh_bypass_ready.txt", ComposerState::Empty),
        ("after_rename_bypass.txt", ComposerState::Empty),
        ("done_then_idle_bypass.txt", ComposerState::Empty),
        ("resumed_transcript_bypass.txt", ComposerState::Empty),
        ("after_interrupt_auto.txt", ComposerState::Empty),
        ("working_bypass.txt", ComposerState::Busy),
        ("draft_bypass.txt", ComposerState::Occupied),
        ("slash_menu_bypass.txt", ComposerState::Occupied),
        ("approval_prompt_default.txt", ComposerState::Occupied),
        // The folder-trust prompt is a blocker, found before the composer is read.
        ("trust_prompt.txt", ComposerState::Occupied),
        // This repository's own configuration: a custom status line below the
        // footer, and the dimmed placeholder in the composer.
        ("repo_fresh_custom_statusline.txt", ComposerState::Empty),
        ("repo_typed_draft.txt", ComposerState::Occupied),
        // The text of a typed line in the placeholder's exact shape cannot be told
        // from the placeholder without the cell colour: it reads as Empty.
        ("repo_typed_placeholder_shape.txt", ComposerState::Empty),
        // A draft whose first line is empty: the second line sits above the rule.
        ("repo_multiline_empty_first.txt", ComposerState::Occupied),
    ];

    #[test]
    fn captured_claude_screens_read_as_their_composer_state() {
        for (file, want) in SCREENS {
            let path = format!(
                "{}/tests/fixtures/claude/{file}",
                env!("CARGO_MANIFEST_DIR")
            );
            let screen = std::fs::read_to_string(&path).unwrap();
            assert_eq!(claude_composer_state(&screen), *want, "{file}");
        }
    }

    #[test]
    fn a_claude_placeholder_is_recognised_by_shape_and_a_typed_line_is_not() {
        let footer = "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n";
        for placeholder in [
            "Try \"fix the lint errors\"",
            "Try \"how does <filepath> work?\"",
        ] {
            let idle = format!("❯ {placeholder}\n──────\n{footer}");
            assert_eq!(
                claude_composer_state(&idle),
                ComposerState::Empty,
                "{placeholder}"
            );
        }
        let typed = format!("❯ hello\n──────\n{footer}");
        assert_eq!(claude_composer_state(&typed), ComposerState::Occupied);
        // No footer under the composer yet: still booting, not ready.
        assert_eq!(claude_composer_state("❯ \n"), ComposerState::Booting);
    }

    // Unix paths in the fixture (`/old/place` is not absolute on Windows); the
    // Windows counterpart follows.
    #[cfg(unix)]
    #[test]
    fn a_moved_profile_is_given_the_folder_its_sign_in_is_kept_under() {
        let dir = std::env::temp_dir().join(format!("connecto-secure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("tmp");
        let find = |profile: &std::path::Path, system: bool| {
            tool_env(Flavor::HOST, &dir, &tmp, profile, system, &[])
                .into_iter()
                .find(|(name, _)| name == "CLAUDE_SECURESTORAGE_CONFIG_DIR")
                .map(|(_, value)| value)
        };
        // No note: Claude uses the config folder's own path.
        assert_eq!(find(&dir, false), None);
        std::fs::write(dir.join(SECURE_STORAGE_FILE), "/old/place/g2\n").unwrap();
        assert_eq!(find(&dir, false), Some("/old/place/g2".into()));
        // Not for the system configuration, and not for a path that is not absolute.
        assert_eq!(find(&dir, true), None);
        std::fs::write(dir.join(SECURE_STORAGE_FILE), "relative").unwrap();
        assert_eq!(find(&dir, false), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_moved_profile_is_given_the_folder_its_sign_in_is_kept_under_on_windows() {
        let dir = std::env::temp_dir().join(format!("connecto-secure-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("tmp");
        let find = |profile: &std::path::Path, system: bool| {
            tool_env(Flavor::HOST, &dir, &tmp, profile, system, &[])
                .into_iter()
                .find(|(name, _)| name == "CLAUDE_SECURESTORAGE_CONFIG_DIR")
                .map(|(_, value)| value)
        };
        assert_eq!(find(&dir, false), None);
        std::fs::write(dir.join(SECURE_STORAGE_FILE), "C:\\old\\place\\g2\r\n").unwrap();
        assert_eq!(find(&dir, false), Some(r"C:\old\place\g2".into()));
        assert_eq!(find(&dir, true), None);
        // Not absolute on Windows: no drive.
        std::fs::write(dir.join(SECURE_STORAGE_FILE), r"\old\place").unwrap();
        assert_eq!(find(&dir, false), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn the_ack_form_removes_only_what_claude_was_measured_rewriting() {
        assert_eq!(Claude.ack_form("a\tb\n"), Claude.ack_form("a    b"));
        assert_eq!(Claude.ack_form("end\n\n"), "end");
        assert_eq!(Claude.ack_form("kept  \n\n\nend"), "kept  \n\n\nend");
        assert_ne!(Claude.ack_form("one body"), Claude.ack_form("other body"));
    }
    use crate::spec::tests::{launch_ctx, login_ctx};

    #[test]
    fn system_config_leaves_the_config_dir_unset_but_keeps_the_hooks() {
        let has = |spec: &LaunchSpec, name: &str| spec.env.iter().any(|(key, _)| key == name);
        let mut ctx = launch_ctx();
        let isolated = Claude.launch(&ctx);
        assert!(has(&isolated, "CLAUDE_CONFIG_DIR"));
        ctx.system_config = true;
        let system = Claude.launch(&ctx);
        assert!(!has(&system, "CLAUDE_CONFIG_DIR"));
        assert!(has(&system, "CONNECTO_HOOK_TOKEN") && has(&system, "CONNECTO_BRIDGE_SOCK"));
        // The hooks still travel in the settings file.
        assert!(
            String::from_utf8_lossy(&system.files[0].1).contains("UserPromptSubmit"),
            "hook settings are written"
        );
        let mut login = login_ctx();
        login.system_config = true;
        assert!(!has(&Claude.login(&login), "CLAUDE_CONFIG_DIR"));
    }

    fn hook_ctx() -> HookCtx {
        HookCtx {
            profile: "/p".into(),
            hooks: true,
        }
    }

    fn args(spec: &LaunchSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    // Unix paths in the fixture (`/run` joined with `/`); the Windows
    // counterpart is `a_windows_session_gets_every_flag_the_design_names`.
    #[cfg(unix)]
    #[test]
    fn a_session_gets_every_flag_the_design_names() {
        let spec = Claude.launch(&launch_ctx());
        assert_eq!(
            args(&spec),
            [
                "--dangerously-skip-permissions",
                "--settings",
                "/run/claude-settings.json",
                "--setting-sources",
                "user,project,local",
                "--strict-mcp-config",
                "--session-id",
                "11111111-2222-4333-8444-555555555555",
                "--append-system-prompt",
                "LAUNCH TEXT",
            ]
        );
    }

    #[test]
    fn a_resumed_session_uses_resume_in_place_of_session_id() {
        let mut ctx = launch_ctx();
        ctx.resume = Some("99999999-2222-4333-8444-555555555555".to_owned());
        let words = args(&Claude.launch(&ctx));
        assert!(!words.iter().any(|word| word == "--session-id"));
        let at = words.iter().position(|word| word == "--resume").unwrap();
        assert_eq!(words[at + 1], "99999999-2222-4333-8444-555555555555");
    }

    // The macOS hook shape through the public API; Windows has the exec form
    // (`windows_hooks_are_exec_form_and_survive_any_path`).
    #[cfg(unix)]
    #[test]
    fn hook_commands_survive_spaces_and_quotes_in_the_path() {
        let spec = Claude.launch(&launch_ctx());
        let (path, body) = &spec.files[0];
        assert_eq!(path.to_str(), Some("/run/claude-settings.json"));
        let value: Value = serde_json::from_slice(body).unwrap();
        for hook in HOOKS {
            assert_eq!(
                value["hooks"][hook][0]["hooks"][0]["command"],
                format!(
                    "'/data/cli/bin/connecto-hook' claude --event {hook}{}",
                    if hook == "UserPromptSubmit" {
                        " --context"
                    } else {
                        ""
                    }
                )
            );
        }
    }

    /// Every Claude Connecto starts runs with full power (A11), however it is
    /// launched, and nothing else in the launch lowers it.
    #[test]
    fn every_launch_runs_with_permissions_bypassed_and_nothing_lowers_it() {
        for (hooks, text, trust) in [(true, true, true), (false, false, false)] {
            let mut ctx = launch_ctx();
            ctx.bridge.hooks = hooks;
            ctx.bridge.text = text.then(|| "T".to_owned());
            ctx.bridge.trust = trust.then(|| "x".to_owned());
            let spec = Claude.launch(&ctx);
            let words = args(&spec);
            assert_eq!(
                words
                    .iter()
                    .filter(|w| *w == "--dangerously-skip-permissions")
                    .count(),
                1,
                "{words:?}"
            );
            for lowering in [
                "--permission-mode",
                "--allowedTools",
                "--allow-dangerously-skip-permissions",
            ] {
                assert!(!words.iter().any(|w| w == lowering), "{lowering}");
            }
            // The settings it is given set no permission mode either.
            let settings: Value = serde_json::from_slice(&spec.files[0].1).unwrap();
            assert!(
                settings["permissions"].get("defaultMode").is_none(),
                "{settings}"
            );
            assert!(settings.get("defaultMode").is_none());
            // Nothing denies Claude's own messaging tools outward any more.
            assert!(settings["permissions"].get("deny").is_none(), "{settings}");
            assert!(!words.iter().any(|w| w == "--disallowedTools"));
        }
    }

    /// The setup choice maps to exactly one set of flags, and a utility session
    /// never takes any of them.
    #[test]
    fn the_launch_mode_picks_the_permission_flags() {
        let words = |mode: AgentMode| {
            let mut ctx = launch_ctx();
            ctx.agent_mode = mode;
            args(&Claude.launch(&ctx))
        };
        let bypass = words(AgentMode::Bypass);
        assert!(bypass.iter().any(|w| w == "--dangerously-skip-permissions"));
        assert!(!bypass.iter().any(|w| w == "--permission-mode"));
        let auto = words(AgentMode::Auto);
        assert!(auto.windows(2).any(|w| w == ["--permission-mode", "auto"]));
        assert!(!auto.iter().any(|w| w == "--dangerously-skip-permissions"));
        let prompt = words(AgentMode::Prompt);
        assert!(!prompt.iter().any(|w| w == "--permission-mode"));
        assert!(!prompt.iter().any(|w| w.contains("skip-permissions")));
        // Everything else about the launch is the same in every mode.
        for w in [&bypass, &auto, &prompt] {
            assert!(w.iter().any(|a| a == "--settings"));
            assert!(w.iter().any(|a| a == "--session-id"));
            assert!(w.iter().any(|a| a == "--strict-mcp-config"));
        }
        // A utility session has neither, whatever the choice.
        for mode in AgentMode::ALL {
            let mut ctx = launch_ctx();
            ctx.agent_mode = mode;
            ctx.utility = Some(crate::UtilityLaunch {
                model: "haiku".into(),
                effort: "low".into(),
            });
            let w = args(&Claude.launch(&ctx));
            assert!(
                !w.iter()
                    .any(|a| a == "--permission-mode" || a.contains("skip-permissions"))
            );
        }
    }

    /// AC7, as decided after the first manual run: other sessions still cannot
    /// reach a Connecto terminal, and Claude's own `SendMessage` and `ListAgents`
    /// are no longer denied outward, so a skill can message its subagents.
    #[test]
    fn a_resume_note_is_the_last_argument_of_a_resumed_conversation_only() {
        let strings = |spec: &LaunchSpec| -> Vec<String> {
            spec.args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let mut ctx = crate::spec::tests::launch_ctx();
        ctx.resume_note = Some("--re-read the guide".to_owned());
        // A new conversation takes no note.
        assert!(
            !strings(&Claude.launch(&ctx))
                .iter()
                .any(|a| a.contains("re-read"))
        );
        ctx.resume = Some("abc".to_owned());
        let args = strings(&Claude.launch(&ctx));
        assert_eq!(&args[args.len() - 2..], ["--", "--re-read the guide"]);
        assert!(args.windows(2).any(|w| w == ["--resume", "abc"]));
    }

    #[test]
    fn a_model_is_passed_only_when_the_terminal_was_added_with_one() {
        let mut ctx = crate::spec::tests::launch_ctx();
        assert!(!args(&Claude.launch(&ctx)).iter().any(|w| w == "--model"));
        ctx.model = Some("opus".to_owned());
        let words = args(&Claude.launch(&ctx));
        assert!(words.windows(2).any(|w| w == ["--model", "opus"]));
        // A later start of the same terminal resumes on the same model.
        ctx.resume = Some("abc".to_owned());
        let words = args(&Claude.launch(&ctx));
        assert!(words.windows(2).any(|w| w == ["--model", "opus"]));
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
        ctx.initial_prompt = Some("--dangerous you are authoring".to_owned());
        let args = strings(&Claude.launch(&ctx));
        assert_eq!(
            &args[args.len() - 2..],
            ["--", "--dangerous you are authoring"]
        );
        // Resuming, or a utility run, takes none.
        ctx.resume = Some("abc".to_owned());
        assert!(
            !strings(&Claude.launch(&ctx))
                .iter()
                .any(|a| a.contains("authoring"))
        );
        ctx.resume = None;
        ctx.utility = Some(crate::UtilityLaunch {
            model: "haiku".into(),
            effort: "low".into(),
        });
        assert!(
            !strings(&Claude.launch(&ctx))
                .iter()
                .any(|a| a.contains("authoring"))
        );
        // And none without one.
        let plain = strings(&Claude.launch(&crate::spec::tests::launch_ctx()));
        assert!(!plain.contains(&"--".to_owned()));
    }

    #[test]
    fn the_launch_refuses_inbound_messaging_and_no_longer_denies_it_outward() {
        let spec = Claude.launch(&launch_ctx());
        let value: Value = serde_json::from_slice(&spec.files[0].1).unwrap();
        assert_eq!(value["crossSessionInbound"], "refuse");
        assert!(value.get("permissions").is_none(), "{value}");
        let words = args(&spec);
        assert!(!words.iter().any(|w| w == "--disallowedTools"), "{words:?}");
        assert!(
            !words
                .iter()
                .any(|w| w == "SendMessage" || w == "ListAgents")
        );
        assert!(!String::from_utf8_lossy(&spec.files[0].1).contains("SendMessage"));
    }

    /// Project skills, agents and settings load (checked on 2.1.318), and what
    /// Connecto sets stays: its own `--settings` carries the hooks, the bypass
    /// flag and the MCP restriction are launch flags, which settings files do
    /// not outrank.
    #[test]
    fn project_settings_load_and_connectos_own_launch_flags_stay() {
        let words = args(&Claude.launch(&launch_ctx()));
        let at = words.iter().position(|a| a == "--setting-sources").unwrap();
        assert_eq!(words[at + 1], "user,project,local");
        for flag in [
            "--dangerously-skip-permissions",
            "--strict-mcp-config",
            "--settings",
        ] {
            assert!(words.iter().any(|w| w == flag), "{flag}");
        }
    }

    #[test]
    fn login_has_no_bypass_flag_and_no_hook_channel_and_runs_in_the_run_folder() {
        let spec = Claude.login(&login_ctx());
        assert_eq!(args(&spec), ["auth", "login"]);
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
        let spec = Claude.status(&login_ctx());
        assert_eq!(spec.cwd.to_str(), Some("/run/tmp"));
    }

    #[test]
    fn status_output_keeps_only_state_and_method() {
        let out = r#"{"loggedIn":true,"authMethod":"claude.ai","email":"a@b","orgName":"x"}"#;
        assert_eq!(
            Claude.parse_status(out),
            AuthState::LoggedIn {
                method: "other".to_owned()
            }
        );
        let out = r#"{"loggedIn":true,"authMethod":"oauth_token"}"#;
        assert_eq!(
            Claude.parse_status(out),
            AuthState::LoggedIn {
                method: "oauth_token".to_owned()
            }
        );
    }

    #[test]
    fn status_failure_is_unknown_and_not_logged_out() {
        assert_eq!(
            Claude.parse_status(r#"{"loggedIn":false,"authMethod":"none"}"#),
            AuthState::LoggedOut
        );
        assert_eq!(Claude.parse_status("boom"), AuthState::Unknown);
        assert_eq!(Claude.parse_status("{}"), AuthState::Unknown);
    }

    fn ended(reply: Reply, background: &[&str]) -> ToolEvent {
        ToolEvent::TurnEnded {
            input: Vec::new(),
            input_total: 0,
            reply,
            lifecycle_known: true,
            background: background
                .iter()
                .map(|id| EventText::new((*id).to_owned()))
                .collect(),
            thread: None,
            turn: None,
        }
    }

    #[test]
    fn real_hook_shapes_become_events() {
        let parse = |json: &str| Claude.parse_hook(json.as_bytes(), &hook_ctx()).unwrap();
        assert_eq!(
            parse(
                r#"{"hook_event_name":"SessionStart","session_id":"s","transcript_path":"/p/t.jsonl"}"#
            ),
            [ToolEvent::Started {
                tool_session: EventText::new("s".to_owned()),
                turn_log: Some(EventText::new("/p/t.jsonl".to_owned())),
            }]
        );
        assert_eq!(
            parse(r#"{"hook_event_name":"UserPromptSubmit","prompt":"hi\nthere"}"#),
            [ToolEvent::PromptSubmitted {
                prompt: EventText::new("hi\nthere".to_owned()),
                turn: None,
            }]
        );
        assert_eq!(
            parse(r#"{"hook_event_name":"Stop","last_assistant_message":"done"}"#),
            [ended(Reply::Text(EventText::new("done".to_owned())), &[])]
        );
        assert_eq!(
            parse(r#"{"hook_event_name":"Notification"}"#),
            [ToolEvent::NeedsUser]
        );
        assert!(parse(r#"{"hook_event_name":"PreCompact"}"#).is_empty());
    }

    /// A Stop with background work lists the task ids. An idle Claude is not
    /// asking for the user.
    #[test]
    fn a_stop_lists_background_tasks_and_idle_notifications_are_not_needs_user() {
        let parse = |json: &str| Claude.parse_hook(json.as_bytes(), &hook_ctx()).unwrap();
        assert_eq!(
            parse(
                r#"{"hook_event_name":"Stop","background_tasks":[{"id":"b1","kind":"shell"},{"id":"b2"}]}"#
            ),
            [ended(Reply::None, &["b1", "b2"])]
        );
        for kind in ["idle_prompt", "auth_success"] {
            let json =
                format!(r#"{{"hook_event_name":"Notification","notification_type":"{kind}"}}"#);
            assert!(parse(&json).is_empty(), "{kind}");
        }
        for kind in ["permission_prompt", "something_new"] {
            let json =
                format!(r#"{{"hook_event_name":"Notification","notification_type":"{kind}"}}"#);
            assert_eq!(parse(&json), [ToolEvent::NeedsUser], "{kind}");
        }
    }

    #[test]
    fn incomplete_and_shrunk_frames_keep_their_lifecycle_facts() {
        let parse = |json: &str| Claude.parse_hook(json.as_bytes(), &hook_ctx()).unwrap();
        let incomplete = parse(r#"{"hook_event_name":"Stop","connecto_incomplete":true}"#);
        assert!(matches!(
            &incomplete[..],
            [ToolEvent::TurnEnded {
                lifecycle_known: false,
                ..
            }]
        ));
        assert_eq!(
            parse(
                r#"{"hook_event_name":"Stop","background_tasks":[{"id":"b"}],"connecto_dropped":["last_assistant_message"]}"#
            ),
            [ended(Reply::Dropped, &["b"])]
        );
        assert!(
            parse(r#"{"hook_event_name":"UserPromptSubmit","connecto_incomplete":true}"#)
                .is_empty()
        );
    }

    #[test]
    fn the_transcript_marks_are_read() {
        let read = |line: &str| Claude.read_log(line, true);
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":"[Request interrupted by user for tool use]"}}"#
            ),
            LogMark::Interrupt { turn: None }
        );
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#
            ),
            LogMark::Prompt {
                text: "hello".into()
            }
        );
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#
            ),
            LogMark::Other
        );
        // A prompt queued into a running turn is an attachment record.
        assert_eq!(
            read(
                r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"<task-notification>x"}}"#
            ),
            LogMark::Prompt {
                text: "<task-notification>x".into()
            }
        );
        assert_eq!(
            read(r#"{"type":"attachment","attachment":{"type":"environment"}}"#),
            LogMark::Other
        );
        assert_eq!(
            read(r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[]}}"#),
            LogMark::FinalReply
        );
        assert_eq!(
            read(r#"{"type":"assistant","message":{"stop_reason":"tool_use"}}"#),
            LogMark::Other
        );
        assert_eq!(
            Claude.read_log(
                r#"{"type":"assistant","message":{"stop_reason":"end_turn","con"#,
                false
            ),
            LogMark::FinalReply
        );
    }

    #[test]
    fn a_self_started_turn_is_recognised_and_may_name_its_task() {
        assert_eq!(
            Claude.continuation("<task-notification><task-id>b7</task-id>done"),
            Some(Continuation {
                task: Some("b7".into())
            })
        );
        assert_eq!(
            Claude.continuation("<task-notification>done"),
            Some(Continuation { task: None })
        );
        assert_eq!(Claude.continuation("please run <task-notification>"), None);
    }

    #[test]
    fn only_a_log_under_the_profile_is_tailed() {
        let ctx = hook_ctx();
        assert!(
            Claude
                .turn_log(Path::new("/p/projects/t.jsonl"), &ctx)
                .is_some()
        );
        assert!(Claude.turn_log(Path::new("/etc/passwd"), &ctx).is_none());
        assert!(
            Claude
                .turn_log(Path::new("/p/../etc/passwd"), &ctx)
                .is_none()
        );
    }

    /// AC7 also needs the hooks to name their events with the bridge on.
    /// The macOS shapes through the public API; the Windows counterpart is
    /// `a_windows_launch_has_exec_hooks_and_a_windows_path`.
    #[cfg(unix)]
    #[test]
    fn with_the_bridge_on_each_hook_names_its_event_and_the_prompt_hook_drains() {
        let spec = Claude.launch(&launch_ctx());
        let value: Value = serde_json::from_slice(&spec.files[0].1).unwrap();
        assert_eq!(
            value["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "'/data/cli/bin/connecto-hook' claude --event UserPromptSubmit --context"
        );
        assert_eq!(
            value["hooks"]["Stop"][0]["hooks"][0]["command"],
            "'/data/cli/bin/connecto-hook' claude --event Stop"
        );
        let words = args(&spec);
        let at = words
            .iter()
            .position(|a| a == "--append-system-prompt")
            .unwrap();
        assert_eq!(words[at + 1], "LAUNCH TEXT");
        let path = spec.env.iter().find(|(name, _)| name == "PATH").unwrap();
        assert_eq!(path.1.to_string_lossy(), "/data/cli/bin:/usr/bin");
        assert!(
            spec.env
                .iter()
                .any(|(name, value)| name == "CONNECTO_BRIDGE_SOCK"
                    && value.to_string_lossy() == "/private/tmp/mg-501/bridge-1.sock")
        );
    }

    #[test]
    fn malformed_hook_frames_are_errors() {
        assert_eq!(
            Claude.parse_hook(b"nope", &hook_ctx()),
            Err(AdapterError::NotJson)
        );
        assert!(Claude.parse_hook(b"{}", &hook_ctx()).is_err());
        assert!(
            Claude
                .parse_hook(br#"{"hook_event_name":"UserPromptSubmit"}"#, &hook_ctx())
                .is_err()
        );
    }

    #[test]
    fn capabilities_do_not_depend_on_the_version() {
        for version in ["2.1.318", "2.1.322", "9.9.9", "", "not a version"] {
            assert!(Claude.capabilities(version).immediate_ack, "{version}");
        }
    }

    /// The settings file repeats the values the bridge needs, because
    /// `--settings` outranks a project's `env` (checked on 2.1.318): a project
    /// cannot redirect the `connecto` command or the hook helper. (`PATH` is
    /// joined with `:` on macOS; Windows: `a_windows_launch_has_exec_hooks_and_a_windows_path`.)
    #[cfg(unix)]
    #[test]
    fn the_settings_pin_the_bridge_environment_over_a_projects() {
        let spec = Claude.launch(&launch_ctx());
        let value: Value = serde_json::from_slice(&spec.files[0].1).unwrap();
        assert_eq!(
            value["env"]["CONNECTO_BRIDGE_SOCK"],
            "/private/tmp/mg-501/bridge-1.sock"
        );
        assert_eq!(value["env"]["CONNECTO_TERMINAL"], "claude");
        assert_eq!(value["env"]["CONNECTO_HOOK_TOKEN"], "token");
        assert_eq!(
            value["env"]["CONNECTO_HOOK_SOCK"],
            "/private/tmp/mg-501/a.sock"
        );
        assert!(
            value["env"]["PATH"]
                .as_str()
                .unwrap()
                .starts_with("/data/cli/bin:")
        );
    }

    fn settings_of(spec: &LaunchSpec) -> Value {
        serde_json::from_slice(&spec.files[0].1).unwrap()
    }

    /// The macOS settings file, on every host, byte for byte as the code before
    /// the Windows port built it (the same `json!` construction, inlined here).
    #[test]
    fn the_unix_settings_file_is_unchanged() {
        for hooks in [true, false] {
            let mut ctx = launch_ctx();
            ctx.bridge.hooks = hooks;
            let spec = Claude::launch_as(Flavor::Unix, &ctx);
            let entries: serde_json::Map<String, Value> = HOOKS
                .iter()
                .map(|name| {
                    let command = if hooks {
                        let context = if *name == "UserPromptSubmit" {
                            " --context"
                        } else {
                            ""
                        };
                        format!("'/data/cli/bin/connecto-hook' claude --event {name}{context}")
                    } else {
                        "'/data/cli/bin/connecto-hook' claude".to_owned()
                    };
                    (
                        (*name).to_owned(),
                        json!([{ "hooks": [{ "type": "command", "command": command }] }]),
                    )
                })
                .collect();
            let env: serde_json::Map<String, Value> = [
                ("CONNECTO_HOOK_SOCK", "/private/tmp/mg-501/a.sock"),
                ("CONNECTO_HOOK_TOKEN", "token"),
                ("CONNECTO_BRIDGE_SOCK", "/private/tmp/mg-501/bridge-1.sock"),
                ("CONNECTO_TERMINAL", "claude"),
                ("PATH", "/data/cli/bin:/usr/bin"),
            ]
            .iter()
            .map(|(name, value)| ((*name).to_owned(), json!(value)))
            .collect();
            let before = json!({
                "hooks": entries,
                "crossSessionInbound": "refuse",
                "env": env,
            })
            .to_string();
            assert_eq!(String::from_utf8_lossy(&spec.files[0].1), before);
            // No status line on macOS: the project's own runs.
            assert!(settings_of(&spec).get("statusLine").is_none());
        }
    }

    /// The Windows hooks, on every host: exec form, the helper as `command`
    /// exactly as given (spaces and `'` included), the words in `args`.
    #[test]
    fn windows_hooks_are_exec_form_and_survive_any_path() {
        let ctx = crate::spec::tests::launch_ctx_windows();
        let helper = ctx.hook_binary.to_string_lossy().into_owned();
        let value = settings_of(&Claude::launch_as(Flavor::Windows, &ctx));
        for hook in HOOKS {
            let mut words = vec!["claude", "--event", hook];
            if hook == "UserPromptSubmit" {
                words.push("--context");
            }
            assert_eq!(
                value["hooks"][hook],
                json!([{ "hooks": [{ "type": "command", "command": helper, "args": words }] }]),
                "{hook}"
            );
        }
        assert_eq!(
            value["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            r"C:\Users\Jane O'Neil\AppData\Roaming\connecto\cli\bin\connecto-hook.exe"
        );
        // Without the bridge: the older form, `claude` alone.
        let mut ctx = crate::spec::tests::launch_ctx_windows();
        ctx.bridge.hooks = false;
        let value = settings_of(&Claude::launch_as(Flavor::Windows, &ctx));
        for hook in HOOKS {
            assert_eq!(
                value["hooks"][hook][0]["hooks"][0],
                json!({ "type": "command", "command": helper, "args": ["claude"] })
            );
        }
        // Nothing else changes: the same refusal of inbound messages.
        assert_eq!(value["crossSessionInbound"], "refuse");
    }

    /// The Windows launch environment, on every host.
    #[test]
    fn a_windows_launch_has_exec_hooks_and_a_windows_path() {
        let ctx = crate::spec::tests::launch_ctx_windows();
        let spec = Claude::launch_as(Flavor::Windows, &ctx);
        let get = |name: &str| {
            spec.env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        let path =
            r"C:\Users\Jane O'Neil\AppData\Roaming\connecto\cli\bin;C:\WINDOWS\system32;C:\WINDOWS";
        assert_eq!(get("PATH").as_deref(), Some(path));
        let tmp = ctx.session_tmp.to_string_lossy().into_owned();
        for name in ["TEMP", "TMP", "TMPDIR", "CLAUDE_CODE_TMPDIR"] {
            assert_eq!(get(name).as_deref(), Some(tmp.as_str()), "{name}");
        }
        assert_eq!(get("TMPPREFIX"), None);
        assert_eq!(
            get("CLAUDE_CONFIG_DIR"),
            Some(ctx.profile.to_string_lossy().into_owned())
        );
        assert_eq!(get("HOME").as_deref(), Some(r"C:\Users\Jane O'Neil"));
        assert_eq!(get("SystemRoot").as_deref(), Some(r"C:\WINDOWS"));
        // The settings repeat the bridge's values, `;` and pipe names included.
        let value = settings_of(&spec);
        assert_eq!(value["env"]["PATH"], path);
        assert_eq!(
            value["env"]["CONNECTO_HOOK_SOCK"],
            r"\\.\pipe\connecto-hook-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            value["env"]["CONNECTO_BRIDGE_SOCK"],
            r"\\.\pipe\connecto-bridge-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(value["env"]["CONNECTO_TERMINAL"], "claude");
        assert_eq!(value["env"]["CONNECTO_HOOK_TOKEN"], "token");
        // The command line is the same as on macOS but for the paths.
        let words = args(&spec);
        assert!(words.iter().any(|w| w == "--dangerously-skip-permissions"));
        assert!(
            words
                .windows(2)
                .any(|w| w == ["--setting-sources", "user,project,local"])
        );
        assert_eq!(spec.cwd, ctx.project);
        assert_eq!(spec.program, ctx.entry);
    }

    // Windows paths joined with `\`: only on Windows.
    #[cfg(windows)]
    #[test]
    fn a_windows_session_gets_every_flag_the_design_names() {
        let spec = Claude.launch(&crate::spec::tests::launch_ctx_windows());
        assert_eq!(
            args(&spec),
            [
                "--dangerously-skip-permissions",
                "--settings",
                r"C:\Users\Jane O'Neil\AppData\Roaming\connecto\run\r1\claude-settings.json",
                "--setting-sources",
                "user,project,local",
                "--strict-mcp-config",
                "--session-id",
                "11111111-2222-4333-8444-555555555555",
                "--append-system-prompt",
                "LAUNCH TEXT",
            ]
        );
        assert_eq!(
            spec.files[0].0.to_str(),
            Some(r"C:\Users\Jane O'Neil\AppData\Roaming\connecto\run\r1\claude-settings.json")
        );
        // The public API gives the Windows shapes on Windows.
        let value = settings_of(&spec);
        assert!(value["hooks"]["Stop"][0]["hooks"][0]["args"].is_array());
    }

    /// D-1: on Windows a project that still has the builtin status lines gets
    /// their PowerShell twins through `--settings`; macOS and a utility
    /// session do not, and a project's own status line is left alone.
    #[test]
    fn windows_runs_the_powershell_twins_of_the_builtin_status_lines() {
        let builtin = r#"{"statusLine":{"type":"command","command":"bash \"${CLAUDE_PROJECT_DIR:-.}/.claude/scripts/statusline.sh\""},"subagentStatusLine":{"type":"command","command":"sh \"${CLAUDE_PROJECT_DIR:-.}/.claude/scripts/subagent-statusline.sh\""}}"#;
        let project = project_with(&[("settings.json", builtin)]);
        let scripts = project.join(".claude").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        for name in ["statusline.ps1", "subagent-statusline.ps1"] {
            std::fs::write(scripts.join(name), "").unwrap();
        }
        let mut ctx = crate::spec::tests::launch_ctx_windows();
        ctx.project = project.clone();
        let value = settings_of(&Claude::launch_as(Flavor::Windows, &ctx));
        for (key, name) in [
            ("statusLine", "statusline.ps1"),
            ("subagentStatusLine", "subagent-statusline.ps1"),
        ] {
            assert_eq!(
                value[key],
                json!({
                    "type": "command",
                    "command": format!(
                        "C:/WINDOWS/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File '{}'",
                        scripts.join(name).to_string_lossy()
                    ),
                }),
                "{key}"
            );
        }
        assert!(
            settings_of(&Claude::launch_as(Flavor::Unix, &ctx))
                .get("statusLine")
                .is_none()
        );
        ctx.utility = Some(crate::UtilityLaunch {
            model: "haiku".into(),
            effort: "low".into(),
        });
        assert!(
            settings_of(&Claude::launch_as(Flavor::Windows, &ctx))
                .get("statusLine")
                .is_none()
        );
        let own = project_with(&[(
            "settings.json",
            r#"{"statusLine":{"type":"command","command":"my own"}}"#,
        )]);
        let mut ctx = crate::spec::tests::launch_ctx_windows();
        ctx.project = own;
        assert!(
            settings_of(&Claude::launch_as(Flavor::Windows, &ctx))
                .get("statusLine")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn the_public_api_uses_the_hosts_flavor() {
        let ctx = launch_ctx();
        assert_eq!(Claude.launch(&ctx), Claude::launch_as(Flavor::HOST, &ctx));
    }

    fn project_with(files: &[(&str, &str)]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mg-conflicts-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        for (name, body) in files {
            std::fs::write(dir.join(".claude").join(name), body).unwrap();
        }
        dir
    }

    /// The two project settings that cannot be protected are found, from either
    /// file, and nothing else is flagged.
    #[test]
    fn project_settings_that_defeat_the_hooks_or_the_bypass_are_found() {
        let none = project_with(&[(
            "settings.json",
            r#"{"env":{"CONNECTO_BRIDGE_SOCK":"/x"},"disableAllHooks":false}"#,
        )]);
        assert_eq!(
            project_settings_conflicts(&none),
            ProjectSettingsConflicts::default()
        );
        let hooks = project_with(&[("settings.json", r#"{"disableAllHooks":true}"#)]);
        let found = project_settings_conflicts(&hooks);
        assert!(
            found
                .hooks_disabled
                .unwrap()
                .contains("settings.json sets disableAllHooks")
        );
        assert!(found.bypass_disabled.is_none());
        let bypass = project_with(&[(
            "settings.local.json",
            r#"{"permissions":{"disableBypassPermissionsMode":"disable"}}"#,
        )]);
        let found = project_settings_conflicts(&bypass);
        assert!(
            found
                .bypass_disabled
                .unwrap()
                .contains("settings.local.json")
        );
        assert!(found.hooks_disabled.is_none());
        // Unreadable or odd files are not a conflict.
        let odd = project_with(&[("settings.json", "not json")]);
        assert_eq!(
            project_settings_conflicts(&odd),
            ProjectSettingsConflicts::default()
        );
        assert_eq!(
            project_settings_conflicts(Path::new("/nonexistent")),
            ProjectSettingsConflicts::default()
        );
    }
}
