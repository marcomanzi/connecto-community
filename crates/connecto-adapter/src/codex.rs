//! Codex. Checked against 0.166.0.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use connecto_core::{AgentMode, AuthState, EventText, Readiness, Reply, ToolEvent};
use serde_json::Value;

use crate::platform::Flavor;
use crate::quote::{QuoteError, powershell_quote, shell_quote};
use crate::spec::{
    AdapterError, Capabilities, ComposerState, ComposerView, Continuation, DraftKeys, HookCtx,
    LaunchCtx, LaunchSpec, LogMark, LoggedTurn, LoginCtx, Row, Screen, ToolAdapter, UtilityLaunch,
    UtilityTool, base_env, hook_env, prompt_matches_exact, under_profile, utility_hook_env,
    with_bridge_path,
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

fn find_rollout(dir: &Path, suffix: &str, depth: u8, budget: &mut usize) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth > 0
                && let Some(found) = find_rollout(&entry.path(), suffix, depth - 1, budget)
            {
                return Some(found);
            }
        } else if kind.is_file() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(suffix) {
                return Some(entry.path());
            }
        }
    }
    None
}

/// The rollout file of `thread_id` under the profile, if there is one.
pub(crate) fn rollout_of(profile: &Path, thread_id: &str) -> Option<PathBuf> {
    if !safe_thread_id(thread_id) {
        return None;
    }
    let mut budget = MAX_ROLLOUT_ENTRIES;
    find_rollout(
        &profile.join("sessions"),
        &format!("-{thread_id}.jsonl"),
        ROLLOUT_DEPTH,
        &mut budget,
    )
}

/// Hidden title threads have no rollout file, which is how they are told apart.
fn has_rollout(profile: &Path, thread_id: &str) -> bool {
    rollout_of(profile, thread_id).is_some()
}

/// The events Codex gets a hook for. `Stop` and `Interrupt` are not added:
/// `notify` stays the turn end, which keeps their unproven interplay small.
const HOOK_EVENTS: [&str; 2] = ["SessionStart", "UserPromptSubmit"];

/// Set to `1` when Connecto's `SessionStart` hook should hand Codex the
/// built-in package's recap, which is compiled into the helper. The helper
/// reads the same name (`connecto_hook::codex::RECAP_ENV`).
pub const RECAP_ENV: &str = "CONNECTO_CODEX_RECAP";

/// The bridge text, for Connecto's `SessionStart` hook to hand to Codex. The
/// helper reads the same name (`connecto_hook::codex::BRIDGE_TEXT_ENV`).
pub const BRIDGE_TEXT_ENV: &str = "CONNECTO_CODEX_BRIDGE_TEXT";

/// The command Codex runs for `event`. On macOS Codex runs it with `sh -c`;
/// on Windows with `pwsh.exe -NoProfile -Command` (or Windows PowerShell),
/// where a quoted path alone is a string, not a command, so `&` calls it
/// (checked on Codex 0.167.2, `docs/windows-port.md` 4.8.3).
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

/// The most tokens of the `SessionStart` hook's context Codex keeps. Its own
/// limit (about 2,500 tokens on 0.167.0) would cut the bridge text, which is
/// over 2,000 with the memory and operations sections, and the recap after
/// it: Codex keeps the start and the end and drops the middle. This leaves
/// room to grow. Codex 0.167.0 takes the key in this spelling only
/// (`additional_context_limit` is ignored); checked with
/// `tests/codex_real_bridge.rs`. Part of the hook definition, so of its
/// trust hash, which preflight reads for these same values.
const SESSION_START_CONTEXT_LIMIT: u32 = 20_000;

