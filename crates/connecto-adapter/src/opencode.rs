//! OpenCode (`opencode`, the v1 line its curl installer puts in
//! `~/.opencode/bin`). Checked against 1.18.8.
//!
//! OpenCode has no hook commands to configure. It loads plugins, and takes
//! extra configuration from `OPENCODE_CONFIG_CONTENT`, merged over the user's
//! own. So Connecto writes its plugin ([`PLUGIN`]) and the bridge
//! instructions into the terminal's run folder and names both there: nothing
//! is written into OpenCode's own folders, and the user's plugins still load.
//! The plugin runs Connecto's hook helper with the same payloads Claude
//! Code's hooks send, so everything after the helper (the frames, their size
//! limits, the turn tracker) is the path Claude already uses.
//!
//! OpenCode keeps its configuration, sign-ins and sessions in the XDG folders
//! (`~/.config/opencode`, `~/.local/share/opencode`, `~/.local/state/opencode`,
//! on macOS too). An isolated terminal points `XDG_CONFIG_HOME`,
//! `XDG_DATA_HOME` and `XDG_STATE_HOME` into its profile; the plugin gives the
//! agent's shell commands the user's own values back, so `git` and `gh` find
//! their settings.
//!
//! Its sessions live in a SQLite database, not a log Connecto can follow, so
//! the plugin reports every end itself, an interrupted turn as a failed one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use connecto_core::{AgentMode, AuthState, Readiness, ToolEvent};
use serde_json::{Map, Value, json};

use crate::claude::Claude;
use crate::platform::Flavor;
use crate::spec::{
    AdapterError, Capabilities, ComposerState, ComposerView, Continuation, DraftKeys, HookCtx,
    LaunchCtx, LaunchSpec, LogMark, LoginCtx, Screen, ToolAdapter, base_env, hook_env,
    with_bridge_path,
};

/// The plugin, without the `CONFIG` line the launch puts in front of it.
const PLUGIN: &str = include_str!("opencode_plugin.mjs");
/// The plugin's file in the run folder.
const PLUGIN_FILE: &str = "connecto-opencode.mjs";
/// The bridge instructions' file in the run folder.
const INSTRUCTIONS_FILE: &str = "connecto-opencode-instructions.md";
/// The plugin's report of the session OpenCode made for a prompt.
const SESSION_NAMED: &str = "SessionNamed";
/// The configuration OpenCode merges over its own files.
const CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";
/// The folders an isolated terminal moves into its profile, and the
/// subfolder of the profile each one becomes.
const XDG_DIRS: [(&str, &str); 3] = [
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_STATE_HOME", "state"),
];

/// Where OpenCode keeps what a terminal reads back (the favorite and recent
/// models, `model.json`): under the isolated profile, or the user's own:
/// `$XDG_STATE_HOME/opencode` when that is set (`xdg_state`), else
/// `~/.local/state/opencode`.
#[must_use]
pub fn opencode_state_dir(
    profile: &Path,
    home: &Path,
    system_config: bool,
    xdg_state: Option<&Path>,
) -> PathBuf {
    if system_config {
        xdg_state
            .map_or_else(|| home.join(".local").join("state"), Path::to_path_buf)
            .join("opencode")
    } else {
        profile.join("state").join("opencode")
    }
}

/// The models a user picked in OpenCode, for a new terminal's chooser: its
/// favorites, then its recent ones, as `provider/model`, from the text of
/// `model.json`. Only names a launch can pass are kept.
#[must_use]
pub fn opencode_models(model_json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(model_json) else {
        return Vec::new();
    };
    let mut found: Vec<String> = Vec::new();
    for list in ["favorite", "recent"] {
        let Some(entries) = value.get(list).and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            let provider = entry.get("providerID").and_then(Value::as_str);
            let model = entry.get("modelID").and_then(Value::as_str);
            if let (Some(provider), Some(model)) = (provider, model) {
                let name = format!("{provider}/{model}");
                if !found.contains(&name) {
                    found.push(name);
                }
            }
        }
    }
    found
}

