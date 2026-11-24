//! Claude Code. Checked against 2.1.318.

use std::ffi::OsString;

use std::path::{Path, PathBuf};

use connecto_core::{
    AgentMode, AuthState, BackgroundReport, EventText, Readiness, Reply, ToolEvent,
};
use serde_json::{Value, json};

use crate::platform::Flavor;
use crate::quote::shell_quote;
use crate::spec::{
    AdapterError, Capabilities, ComposerState, ComposerView, Continuation, DraftKeys, HookCtx,
    LaunchCtx, LaunchSpec, LogMark, LoggedTurn, LoginCtx, Row, Screen, ToolAdapter, UtilityAccess,
    UtilityTool, base_env, hook_env, prompt_matches_exact, under_profile, utility_hook_env,
    windows_var, with_bridge_path,
};
use crate::statusline::windows_statuslines;

/// How a Claude terminal reaches its model. The default is as usual: Claude
/// with its own sign-in.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ClaudeProviders {
    /// Through TogetherLink, with a Together AI key.
    pub together: Option<TogetherLink>,
}

/// What a Claude terminal leaves out of its context, from the project's Agent
/// Token Optimization (docs/specs/token-optimization.md §5.2). Facts from
/// 2.1.328: `enabledPlugins` with a plugin's id set to `false` and
/// `skillOverrides` with a skill set to `"off"`, given in Connecto's own
/// `--settings` (the "flag" source, above user, project and local), remove
/// them; `--autocompact <tokens>` (at least 100k) sets the compaction window
/// for one start. Empty, nothing changes: the settings file is the same bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCuts {
    /// Plugin ids (`<name>@synced`, `<name>@<marketplace>`).
    pub plugins_off: Vec<String>,
    /// Skill names.
    pub skills_off: Vec<String>,
    /// Tokens; `None` leaves Claude's default.
    pub autocompact: Option<u32>,
}

impl ClaudeCuts {
    /// The settings keys the cuts add (none when the lists are empty).
    fn settings(&self) -> serde_json::Map<String, Value> {
        let mut keys = serde_json::Map::new();
        if !self.plugins_off.is_empty() {
            let off = self
                .plugins_off
                .iter()
                .map(|id| (id.clone(), json!(false)))
                .collect();
            keys.insert("enabledPlugins".to_owned(), Value::Object(off));
        }
        if !self.skills_off.is_empty() {
            let off = self
                .skills_off
                .iter()
                .map(|name| (name.clone(), json!("off")))
                .collect();
            keys.insert("skillOverrides".to_owned(), Value::Object(off));
        }
        keys
    }
}

/// TogetherLink (`togetherlink claude <claude's arguments>`), Together AI's
/// launcher for Claude Code. Facts from TogetherLink 0.9.98:
///
/// - It finds `claude` on `PATH`, and runs it with its own environment plus
///   the gateway's variables (`ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`,
///   the model names). `CLAUDE_CONFIG_DIR`, `HOME` and Connecto's variables
///   reach Claude as they are. So the launch puts the folder of the copy
///   Connecto would have run first on `PATH`, after the bridge folder.
/// - It takes the key from `--api-key`, then its own saved one
///   (`togetherlink configure`), then `TOGETHER_API_KEY`. The key is given in
///   that variable, never on the command line, so no process list shows it.
/// - It refuses Claude's `--model` (its models are its own: `--main`), so a
///   model the terminal was added with is not passed.
/// - It reads `--settings` (a file or JSON) and gives Claude the same
///   settings, with its own added, so Connecto's hooks stay.
#[derive(Clone, PartialEq, Eq)]
pub struct TogetherLink {
    /// The `togetherlink` program.
    pub program: PathBuf,
    /// The Together AI key, for `TOGETHER_API_KEY`.
    pub api_key: String,
}

/// Never the key: a launch's description can end up in a log.
impl std::fmt::Debug for ClaudeProviders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeProviders")
            .field("together", &self.together)
            .finish()
    }
}

impl std::fmt::Debug for TogetherLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TogetherLink")
            .field("program", &self.program)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

/// The variable TogetherLink reads the Together AI key from.
pub const TOGETHER_API_KEY: &str = "TOGETHER_API_KEY";

/// The arguments for Claude, as TogetherLink passes them on: without
/// `--model` and its value, which it refuses.
fn without_model(args: Vec<OsString>) -> Vec<OsString> {
    let mut kept = Vec::with_capacity(args.len());
    let mut words = args.into_iter();
    while let Some(word) = words.next() {
        if word == "--" {
            // The prompt: passed whole.
            kept.push(word);
            kept.extend(words.by_ref());
            break;
        }
        if word == "--model" {
            words.next();
            continue;
        }
        kept.push(word);
    }
    kept
}

/// `PATH` with `dir` first (what is there already after it).
fn with_dir_first(flavor: Flavor, env: &mut Vec<(OsString, OsString)>, dir: &Path) {
    let is_path = |name: &OsString| match flavor {
        Flavor::Unix => name == "PATH",
        Flavor::Windows => name.to_string_lossy().eq_ignore_ascii_case("PATH"),
    };
    let existing = env
        .iter()
        .position(|(name, _)| is_path(name))
        .map(|index| env.remove(index).1);
    let mut path = dir.as_os_str().to_owned();
    if let Some(existing) = existing {
        path.push(flavor.path_separator());
        path.push(existing);
    }
    env.push(("PATH".into(), path));
}

const HOOKS: [&str; 5] = [
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "Notification",
];
/// `StopFailure` (fired instead of `Stop` when a turn ends in an API error)
/// exists from Claude Code 2.1.78. Before 2.1.101 an unknown hook event name
/// made Claude ignore the whole settings file, every Connecto hook with it, so
/// it is registered only for a version known to have it.
const STOP_FAILURE_SINCE: [u32; 3] = [2, 1, 78];
/// The first Claude whose command hooks take `"onFailure": "block"`: a hook
/// that fails or times out then blocks the prompt instead of letting it
/// through (Claude's hook docs; the field and its event list were checked
/// in the 2.1.328 binary, where `UserPromptSubmit` honours it).
const BLOCK_ON_FAILURE_SINCE: [u32; 3] = [2, 1, 295];

/// A version as the store probed it: `2.1.318`, `2.1.318 (Claude Code)`,
/// `2.1.79-beta.2`, `2.1.80+build.5`. The core numbers, and whether it is a
/// prerelease (which comes before its release). `None` when it cannot be read.
fn parse_version(text: &str) -> Option<([u32; 3], bool)> {
    let token = text.split_whitespace().next()?;
    let token = token.strip_prefix('v').unwrap_or(token);
    let (core, prerelease) = match token.find(['-', '+']) {
        Some(at) => (&token[..at], token[at..].starts_with('-')),
        None => (token, false),
    };
    let mut numbers = core.split('.').map(|n| n.parse::<u32>().ok());
    let version = [numbers.next()??, numbers.next()??, numbers.next()??];
    numbers.next().is_none().then_some((version, prerelease))
}

/// Whether `version` is known to have `StopFailure`: 2.1.78 or later; a
/// prerelease of 2.1.78 itself may not, so it does not count. An unknown or
/// unreadable version does not.
fn knows_stop_failure(version: Option<&str>) -> bool {
    match version.and_then(parse_version) {
        Some((core, prerelease)) => {
            core > STOP_FAILURE_SINCE || (core == STOP_FAILURE_SINCE && !prerelease)
        }
        None => false,
    }
}

/// Whether this session's prompt hook may hold a prompt for an operation
/// (ADR 0031 Intercept): a Claude known to block the prompt when the hook
/// fails or times out, so a block can never be dropped while the operation
/// runs, and a project with an operations folder at launch, so no other
/// project's prompts depend on the helper finishing. An unknown or
/// unreadable version does not.
fn prompt_hook_holds(version: Option<&str>, project: &Path) -> bool {
    let known = match version.and_then(parse_version) {
        Some((core, prerelease)) => {
            core > BLOCK_ON_FAILURE_SINCE || (core == BLOCK_ON_FAILURE_SINCE && !prerelease)
        }
        None => false,
    };
    known && project.join(".connecto").join("ops").is_dir()
}

/// The hook events to register for a CLI of `version`: all of them, but
/// `StopFailure` only where that version is known to have it.
fn hook_events(version: Option<&str>) -> Vec<&'static str> {
    let stop_failure = knows_stop_failure(version);
    HOOKS
        .iter()
        .copied()
        .filter(|name| stop_failure || *name != "StopFailure")
        .collect()
}

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
/// is printed instead of run (checked on 2.1.322, docs/windows-port.md 4.8.2).
/// The prompt hook's timeout, in seconds, set rather than left to Claude's
/// default: the helper's intercept steps (ADR 0031) are bounded to finish
/// well inside it (`GO_BY` in `connecto-hook`).
const PROMPT_HOOK_TIMEOUT_S: u64 = 60;