fn hook_config_as(flavor: Flavor, helper: &Path) -> Result<Vec<String>, QuoteError> {
    HOOK_EVENTS
        .iter()
        .map(|event| {
            let command = hook_command(flavor, &helper.to_string_lossy(), event)?;
            // Only `SessionStart` carries the launch's instructions; the
            // prompt hook's notices keep Codex's own limit.
            let limit = if *event == "SessionStart" {
                format!(",additionalContextLimit={SESSION_START_CONTEXT_LIMIT}")
            } else {
                String::new()
            };
            Ok(format!(
                "hooks.{event}=[{{hooks=[{{type=\"command\",command={}{limit}}}]}}]",
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
/// `Busy` when Codex is working. A placeholder Codex draws dimmed (its own, or
/// one of the example prompts it picks at random) is not typed text.
pub fn codex_composer_state(screen: Screen<'_>) -> ComposerState {
    let rows: Vec<Row<'_>> = screen
        .rows()
        .into_iter()
        .filter(|row| !row.drawn.is_empty())
        .collect();
    let Some(at) = rows.iter().rposition(|row| row.drawn.starts_with('›')) else {
        return ComposerState::Booting;
    };
    let typed = rows[at].typed.trim_start_matches('›').trim();
    let lines: Vec<&str> = rows.iter().map(|row| row.drawn).collect();
    if typed.is_empty() || typed == COMPOSER_PLACEHOLDER {
        // An empty composer.
    } else if COMPOSER_PLACEHOLDER.starts_with(typed) {
        // The placeholder is still being drawn.
        return ComposerState::Booting;
    } else {
        return ComposerState::Occupied;
    }
    let below = &lines[at + 1..];
    let (_, rest) = codex_box(screen);
    // Text typed on a later row of the box (its first row empty): a draft,
    // whatever the footer looks like.
    if matches!(&rest, crate::spec::BoxRows::Read(rows) if rows.iter().any(|r| !r.is_empty())) {
        return ComposerState::Occupied;
    }
    // Ready once its footer is drawn: rows of the footer's shape, or the
    // footer the box's geometry verifies (a status row naming the folder,
    // drawn without a `·` on some versions).
    let footer_drawn = !below.is_empty() && below.iter().all(|line| is_footer_line(line));
    if !footer_drawn && !matches!(rest, crate::spec::BoxRows::Read(_)) {
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

/// Codex's input box, from its geometry: its last row starting with `›` at
/// the first column (earlier prompts above are drawn the same way; the box
/// is the last), the rows under it indented, down to the footer block at
/// the screen's bottom (the footer's shape, after any blank rows). What is
/// typed on its first row (without the marker) and on the others. A row at
/// the first column under the marker, or no footer block, is ambiguous.
fn codex_box(screen: Screen<'_>) -> (String, crate::spec::BoxRows) {
    use crate::spec::BoxRows;
    let rows = crate::spec::raw_rows(screen);
    let Some(at) = rows
        .iter()
        .rposition(|(d, _)| *d == "›" || d.starts_with("› ") || d.starts_with("›\u{a0}"))
    else {
        return (String::new(), BoxRows::Ambiguous);
    };
    let below = &rows[at + 1..];
    // The footer, from the screen's bottom up: hint rows, then the one
    // status row (it names the folder), then the blank row that separates
    // it from the box. Anything else there: where the box ends is unknown.
    let mut end = below.len();
    while end > 0 && below[end - 1].0.trim().is_empty() {
        end -= 1;
    }
    while end > 0 && is_hint_row(below[end - 1].0) {
        end -= 1;
    }
    let Some(status) = end.checked_sub(1).filter(|&s| is_status_row(below[s].0)) else {
        return (String::new(), BoxRows::Ambiguous);
    };
    let Some(footer) = status
        .checked_sub(1)
        .filter(|&s| below[s].0.trim().is_empty())
    else {
        return (String::new(), BoxRows::Ambiguous);
    };
    let inside = &below[..footer];
    if inside
        .iter()
        .any(|(d, _)| d.chars().next().is_some_and(|c| c != ' '))
    {
        return (String::new(), BoxRows::Ambiguous);
    }
    let typed = rows[at].1;
    let first_line = typed.strip_prefix('›').unwrap_or(typed).trim().to_owned();
    let rest = inside.iter().map(|(_, t)| t.trim().to_owned()).collect();
    (first_line, BoxRows::Read(rest))
}

/// Codex's status row: the model, then the folder it works in (`~/…` or
/// `/…`), perhaps the thread's name after it.
fn is_status_row(line: &str) -> bool {
    line.split([' ', '·']).any(|word| {
        word == "~" || word.starts_with("~/") || (word.starts_with('/') && word.len() > 1)
    })
}

/// A hint row under Codex's status row.
fn is_hint_row(line: &str) -> bool {
    line.contains("? for shortcuts") || line.contains("f2 to view")
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
        if let Some(utility) = &ctx.utility {
            return Self::utility_launch(flavor, ctx, utility, notify);
        }
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
        if let Some(effort) = &ctx.effort {
            args.extend([
                "-c".into(),
                format!("model_reasoning_effort={}", toml_string(effort)).into(),
            ]);
        }
        if let Some(model) = &ctx.model {
            args.push("-c".into());
            args.push(format!("model={}", toml_string(model)).into());
        }
        #[cfg(feature = "test-endpoints")]
        push_fake_model(ctx, &mut args);
        // Hooks are never registered without a trust state to go with them.
        // The trust state was read for these same values, so a helper path
        // they cannot be built for has none either.
        let mut hooks = false;
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
            hooks = true;
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
        // The bridge text and the package recap, through the `SessionStart`
        // hook: Codex adds the hook's context as a developer message of its
        // own, beside the configured `developer_instructions` and AGENTS.md,
        // when a thread starts, resumes, is cleared or compacted (ADR 0046).
        // `-c developer_instructions` is never used: it replaces the ones the
        // user configured. The bridge text goes in Codex's environment, which
        // the hook inherits; the recap is compiled into the helper and only
        // turned on. Without the hooks Codex has no channel that adds to its
        // instructions, and both are left out (the bridge is then off anyway:
        // preflight gives no text without hooks and their trust).
        if hooks {
            if let Some(text) = &ctx.bridge.text {
                env.push((BRIDGE_TEXT_ENV.into(), text.into()));
            }
            if ctx.package_text.is_some() {
                env.push((RECAP_ENV.into(), "1".into()));
            }
        }
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

    /// A utility session: Codex has no launch without tools (spike S2), so
    /// every utility access gets the narrowest one it has: the read-only
    /// sandbox, never an approval prompt, no MCP server, no web search, no
    /// bypass flag, no hooks and no bridge. The turn ends by `notify`; the
    /// prompt and reply are read from the rollout where `notify` lacks them.
    fn utility_launch(
        flavor: Flavor,
        ctx: &LaunchCtx,
        utility: &UtilityLaunch,
        notify: String,
    ) -> LaunchSpec {
        let mut args: Vec<OsString> = vec![
            "--no-daemon".into(),
            "--no-alt-screen".into(),
            "--sandbox".into(),
            "read-only".into(),
            "--ask-for-approval".into(),
            "never".into(),
            "-C".into(),
            ctx.project.clone().into(),
            "-c".into(),
            "check_for_update_on_startup=false".into(),
            "-c".into(),
            notify.into(),
            "-c".into(),
            "mcp_servers={}".into(),
            "-c".into(),
            "web_search=\"disabled\"".into(),
        ];
        if let Some(model) = &utility.model {
            args.push("-c".into());
            args.push(format!("model={}", toml_string(model)).into());
        }
        args.push("-c".into());
        args.push(
            format!(
                "model_reasoning_effort={}",
                toml_string(codex_effort(&utility.effort))
            )
            .into(),
        );
        #[cfg(feature = "test-endpoints")]
        push_fake_model(ctx, &mut args);
        let mut env = tool_env(
            flavor,
            &ctx.home,
            &ctx.session_tmp,
            &ctx.profile,
            ctx.system_config,
            &ctx.inherited,
        );
        env.extend(utility_hook_env(ctx));
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

/// Point Codex at the fake model endpoint of the end-to-end tests. The
/// default model offers the fake endpoint no tools at all.
#[cfg(feature = "test-endpoints")]
fn push_fake_model(ctx: &LaunchCtx, args: &mut Vec<OsString>) {
    if let Some(url) = &ctx.fake_model {
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
}

/// Codex's word for an effort: it has no `max`, its top is `xhigh`.
fn codex_effort(effort: &str) -> &str {
    if effort == "max" { "xhigh" } else { effort }
}

/// Codex's folder trust prompt is on `screen`.
fn codex_trust_screen(screen: &str) -> bool {
    screen.contains("trust the contents of this directory") || screen.contains("Do you trust")
}

/// A Codex screen that waits for a person, named by what it is.
fn codex_blocked_reason(screen: &str) -> Option<String> {
    let has = |text: &str| screen.contains(text);
    if codex_trust_screen(screen) {
        Some("blocked at Codex's folder trust prompt: open a Codex terminal in this project and answer it once".to_owned())
    } else if has("Sign in with ChatGPT") || has("Provide your own API key") {
        Some("Codex is not logged in for this project".to_owned())
    } else if has("Update available") && has("Skip") {
        Some("blocked at Codex's update prompt".to_owned())
    } else {
        None
    }
}

/// The text of a rollout `response_item` that is a user message: its
/// `input_text` blocks, joined.
pub(crate) fn user_item_text(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let texts: Vec<&str> = payload
        .get("content")?
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// Measured on 0.167.1: the prompt is a `response_item` user `message` (the
/// environment context before it is one too, and never matches); 0.166.0 also
/// wrote it as an `event_msg` `user_message`. The reply is `task_complete`'s
/// `last_agent_message`, which closes the turn. `agent_message` events before
/// it are commentary between tool calls, never the response. A failed turn
/// has an `error` event.
fn codex_read_turn(log: &str, sent: &str) -> LoggedTurn {
    let mut turn = LoggedTurn::default();
    let prompt = |turn: &mut LoggedTurn, message: &str| {
        let whole = prompt_matches_exact(sent, message);
        turn.prompt_chars = turn.prompt_chars.max(message.chars().count());
        turn.reply = None;
        turn.complete = false;
        if whole || !turn.prompt_whole {
            turn.prompt_whole |= whole;
            turn.error = None;
        }
    };
    for line in log.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(payload) = value.get("payload") else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("response_item") => {
                if let Some(message) = user_item_text(payload) {
                    prompt(&mut turn, &message);
                }
                continue;
            }
            Some("event_msg") => {}
            _ => continue,
        }
        let text = |field: &str| {
            payload
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        match payload.get("type").and_then(Value::as_str) {
            Some("user_message") => {
                // The same prompt again (0.166.0 writes both forms): it must
                // not clear what the first one proved.
                if let Some(message) = text("message")
                    && !(turn.prompt_whole && prompt_matches_exact(sent, &message))
                {
                    prompt(&mut turn, &message);
                }
            }
            Some("task_complete") => {
                turn.reply = text("last_agent_message");
                turn.complete = true;
            }
            Some("error") => {
                turn.error =
                    Some(text("message").unwrap_or_else(|| "Codex reported an error".to_owned()));
            }
            Some("turn_aborted") => {
                turn.error = Some("the turn was interrupted".to_owned());
            }
            _ => {}
        }
    }
    turn
}

impl UtilityTool for Codex {
    fn ready(&self, screen: &str) -> bool {
        codex_composer_state(Screen::plain(screen)) == ComposerState::Empty
    }

    fn blocked(&self, screen: &str) -> Option<String> {
        codex_blocked_reason(screen)
    }

    fn untrusted(&self, screen: &str) -> Option<String> {
        codex_trust_screen(screen).then(|| {
                "Codex has not been trusted for this project folder yet: open a Codex terminal in the project once and accept".to_owned()
            })
    }

    /// A utility session has no hooks (they need a trust read that only a
    /// bridged terminal makes), so only `notify` reports.
    fn prompt_hook(&self) -> bool {
        false
    }

    fn composer(&self, screen: &str) -> ComposerState {
        codex_composer_state(Screen::plain(screen))
    }

    /// Codex drops trailing whitespace (measured on 0.166.0), which the
    /// whitespace-blind comparison already allows.
    fn prompt_matches(&self, sent: &str, submitted: &str) -> bool {
        prompt_matches_exact(sent, submitted)
    }

    fn log_path(
        &self,
        reported: Option<&Path>,
        thread: Option<&str>,
        ctx: &HookCtx,
    ) -> Option<PathBuf> {
        if let Some(path) = reported.and_then(|path| under_profile(path, &ctx.profile)) {
            return Some(path);
        }
        rollout_of(&ctx.profile, thread?).and_then(|path| under_profile(&path, &ctx.profile))
    }

    fn read_turn(&self, log: &str, sent: &str) -> LoggedTurn {
        codex_read_turn(log, sent)
    }

    fn reply_error(&self, _reply: &str) -> Option<String> {
        None
    }
}

impl ToolAdapter for Codex {
    fn launch(&self, ctx: &LaunchCtx) -> LaunchSpec {
        Self::launch_as(Flavor::HOST, ctx)
    }

    fn utility(&self) -> Option<&dyn UtilityTool> {
        Some(self)
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
                failure: None,
                continued: false,
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
            failure: None,
            continued: false,
        }])
    }

    fn readiness(&self, screen: &str) -> Readiness {
        if codex_composer_state(Screen::plain(screen)) == ComposerState::Empty {
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

    /// Shift+Enter (ESC CR) is a newline; Ctrl-C empties the composer or
    /// interrupts a turn; a long paste is shown as `[Pasted Content N chars]`.
    fn draft_keys(&self) -> DraftKeys {
        DraftKeys {
            multiline: true,
            esc_enter_newline: true,
            ctrl_c_clears: false,
            collapses_pastes: true,
        }
    }

    fn composer(&self, screen: Screen<'_>) -> Option<ComposerView> {
        let state = codex_composer_state(screen);
        // Read from the box's geometry; what is typed on its first row,
        // whatever the state.
        let (first_line, rest) = codex_box(screen);
        // A box not read for certain: the first row as before, for what
        // only needs a guess.
        let first_line = match rest {
            crate::spec::BoxRows::Read(_) => first_line,
            _ if state == ComposerState::Occupied => crate::claude::composer_line(screen, '›'),
            _ => String::new(),
        };
        Some(ComposerView {
            state,
            first_line,
            rest,
        })
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
    use serde_json::json;

    fn has(spec: &LaunchSpec, name: &str) -> bool {
        spec.env.iter().any(|(key, _)| key == name)
    }

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

    /// A terminal leaves MCP to the user: the servers in the profile's
    /// `config.toml` (isolated or the user's own) load. Only utility sessions
    /// switch them off.
    #[test]
    fn a_terminal_launch_keeps_the_users_mcp_servers() {
        let words = args(&Codex.launch(&launch_ctx()));
        assert!(
            !words.iter().any(|w| w.starts_with("mcp_servers")),
            "{words:?}"
        );
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

    fn env_value(spec: &LaunchSpec, name: &str) -> Option<String> {
        spec.env
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.to_string_lossy().into_owned())
    }

    /// The recap reaches Codex through Connecto's `SessionStart` hook, which
    /// hands it over as a developer message of its own (checked on Codex
    /// 0.167 in a session rollout). The text is compiled into the helper; the
    /// launch only turns it on with [`RECAP_ENV`], so there is no file a
    /// session could rewrite. It never touches `developer_instructions`
    /// (which would replace the configured ones) nor the command line, and
    /// the hook command is the same, so its trust hash is too.
    #[test]
    fn the_recap_goes_through_the_session_start_hook_not_the_command_line() {
        let mut ctx = launch_ctx();
        let without = Codex.launch(&ctx);
        ctx.package_text = Some("RECAP".into());
        for bridge in [Some("LAUNCH TEXT".to_owned()), None] {
            ctx.bridge.text.clone_from(&bridge);
            let spec = Codex.launch(&ctx);
            assert_eq!(env_value(&spec, RECAP_ENV).as_deref(), Some("1"));
            assert!(spec.files.is_empty(), "no file: {:?}", spec.files);
            let words = args(&spec);
            assert!(!words.iter().any(|w| w.contains("RECAP")), "{words:?}");
            if bridge.is_some() {
                // The command line is the one it was without the recap.
                assert_eq!(words, args(&without));
            }
        }
        let hooks = hook_config_as(Flavor::Unix, Path::new("/h/connecto-hook")).unwrap();
        assert!(
            hooks[0].contains("codex-hook --event SessionStart\""),
            "{hooks:?}"
        );
    }

    /// Without Connecto's hooks there is no channel that adds to the
    /// configured instructions, so neither the recap nor the bridge text is
    /// passed at all (and never by `developer_instructions`, which would
    /// replace the user's); nor to a utility session.
    #[test]
    fn no_hooks_no_recap() {
        let mut ctx = launch_ctx();
        ctx.package_text = Some("RECAP".into());
        ctx.bridge.text = Some("LAUNCH TEXT".into());
        for (hooks, trust) in [(false, ctx.bridge.trust.clone()), (true, None)] {
            ctx.bridge.hooks = hooks;
            ctx.bridge.trust = trust;
            let spec = Codex.launch(&ctx);
            assert_eq!(env_value(&spec, RECAP_ENV), None);
            assert_eq!(env_value(&spec, BRIDGE_TEXT_ENV), None);
            assert!(spec.files.is_empty());
            let words = args(&spec);
            assert!(
                !words
                    .iter()
                    .any(|w| w.contains("RECAP") || w.contains("LAUNCH TEXT")),
                "{words:?}"
            );
        }
        let mut ctx = utility_ctx(crate::UtilityAccess::ReadOnly);
        ctx.package_text = Some("RECAP".into());
        ctx.bridge.text = Some("LAUNCH TEXT".into());
        let spec = Codex.launch(&ctx);
        assert_eq!(env_value(&spec, RECAP_ENV), None);
        assert_eq!(env_value(&spec, BRIDGE_TEXT_ENV), None);
    }

    /// The bridge text reaches the `SessionStart` hook whole, through Codex's
    /// environment, in a new and a resumed session: nothing goes into
    /// `developer_instructions` (which replaces the user's configured ones),
    /// and the command line is the one a launch without the text has.
    #[test]
    fn the_bridge_text_goes_through_the_session_start_hook_not_developer_instructions() {
        let text = "You are terminal \"codex-1\".\n`connecto list` 'q' \\ $HOME ünï";
        for resume in [None, Some("abc".to_owned())] {
            let mut ctx = launch_ctx();
            ctx.resume.clone_from(&resume);
            ctx.bridge.text = None;
            let without = args(&Codex.launch(&ctx));
            ctx.bridge.text = Some(text.to_owned());
            let spec = Codex.launch(&ctx);
            assert_eq!(env_value(&spec, BRIDGE_TEXT_ENV).as_deref(), Some(text));
            assert_eq!(args(&spec), without, "{resume:?}");
            assert!(
                !args(&spec)
                    .iter()
                    .any(|w| w.contains("developer_instructions")),
                "{resume:?}"
            );
            assert!(spec.files.is_empty());
        }
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
                failure: None,
                continued: false,
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
        // The launch text goes to the `SessionStart` hook, never into
        // `developer_instructions`, which would replace the user's own.
        assert_eq!(
            env_value(&spec, BRIDGE_TEXT_ENV).as_deref(),
            Some("LAUNCH TEXT")
        );
        assert!(
            !args.iter().any(|a| a.contains("developer_instructions")),
            "{args:?}"
        );
        let mut ctx = launch_ctx();
        ctx.bridge.trust = None;
        let spec = Codex.launch(&ctx);
        let args: Vec<_> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args.iter().any(|a| a.starts_with("hooks.")),
            "hooks without trust"
        );
        assert_eq!(env_value(&spec, BRIDGE_TEXT_ENV), None);
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

    // Regression: a footer drawn without a `·` was not recognised, so an
    // empty composer read as Booting and every voice write was refused. A
    // turn at work above it still reads Busy.
    #[test]
    fn a_footer_without_a_dot_is_verified_by_the_boxs_geometry() {
        let screen = include_str!("../tests/fixtures/codex/footer_without_dot.txt");
        assert_eq!(
            codex_composer_state(Screen::plain(screen)),
            ComposerState::Empty
        );
        let busy = screen.replace(
            "› Ask Codex to do anything",
            "• Working (4s • esc to interrupt)\n› Ask Codex to do anything",
        );
        assert_eq!(
            codex_composer_state(Screen::plain(&busy)),
            ComposerState::Busy
        );
        // Regression: a blank first row with a draft on the second read
        // Empty, so the relay could send into the draft.
        for fixture in [
            screen,
            include_str!("../tests/fixtures/codex/fresh_default.txt"),
        ] {
            let two_lines =
                fixture.replace("› Ask Codex to do anything\n", "›\n  existing draft\n");
            assert_ne!(two_lines, fixture);
            assert_eq!(
                codex_composer_state(Screen::plain(&two_lines)),
                ComposerState::Occupied
            );
            assert_eq!(
                Codex.readiness(&two_lines),
                Readiness::NotReady,
                "the relay does not send into it"
            );
        }
        // A typed footer-shaped row with no blank row under the box: still
        // not ready.
        let typed = screen.replace(
            "› Ask Codex to do anything\n\n",
            "› Ask Codex to do anything\n",
        );
        assert_eq!(
            codex_composer_state(Screen::plain(&typed)),
            ComposerState::Booting
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
        // A footer with no "·": its status row, verified by the box's
        // geometry (it names the folder, under a blank row), so ready.
        ("footer_without_dot.txt", ComposerState::Empty),
    ];

    #[test]
    fn captured_screens_read_as_their_composer_state() {
        for (file, want) in SCREENS {
            let path = format!("{}/tests/fixtures/codex/{file}", env!("CARGO_MANIFEST_DIR"));
            let screen = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                codex_composer_state(Screen::plain(&screen)),
                *want,
                "{file}"
            );
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
        assert_eq!(
            codex_composer_state(Screen::plain(idle)),
            ComposerState::Empty
        );
        // On the status row directly above the composer it is busy.
        let busy = "› explain it\n• Working (4s • esc to interrupt)\n› Ask Codex to do anything\n  GPT-6 · ~/p\n";
        assert_eq!(
            codex_composer_state(Screen::plain(busy)),
            ComposerState::Busy
        );
    }

    #[test]
    fn a_faint_example_prompt_in_the_composer_is_not_a_draft() {
        // Codex can show one of its example prompts, dimmed, in place of
        // "Ask Codex to do anything".
        let drawn = "› Explain this codebase\n\n  GPT-6 · ~/p\n";
        let typed = "›                      \n\n  GPT-6 · ~/p\n";
        let screen = Screen { drawn, typed };
        assert_eq!(codex_composer_state(screen), ComposerState::Empty);
        assert_eq!(
            ToolAdapter::composer(&Codex, screen).unwrap().first_line,
            ""
        );
        // The same words typed are a draft.
        assert_eq!(
            codex_composer_state(Screen::plain(drawn)),
            ComposerState::Occupied
        );
        assert_eq!(
            ToolAdapter::composer(&Codex, Screen::plain(drawn))
                .unwrap()
                .first_line,
            "Explain this codebase"
        );
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
                r#"hooks.SessionStart=[{hooks=[{type="command",command="'/data/cli/bin/connecto-hook' codex-hook --event SessionStart",additionalContextLimit=20000}]}]"#,
                r#"hooks.UserPromptSubmit=[{hooks=[{type="command",command="'/data/cli/bin/connecto-hook' codex-hook --event UserPromptSubmit --context"}]}]"#,
            ]
        );
    }

    /// The Windows shape, on every host: Codex runs the command with
    /// PowerShell, so the helper is called with `&` and quoted with single
    /// quotes only, a `'` written twice (docs/windows-port.md 4.8.3).
    #[test]
    fn windows_hook_commands_are_powershell_calls() {
        assert_eq!(
            hook_command(Flavor::Windows, WINDOWS_HELPER, "SessionStart").unwrap(),
            r"& 'C:\Users\Jane O''Neil\AppData\Roaming\connecto\cli\bin\connecto-hook.exe' codex-hook --event SessionStart"
        );
        assert_eq!(
            hook_config_as(Flavor::Windows, Path::new(WINDOWS_HELPER)).unwrap(),
            [
                r#"hooks.SessionStart=[{hooks=[{type="command",command="& 'C:\\Users\\Jane O''Neil\\AppData\\Roaming\\connecto\\cli\\bin\\connecto-hook.exe' codex-hook --event SessionStart",additionalContextLimit=20000}]}]"#,
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
    /// letter included (docs/windows-port.md 4.8.3).
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
            model: Some("gpt-mini".into()),
            effort: "max".into(),
            access,
        });
        ctx
    }

    /// Codex cannot start without tools (spike S2), so both utility accesses
    /// get its narrowest launch: the read-only sandbox and no approvals, with no
    /// bypass flag, no hooks, no launch text and no bridge.
    #[test]
    fn a_utility_launch_is_read_only_with_no_approvals_no_bypass_and_no_bridge() {
        for access in [
            crate::UtilityAccess::Minimal,
            crate::UtilityAccess::ReadOnly,
        ] {
            for mode in AgentMode::ALL {
                let mut ctx = utility_ctx(access);
                ctx.agent_mode = mode;
                ctx.bridge.hooks = true;
                ctx.bridge.text = Some("LAUNCH TEXT".into());
                ctx.initial_prompt = Some("first".into());
                let spec = Codex.launch(&ctx);
                let words = args(&spec);
                assert!(words.windows(2).any(|w| w == ["--sandbox", "read-only"]));
                assert!(
                    words
                        .windows(2)
                        .any(|w| w == ["--ask-for-approval", "never"])
                );
                for bad in [
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--approve-for-me",
                    "--yolo",
                    "--full-auto",
                    "--search",
                    "--dangerously-bypass-hook-trust",
                    "--",
                ] {
                    assert!(!words.iter().any(|w| w == bad), "{bad} in {words:?}");
                }
                for word in &words {
                    for key in [
                        "hooks.",
                        "developer_instructions",
                        "sandbox_mode",
                        "approval_policy",
                        "sandbox_permissions",
                        "profile=",
                    ] {
                        assert!(!word.starts_with(key), "{word}");
                    }
                }
                assert_eq!(
                    words.iter().filter(|w| *w == "--sandbox").count(),
                    1,
                    "nothing widens the sandbox"
                );
                assert!(words.iter().any(|w| w == "mcp_servers={}"));
                assert!(words.iter().any(|w| w == r#"model="gpt-mini""#));
                assert!(
                    words
                        .iter()
                        .any(|w| w == r#"model_reasoning_effort="xhigh""#)
                );
                assert!(words.iter().any(|w| w.starts_with("notify=")));
                assert_eq!(spec.cwd, ctx.project, "it works in the project");
                for name in ["CONNECTO_BRIDGE_SOCK", "CONNECTO_TERMINAL"] {
                    assert!(!has(&spec, name), "{name}");
                }
                let path = spec.env.iter().find(|(k, _)| k == "PATH").unwrap();
                assert!(!path.1.to_string_lossy().contains("/data/cli/bin"));
                assert!(has(&spec, "CONNECTO_HOOK_SOCK"));
            }
        }
        let mut ctx = utility_ctx(crate::UtilityAccess::Minimal);
        if let Some(u) = ctx.utility.as_mut() {
            u.model = None;
            u.effort = "low".into();
        }
        let words = args(&Codex.launch(&ctx));
        assert!(
            !words.iter().any(|w| w.starts_with("model=")),
            "the CLI's default"
        );
        assert!(words.iter().any(|w| w == r#"model_reasoning_effort="low""#));
    }

    #[test]
    fn the_utility_surface_finds_the_rollout_and_reads_the_turn() {
        let profile = Profile::new("utility");
        let tool = Codex.utility().unwrap();
        assert!(!tool.prompt_hook());
        let found = tool.log_path(None, Some(THREAD), &profile.ctx()).unwrap();
        assert!(
            found
                .to_string_lossy()
                .ends_with(&format!("{THREAD}.jsonl"))
        );
        assert_eq!(
            tool.log_path(None, Some(TITLE_THREAD), &profile.ctx()),
            None
        );
        assert_eq!(tool.log_path(None, Some("../x"), &profile.ctx()), None);
        assert_eq!(tool.log_path(None, None, &profile.ctx()), None);
        assert!(
            tool.blocked("> Do you trust the contents of this directory?")
                .is_some()
        );
        assert_eq!(tool.reply_error("API Error: x"), None);
        assert!(
            tool.untrusted("Do you trust the contents of this directory?")
                .unwrap()
                .starts_with("Codex has not been trusted")
        );
        assert_eq!(tool.untrusted("› Ask Codex to do anything"), None);

        let sent = format!("{}END", "x ".repeat(150_000));
        let ev = |payload: Value| {
            json!({"timestamp":"t","type":"event_msg","payload":payload}).to_string()
        };
        let log = [
            ev(json!({"type":"task_started","turn_id":"t1"})),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context/>"}]}}).to_string(),
            ev(json!({"type":"user_message","message": sent})),
            ev(json!({"type":"agent_message","message":"partial"})),
            ev(json!({"type":"task_complete","turn_id":"t1","last_agent_message":"[]"})),
        ]
        .join("\n");
        let turn = tool.read_turn(&log, &sent);
        assert!(turn.prompt_whole);
        assert_eq!(turn.reply.as_deref(), Some("[]"));
        assert!(turn.complete);
        let failed = [
            ev(json!({"type":"user_message","message": sent})),
            ev(json!({"type":"error","message":"stream disconnected"})),
        ]
        .join("\n");
        let turn = tool.read_turn(&failed, &sent);
        assert_eq!(turn.error.as_deref(), Some("stream disconnected"));
        assert!(turn.reply.is_none());
        // Commentary with no `task_complete` yet is neither the reply nor done.
        let unfinished = [
            ev(json!({"type":"user_message","message": sent})),
            ev(json!({"type":"agent_message","message":"I will read the file."})),
        ]
        .join("\n");
        let turn = tool.read_turn(&unfinished, &sent);
        assert!(turn.reply.is_none() && !turn.complete);
        // A prompt with characters missing is not the prompt.
        let short = format!("{}{}", &sent[..1000], &sent[1400..]);
        let cut = [ev(json!({"type":"user_message","message": short}))].join("\n");
        assert!(!tool.read_turn(&cut, &sent).prompt_whole);
        assert!(tool.prompt_matches("a  b\n", "a b"));
        // 0.167.1 writes the prompt only as a user `response_item`, after the
        // environment context, which is a user item too.
        let item = |text: &str| {
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}).to_string()
        };
        let current = [
            item("<environment_context>cwd</environment_context>"),
            item(&sent),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"[]"}]}}).to_string(),
            ev(json!({"type":"task_complete","turn_id":"t1","last_agent_message":"[]"})),
        ]
        .join("\n");
        let turn = tool.read_turn(&current, &sent);
        assert!(turn.prompt_whole, "{turn:?}");
        assert_eq!(turn.reply.as_deref(), Some("[]"));
        assert!(turn.complete);
    }
}