/// A path as a `file:` URL, the way OpenCode's `plugin` list takes a local
/// file. OpenCode decodes it as a URL, so every byte outside the unreserved
/// ones and the separators is percent-encoded: a `#` would start a fragment,
/// a `?` a query, and a `%` an escape (a run folder can hold any of them).
fn file_url(flavor: Flavor, path: &Path) -> String {
    let text = path.to_string_lossy();
    let (lead, path) = match flavor {
        Flavor::Unix => ("file://", text.into_owned()),
        Flavor::Windows => ("file:///", text.replace('\\', "/")),
    };
    let mut url = String::from(lead);
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

/// The environment OpenCode runs in: its folders in the profile, unless the
/// project uses the user's own.
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
        env.retain(|(name, _)| !XDG_DIRS.iter().any(|(var, _)| name == *var));
        for (var, folder) in XDG_DIRS {
            env.push((var.into(), profile.join(folder).into()));
        }
    }
    env
}

/// What the agent's shell commands get back in an isolated terminal: the
/// user's own XDG values, or empty (which every XDG reader takes as unset).
fn shell_env(ctx: &LaunchCtx) -> Option<Value> {
    if ctx.system_config {
        return None;
    }
    let mut env = Map::new();
    for (var, _) in XDG_DIRS {
        let own = ctx
            .inherited
            .iter()
            .find(|(name, _)| name == var)
            .map(|(_, value)| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        env.insert(var.to_owned(), Value::String(own));
    }
    Some(Value::Object(env))
}

/// The OpenCode adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCode;

impl OpenCode {
    /// The plugin file's text for one launch.
    fn plugin(ctx: &LaunchCtx) -> String {
        let config = json!({
            "hook": ctx.hook_binary.to_string_lossy(),
            "inbox": ctx.bridge.hooks,
            "session": ctx.resume,
            "shellEnv": shell_env(ctx),
        });
        format!("const CONFIG = {config};\n{PLUGIN}")
    }

    /// [`ToolAdapter::launch`] for `flavor`'s system.
    fn launch_as(flavor: Flavor, ctx: &LaunchCtx) -> LaunchSpec {
        let plugin = ctx.run.join(PLUGIN_FILE);
        let mut files = vec![(plugin.clone(), Self::plugin(ctx).into_bytes())];
        let mut config = json!({ "plugin": [file_url(flavor, &plugin)] });
        // The bridge text and the package instructions: `instructions` adds to
        // the project's AGENTS.md (or CLAUDE.md), it does not replace it.
        if let Some(text) = ctx.launch_text() {
            let instructions = ctx.run.join(INSTRUCTIONS_FILE);
            files.push((instructions.clone(), text.into_bytes()));
            config["instructions"] = json!([instructions.to_string_lossy()]);
        }
        let mut args: Vec<OsString> = Vec::new();
        if let Some(id) = &ctx.resume {
            args.extend(["--session".into(), id.clone().into()]);
        }
        // OpenCode has no reviewing mode of its own: Auto is its prompts.
        if ctx.agent_mode == AgentMode::Bypass {
            args.push("--auto".into());
        }
        if let Some(model) = &ctx.model {
            args.extend(["--model".into(), model.clone().into()]);
        }
        let first = match &ctx.resume {
            Some(_) => ctx.resume_note.as_ref(),
            None => ctx.initial_prompt.as_ref(),
        };
        if let Some(message) = first {
            // In one word: a message may start with `-`.
            args.push(format!("--prompt={message}").into());
        }
        let mut env = tool_env(
            flavor,
            &ctx.home,
            &ctx.session_tmp,
            &ctx.profile,
            ctx.system_config,
            &ctx.inherited,
        );
        env.retain(|(name, _)| name != CONFIG_ENV);
        env.push((CONFIG_ENV.into(), config.to_string().into()));
        env.extend(hook_env(ctx));
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

impl ToolAdapter for OpenCode {
    fn launch(&self, ctx: &LaunchCtx) -> LaunchSpec {
        Self::launch_as(Flavor::HOST, ctx)
    }

    /// OpenCode signs in to each model provider on its own: `opencode
    /// providers login` here, or `/connect` inside a terminal. It also runs
    /// without any (OpenCode Zen's free models, or a key in the environment).
    fn login(&self, ctx: &LoginCtx) -> LaunchSpec {
        LaunchSpec {
            program: ctx.entry.clone(),
            args: vec!["providers".into(), "login".into()],
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

    /// Whether `opencode` runs at all: it prints its version.
    fn status(&self, ctx: &LoginCtx) -> LaunchSpec {
        LaunchSpec {
            args: vec!["--version".into()],
            cwd: ctx.session_tmp.clone(),
            ..self.login(ctx)
        }
    }

    /// An OpenCode that answers is ready to start. Which providers it is
    /// signed in to is not a login state: a terminal with none still has the
    /// free models, and `/connect` adds one.
    fn parse_status(&self, output: &str) -> AuthState {
        let answered = output.split_whitespace().any(|word| {
            let word = word.trim_start_matches('v');
            word.split('.').count() >= 3 && word.starts_with(|c: char| c.is_ascii_digit())
        });
        if answered {
            AuthState::LoggedIn {
                method: "providers".to_owned(),
            }
        } else {
            AuthState::Unknown
        }
    }

    /// The plugin sends Claude's hook payloads, with no background tasks.
    /// Its own `SessionNamed` (the session OpenCode made for a prompt) only
    /// names the session; a cut one names nothing.
    fn parse_hook(&self, payload: &[u8], ctx: &HookCtx) -> Result<Vec<ToolEvent>, AdapterError> {
        let value: Value = serde_json::from_slice(payload).map_err(|_| AdapterError::NotJson)?;
        if value.get("hook_event_name").and_then(Value::as_str) == Some(SESSION_NAMED) {
            if value.get("connecto_incomplete").and_then(Value::as_bool) == Some(true) {
                return Ok(Vec::new());
            }
            let id = value
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or(AdapterError::MissingField("session_id"))?;
            return Ok(vec![ToolEvent::SessionNamed {
                tool_session: connecto_core::EventText::new(id.to_owned()),
            }]);
        }
        Claude.parse_hook(payload, ctx)
    }

    /// OpenCode's ready signal is the plugin's `SessionStart`, not the screen.
    fn readiness(&self, _screen: &str) -> Readiness {
        Readiness::Unknown
    }

    /// Whitespace around a prompt is not counted as a difference.
    fn ack_form(&self, text: &str) -> String {
        text.trim().to_owned()
    }

    fn send_anyway_text(&self) -> &'static str {
        "OpenCode has not reported that it started. The message may be lost; it would then end Uncertain."
    }

    fn capabilities(&self, _version: &str) -> Capabilities {
        Capabilities {
            immediate_ack: true,
        }
    }

    /// No log is followed: the plugin reports every end.
    fn turn_log(&self, _reported: &Path, _ctx: &HookCtx) -> Option<PathBuf> {
        None
    }

    fn read_log(&self, _line: &str, _whole: bool) -> LogMark {
        LogMark::Other
    }

    /// OpenCode starts no turn by itself that Connecto could tell apart.
    fn continuation(&self, _prompt: &str) -> Option<Continuation> {
        None
    }

    /// Not measured: the prompt is a multi-line text area, and a long paste
    /// is taken to be shown as one placeholder. Nothing else is assumed, so
    /// any other key makes the draft unknown.
    fn draft_keys(&self) -> DraftKeys {
        DraftKeys {
            multiline: true,
            esc_enter_newline: false,
            ctrl_c_clears: false,
            collapses_pastes: true,
        }
    }

    fn composer(&self, screen: Screen<'_>) -> Option<ComposerView> {
        let state = opencode_composer_state(screen);
        let first_line = if state == ComposerState::Occupied {
            prompt_box(screen)
                .and_then(|(text, _)| text.into_iter().find(|line| !line.is_empty()))
                .unwrap_or_default()
        } else {
            String::new()
        };
        Some(ComposerView {
            state,
            first_line,
            rest: crate::spec::BoxRows::NotRead,
        })
    }
}

/// How the placeholder OpenCode draws in an empty prompt starts: `Ask
/// anything... "Fix a TODO in the codebase"`, a suggestion in quotes (1.18.8
/// writes three dots, 1.19.6 one `…`). It is drawn grey, not faint, so it is
/// told by its words and its place.
const PLACEHOLDERS: [&str; 2] = ["Ask anything... \"", "Ask anything… \""];

/// A row of OpenCode's logo, drawn above the prompt on its home screen only.
const LOGO_ROW: &str = "█▀▀█ █▀▀█";

/// Whether `line`, the prompt box's one row of text, is the placeholder:
/// on the home screen only (a conversation's empty prompt has none), and in
/// its whole shape, a quoted suggestion after `Ask anything... `. A draft
/// that merely starts with the same words is a draft.
fn is_placeholder(screen: Screen<'_>, text: &[String]) -> bool {
    let typed: Vec<&String> = text.iter().filter(|line| !line.is_empty()).collect();
    let [line] = typed.as_slice() else {
        return false;
    };
    PLACEHOLDERS
        .iter()
        .any(|start| line.starts_with(start) && line.len() > start.len())
        && line.ends_with('"')
        && screen.rows().iter().any(|row| row.drawn.contains(LOGO_ROW))
}

/// The prompt box's rows of text (without the `┃` edge), and whether a turn
/// is at work, or `None` when the box is not drawn.
///
/// Measured on 1.18.8: the box is a run of rows that start with `┃`, closed by
/// a row that starts with `╹▀`. Its last `┃` row names the agent and the model
/// (`Build · Mock 1`); the rows above it hold the prompt. While a turn works,
/// the row under the box says `esc interrupt`. A conversation's own messages
/// are drawn with the same edge, but never closed by `╹▀`, so the last closed
/// box is the prompt.
fn prompt_box(screen: Screen<'_>) -> Option<(Vec<String>, bool)> {
    let rows = screen.rows();
    let bottom = rows.iter().rposition(|row| row.drawn.starts_with("╹▀"))?;
    let mut top = bottom;
    while top > 0 && rows[top - 1].drawn.starts_with('┃') {
        top -= 1;
    }
    // At least one prompt row and the agent's row.
    if bottom - top < 2 {
        return None;
    }
    let text = rows[top..bottom - 1]
        .iter()
        .map(|row| {
            row.typed
                .strip_prefix('┃')
                .unwrap_or(row.typed)
                .trim()
                .to_owned()
        })
        .collect();
    let busy = rows[bottom + 1..]
        .iter()
        .take(2)
        .any(|row| row.drawn.contains("esc interrupt"));
    Some((text, busy))
}

/// What the live screen says about OpenCode's prompt ([`prompt_box`]): text
/// in it other than the placeholder is a draft, `esc interrupt` under it a
/// turn at work, and no box yet is a start.
#[must_use]
pub fn opencode_composer_state(screen: Screen<'_>) -> ComposerState {
    let Some((text, busy)) = prompt_box(screen) else {
        return ComposerState::Booting;
    };
    if text.iter().any(|line| !line.is_empty()) && !is_placeholder(screen, &text) {
        ComposerState::Occupied
    } else if busy {
        ComposerState::Busy
    } else {
        ComposerState::Empty
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use connecto_core::{EventText, Reply};

    use super::*;
    use crate::spec::tests::{launch_ctx, login_ctx};

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

    fn file(spec: &LaunchSpec, name: &str) -> String {
        spec.files
            .iter()
            .find(|(path, _)| path.ends_with(name))
            .map(|(_, body)| String::from_utf8(body.clone()).unwrap())
            .unwrap()
    }

    fn config(spec: &LaunchSpec) -> Value {
        serde_json::from_str(&env_of(spec, CONFIG_ENV).unwrap()).unwrap()
    }

    #[test]
    fn a_session_gets_the_plugin_the_instructions_a_private_profile_and_the_bypass() {
        let ctx = launch_ctx();
        let spec = OpenCode.launch(&ctx);
        let in_run = |name: &str| ctx.run.join(name).to_string_lossy().into_owned();
        assert_eq!(spec.program, PathBuf::from("/store/tool"));
        assert_eq!(spec.cwd, PathBuf::from("/project"));
        assert_eq!(args(&spec), ["--auto"]);
        let config = config(&spec);
        assert_eq!(
            config["plugin"],
            json!([file_url(Flavor::HOST, &ctx.run.join(PLUGIN_FILE))])
        );
        assert_eq!(config["instructions"], json!([in_run(INSTRUCTIONS_FILE)]));
        assert_eq!(file(&spec, INSTRUCTIONS_FILE), "LAUNCH TEXT");
        let plugin = file(&spec, PLUGIN_FILE);
        assert_eq!(
            plugin.lines().next().unwrap(),
            r#"const CONFIG = {"hook":"/data/cli/bin/connecto-hook","inbox":true,"session":null,"shellEnv":{"XDG_CONFIG_HOME":"","XDG_DATA_HOME":"","XDG_STATE_HOME":""}};"#
        );
        assert!(plugin.contains("export const ConnectoPlugin"));
        let profile = |folder: &str| {
            Path::new("/profile")
                .join(folder)
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(env_of(&spec, "XDG_CONFIG_HOME"), Some(profile("config")));
        assert_eq!(env_of(&spec, "XDG_DATA_HOME"), Some(profile("data")));
        assert_eq!(env_of(&spec, "XDG_STATE_HOME"), Some(profile("state")));
        assert_eq!(
            env_of(&spec, "CONNECTO_HOOK_TOKEN").as_deref(),
            Some("token")
        );
        let separator = Flavor::HOST.path_separator();
        assert_eq!(
            env_of(&spec, "PATH"),
            Some(format!("/data/cli/bin{separator}/usr/bin"))
        );
    }

    #[test]
    fn system_config_leaves_opencodes_folders_and_the_shell_alone() {
        let mut ctx = launch_ctx();
        ctx.system_config = true;
        let spec = OpenCode.launch(&ctx);
        for var in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"] {
            assert_eq!(env_of(&spec, var), None, "{var}");
        }
        assert!(file(&spec, PLUGIN_FILE).starts_with(
            r#"const CONFIG = {"hook":"/data/cli/bin/connecto-hook","inbox":true,"session":null,"shellEnv":null};"#
        ));
    }

    /// An isolated terminal hands the agent's commands the user's own XDG
    /// folders, and replaces any it inherited for OpenCode itself.
    #[test]
    fn an_inherited_xdg_folder_goes_back_to_the_shell_only() {
        let mut ctx = launch_ctx();
        ctx.inherited
            .push(("XDG_CONFIG_HOME".into(), "/home/.xdg".into()));
        let spec = OpenCode.launch(&ctx);
        let count = spec
            .env
            .iter()
            .filter(|(name, _)| name == "XDG_CONFIG_HOME")
            .count();
        assert_eq!(count, 1);
        assert_ne!(
            env_of(&spec, "XDG_CONFIG_HOME").as_deref(),
            Some("/home/.xdg")
        );
        assert!(file(&spec, PLUGIN_FILE).contains(r#""XDG_CONFIG_HOME":"/home/.xdg""#));
    }

    #[test]
    fn a_users_own_config_content_is_replaced_by_connectos() {
        let mut ctx = launch_ctx();
        ctx.inherited
            .push((CONFIG_ENV.into(), r#"{"plugin":["x"]}"#.into()));
        let spec = OpenCode.launch(&ctx);
        assert_eq!(spec.env.iter().filter(|(n, _)| n == CONFIG_ENV).count(), 1);
        assert_ne!(config(&spec)["plugin"], json!(["x"]));
    }

    #[test]
    fn each_mode_the_model_and_the_first_prompt_reach_the_launch() {
        let mut ctx = launch_ctx();
        ctx.agent_mode = AgentMode::Prompt;
        ctx.model = Some("openrouter/google/gemma-4-26b-a4b-it:free".to_owned());
        ctx.initial_prompt = Some("-first".to_owned());
        ctx.bridge.text = None;
        ctx.bridge.hooks = false;
        let spec = OpenCode.launch(&ctx);
        assert_eq!(
            args(&spec),
            [
                "--model",
                "openrouter/google/gemma-4-26b-a4b-it:free",
                "--prompt=-first"
            ]
        );
        assert_eq!(config(&spec).get("instructions"), None);
        assert!(file(&spec, PLUGIN_FILE).contains(r#""inbox":false"#));
        ctx.agent_mode = AgentMode::Auto;
        assert!(!args(&OpenCode.launch(&ctx)).contains(&"--auto".to_owned()));
    }

    #[test]
    fn a_resume_names_the_session_and_takes_the_note_not_the_first_prompt() {
        let mut ctx = launch_ctx();
        ctx.initial_prompt = Some("first".to_owned());
        ctx.resume_note = Some("note".to_owned());
        ctx.resume = Some("ses_abc123".to_owned());
        let spec = OpenCode.launch(&ctx);
        assert_eq!(
            args(&spec),
            ["--session", "ses_abc123", "--auto", "--prompt=note"]
        );
        assert!(file(&spec, PLUGIN_FILE).contains(r#""session":"ses_abc123""#));
    }

    #[test]
    fn a_windows_plugin_path_becomes_a_file_url() {
        assert_eq!(
            file_url(
                Flavor::Windows,
                Path::new(r"C:\Users\Jane O'Neil\run\p.mjs")
            ),
            "file:///C:/Users/Jane%20O%27Neil/run/p.mjs"
        );
        assert_eq!(
            file_url(Flavor::Unix, Path::new("/Users/a b/run/p.mjs")),
            "file:///Users/a%20b/run/p.mjs"
        );
        // What a URL would read as a fragment, a query or an escape.
        assert_eq!(
            file_url(Flavor::Unix, Path::new("/t/oc#1 ?x 100%/é.mjs")),
            "file:///t/oc%231%20%3Fx%20100%25/%C3%A9.mjs"
        );
    }

    #[test]
    fn login_is_opencodes_own_and_status_asks_the_version() {
        let spec = OpenCode.login(&login_ctx());
        assert_eq!(args(&spec), ["providers", "login"]);
        assert_eq!(spec.cwd, PathBuf::from("/run"));
        assert_eq!(
            env_of(&spec, "XDG_DATA_HOME"),
            Some(
                Path::new("/profile")
                    .join("data")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        let status = OpenCode.status(&login_ctx());
        assert_eq!(args(&status), ["--version"]);
        assert_eq!(status.cwd, PathBuf::from("/run/tmp"));
        assert_eq!(env_of(&status, "CONNECTO_HOOK_TOKEN"), None);
        assert!(matches!(
            OpenCode.parse_status("1.18.8\n"),
            AuthState::LoggedIn { .. }
        ));
        assert_eq!(OpenCode.parse_status(""), AuthState::Unknown);
    }

    fn hook_ctx() -> HookCtx {
        HookCtx {
            profile: "/profile".into(),
            hooks: true,
        }
    }

    #[test]
    fn the_plugins_payloads_become_events() {
        // Up before OpenCode has a session: ready, with no id to resume.
        let up = br#"{"hook_event_name":"SessionStart","session_id":""}"#;
        assert_eq!(
            OpenCode.parse_hook(up, &hook_ctx()).unwrap(),
            [ToolEvent::Started {
                tool_session: EventText::new(String::new()),
                turn_log: None,
            }]
        );
        // The session OpenCode made for a prompt: named, not started.
        let named = br#"{"hook_event_name":"SessionNamed","session_id":"ses_1"}"#;
        assert_eq!(
            OpenCode.parse_hook(named, &hook_ctx()).unwrap(),
            [ToolEvent::SessionNamed {
                tool_session: EventText::new("ses_1".to_owned()),
            }]
        );
        let cut = br#"{"hook_event_name":"SessionNamed","connecto_incomplete":true}"#;
        assert!(OpenCode.parse_hook(cut, &hook_ctx()).unwrap().is_empty());
        let prompt =
            br#"{"hook_event_name":"UserPromptSubmit","session_id":"ses_1","prompt":"hello"}"#;
        assert_eq!(
            OpenCode.parse_hook(prompt, &hook_ctx()).unwrap(),
            [ToolEvent::PromptSubmitted {
                prompt: EventText::new("hello".to_owned()),
                turn: None,
            }]
        );
        let stop =
            br#"{"hook_event_name":"Stop","session_id":"ses_1","last_assistant_message":"done"}"#;
        assert!(matches!(
            OpenCode.parse_hook(stop, &hook_ctx()).unwrap().as_slice(),
            [ToolEvent::TurnEnded {
                reply: Reply::Text(_),
                failure: None,
                ..
            }]
        ));
        let interrupted =
            br#"{"hook_event_name":"StopFailure","session_id":"ses_1","error":"interrupted"}"#;
        assert!(matches!(
            OpenCode
                .parse_hook(interrupted, &hook_ctx())
                .unwrap()
                .as_slice(),
            [ToolEvent::TurnEnded {
                reply: Reply::None,
                failure: Some(_),
                ..
            }]
        ));
        let asks = br#"{"hook_event_name":"Notification","notification_type":"permission_prompt"}"#;
        assert_eq!(
            OpenCode.parse_hook(asks, &hook_ctx()).unwrap(),
            [ToolEvent::NeedsUser]
        );
    }

    /// Screens as OpenCode 1.18.8 draws them (measured in a Connecto
    /// terminal, 120 columns), cut to the prompt box and what is under it.
    const HOME_IDLE: &str = "\
                                         █▀▀█ █▀▀█ █▀▀█ █▀▀▄ █▀▀▀ █▀▀█ █▀▀█ █▀▀█
                       ┃
                       ┃  Ask anything... \"Fix a TODO in the codebase\"
                       ┃
                       ┃  Build auto · Mock 1 Mock
                       ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
                       tab agents  ctrl+p commands

  ~/project                                                          1.18.8
";

    /// The same on 1.19.6: one `…`, and a line about OpenCode 2 under the logo.
    const HOME_IDLE_1_19: &str = "\
                                         █▀▀█ █▀▀█ █▀▀█ █▀▀▄ █▀▀▀ █▀▀█ █▀▀█ █▀▀█
                                           /update to upgrade to OpenCode 2.0
                       ┃
                       ┃  Ask anything… \"What is the tech stack of this project?\"
                       ┃
                       ┃  Build auto · Mock 1 Mock
                       ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
                                                       tab agents  ctrl+p commands
  ~/project                                                          1.19.6
";

    #[test]
    fn the_home_placeholder_of_either_version_is_an_empty_prompt() {
        let state = |text: &str| opencode_composer_state(Screen::plain(text));
        assert_eq!(state(HOME_IDLE), ComposerState::Empty);
        assert_eq!(state(HOME_IDLE_1_19), ComposerState::Empty);
        let draft = HOME_IDLE_1_19.replace(
            "Ask anything… \"What is the tech stack of this project?\"",
            "Ask anything… about this",
        );
        assert_eq!(state(&draft), ComposerState::Occupied);
    }

    fn session(prompt: &str, under: &str) -> String {
        format!(
            "  ┃\n  ┃  please SLOW\n  ┃\n\n     Hello from the mock model.\n\n     ▣  Build · Mock 1 · 6.6s\n\n  ┃\n  ┃  {prompt}\n  ┃\n  ┃  Build auto · Mock 1 Mock\n  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n{under}\n"
        )
    }

    #[test]
    fn the_last_closed_box_is_the_prompt_and_gives_the_state() {
        let state = |text: &str| opencode_composer_state(Screen::plain(text));
        assert_eq!(state(HOME_IDLE), ComposerState::Empty);
        assert_eq!(
            state(&HOME_IDLE.replace(
                "Ask anything... \"Fix a TODO in the codebase\"",
                "draft text"
            )),
            ComposerState::Occupied
        );
        let hints = "   ~/project                                   tab agents  ctrl+p commands";
        // In a conversation the empty prompt has no placeholder.
        assert_eq!(state(&session("", hints)), ComposerState::Empty);
        assert_eq!(
            state(&session("second draft", hints)),
            ComposerState::Occupied
        );
        assert_eq!(
            OpenCode
                .composer(Screen::plain(&session("second draft", hints)))
                .unwrap()
                .first_line,
            "second draft"
        );
        let working = "   ⬝⬝⬝⬝⬝⬝⬝⬝  esc interrupt                     tab agents  ctrl+p commands";
        assert_eq!(state(&session("", working)), ComposerState::Busy);
        // A draft typed while a turn works is still a draft.
        assert_eq!(state(&session("next", working)), ComposerState::Occupied);
    }

    /// A draft that starts with the placeholder's words is a draft: in a
    /// conversation (where the empty prompt has no placeholder), and on the
    /// home screen unless it has the placeholder's whole shape.
    #[test]
    fn a_draft_that_starts_like_the_placeholder_is_a_draft() {
        let state = |text: &str| opencode_composer_state(Screen::plain(text));
        let hints = "   ~/project                                   tab agents  ctrl+p commands";
        for draft in [
            "Ask anything about this change",
            "Ask anything... about this",
            "Ask anything... \"",
        ] {
            assert_eq!(
                state(&session(draft, hints)),
                ComposerState::Occupied,
                "{draft}"
            );
            let home = HOME_IDLE.replace("Ask anything... \"Fix a TODO in the codebase\"", draft);
            assert_eq!(state(&home), ComposerState::Occupied, "{draft}");
        }
        // The placeholder itself, in a conversation, is somebody's draft.
        assert_eq!(
            state(&session(
                "Ask anything... \"Fix a TODO in the codebase\"",
                hints
            )),
            ComposerState::Occupied
        );
        // And on the home screen, under the logo, it is the placeholder.
        assert_eq!(state(HOME_IDLE), ComposerState::Empty);
    }

    #[test]
    fn a_screen_without_the_closed_prompt_box_is_starting() {
        let state = |text: &str| opencode_composer_state(Screen::plain(text));
        assert_eq!(state(""), ComposerState::Booting);
        // The logo alone, or a conversation's message box, which is never
        // closed by `╹▀`.
        assert_eq!(
            state("█▀▀█ █▀▀█\n  ┃\n  ┃  say hi\n  ┃\n"),
            ComposerState::Booting
        );
        assert_eq!(state("  ╹▀▀▀▀▀▀▀▀\n"), ComposerState::Booting);
    }

    #[test]
    fn no_log_is_followed() {
        assert_eq!(
            OpenCode.turn_log(Path::new("/profile/data/x.jsonl"), &hook_ctx()),
            None
        );
    }

    #[test]
    fn the_chooser_offers_the_favorite_then_the_recent_models_once_each() {
        let text = r#"{"recent":[{"providerID":"lmstudio","modelID":"qwen/qwen3-coder-30b"},{"providerID":"openrouter","modelID":"z-ai/glm-5"}],"favorite":[{"providerID":"openrouter","modelID":"z-ai/glm-5"}],"variant":{}}"#;
        assert_eq!(
            opencode_models(text),
            ["openrouter/z-ai/glm-5", "lmstudio/qwen/qwen3-coder-30b"]
        );
        assert!(opencode_models("not json").is_empty());
        assert!(opencode_models("{}").is_empty());
    }

    #[test]
    fn the_state_folder_follows_the_config_mode() {
        let (profile, home) = (Path::new("/profile"), Path::new("/home"));
        let own = Path::new("/xdg/state");
        assert_eq!(
            opencode_state_dir(profile, home, false, Some(own)),
            profile.join("state").join("opencode")
        );
        assert_eq!(
            opencode_state_dir(profile, home, true, None),
            home.join(".local").join("state").join("opencode")
        );
        // The user's own XDG state folder, when they set one.
        assert_eq!(
            opencode_state_dir(profile, home, true, Some(own)),
            own.join("opencode")
        );
    }
}