fn hook_entry(
    flavor: Flavor,
    hook_binary: &str,
    name: &str,
    bridge_hooks: bool,
    holds: bool,
) -> Value {
    let context = name == "UserPromptSubmit";
    let holds = holds && context && bridge_hooks;
    match flavor {
        Flavor::Unix => {
            let command = if bridge_hooks {
                let context = if context { " --context" } else { "" };
                let holds = if holds { " --holds" } else { "" };
                format!(
                    "{} claude --event {name}{context}{holds}",
                    shell_quote(hook_binary)
                )
            } else {
                format!("{} claude", shell_quote(hook_binary))
            };
            let mut entry = json!({ "type": "command", "command": command });
            if context && bridge_hooks {
                entry["timeout"] = json!(PROMPT_HOOK_TIMEOUT_S);
            }
            if holds {
                entry["onFailure"] = json!("block");
            }
            entry
        }
        Flavor::Windows => {
            let mut args = vec!["claude"];
            if bridge_hooks {
                args.extend(["--event", name]);
                if context {
                    args.push("--context");
                }
                if holds {
                    args.push("--holds");
                }
            }
            let mut entry = json!({ "type": "command", "command": hook_binary, "args": args });
            if context && bridge_hooks {
                entry["timeout"] = json!(PROMPT_HOOK_TIMEOUT_S);
            }
            if holds {
                entry["onFailure"] = json!("block");
            }
            entry
        }
    }
}

/// One hook per event, the bridge's environment, and `extra` (top-level keys
/// only Windows adds: the status lines).
fn settings(
    flavor: Flavor,
    hook_binary: &str,
    bridge_hooks: bool,
    holds: bool,
    events: &[&str],
    env: &[(String, String)],
    extra: serde_json::Map<String, Value>,
) -> Vec<u8> {
    let hooks: serde_json::Map<String, Value> = events
        .iter()
        .map(|name| {
            let hook = hook_entry(flavor, hook_binary, name, bridge_hooks, holds);
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

/// The background work a `Stop` lists. A task may be an object with an `id`
/// and, from 2.1.322, its `type`, `description` and (for a shell) `command`;
/// or a bare id.
fn background_tasks(value: &Value) -> Vec<BackgroundReport> {
    let optional = |task: &Value, field: &str| {
        task.get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| EventText::new(text.to_owned()))
    };
    value
        .get("background_tasks")
        .and_then(Value::as_array)
        .map(|tasks| {
            tasks
                .iter()
                .filter_map(|task| {
                    let id = task
                        .get("id")
                        .or(Some(task))
                        .and_then(Value::as_str)
                        .map(|id| EventText::new(id.to_owned()))?;
                    Some(BackgroundReport {
                        id,
                        kind: optional(task, "type"),
                        description: optional(task, "description"),
                        command: optional(task, "command"),
                    })
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
/// `❯` echo higher up does not count. Its text is the typed draft, or faint
/// text that is not: the placeholder `Try "…"` or a suggested next prompt,
/// drawn dimmed with Claude's own cursor (reverse video) on its first letter.
/// The rows between it and the box's bottom rule are the rest of a draft: any
/// such row with typed text makes the composer `Occupied`, so a multi-line
/// draft with an empty first line is not ready. The rows after the rule are
/// the footer and any status rows the user configured; they are not the draft.
/// Where the screen is known only as text (no faint cells), a line in the exact
/// shape of the placeholder reads as `Empty`. A menu or prompt answered by the
/// same `❯` line reads as `Occupied`.
pub fn claude_composer_state(screen: Screen<'_>) -> ComposerState {
    let rows: Vec<Row<'_>> = screen
        .rows()
        .into_iter()
        .filter(|row| !row.drawn.is_empty())
        .collect();
    let Some(at) = rows.iter().rposition(|row| row.drawn.starts_with('❯')) else {
        return ComposerState::Booting;
    };
    let typed = rows[at].typed.trim_start_matches('❯').trim();
    let placeholder = typed.starts_with("Try \"") && typed.ends_with('"');
    if !(typed.is_empty() || placeholder) {
        return ComposerState::Occupied;
    }
    let below = &rows[at + 1..];
    match below.iter().position(|row| is_rule(row.drawn)) {
        // The rows between the composer and the bottom rule are draft text,
        // unless all of it is faint (a long suggestion wrapped).
        Some(rule) if below[..rule].iter().any(|row| !row.typed.is_empty()) => {
            return ComposerState::Occupied;
        }
        Some(_) => {}
        // No rule: only footer and warning rows may follow the composer.
        None => {
            if below
                .iter()
                .any(|row| !is_footer_row(row.drawn) && !row.drawn.starts_with('⚠'))
            {
                return ComposerState::Occupied;
            }
        }
    }
    let below: Vec<&str> = below.iter().map(|row| row.drawn).collect();
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
            // A utility session: no bypass or permission-mode flag, nothing
            // from the project's own settings, no MCP server, and the model and
            // effort asked for. Minimal (titles) has no tool at all; ReadOnly
            // (memory) has only the tools that read and search files.
            args.extend([
                "--settings".into(),
                settings_path.clone().into(),
                "--setting-sources".into(),
                "user".into(),
                "--strict-mcp-config".into(),
                "--tools".into(),
                utility_tools(utility.access).into(),
            ]);
            if let Some(model) = &utility.model {
                args.extend(["--model".into(), model.into()]);
            }
            args.extend(["--effort".into(), utility.effort.clone().into()]);
        } else {
            // How the tool asks before it acts is the user's choice (setup).
            match ctx.agent_mode {
                AgentMode::Bypass => args.push("--dangerously-skip-permissions".into()),
                AgentMode::Auto => args.extend(["--permission-mode".into(), "auto".into()]),
                AgentMode::Prompt => {}
            }
            // No `--strict-mcp-config` here: with no `--mcp-config` it would
            // drop every MCP server the user added (the profile's
            // `.claude.json`, the project's `.mcp.json`, plugins). A terminal
            // loads them like a plain `claude` does, in the isolated profile
            // or the user's own; only utility sessions run without them.
            args.extend([
                "--settings".into(),
                settings_path.clone().into(),
                "--setting-sources".into(),
                SETTING_SOURCES.into(),
            ]);
            if let Some(effort) = &ctx.effort {
                args.extend(["--effort".into(), effort.into()]);
            }
            if let Some(model) = &ctx.model {
                args.extend(["--model".into(), model.into()]);
            }
            if let Some(tokens) = ctx.claude_cuts.autocompact {
                args.extend(["--autocompact".into(), tokens.to_string().into()]);
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
        if let (Some(text), None) = (&ctx.bridge.text, &ctx.utility) {
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
        if ctx.utility.is_none()
            && ctx.session_name.as_deref() == Some(connecto_core::session::APP_SESSION)
        {
            env.push(("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".into(), "50".into()));
        }
        // Through TogetherLink: a terminal, never a utility session.
        let together = ctx
            .claude
            .together
            .as_ref()
            .filter(|_| ctx.utility.is_none());
        if let Some(together) = together {
            // TogetherLink runs the `claude` it finds on `PATH`: the copy this
            // launch would have run comes first (after the bridge folder,
            // which holds no model CLI).
            if let Some(dir) = ctx.entry.parent() {
                with_dir_first(flavor, &mut env, dir);
            }
            env.push((TOGETHER_API_KEY.into(), together.api_key.clone().into()));
        }
        // A title keeps the environment it always had. Any other utility
        // session gets no bridge: no socket, no terminal id, no `connecto`.
        let bridgeless = ctx
            .utility
            .as_ref()
            .is_some_and(|u| u.access != UtilityAccess::Minimal);
        if bridgeless {
            env.extend(utility_hook_env(ctx));
        } else {
            env.extend(hook_env(ctx));
            with_bridge_path(flavor, &mut env, ctx);
        }
        #[cfg(feature = "test-endpoints")]
        if let Some(url) = &ctx.fake_model {
            env.push(("ANTHROPIC_BASE_URL".into(), url.into()));
            env.push(("ANTHROPIC_AUTH_TOKEN".into(), "fake-local-model".into()));
        }
        // Windows: the PowerShell twins of the builtin status lines (see
        // `statusline`). A utility session reads no project settings.
        let mut extra = match flavor {
            Flavor::Windows if ctx.utility.is_none() => windows_statuslines(
                &ctx.project,
                windows_var(&ctx.inherited, "SystemRoot").and_then(std::ffi::OsStr::to_str),
            ),
            _ => serde_json::Map::new(),
        };
        // The project's token cuts: a terminal only, never a utility session.
        if ctx.utility.is_none() {
            extra.extend(ctx.claude_cuts.settings());
        }
        let settings_file = settings(
            flavor,
            &ctx.hook_binary.to_string_lossy(),
            ctx.bridge.hooks,
            prompt_hook_holds(ctx.cli_version.as_deref(), &ctx.project),
            &hook_events(ctx.cli_version.as_deref()),
            &bridge_env_pairs(&env),
            extra,
        );
        let (program, args) = match together {
            Some(together) => {
                let mut wrapped: Vec<OsString> = vec!["claude".into()];
                wrapped.extend(without_model(args));
                (together.program.clone(), wrapped)
            }
            None => (ctx.entry.clone(), args),
        };
        LaunchSpec {
            program,
            args,
            cwd: ctx.project.clone(),
            env,
            files: vec![(settings_path, settings_file)],
        }
    }
}

/// The `--tools` value of a utility session. Read, Grep and Glob read and
/// search; no tool that edits, writes, runs a command or fetches is named.
const fn utility_tools(access: UtilityAccess) -> &'static str {
    match access {
        UtilityAccess::Minimal => "",
        UtilityAccess::ReadOnly => "Read,Grep,Glob",
    }
}

/// Whether Claude's composer is up and waiting for a prompt, for a utility
/// session. The footer under the input box names the mode: "? for shortcuts",
/// or "... (shift+tab to cycle)" when a permission mode is on.
#[must_use]
pub fn claude_utility_ready(screen: &str) -> bool {
    screen.contains("? for shortcuts") || screen.contains("shift+tab to cycle")
}

/// Claude's folder trust prompt is on `screen`.
fn claude_trust_screen(screen: &str) -> bool {
    screen.contains("Is this a project you created or one you trust")
        || screen.contains("trust this folder")
}

/// A Claude screen that waits for a person, named by what it is. These never
/// clear by themselves, and a hidden session never answers one.
#[must_use]
pub fn claude_blocked_reason(screen: &str) -> Option<String> {
    let has = |text: &str| screen.contains(text);
    if claude_trust_screen(screen) {
        Some("blocked at the folder trust prompt: open the session and answer it once".to_owned())
    } else if has("Select login method")
        || has("Please run /login")
        || has("Not logged in")
        || has("Invalid API key")
    {
        Some("Claude is not logged in for this project".to_owned())
    } else if has("Choose the text style") || has("Press Enter to continue") {
        Some("blocked at Claude's first-run setup: open the session and finish it once".to_owned())
    } else if has("Bypass Permissions mode") && has("Yes, I accept") {
        Some("blocked at the permissions warning: open the session and answer it".to_owned())
    } else {
        None
    }
}

/// The text of a transcript record's `message.content`: a string, or its text
/// blocks joined.
pub(crate) fn record_text(value: &Value) -> Option<String> {
    match value.pointer("/message/content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

/// A prompt without the wrapper Claude puts around a large paste:
/// `\n\n<pasted_content id="x">\n…\n</pasted_content id="x">\n` (2.1.325). Only
/// one whole outer wrapper with matching ids is removed; anything else is left
/// as it is, so it cannot match.
fn unwrap_paste(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("<pasted_content id=\"") else {
        return text;
    };
    let Some((id, inner)) = rest.split_once("\">") else {
        return text;
    };
    inner
        .strip_suffix(&format!("</pasted_content id=\"{id}\">"))
        .unwrap_or(text)
}

impl UtilityTool for Claude {
    fn ready(&self, screen: &str) -> bool {
        claude_utility_ready(screen)
    }

    fn blocked(&self, screen: &str) -> Option<String> {
        claude_blocked_reason(screen)
    }

    fn untrusted(&self, screen: &str) -> Option<String> {
        claude_trust_screen(screen).then(|| {
            "Claude has not been trusted for this project folder yet: open a Claude terminal in the project once and accept".to_owned()
        })
    }

    fn prompt_hook(&self) -> bool {
        true
    }

    fn composer(&self, screen: &str) -> ComposerState {
        claude_composer_state(Screen::plain(screen))
    }

    fn prompt_matches(&self, sent: &str, submitted: &str) -> bool {
        prompt_matches_exact(sent, unwrap_paste(submitted))
    }

    fn log_path(
        &self,
        reported: Option<&Path>,
        _thread: Option<&str>,
        ctx: &HookCtx,
    ) -> Option<PathBuf> {
        under_profile(reported?, &ctx.profile)
    }

    /// Measured on 2.1.325: a large paste is the user record's text inside a
    /// `<pasted_content>` wrapper; the reply is an assistant record with text
    /// blocks and `stop_reason`, written 1 to 120 ms after the `Stop` hook. An
    /// API failure is an assistant record flagged `isApiErrorMessage`.
    ///
    /// A turn that used tools has several assistant messages: commentary that
    /// ends in a `tool_use` stop, a `tool_result` user record, and so on. Only
    /// the last message is the response, and only an `end_turn` or
    /// `stop_sequence` stop ends the turn (`max_tokens` is a cut reply).
    fn read_turn(&self, log: &str, sent: &str) -> LoggedTurn {
        let mut turn = LoggedTurn::default();
        // The text blocks of the latest assistant message, and its id.
        let mut texts: Vec<String> = Vec::new();
        let mut message: Option<String> = None;
        for line in log.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let flag = |name: &str| value.get(name).and_then(Value::as_bool) == Some(true);
            if flag("isSidechain") {
                continue;
            }
            match value.get("type").and_then(Value::as_str) {
                Some("user") if !flag("isMeta") => {
                    // Any user record (a prompt or a tool result) starts a new
                    // step: what came before it is not the response.
                    texts.clear();
                    message = None;
                    turn.complete = false;
                    let tool_result = value
                        .pointer("/message/content/0/type")
                        .and_then(Value::as_str)
                        == Some("tool_result");
                    let Some(text) = record_text(&value).filter(|_| !tool_result) else {
                        continue;
                    };
                    let whole = self.prompt_matches(sent, &text);
                    turn.prompt_chars = turn.prompt_chars.max(text.chars().count());
                    if whole || !turn.prompt_whole {
                        turn.prompt_whole |= whole;
                        turn.error = None;
                    }
                }
                Some("assistant") => {
                    let id = value
                        .pointer("/message/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    if id.is_some() && message.is_some() && id != message {
                        texts.clear();
                    }
                    if id.is_some() {
                        message = id;
                    }
                    if let Some(text) = record_text(&value) {
                        if flag("isApiErrorMessage") {
                            turn.error = Some(text.clone());
                        }
                        texts.push(text);
                    }
                    turn.complete = ends_turn(&value);
                }
                _ => {}
            }
        }
        turn.reply = (!texts.is_empty()).then(|| texts.join("\n"));
        turn
    }

    fn reply_error(&self, reply: &str) -> Option<String> {
        let text = reply.trim_start();
        text.starts_with("API Error")
            .then(|| text.chars().take(200).collect())
    }
}

impl ToolAdapter for Claude {
    fn launch(&self, ctx: &LaunchCtx) -> LaunchSpec {
        Self::launch_as(Flavor::HOST, ctx)
    }

    fn utility(&self) -> Option<&dyn UtilityTool> {
        Some(self)
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
                    background: background_tasks(&value),
                    thread: None,
                    turn: None,
                    failure: None,
                    // Another `Stop` hook blocked the last `Stop`, and the
                    // same turn went on: this ends it again.
                    continued: value.get("stop_hook_active").and_then(Value::as_bool) == Some(true),
                }]
            }
            // Fired instead of `Stop` when the turn ends in an API error. Its
            // `last_assistant_message` is the error text, never a reply, and
            // it lists no background work.
            "StopFailure" => {
                let error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .filter(|error| !error.is_empty() && error.len() <= 64)
                    .unwrap_or("unknown");
                vec![ToolEvent::TurnEnded {
                    input: Vec::new(),
                    input_total: 0,
                    reply: Reply::None,
                    lifecycle_known: !incomplete,
                    background: Vec::new(),
                    thread: None,
                    turn: None,
                    failure: Some(EventText::new(error.to_owned())),
                    continued: false,
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
            let ends = TERMINAL_STOPS
                .iter()
                .any(|stop| line.contains(&format!(r#""stop_reason":"{stop}""#)));
            return if ends && !line.contains(r#""isSidechain":true"#) {
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
                if ends_turn(&value) {
                    LogMark::FinalReply
                } else if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                    LogMark::Other
                } else {
                    LogMark::Activity
                }
            }
            Some("user") => {
                let text = prompt_text(value.pointer("/message/content"));
                let sidechain = value.get("isSidechain").and_then(Value::as_bool) == Some(true);
                let meta = value.get("isMeta").and_then(Value::as_bool) == Some(true);
                let tool_result = value
                    .pointer("/message/content/0/type")
                    .and_then(Value::as_str)
                    == Some("tool_result");
                match text {
                    Some(text) if text.starts_with("[Request interrupted by user") => {
                        LogMark::Interrupt { turn: None }
                    }
                    Some(text) => LogMark::Prompt {
                        text: prompt_head(&text),
                    },
                    // A tool's result: the main conversation at work.
                    None if tool_result && !sidechain && !meta => LogMark::Activity,
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
                // A message with an image is logged as a list of blocks.
                prompt_text(value.pointer("/attachment/prompt")).map_or(LogMark::Other, |text| {
                    LogMark::Prompt {
                        text: prompt_head(&text),
                    }
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

    /// Shift+Enter (ESC CR) is a newline; Ctrl-C empties the draft or
    /// interrupts a turn; a long paste is shown as `[Pasted text #N …]`.
    fn draft_keys(&self) -> DraftKeys {
        DraftKeys {
            multiline: true,
            esc_enter_newline: true,
            ctrl_c_clears: false,
            collapses_pastes: true,
        }
    }

    fn composer(&self, screen: Screen<'_>) -> Option<ComposerView> {
        let state = claude_composer_state(screen);
        // Read from the box's geometry; what is typed on its first row,
        // whatever the state (a line shaped like the hint but not faint is
        // someone's text).
        let (first_line, rest) = claude_box(screen);
        // A box not read for certain: the first row as before (the last
        // row starting with the marker), for what only needs a guess.
        let first_line = match rest {
            crate::spec::BoxRows::Read(_) => first_line,
            _ if state == ComposerState::Occupied => composer_line(screen, '❯'),
            _ => String::new(),
        };
        Some(ComposerView {
            state,
            first_line,
            rest,
        })
    }
}

/// The typed text of the last line that starts with `glyph`, without it.
pub(crate) fn composer_line(screen: Screen<'_>, glyph: char) -> String {
    screen
        .rows()
        .into_iter()
        .rfind(|row| row.drawn.starts_with(glyph))
        .map(|row| row.typed.trim_start_matches(glyph).trim().to_owned())
        .unwrap_or_default()
}

/// Claude's input box, from its geometry: the rows between its two rules
/// (rows of `─` from the first column, nearly as wide as the screen, the
/// top one perhaps with a label in it), the first of them starting with `❯` at the first column,
/// the others indented. What is typed on its first row (without the marker,
/// nothing faint) and on the others. Any other layout (fewer than two
/// rules, the marker elsewhere, a row at the first column inside the box)
/// is ambiguous: typed text can look like a marker or a rule, but it never
/// sits where the box's own do.
pub(crate) fn claude_box(screen: Screen<'_>) -> (String, crate::spec::BoxRows) {
    use crate::spec::BoxRows;
    let rows = crate::spec::raw_rows(screen);
    let width = rows
        .iter()
        .map(|(d, _)| d.chars().count())
        .max()
        .unwrap_or(0);
    // A rule spans the screen from its first column: `─` only, or `─` on
    // both sides of one label (Claude names the session in its top rule).
    let rule = |line: &str| {
        let n = line.chars().count();
        let middle = line.trim_start_matches('─').trim_end_matches('─');
        n >= 20
            && n + 2 >= width
            && line.starts_with('─')
            && line.ends_with('─')
            && (middle.is_empty() || (middle.starts_with(' ') && middle.ends_with(' ')))
    };
    let rules: Vec<usize> = (0..rows.len()).filter(|&i| rule(rows[i].0)).collect();
    let [.., top, bottom] = rules[..] else {
        return (String::new(), BoxRows::Ambiguous);
    };
    let inside = &rows[top + 1..bottom];
    let Some(((first, first_typed), others)) = inside.split_first() else {
        return (String::new(), BoxRows::Ambiguous);
    };
    if !first.starts_with('❯')
        || others
            .iter()
            .any(|(d, _)| d.chars().next().is_some_and(|c| c != ' '))
    {
        return (String::new(), BoxRows::Ambiguous);
    }
    let first_line = first_typed
        .strip_prefix('❯')
        .unwrap_or(first_typed)
        .trim()
        .to_owned();
    let rest = others.iter().map(|(_, t)| t.trim().to_owned()).collect();
    (first_line, BoxRows::Read(rest))
}

/// The start of a prompt, enough to match a hook's prompt to its log record.
/// The `stop_reason`s that end a Claude turn. `end_turn` is a normal reply;
/// `stop_sequence` also ends one, and is what an API failure record
/// (`isApiErrorMessage`) carries. `tool_use` continues the turn, and
/// `max_tokens` is a cut reply the tool goes on from.
pub(crate) const TERMINAL_STOPS: [&str; 2] = ["end_turn", "stop_sequence"];

/// Whether an assistant record ends the main conversation's turn: one of
/// [`TERMINAL_STOPS`], and not a subagent's (sidechain) record. The one rule
/// for the turn log's end marks (`read_log`), the reply (`read_turn`) and past
/// conversations (`past_log`).
pub(crate) fn ends_turn(value: &Value) -> bool {
    let sidechain = value.get("isSidechain").and_then(Value::as_bool) == Some(true);
    !sidechain
        && value
            .pointer("/message/stop_reason")
            .and_then(Value::as_str)
            .is_some_and(|stop| TERMINAL_STOPS.contains(&stop))
}

/// A prompt's text: a string, or the text blocks of a list joined by
/// newlines. A list with no text block (a tool result) is no prompt.
fn prompt_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(blocks)) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

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
            assert_eq!(
                claude_composer_state(Screen::plain(&screen)),
                *want,
                "{file}"
            );
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
                claude_composer_state(Screen::plain(&idle)),
                ComposerState::Empty,
                "{placeholder}"
            );
        }
        let typed = format!("❯ hello\n──────\n{footer}");
        assert_eq!(
            claude_composer_state(Screen::plain(&typed)),
            ComposerState::Occupied
        );
        // No footer under the composer yet: still booting, not ready.
        assert_eq!(
            claude_composer_state(Screen::plain("❯ \n")),
            ComposerState::Booting
        );
    }

    #[test]
    fn a_faint_suggestion_in_the_composer_is_not_a_draft_and_typed_text_is() {
        let footer = "──────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n";
        // A suggested next prompt, drawn dimmed: any wording, not only `Try "…"`.
        let drawn = format!("❯ run the tests and fix what fails\n{footer}");
        let typed = format!("❯                                  \n{footer}");
        let screen = Screen {
            drawn: &drawn,
            typed: &typed,
        };
        assert_eq!(claude_composer_state(screen), ComposerState::Empty);
        assert_eq!(
            ToolAdapter::composer(&Claude, screen).unwrap().first_line,
            ""
        );
        // A long suggestion wrapped onto a second row above the rule.
        let drawn = format!("❯ run the tests and\n  fix what fails\n{footer}");
        let typed = format!("❯                  \n                \n{footer}");
        let wrapped = Screen {
            drawn: &drawn,
            typed: &typed,
        };
        assert_eq!(claude_composer_state(wrapped), ComposerState::Empty);
        // Typed text with the rest of a completion as ghost text after it.
        let drawn = format!("❯ /compact\n{footer}");
        let typed = format!("❯ /com    \n{footer}");
        let ghost = Screen {
            drawn: &drawn,
            typed: &typed,
        };
        assert_eq!(claude_composer_state(ghost), ComposerState::Occupied);
        assert_eq!(
            ToolAdapter::composer(&Claude, ghost).unwrap().first_line,
            "/com"
        );
        // Typed text in the placeholder's shape is still typed text, as
        // faint and plain cells are told apart; and so is any typed line.
        let typed = format!("❯ run the tests\n{footer}");
        assert_eq!(
            claude_composer_state(Screen::plain(&typed)),
            ComposerState::Occupied
        );
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

    /// The insights task finds the config folder the terminal really runs
    /// on: the isolated profile Connecto sets, or the person's ~/.claude
    /// with the system config. Evaluated by a shell, with the terminal's
    /// environment, as the task has the terminal do it.
    #[cfg(unix)]
    #[test]
    fn the_insights_task_finds_the_terminals_own_config_folder() {
        use connecto_core::cli_improvements::{CONFIG_DIR, insights_prompt};
        assert!(insights_prompt().contains(CONFIG_DIR));
        let folder = |spec: &LaunchSpec| {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s \"{CONFIG_DIR}\""))
                .env_clear()
                .envs(spec.env.iter().map(|(k, v)| (k, v)))
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        let mut ctx = launch_ctx();
        ctx.initial_prompt = Some(insights_prompt());
        let isolated = Claude.launch(&ctx);
        assert_eq!(folder(&isolated), "/profile", "the isolated profile");
        ctx.system_config = true;
        let system = Claude.launch(&ctx);
        let home = system
            .env
            .iter()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .unwrap();
        assert_eq!(
            folder(&system),
            format!("{home}/.claude"),
            "the person's own"
        );
    }

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
            assert!(
                !w.iter().any(|a| a == "--strict-mcp-config"),
                "MCP servers load"
            );
        }
        // A utility session has neither, whatever the choice.
        for mode in AgentMode::ALL {
            let mut ctx = launch_ctx();
            ctx.agent_mode = mode;
            ctx.utility = Some(crate::UtilityLaunch {
                model: Some("haiku".into()),
                effort: "low".into(),
                access: crate::UtilityAccess::Minimal,
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

    /// The insights terminal (Settings > CLI improvements): Claude on Opus,
    /// its task the last argument, after `--`.
    #[test]
    fn the_insights_terminal_runs_on_opus_with_its_task() {
        use connecto_core::cli_improvements::{INSIGHTS_MODEL, insights_prompt};
        let mut ctx = crate::spec::tests::launch_ctx();
        ctx.model = Some(INSIGHTS_MODEL.to_owned());
        ctx.initial_prompt = Some(insights_prompt());
        let words = args(&Claude.launch(&ctx));
        assert!(
            words.windows(2).any(|w| w == ["--model", "opus"]),
            "{words:?}"
        );
        assert_eq!(
            &words[words.len() - 2..],
            ["--".to_owned(), insights_prompt()]
        );
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
            model: Some("haiku".into()),
            effort: "low".into(),
            access: crate::UtilityAccess::Minimal,
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

    /// Project skills, agents, settings and MCP servers load (checked on
    /// 2.1.318), and what Connecto sets stays: its own `--settings` carries the
    /// hooks, and the bypass flag is a launch flag, which settings files do not
    /// outrank.
    #[test]
    fn project_settings_load_and_connectos_own_launch_flags_stay() {
        let words = args(&Claude.launch(&launch_ctx()));
        let at = words.iter().position(|a| a == "--setting-sources").unwrap();
        assert_eq!(words[at + 1], "user,project,local");
        for flag in ["--dangerously-skip-permissions", "--settings"] {
            assert!(words.iter().any(|w| w == flag), "{flag}");
        }
        assert!(
            !words.iter().any(|w| w == "--strict-mcp-config"),
            "the user's MCP servers are not dropped"
        );
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
                .map(|id| BackgroundReport::id_only(EventText::new((*id).to_owned())))
                .collect(),
            thread: None,
            turn: None,
            failure: None,
            continued: false,
        }
    }

    /// 2.1.322 lists each task with its type, description and, for a shell,
    /// its command. Each is kept; a bare id still counts.
    #[test]
    fn a_stop_keeps_each_tasks_type_description_and_command() {
        let parse = |json: &str| Claude.parse_hook(json.as_bytes(), &hook_ctx()).unwrap();
        let events = parse(
            r#"{"hook_event_name":"Stop","background_tasks":[
                {"id":"b1","type":"shell","status":"running","description":"Grep the log","command":"python3 grind.py big.log"},
                {"id":"a2","type":"subagent","status":"running","description":"Review the diff","agent_type":"general"},
                {"id":"m3","type":"monitor","description":""},
                "bare"
            ]}"#,
        );
        let background = match &events[..] {
            [ToolEvent::TurnEnded { background, .. }] => background.clone(),
            _ => Vec::new(),
        };
        assert_eq!(background.len(), 4, "{events:?}");
        let text = |text: &Option<EventText>| text.as_ref().map(|t| t.expose().clone());
        let seen: Vec<_> = background
            .iter()
            .map(|task| {
                (
                    task.id.expose().clone(),
                    text(&task.kind),
                    text(&task.description),
                    text(&task.command),
                )
            })
            .collect();
        let some = |text: &str| Some(text.to_owned());
        assert_eq!(
            seen,
            [
                (
                    "b1".to_owned(),
                    some("shell"),
                    some("Grep the log"),
                    some("python3 grind.py big.log")
                ),
                (
                    "a2".to_owned(),
                    some("subagent"),
                    some("Review the diff"),
                    None
                ),
                ("m3".to_owned(), some("monitor"), None, None),
                ("bare".to_owned(), None, None, None),
            ]
        );
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

    /// `StopFailure` (fired instead of `Stop` on an API error) is a turn end
    /// that failed: its error type, no reply, no background list. A `Stop`
    /// after a blocked one says so (`stop_hook_active`).
    #[test]
    fn stop_failure_is_a_failed_end_and_a_continued_stop_says_so() {
        let parse = |json: &str| Claude.parse_hook(json.as_bytes(), &hook_ctx()).unwrap();
        let failed = parse(
            r#"{"hook_event_name":"StopFailure","error":"rate_limit","error_details":"429 Too Many Requests","last_assistant_message":"API Error: Rate limit reached"}"#,
        );
        let [
            ToolEvent::TurnEnded {
                reply,
                failure,
                continued,
                lifecycle_known,
                ..
            },
        ] = failed.as_slice()
        else {
            unreachable!("not one turn end: {failed:?}");
        };
        assert_eq!(*reply, Reply::None, "the error text is never a reply");
        assert_eq!(
            failure.as_ref().map(|e| e.expose().as_str()),
            Some("rate_limit")
        );
        assert!(!continued && *lifecycle_known);
        let continued_stop = parse(
            r#"{"hook_event_name":"Stop","stop_hook_active":true,"last_assistant_message":"done"}"#,
        );
        assert!(matches!(
            continued_stop.as_slice(),
            [ToolEvent::TurnEnded {
                continued: true,
                failure: None,
                ..
            }]
        ));
        let first_stop = parse(r#"{"hook_event_name":"Stop","stop_hook_active":false}"#);
        assert!(matches!(
            first_stop.as_slice(),
            [ToolEvent::TurnEnded {
                continued: false,
                ..
            }]
        ));
    }

    /// `StopFailure` is registered only for a version known to have it
    /// (2.1.78 or later): before 2.1.101 an unknown hook name made Claude
    /// ignore the whole settings file, and with it every Connecto hook.
    #[test]
    fn stop_failure_is_registered_only_where_claude_knows_it() {
        let has = |version: Option<&str>| hook_events(version).contains(&"StopFailure");
        assert!(!has(Some("2.1.77")));
        assert!(!has(Some("2.0.9")));
        assert!(has(Some("2.1.78")));
        assert!(has(Some("2.1.318")));
        assert!(has(Some("2.1.318 (Claude Code)")));
        assert!(has(Some("3.0.0")));
        // A prerelease comes before its release: of 2.1.78 itself, not known.
        assert!(!has(Some("2.1.78-beta.1")));
        assert!(has(Some("2.1.79-beta.1")));
        assert!(has(Some("2.1.80+build.5")));
        // Unknown or unreadable: omitted, so an old CLI keeps every other hook.
        assert!(!has(None));
        assert!(!has(Some("")));
        assert!(!has(Some("latest")));
        assert!(!has(Some("2.1")));
        assert!(hook_events(None).contains(&"Stop"), "the rest stay");
        let mut ctx = launch_ctx();
        let value = |ctx: &LaunchCtx| -> Value {
            serde_json::from_slice(&Claude.launch(ctx).files[0].1).unwrap()
        };
        assert!(value(&ctx)["hooks"]["StopFailure"][0]["hooks"][0].is_object());
        ctx.cli_version = Some("2.1.77".to_owned());
        assert!(value(&ctx)["hooks"].get("StopFailure").is_none());
        ctx.cli_version = None;
        assert!(value(&ctx)["hooks"].get("StopFailure").is_none());
    }

    /// Main-conversation work is `Activity`: an assistant record that does not
    /// end the turn, and a tool result. A subagent's is not.
    #[test]
    fn assistant_work_and_tool_results_are_activity() {
        let read = |line: &str| Claude.read_log(line, true);
        assert_eq!(
            read(r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[]}}"#),
            LogMark::Activity
        );
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#
            ),
            LogMark::Activity
        );
        assert_eq!(
            read(
                r#"{"type":"assistant","isSidechain":true,"message":{"stop_reason":"tool_use","content":[]}}"#
            ),
            LogMark::Other
        );
        assert_eq!(
            read(r#"{"type":"system","subtype":"stop_hook_summary"}"#),
            LogMark::Other
        );
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
            LogMark::Activity
        );
        // Every terminal outcome `read_turn` accepts ends the turn here too:
        // an API failure ends with `stop_sequence`.
        for stop in TERMINAL_STOPS {
            let line = format!(
                r#"{{"type":"assistant","message":{{"stop_reason":"{stop}","content":[]}}}}"#
            );
            assert_eq!(read(&line), LogMark::FinalReply, "{stop}");
            assert_eq!(
                Claude.read_log(&line, false),
                LogMark::FinalReply,
                "{stop} cut"
            );
        }
        assert_eq!(
            read(
                r#"{"type":"assistant","isApiErrorMessage":true,"message":{"stop_reason":"stop_sequence","content":[{"type":"text","text":"API Error: 529"}]}}"#
            ),
            LogMark::FinalReply
        );
        for other in ["tool_use", "max_tokens"] {
            let line = format!(
                r#"{{"type":"assistant","message":{{"stop_reason":"{other}","content":[]}}}}"#
            );
            assert_eq!(read(&line), LogMark::Activity, "{other}");
        }
        // A subagent's end is not the main turn's.
        assert_eq!(
            read(
                r#"{"type":"assistant","isSidechain":true,"message":{"stop_reason":"end_turn","content":[]}}"#
            ),
            LogMark::Other
        );
        // A queued message with an image is logged as a list of blocks.
        assert_eq!(
            read(
                r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":[{"type":"text","text":"look at this"},{"type":"image","source":{"type":"base64","data":"AA"}}]}}"#
            ),
            LogMark::Prompt {
                text: "look at this".into()
            }
        );
        assert_eq!(
            read(
                r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":[{"type":"image","source":{}}]}}"#
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
            LogMark::Activity
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

    #[test]
    fn only_a_claude_that_blocks_on_failure_in_a_project_with_operations_holds_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        assert!(
            !prompt_hook_holds(Some("2.1.328"), project),
            "no ops folder"
        );
        std::fs::create_dir_all(project.join(".connecto/ops")).unwrap();
        assert!(prompt_hook_holds(Some("2.1.328"), project));
        assert!(prompt_hook_holds(Some("2.1.295 (Claude Code)"), project));
        assert!(!prompt_hook_holds(Some("2.1.295-beta.1"), project));
        assert!(!prompt_hook_holds(Some("2.1.294"), project));
        assert!(!prompt_hook_holds(None, project));
        let unix = hook_entry(Flavor::Unix, "/h", "UserPromptSubmit", true, true);
        assert_eq!(unix["onFailure"], "block");
        assert!(
            unix["command"]
                .as_str()
                .unwrap()
                .ends_with("--context --holds")
        );
        let windows = hook_entry(Flavor::Windows, "/h", "UserPromptSubmit", true, true);
        assert_eq!(windows["onFailure"], "block");
        assert_eq!(
            windows["args"].as_array().unwrap().last().unwrap(),
            "--holds"
        );
        // Never on another event, nor without the bridge's hooks.
        assert!(
            hook_entry(Flavor::Unix, "/h", "Stop", true, true)
                .get("onFailure")
                .is_none()
        );
        assert!(
            hook_entry(Flavor::Unix, "/h", "UserPromptSubmit", false, true)
                .get("onFailure")
                .is_none()
        );
        let plain = hook_entry(Flavor::Unix, "/h", "UserPromptSubmit", true, false);
        assert!(plain.get("onFailure").is_none());
        assert!(!plain["command"].as_str().unwrap().contains("--holds"));
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
                    let mut entry = json!({ "type": "command", "command": command });
                    // Added for project operations (ADR 0031).
                    if hooks && *name == "UserPromptSubmit" {
                        entry["timeout"] = json!(60);
                    }
                    ((*name).to_owned(), json!([{ "hooks": [entry] }]))
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
            let mut entry = json!({ "type": "command", "command": helper, "args": words.clone() });
            if hook == "UserPromptSubmit" {
                words.push("--context");
                entry =
                    json!({ "type": "command", "command": helper, "args": words, "timeout": 60 });
            }
            assert_eq!(
                value["hooks"][hook],
                json!([{ "hooks": [entry] }]),
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
            model: Some("haiku".into()),
            effort: "low".into(),
            access: crate::UtilityAccess::Minimal,
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

    fn utility_ctx(access: crate::UtilityAccess) -> LaunchCtx {
        let mut ctx = launch_ctx();
        ctx.bridge = crate::spec::BridgeLaunch {
            text: None,
            hooks: false,
            trust: None,
            terminal: String::new(),
            ..ctx.bridge
        };
        ctx.utility = Some(crate::UtilityLaunch {
            model: Some("haiku".into()),
            effort: "low".into(),
            access,
        });
        ctx
    }

    fn env_of(spec: &LaunchSpec, name: &str) -> Option<String> {
        spec.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string_lossy().into_owned())
    }

    /// Acceptance criterion 11, as redirected: a memory utility session can
    /// read and search the project and nothing else Claude lets a launch take
    /// away. Every option is on an allowlist, so adding any flag (a tool, a
    /// permission mode, an MCP config, a directory) fails here.
    #[test]
    fn a_memory_utility_launch_only_reads_and_has_no_bypass_and_no_bridge() {
        for mode in AgentMode::ALL {
            let mut ctx = utility_ctx(crate::UtilityAccess::ReadOnly);
            ctx.agent_mode = mode;
            let spec = Claude.launch(&ctx);
            let words = args(&spec);
            let allowed = [
                "--settings",
                "--setting-sources",
                "--strict-mcp-config",
                "--tools",
                "--model",
                "--effort",
                "--session-id",
            ];
            for word in words.iter().filter(|w| w.starts_with('-')) {
                assert!(allowed.contains(&word.as_str()), "{word} in {words:?}");
            }
            let at = words.iter().position(|w| w == "--tools").unwrap();
            let tools: Vec<&str> = words[at + 1].split(',').collect();
            assert_eq!(tools, ["Read", "Grep", "Glob"], "only reading tools");
            assert!(words.windows(2).any(|w| w == ["--setting-sources", "user"]));
            assert!(!words.iter().any(|w| w == "--append-system-prompt"));
            assert_eq!(spec.cwd, ctx.project, "it works in the project");
            let settings: Value = serde_json::from_slice(&spec.files[0].1).unwrap();
            assert!(settings.get("permissions").is_none(), "{settings}");
            assert!(settings.get("mcpServers").is_none(), "{settings}");
            assert!(settings["env"].get("CONNECTO_BRIDGE_SOCK").is_none());
            assert!(settings["env"].get("CONNECTO_TERMINAL").is_none());
            for name in ["CONNECTO_BRIDGE_SOCK", "CONNECTO_TERMINAL"] {
                assert_eq!(env_of(&spec, name), None, "{name}");
            }
            assert!(
                !env_of(&spec, "PATH").unwrap().contains("/data/cli/bin"),
                "no `connecto` command on PATH"
            );
            // The turn hooks still report to Connecto.
            assert!(env_of(&spec, "CONNECTO_HOOK_SOCK").is_some());
        }
    }

    /// Titles keep today's launch, word for word.
    #[test]
    fn a_title_utility_launch_is_unchanged() {
        let ctx = utility_ctx(crate::UtilityAccess::Minimal);
        let spec = Claude.launch(&ctx);
        // Joined as the platform joins it: `/run\claude-settings.json` on Windows.
        let settings = ctx.run.join(SETTINGS_FILE).to_string_lossy().into_owned();
        assert_eq!(
            args(&spec),
            [
                "--settings",
                settings.as_str(),
                "--setting-sources",
                "user",
                "--strict-mcp-config",
                "--tools",
                "",
                "--model",
                "haiku",
                "--effort",
                "low",
                "--session-id",
                "11111111-2222-4333-8444-555555555555",
            ]
        );
        assert_eq!(spec.cwd, ctx.project);
        assert!(env_of(&spec, "CONNECTO_BRIDGE_SOCK").is_some(), "as before");
        // No model named: the CLI's own default.
        let mut ctx = ctx;
        if let Some(u) = ctx.utility.as_mut() {
            u.model = None;
        }
        assert!(!args(&Claude.launch(&ctx)).iter().any(|w| w == "--model"));
    }

    #[test]
    fn the_utility_surface_reads_screens_and_the_transcript() {
        let tool = Claude.utility().unwrap();
        assert!(tool.ready("  ? for shortcuts"));
        assert!(tool.ready("⏵⏵ auto mode on (shift+tab to cycle)"));
        assert!(!tool.ready("Press Enter to continue"));
        assert!(tool.prompt_hook());
        assert!(
            tool.blocked("Quick safety check: Is this a project you created or one you trust?")
                .unwrap()
                .contains("trust")
        );
        assert_eq!(tool.blocked("? for shortcuts"), None);
        assert!(
            tool.untrusted("Quick safety check: Is this a project you created or one you trust?")
                .unwrap()
                .starts_with("Claude has not been trusted")
        );
        assert_eq!(tool.untrusted("? for shortcuts"), None);
        assert!(
            tool.reply_error("API Error: Can't reach the API server (ENOTFOUND)")
                .is_some()
        );
        assert_eq!(tool.reply_error("[]"), None);
        let ctx = HookCtx {
            profile: "/p".into(),
            hooks: false,
        };
        assert_eq!(
            tool.log_path(Some(Path::new("/p/projects/x/s.jsonl")), None, &ctx),
            Some(PathBuf::from("/p/projects/x/s.jsonl"))
        );
        assert_eq!(
            tool.log_path(Some(Path::new("/etc/x.jsonl")), None, &ctx),
            None
        );
        assert_eq!(tool.log_path(None, Some("abc"), &ctx), None);

        // A long paste arrives inside Claude's wrapper; the reply follows it.
        let sent = format!("{}\nFINAL WORD", "line of filler\n".repeat(20_000));
        let user = json!({"type":"user","message":{"role":"user","content":
            format!("\n\n<pasted_content id=\"ab12\">\n{sent}\n</pasted_content id=\"ab12\">\n")}});
        let meta = json!({"type":"user","isMeta":true,"message":{"content":"<system-reminder>x</system-reminder>"}});
        let thinking = json!({"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"}],"stop_reason":null}});
        let reply = json!({"type":"assistant","message":{"content":[{"type":"text","text":"[{\"a\":1}]"}],"stop_reason":"end_turn"}});
        let log = format!("{meta}\n{user}\n{thinking}\n{reply}\n");
        let turn = tool.read_turn(&log, &sent);
        assert!(turn.prompt_whole);
        assert_eq!(turn.reply.as_deref(), Some("[{\"a\":1}]"));
        assert!(turn.complete);
        assert_eq!(turn.error, None);
        // The reply not written yet: the prompt is there, no reply.
        let early = tool.read_turn(&format!("{user}\n{thinking}\n"), &sent);
        assert!(early.prompt_whole && early.reply.is_none() && !early.complete);
        // A prompt cut short is not whole.
        let cut = json!({"type":"user","message":{"content": &sent[..sent.len() / 2]}});
        let turn = tool.read_turn(&format!("{cut}\n{reply}\n"), &sent);
        assert!(!turn.prompt_whole);
        assert!(turn.prompt_chars < sent.chars().count());
        // An API failure is flagged as one.
        let failed = json!({"type":"assistant","isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"API Error: Can't reach the API server"}],"stop_reason":"stop_sequence"}});
        let turn = tool.read_turn(&format!("{user}\n{failed}\n"), &sent);
        assert!(turn.error.unwrap().starts_with("API Error"));
        // A turn that used a tool: the commentary before the tool call is not
        // the response, only the final message is.
        let short = "Read Cargo.toml and reply [] if nothing is wrong.";
        let ask = json!({"type":"user","message":{"content": short}});
        let step = json!({"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"I will read the file."},{"type":"tool_use","id":"t1","name":"Read","input":{}}],"stop_reason":"tool_use"}});
        let result = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"[package]"}]}});
        let last = json!({"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"[]"}],"stop_reason":"end_turn"}});
        let turn = tool.read_turn(&format!("{ask}\n{step}\n{result}\n{last}\n"), short);
        assert!(turn.prompt_whole);
        assert_eq!(turn.reply.as_deref(), Some("[]"));
        assert!(turn.complete);
        // Stopped at the tool call (the rest not written yet): not complete.
        let turn = tool.read_turn(&format!("{ask}\n{step}\n"), short);
        assert!(!turn.complete, "a tool_use stop does not end the turn");
        let turn = tool.read_turn(&format!("{ask}\n{step}\n{result}\n"), short);
        assert_eq!(turn.reply, None);
        // A reply cut at the token limit is not a complete one.
        let cut_reply = json!({"type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"[{\"a\""}],"stop_reason":"max_tokens"}});
        assert!(
            !tool
                .read_turn(&format!("{ask}\n{cut_reply}\n"), short)
                .complete
        );
    }

    /// Nothing may be missing from a memory prompt, not even 400 characters
    /// of 10,000 with the ending intact (the titles' looser check allows it).
    #[test]
    fn a_prompt_must_arrive_exactly_with_only_the_paste_wrapper_and_whitespace_removed() {
        let tool = Claude.utility().unwrap();
        let sent: String = (0..2000).map(|n| format!("w{n:04}\t")).collect::<String>() + "END";
        assert!(sent.len() > 10_000);
        let missing = format!("{}{}", &sent[..4000], &sent[4400..]);
        assert!(
            crate::prompt_arrived_whole(&sent, &missing),
            "the legacy check"
        );
        assert!(!tool.prompt_matches(&sent, &missing));
        let wrapped = format!(
            "\n\n<pasted_content id=\"ab12\">\n{}\n</pasted_content id=\"ab12\">\n",
            sent.replace('\t', "    ")
        );
        assert!(tool.prompt_matches(&sent, &wrapped));
        assert!(tool.prompt_matches(&sent, &sent));
        let mismatched = wrapped.replace(
            "</pasted_content id=\"ab12\">",
            "</pasted_content id=\"zz\">",
        );
        assert!(!tool.prompt_matches(&sent, &mismatched));
        assert!(!tool.prompt_matches(&sent, &format!("{sent} extra")));
    }

    fn together(ctx: &mut LaunchCtx) {
        ctx.claude = ClaudeProviders {
            together: Some(TogetherLink {
                program: "/home/.local/bin/togetherlink".into(),
                api_key: "tgp_v1_test-key-value".to_owned(),
            }),
        };
    }

    /// Through TogetherLink: its program, `claude` and then exactly Claude's
    /// own arguments; the key only in the environment; the copy Connecto
    /// would have run first on `PATH` after the bridge folder; the profile
    /// as it would be.
    #[cfg(unix)]
    #[test]
    fn a_together_launch_wraps_the_same_claude_and_keeps_the_key_off_the_command_line() {
        for system in [false, true] {
            let mut ctx = crate::spec::tests::launch_ctx();
            ctx.system_config = system;
            ctx.entry = "/data/cli/claude/2.1.322/claude".into();
            let plain = Claude.launch(&ctx);
            together(&mut ctx);
            let spec = Claude.launch(&ctx);
            assert_eq!(spec.program, PathBuf::from("/home/.local/bin/togetherlink"));
            let words = args(&spec);
            assert_eq!(words[0], "claude");
            assert_eq!(words[1..], args(&plain)[..]);
            assert!(!words.iter().any(|w| w.contains("tgp_v1_test-key-value")));
            assert_eq!(
                env_of(&spec, "TOGETHER_API_KEY").as_deref(),
                Some("tgp_v1_test-key-value")
            );
            assert_eq!(env_of(&plain, "TOGETHER_API_KEY"), None);
            assert_eq!(
                env_of(&spec, "PATH").as_deref(),
                Some("/data/cli/bin:/data/cli/claude/2.1.322:/usr/bin")
            );
            assert_eq!(
                env_of(&spec, "CLAUDE_CONFIG_DIR"),
                env_of(&plain, "CLAUDE_CONFIG_DIR")
            );
            assert_eq!(env_of(&spec, "CLAUDE_CONFIG_DIR").is_some(), !system);
            for name in [
                "CONNECTO_HOOK_TOKEN",
                "CONNECTO_BRIDGE_SOCK",
                "HOME",
                "TMPDIR",
            ] {
                assert_eq!(env_of(&spec, name), env_of(&plain, name), "{name}");
            }
            // The settings carry the same hooks, the key nowhere.
            let settings = settings_of(&spec);
            assert_eq!(settings["hooks"], settings_of(&plain)["hooks"]);
            assert_eq!(
                settings["env"]["PATH"],
                "/data/cli/bin:/data/cli/claude/2.1.322:/usr/bin"
            );
            assert!(!String::from_utf8_lossy(&spec.files[0].1).contains("tgp_v1_test-key-value"));
        }
    }

    #[test]
    fn a_together_launch_leaves_out_the_model_togetherlink_refuses_but_not_the_prompt() {
        let mut ctx = crate::spec::tests::launch_ctx();
        ctx.model = Some("opus".to_owned());
        ctx.initial_prompt = Some("--model is a word here".to_owned());
        together(&mut ctx);
        let words = args(&Claude.launch(&ctx));
        assert!(!words.iter().any(|w| w == "--model" || w == "opus"));
        assert_eq!(&words[words.len() - 2..], ["--", "--model is a word here"]);
    }

    #[test]
    fn a_utility_session_never_goes_through_togetherlink() {
        let mut ctx = utility_ctx(crate::UtilityAccess::ReadOnly);
        together(&mut ctx);
        let spec = Claude.launch(&ctx);
        assert_eq!(spec.program, ctx.entry);
        assert_eq!(env_of(&spec, "TOGETHER_API_KEY"), None);
        assert!(args(&spec).windows(2).any(|w| w == ["--model", "haiku"]));
    }

    #[test]
    fn the_together_key_is_never_in_a_description_of_the_launch() {
        let mut ctx = crate::spec::tests::launch_ctx();
        together(&mut ctx);
        let described = format!("{:?}", ctx.claude);
        assert!(described.contains("togetherlink"));
        assert!(!described.contains("tgp_v1_test-key-value"));
    }

    fn cuts() -> ClaudeCuts {
        ClaudeCuts {
            plugins_off: vec![
                "product-management@synced".to_owned(),
                "product-management@knowledge-work-plugins".to_owned(),
            ],
            skills_off: vec!["dataviz".to_owned()],
            autocompact: Some(200_000),
        }
    }

    /// Token optimization §5.2: the cuts go into Connecto's own `--settings`
    /// file, `--autocompact` only when set.
    #[test]
    fn a_terminal_gets_the_projects_token_cuts() {
        let mut ctx = launch_ctx();
        ctx.claude_cuts = cuts();
        let spec = Claude.launch(&ctx);
        let value = settings_of(&spec);
        assert_eq!(value["enabledPlugins"]["product-management@synced"], false);
        assert_eq!(
            value["enabledPlugins"]["product-management@knowledge-work-plugins"],
            false
        );
        assert_eq!(value["skillOverrides"]["dataviz"], "off");
        // Connecto's own keys stay.
        assert_eq!(value["crossSessionInbound"], "refuse");
        assert!(value["hooks"].is_object());
        let words = args(&spec);
        let at = words.iter().position(|a| a == "--autocompact").unwrap();
        assert_eq!(words[at + 1], "200000");
        // Without compaction, no flag; with only compaction, no settings key.
        ctx.claude_cuts.autocompact = None;
        assert!(
            !args(&Claude.launch(&ctx))
                .iter()
                .any(|a| a == "--autocompact")
        );
        ctx.claude_cuts = ClaudeCuts {
            autocompact: Some(150_000),
            ..ClaudeCuts::default()
        };
        let spec = Claude.launch(&ctx);
        assert!(settings_of(&spec).get("enabledPlugins").is_none());
        assert!(settings_of(&spec).get("skillOverrides").is_none());
        assert!(args(&spec).iter().any(|a| a == "150000"));
    }

    /// With no cut, the settings file and the arguments are exactly what they
    /// were before the feature.
    #[test]
    fn no_cut_leaves_the_launch_byte_identical() {
        for base in [launch_ctx(), crate::spec::tests::launch_ctx_windows()] {
            let plain = Claude.launch(&base);
            let mut ctx = base.clone();
            ctx.claude_cuts = ClaudeCuts::default();
            let same = Claude.launch(&ctx);
            assert_eq!(plain.files, same.files);
            assert_eq!(plain.args, same.args);
            let value = settings_of(&plain);
            assert!(value.get("enabledPlugins").is_none());
            assert!(value.get("skillOverrides").is_none());
        }
    }

    /// A utility session never gets the cuts, whatever the project set.
    #[test]
    fn a_utility_session_never_gets_the_token_cuts() {
        for access in [
            crate::UtilityAccess::Minimal,
            crate::UtilityAccess::ReadOnly,
        ] {
            let plain = Claude.launch(&utility_ctx(access));
            let mut ctx = utility_ctx(access);
            ctx.claude_cuts = cuts();
            let spec = Claude.launch(&ctx);
            assert_eq!(plain.files, spec.files);
            assert_eq!(plain.args, spec.args);
            assert!(!args(&spec).iter().any(|a| a == "--autocompact"));
        }
    }

    /// Through TogetherLink the cuts reach Claude the same way.
    #[test]
    fn a_together_launch_keeps_the_token_cuts() {
        let mut ctx = launch_ctx();
        together(&mut ctx);
        ctx.claude_cuts = cuts();
        let spec = Claude.launch(&ctx);
        assert_eq!(settings_of(&spec)["skillOverrides"]["dataviz"], "off");
        assert!(args(&spec).iter().any(|a| a == "--autocompact"));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod composer_box_tests {
    use super::*;
    use crate::spec::BoxRows;

    const CLAUDE: &str = include_str!("../tests/fixtures/claude/repo_typed_draft.txt");
    const CODEX: &str = include_str!("../tests/fixtures/codex/draft_rename_alt.txt");

    fn claude(screen: &str) -> ComposerView {
        crate::spec::ToolAdapter::composer(&Claude, Screen::plain(screen)).unwrap()
    }

    fn codex(screen: &str) -> ComposerView {
        crate::spec::ToolAdapter::composer(&crate::codex::Codex, Screen::plain(screen)).unwrap()
    }

    /// Claude's box with `rows` in place of its draft row (Claude draws a
    /// no-break space after its marker).
    fn claude_with(rows: &str) -> String {
        CLAUDE.replace("❯\u{a0}Try a draft", rows)
    }

    #[test]
    fn claudes_box_is_read_between_its_rules() {
        let v = claude(CLAUDE);
        assert_eq!(v.first_line, "Try a draft");
        assert_eq!(v.rest, BoxRows::Read(Vec::new()));
        // A blank row inside, a row starting with `─` that is not the rule.
        let v = claude(&claude_with("❯\u{a0}yes\n\n  extra text"));
        assert_eq!(v.first_line, "yes");
        assert_eq!(
            v.rest,
            BoxRows::Read(vec![String::new(), "extra text".into()])
        );
        let v = claude(&claude_with("❯\u{a0}yes\n  ─ extra text"));
        assert_eq!(v.rest, BoxRows::Read(vec!["─ extra text".into()]));
    }

    // Regression: the box was read from the last row starting with the
    // marker, so a draft above an indented `❯` row was hidden.
    #[test]
    fn a_marker_typed_on_a_later_row_does_not_hide_the_draft() {
        let v = claude(&claude_with("❯\u{a0}saved draft\n  ❯ yes"));
        assert_eq!(v.first_line, "saved draft");
        assert_eq!(v.rest, BoxRows::Read(vec!["❯ yes".into()]));
        let v = codex(&CODEX.replace("› /rename x", "› saved draft\n  › yes"));
        assert_eq!(v.first_line, "saved draft");
        assert!(
            matches!(&v.rest, BoxRows::Read(r) if r.iter().any(|r| r == "› yes")),
            "{v:?}"
        );
    }

    // Regression: a typed rule ended the box, hiding the rows under it.
    #[test]
    fn a_typed_rule_is_not_the_boxs_rule() {
        let v = claude(&claude_with("❯\u{a0}yes\n  ────\n  extra text"));
        assert_eq!(v.first_line, "yes");
        assert_eq!(
            v.rest,
            BoxRows::Read(vec!["────".into(), "extra text".into()])
        );
        // A rule at the first column inside the box, however wide: no
        // longer a box read for certain.
        let wide = "─".repeat(130);
        let v = claude(&claude_with(&format!("❯\u{a0}yes\n{wide}\n  extra text")));
        assert_eq!(v.rest, BoxRows::Ambiguous);
        let v = claude(&claude_with("❯\u{a0}yes\n────\n  extra text"));
        assert_eq!(v.rest, BoxRows::Ambiguous);
        // No rules at all.
        assert_eq!(claude("❯ yes").rest, BoxRows::Ambiguous);
    }

    #[test]
    fn codexs_box_is_read_down_to_its_footer() {
        let v = codex(CODEX);
        assert_eq!(v.first_line, "/rename x");
        assert!(v.rest.all_empty(), "{v:?}");
        let v = codex(&CODEX.replace("› /rename x", "› yes\n\n  extra text"));
        assert!(
            matches!(&v.rest, BoxRows::Read(r) if r.iter().any(|r| r == "extra text")),
            "{v:?}"
        );
        // A row at the first column under the marker: ambiguous.
        let v = codex(&CODEX.replace("› /rename x", "› yes\nextra text"));
        assert_eq!(v.rest, BoxRows::Ambiguous);
    }

    // Regression: any row with ` · ` was taken for the footer, so a typed
    // `extra · note` right above it was not read as the box's.
    #[test]
    fn codexs_footer_is_its_status_row_under_a_blank_row() {
        for first in ["› ", "› yes"] {
            let screen = CODEX.replace("› /rename x\n\n", &format!("{first}\n  extra · note\n"));
            assert_eq!(codex(&screen).rest, BoxRows::Ambiguous, "{first:?}");
        }
        // Every footer the fixtures show is read.
        for fixture in [
            include_str!("../tests/fixtures/codex/fresh_default.txt"),
            include_str!("../tests/fixtures/codex/named_narrow_80.txt"),
            include_str!("../tests/fixtures/codex/footer_without_dot.txt"),
            include_str!("../tests/fixtures/codex/reported_error_screen.txt"),
            include_str!("../tests/fixtures/codex/after_interrupt_inline.txt"),
            include_str!("../tests/fixtures/codex/slash_popup_inline.txt"),
        ] {
            assert!(
                matches!(codex(fixture).rest, BoxRows::Read(_)),
                "{}",
                &fixture[fixture.len().saturating_sub(300)..]
            );
        }
    }
}
