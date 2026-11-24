//! Pi (the `pi` coding agent). Checked against 1.5.2.
//!
//! Pi has no hooks of its own to configure. It loads extensions, so Connecto
//! writes one into the terminal's run folder ([`EXTENSION`]) and names it on
//! the command line. The extension runs Connecto's hook helper with the same
//! payloads Claude Code's hooks send, so everything after the helper (the
//! frames, their size limits, the turn tracker) is the path Claude already
//! uses.
//!
//! Pi talks to whichever model providers it has credentials for. Connecto
//! gives it those at launch, in the environment ([`PiProviders`]): nothing is
//! written into Pi's own `auth.json`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use connecto_core::{AgentMode, AuthState, Readiness, ToolEvent};
use serde_json::{Value, json};

use crate::claude::{Claude, prompt_head};
use crate::platform::Flavor;
use crate::spec::{
    AdapterError, Capabilities, ComposerState, ComposerView, Continuation, DraftKeys, HookCtx,
    LaunchCtx, LaunchSpec, LogMark, LoginCtx, Screen, ToolAdapter, base_env, hook_env,
    under_profile, with_bridge_path,
};

/// The extension, without the `CONFIG` line the launch puts in front of it.
const EXTENSION: &str = include_str!("pi_extension.mjs");
/// The extension's file in the run folder.
const EXTENSION_FILE: &str = "connecto-pi.mjs";

/// The model providers a Pi terminal is given at launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PiProviders {
    /// Variables Pi reads its credentials from (`OPENROUTER_API_KEY`,
    /// `TOGETHER_API_KEY`), with their values.
    pub env: Vec<(OsString, OsString)>,
    /// Base URL of a local LM Studio server (`http://localhost:1234`), whose
    /// models the extension offers under the provider `lmstudio`.
    pub lmstudio_url: Option<String>,
    /// The model a terminal starts on when none was chosen for it
    /// (`provider/model`). `None` leaves the choice to Pi.
    pub default_model: Option<String>,
}

/// The extension's file for a run of Pi outside a terminal (asking Pi for its
/// models): it offers the local server's models and reports nothing.
#[must_use]
pub fn pi_extension_for_listing(lmstudio_url: Option<&str>) -> String {
    let config = json!({ "hook": "", "inbox": false, "lmstudio": lmstudio_url, "silent": true });
    format!("const CONFIG = {config};\n{EXTENSION}")
}

/// Where Pi keeps its configuration and sessions, in the tool's environment.
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
        env.push(("PI_CODING_AGENT_DIR".into(), profile.into()));
    }
    // Connecto shows when an agent has a newer release; Pi need not ask.
    env.push(("PI_SKIP_VERSION_CHECK".into(), "1".into()));
    env
}

/// A rule of Pi's editor: a row of `─`, which may carry a label while Pi works
/// (`── ⠧ Working ───`).
fn is_rule(line: &str) -> bool {
    line.starts_with("──") && line.ends_with('─')
}

/// What the live screen says about Pi's editor. Measured on 1.5.2 in the
/// `regular` terminal mode Connecto launches.
///
/// The editor is what sits between the last two rules on the screen; under the
/// lower rule come a menu, if one is open, and the footer (the folder, then the
/// context use and the model). Text between the rules is a draft, or the filter
/// of an open menu: `Occupied`. An empty editor whose upper rule carries a label
/// is `Busy`. Without both rules and a footer line the editor is not drawn yet.
/// Faint text in the editor is not a draft.
pub fn pi_composer_state(screen: Screen<'_>) -> ComposerState {
    let rows = screen.rows();
    let lines: Vec<&str> = rows.iter().map(|row| row.drawn).collect();
    let Some(bottom) = lines.iter().rposition(|line| is_rule(line)) else {
        return ComposerState::Booting;
    };
    let Some(top) = lines[..bottom].iter().rposition(|line| is_rule(line)) else {
        return ComposerState::Booting;
    };
    if lines[bottom + 1..].iter().all(|line| line.is_empty()) {
        return ComposerState::Booting;
    }
    if rows[top + 1..bottom]
        .iter()
        .any(|row| !row.typed.is_empty())
    {
        return ComposerState::Occupied;
    }
    let labelled = lines[top]
        .chars()
        .any(|ch| ch != '─' && !ch.is_whitespace());
    if labelled {
        ComposerState::Busy
    } else {
        ComposerState::Empty
    }
}

/// The editor's first row of typed text.
fn editor_line(screen: Screen<'_>) -> String {
    let rows = screen.rows();
    let lines: Vec<&str> = rows.iter().map(|row| row.drawn).collect();
    let Some(bottom) = lines.iter().rposition(|line| is_rule(line)) else {
        return String::new();
    };
    let Some(top) = lines[..bottom].iter().rposition(|line| is_rule(line)) else {
        return String::new();
    };
    rows[top + 1..bottom]
        .iter()
        .find(|row| !row.typed.is_empty())
        .map(|row| row.typed.to_owned())
        .unwrap_or_default()
}

/// The Pi adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct Pi;

impl Pi {
    /// The extension file's text for one launch.
    fn extension(ctx: &LaunchCtx) -> String {
        let mut config = json!({
            "hook": ctx.hook_binary.to_string_lossy(),
            "inbox": ctx.bridge.hooks,
            "lmstudio": ctx.pi.lmstudio_url,
        });
        // The bridge text and the package recap: the extension adds them as
        // sections of the prompt Pi resolved itself, so the project's or
        // profile's APPEND_SYSTEM.md stays (`--append-system-prompt` would
        // replace it; ADR 0046). JSON in the file, so the text needs no other
        // escaping and has no command-line length limit.
        if let Some(bridge) = &ctx.bridge.text {
            config["bridge"] = json!(bridge);
        }
        if let Some(recap) = &ctx.package_text {
            config["recap"] = json!(recap);
        }
        format!("const CONFIG = {config};\n{EXTENSION}")
    }

    /// [`ToolAdapter::launch`] for `flavor`'s system.
    fn launch_as(flavor: Flavor, ctx: &LaunchCtx) -> LaunchSpec {
        let extension = ctx.run.join(EXTENSION_FILE);
        let files = vec![(extension.clone(), Self::extension(ctx).into_bytes())];
        // The main screen, not the alternate one: Connecto keeps the
        // scrollback, as it does for the other agents.
        let mut args: Vec<OsString> = vec!["--tui-mode".into(), "regular".into()];
        match &ctx.resume {
            Some(id) => args.extend(["--session".into(), id.clone().into()]),
            None => args.extend(["--session-id".into(), ctx.session_id.clone().into()]),
        }
        args.extend(["--extension".into(), extension.into()]);
        // Pi never asks before a tool call. What it does ask is whether the
        // project's own `.pi` resources may load: full power says yes.
        if ctx.agent_mode == AgentMode::Bypass {
            args.push("--approve".into());
        }
        if let Some(effort) = &ctx.effort {
            args.extend(["--thinking".into(), effort.into()]);
        }
        if let Some(model) = ctx.model.as_ref().or(ctx.pi.default_model.as_ref()) {
            args.extend(["--model".into(), model.clone().into()]);
        }
        if let Some(name) = &ctx.session_name {
            args.extend(["--name".into(), name.clone().into()]);
        }
        // No `--append-system-prompt`: it replaces the APPEND_SYSTEM.md Pi
        // would load. The bridge text is in the extension's config.
        let first = match &ctx.resume {
            Some(_) => ctx.resume_note.as_ref(),
            None => ctx.initial_prompt.as_ref(),
        };
        if let Some(message) = first {
            args.push("--".into());
            args.push(message.into());
        }
        let mut env = tool_env(
            flavor,
            &ctx.home,
            &ctx.session_tmp,
            &ctx.profile,
            ctx.system_config,
            &ctx.inherited,
        );
        env.extend(ctx.pi.env.iter().cloned());
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

impl ToolAdapter for Pi {
    fn launch(&self, ctx: &LaunchCtx) -> LaunchSpec {
        Self::launch_as(Flavor::HOST, ctx)
    }

    /// Pi has no one account to sign in to: its providers are set in
    /// Connecto's settings, or with `/login` inside Pi. So "login" is Pi
    /// itself, started in the run folder, where `/login` can be typed.
    fn login(&self, ctx: &LoginCtx) -> LaunchSpec {
        LaunchSpec {
            program: ctx.entry.clone(),
            args: vec!["--tui-mode".into(), "regular".into(), "--no-session".into()],
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

    /// Whether Pi runs at all: it prints its version.
    fn status(&self, ctx: &LoginCtx) -> LaunchSpec {
        LaunchSpec {
            args: vec!["--version".into()],
            cwd: ctx.session_tmp.clone(),
            ..self.login(ctx)
        }
    }

    /// A Pi that answers is ready to start. Which providers it can reach is
    /// not a login state: a terminal with none says so itself.
    fn parse_status(&self, output: &str) -> AuthState {
        let answered = output.split_whitespace().any(|word| {
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

    /// The extension sends Claude's hook payloads, with no background tasks.
    fn parse_hook(&self, payload: &[u8], ctx: &HookCtx) -> Result<Vec<ToolEvent>, AdapterError> {
        Claude.parse_hook(payload, ctx)
    }

    /// Pi's ready signal is the extension's `SessionStart`, not the screen.
    fn readiness(&self, _screen: &str) -> Readiness {
        Readiness::Unknown
    }

    /// Measured on 1.5.2: whitespace around the text is dropped on submission.
    fn ack_form(&self, text: &str) -> String {
        text.trim().to_owned()
    }

    fn send_anyway_text(&self) -> &'static str {
        "Pi has not reported that it started. The message may be lost; it would then end Uncertain."
    }

    fn capabilities(&self, _version: &str) -> Capabilities {
        Capabilities {
            immediate_ack: true,
        }
    }

    fn turn_log(&self, reported: &Path, ctx: &HookCtx) -> Option<PathBuf> {
        under_profile(reported, &ctx.profile)
    }

    /// Pi's session file: one JSON record a line, a `message` record for each
    /// message of the conversation.
    fn read_log(&self, line: &str, whole: bool) -> LogMark {
        if !whole {
            // The stop reason follows the text, so a cut line cannot say how
            // its turn went.
            return LogMark::Other;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return LogMark::Other;
        };
        if value.get("type").and_then(Value::as_str) != Some("message") {
            return LogMark::Other;
        }
        let Some(message) = value.get("message") else {
            return LogMark::Other;
        };
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => match message.get("stopReason").and_then(Value::as_str) {
                Some("aborted") => LogMark::Interrupt { turn: None },
                // A reply that asks for tools is not the turn's last.
                Some("toolUse") | None => LogMark::Other,
                Some(_) => LogMark::FinalReply,
            },
            Some("user") => {
                let text = match message.get("content") {
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(Value::Array(parts)) => {
                        let texts: Vec<&str> = parts
                            .iter()
                            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect();
                        (!texts.is_empty()).then(|| texts.join("\n"))
                    }
                    _ => None,
                };
                text.map_or(LogMark::Other, |text| LogMark::Prompt {
                    text: prompt_head(&text),
                })
            }
            _ => LogMark::Other,
        }
    }

    /// Pi starts no turn by itself.
    fn continuation(&self, _prompt: &str) -> Option<Continuation> {
        None
    }

    /// Measured on 1.5.2: Shift+Enter and Ctrl-J put a newline in the draft,
    /// but ESC CR (Alt+Enter) queues or sends it; Ctrl-C empties a draft, and
    /// on an empty editor a second one quits; a long paste is shown whole.
    fn draft_keys(&self) -> DraftKeys {
        DraftKeys {
            multiline: true,
            esc_enter_newline: false,
            ctrl_c_clears: false,
            collapses_pastes: false,
        }
    }

    fn composer(&self, screen: Screen<'_>) -> Option<ComposerView> {
        let state = pi_composer_state(screen);
        let first_line = if state == ComposerState::Occupied {
            editor_line(screen)
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use connecto_core::{EventText, Reply};

    use super::*;
    use crate::spec::tests::{launch_ctx, login_ctx};

    /// Screens as Pi 1.5.2 draws them in `regular` mode (trimmed to the
    /// bottom of the screen), with the state each one must read as.
    const FOOTER: &str = "~/project\n↑5 ↓5 0.0%/128k (auto)                    gemma";

    fn screen(top: &str, editor: &str, below: &str) -> String {
        format!(" REPLY to something\n\n{top}\n{editor}\n────────────────────\n{below}{FOOTER}\n")
    }

    #[test]
    fn the_editor_between_the_last_two_rules_gives_the_state() {
        let rule = "────────────────────";
        assert_eq!(
            pi_composer_state(Screen::plain(&screen(rule, "", ""))),
            ComposerState::Empty
        );
        assert_eq!(
            pi_composer_state(Screen::plain(&screen("── ⠧ Working ───────", "", ""))),
            ComposerState::Busy
        );
        assert_eq!(
            pi_composer_state(Screen::plain(&screen(rule, "draft", ""))),
            ComposerState::Occupied
        );
        // A draft typed while Pi works is still a draft.
        assert_eq!(
            pi_composer_state(Screen::plain(&screen("── ⠧ Working ───────", "cd", ""))),
            ComposerState::Occupied
        );
        // The slash menu sits under the lower rule, above the footer.
        let menu = "→ settings           Open settings menu\n  model              Select model\n  (1/26)\n";
        assert_eq!(
            pi_composer_state(Screen::plain(&screen(rule, "/", menu))),
            ComposerState::Occupied
        );
    }

    #[test]
    fn an_editor_that_is_not_fully_drawn_is_booting() {
        assert_eq!(pi_composer_state(Screen::plain("")), ComposerState::Booting);
        assert_eq!(
            pi_composer_state(Screen::plain(" ▀▀█  v1.5.2\n █▀ █ escape interrupt\n")),
            ComposerState::Booting
        );
        // One rule only, or both with no footer under them yet.
        assert_eq!(
            pi_composer_state(Screen::plain("text\n────────────\n~/project\n")),
            ComposerState::Booting
        );
        assert_eq!(
            pi_composer_state(Screen::plain("────────────\n\n────────────\n\n")),
            ComposerState::Booting
        );
    }

    #[test]
    fn a_rule_in_the_transcript_does_not_stand_for_the_editor() {
        let text = format!(
            "a table\n──────────\nmore\n──────────\n{}",
            screen("────────────────────", "typed", "")
        );
        assert_eq!(
            pi_composer_state(Screen::plain(&text)),
            ComposerState::Occupied
        );
        assert_eq!(
            Pi.composer(Screen::plain(&text)).unwrap().first_line,
            "typed"
        );
    }

    #[test]
    fn faint_text_in_the_editor_is_not_a_draft() {
        let rule = "────────────────────";
        let drawn = screen(rule, "a suggestion", "");
        let typed = screen(rule, "            ", "");
        let faint = Screen {
            drawn: &drawn,
            typed: &typed,
        };
        assert_eq!(pi_composer_state(faint), ComposerState::Empty);
        let typed = screen(rule, "a           ", "");
        let partly = Screen {
            drawn: &drawn,
            typed: &typed,
        };
        assert_eq!(pi_composer_state(partly), ComposerState::Occupied);
        assert_eq!(Pi.composer(partly).unwrap().first_line, "a");
    }

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

    fn config_of(spec: &LaunchSpec) -> Value {
        let extension = spec
            .files
            .iter()
            .find(|(path, _)| path.ends_with(EXTENSION_FILE))
            .map(|(_, body)| String::from_utf8(body.clone()).unwrap())
            .unwrap();
        let line = extension.lines().next().unwrap();
        let json = line
            .strip_prefix("const CONFIG = ")
            .and_then(|rest| rest.strip_suffix(';'))
            .unwrap();
        serde_json::from_str(json).unwrap()
    }

    /// `--append-system-prompt` replaces the APPEND_SYSTEM.md Pi would load
    /// (the project's when trusted, the profile's otherwise), so neither the
    /// package recap nor the bridge text goes there: both ride in the
    /// extension's config, which adds them to the prompt Pi resolved. No
    /// launch passes the flag, new or resumed, and the extension is the only
    /// file written.
    #[test]
    fn the_recap_goes_through_the_extension_not_the_append_flag() {
        for resume in [None, Some("abc".to_owned())] {
            let mut ctx = launch_ctx();
            ctx.resume.clone_from(&resume);
            for (bridge, recap) in [
                (None, Some("RECAP")),
                (Some("LAUNCH \"TEXT\"\nline two"), Some("RECAP")),
                (Some("LAUNCH \"TEXT\"\nline two"), None),
            ] {
                ctx.bridge.text = bridge.map(str::to_owned);
                ctx.package_text = recap.map(str::to_owned);
                let spec = Pi.launch(&ctx);
                let words = args(&spec);
                assert!(
                    !words.iter().any(|a| a == "--append-system-prompt"),
                    "{words:?}"
                );
                assert_eq!(spec.files.len(), 1, "{resume:?}");
                let config = config_of(&spec);
                assert_eq!(config["bridge"].as_str(), bridge, "{resume:?}");
                assert_eq!(config["recap"].as_str(), recap, "{resume:?}");
            }
        }
    }

    /// The extension, run by node against a stand-in for Pi: its
    /// `before_agent_start` handler adds the bridge text and then the recap
    /// as sections of their own, and leaves the resolved APPEND_SYSTEM.md
    /// text (a trusted project's or the profile's) and the project context
    /// untouched. Skipped where node is not installed.
    #[test]
    fn the_extension_adds_the_recap_as_a_prompt_section() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("node is not installed: skipped");
            return;
        }
        let bridge = "BRIDGE `connecto` \"q\" \\ $HOME </script> \u{2028} ünï\nline two";
        let mut ctx = launch_ctx();
        ctx.bridge.text = Some(bridge.into());
        ctx.package_text = Some("RECAP \"quoted\"\nline two".into());
        let dir = tempfile::Builder::new()
            .prefix("pi #recap ")
            .tempdir()
            .unwrap();
        let extension = dir.path().join("connecto-pi.mjs");
        std::fs::write(&extension, Pi::extension(&ctx)).unwrap();
        let harness = dir.path().join("harness.mjs");
        std::fs::write(
            &harness,
            r#"
import { pathToFileURL } from "node:url";
const ext = (await import(pathToFileURL(process.argv[2]).href)).default;
const handlers = {};
const pi = { on: (name, f) => { (handlers[name] ??= []).push(f); }, registerProvider() {} };
await ext(pi);
const out = [];
for (const append of ["PROJECT-APPEND", "PROFILE-APPEND"]) {
  const options = { sections: {}, appendSystemPrompt: append, contextFiles: [{ path: "AGENTS.md", content: "A" }] };
  for (const f of handlers.before_agent_start ?? []) {
    await f({ messageType: "user", prompt: "hi", systemPrompt: "", systemPromptOptions: options }, {});
  }
  out.push({ ...options, order: Object.keys(options.sections) });
}
// Pi 0.85: no sections; the handler gets the prompt Pi resolved and may hand
// a changed one back.
let prompt = "BASE\n\nPROJECT-APPEND";
for (const f of handlers.before_agent_start ?? []) {
  const result = await f({ prompt: "hi", images: [], systemPrompt: prompt, systemPromptOptions: { appendSystemPrompt: ["PROJECT-APPEND"] } }, {});
  if (result?.systemPrompt !== undefined) prompt = result.systemPrompt;
}
out.push({ systemPrompt: prompt });
console.log(JSON.stringify(out));
"#,
        )
        .unwrap();
        let run = std::process::Command::new("node")
            .arg(&harness)
            .arg(&extension)
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let out: Value = serde_json::from_slice(&run.stdout).unwrap();
        for (options, append) in out
            .as_array()
            .unwrap()
            .iter()
            .zip(["PROJECT-APPEND", "PROFILE-APPEND"])
        {
            assert_eq!(options["sections"]["connecto-bridge"], bridge);
            assert_eq!(
                options["sections"]["connecto-status-recap"],
                "RECAP \"quoted\"\nline two"
            );
            assert_eq!(
                options["order"],
                serde_json::json!(["connecto-bridge", "connecto-status-recap"])
            );
            assert_eq!(options["appendSystemPrompt"], append);
            assert_eq!(options["contextFiles"][0]["content"], "A");
        }
        // Pi 0.85, which has no sections: added at the end of the prompt it
        // resolved, its APPEND_SYSTEM.md kept, in the same order and tags.
        assert_eq!(
            out[2]["systemPrompt"],
            format!(
                "BASE\n\nPROJECT-APPEND\n\n<connecto-bridge>\n{bridge}\n</connecto-bridge>\n\n\
                 <connecto-status-recap>\nRECAP \"quoted\"\nline two\n</connecto-status-recap>"
            )
        );
        let run_with = |ctx: &LaunchCtx| -> Value {
            std::fs::write(&extension, Pi::extension(ctx)).unwrap();
            let run = std::process::Command::new("node")
                .arg(&harness)
                .arg(&extension)
                .output()
                .unwrap();
            serde_json::from_slice(&run.stdout).unwrap()
        };
        // Without the package: the bridge text alone.
        ctx.package_text = None;
        assert_eq!(
            run_with(&ctx)[0]["sections"],
            serde_json::json!({ "connecto-bridge": bridge })
        );
        // With neither the extension adds no section, and leaves Pi 0.85's
        // prompt as it was.
        ctx.bridge.text = None;
        let out = run_with(&ctx);
        assert_eq!(out[0]["sections"], serde_json::json!({}));
        assert_eq!(out[2]["systemPrompt"], "BASE\n\nPROJECT-APPEND");
    }

    /// Pi has no MCP of its own: it comes from an extension the user installs
    /// (in the isolated profile or their own). Connecto adds its extension and
    /// never turns the others off.
    #[test]
    fn a_launch_keeps_the_users_own_extensions() {
        let words = args(&Pi.launch(&launch_ctx()));
        assert!(!words.iter().any(|w| w == "--no-extensions"), "{words:?}");
    }

    #[test]
    fn a_session_gets_its_id_the_extension_the_instructions_and_a_private_profile() {
        let ctx = launch_ctx();
        let spec = Pi.launch(&ctx);
        // Joined as the platform joins them: `/run\connecto-pi.mjs` on Windows.
        let in_run = |file: &str| ctx.run.join(file).to_string_lossy().into_owned();
        let extension = in_run(EXTENSION_FILE);
        assert_eq!(spec.program, PathBuf::from("/store/tool"));
        assert_eq!(spec.cwd, PathBuf::from("/project"));
        assert_eq!(
            args(&spec),
            [
                "--tui-mode",
                "regular",
                "--session-id",
                "11111111-2222-4333-8444-555555555555",
                "--extension",
                extension.as_str(),
                "--approve",
            ]
        );
        assert_eq!(
            env_of(&spec, "PI_CODING_AGENT_DIR").as_deref(),
            Some("/profile")
        );
        assert_eq!(
            env_of(&spec, "CONNECTO_HOOK_TOKEN").as_deref(),
            Some("token")
        );
        let separator = crate::platform::Flavor::HOST.path_separator();
        assert_eq!(
            env_of(&spec, "PATH"),
            Some(format!("/data/cli/bin{separator}/usr/bin"))
        );
        let file = |name: &str| {
            spec.files
                .iter()
                .find(|(path, _)| path.ends_with(name))
                .map(|(_, body)| String::from_utf8(body.clone()).unwrap())
                .unwrap()
        };
        // The instructions are in the extension's config, its only file.
        assert_eq!(spec.files.len(), 1);
        let extension = file("connecto-pi.mjs");
        let config = extension.lines().next().unwrap();
        assert_eq!(
            config,
            r#"const CONFIG = {"bridge":"LAUNCH TEXT","hook":"/data/cli/bin/connecto-hook","inbox":true,"lmstudio":null};"#
        );
        assert!(extension.contains("export default async function connecto(pi)"));
    }

    #[test]
    fn a_resume_names_the_session_and_takes_the_note_not_the_first_prompt() {
        let mut ctx = launch_ctx();
        ctx.initial_prompt = Some("-first".to_owned());
        ctx.resume_note = Some("note".to_owned());
        let fresh = args(&Pi.launch(&ctx));
        assert_eq!(fresh[fresh.len() - 2..], ["--", "-first"]);
        ctx.resume = Some("abc-123".to_owned());
        let resumed = args(&Pi.launch(&ctx));
        assert_eq!(resumed[2..4], ["--session", "abc-123"]);
        assert!(!resumed.contains(&"--session-id".to_owned()));
        assert_eq!(resumed[resumed.len() - 2..], ["--", "note"]);
    }

    #[test]
    fn the_mode_the_model_the_name_and_the_providers_reach_the_launch() {
        let mut ctx = launch_ctx();
        ctx.agent_mode = AgentMode::Prompt;
        ctx.model = Some("lmstudio/google/gemma-4-e2b".to_owned());
        ctx.session_name = Some("reviewer".to_owned());
        ctx.bridge.text = None;
        ctx.bridge.hooks = false;
        ctx.pi = PiProviders {
            env: vec![("OPENROUTER_API_KEY".into(), "k".into())],
            lmstudio_url: Some("http://localhost:1235".to_owned()),
            // The terminal's own model comes before the default one.
            default_model: Some("openrouter/other".to_owned()),
        };
        let spec = Pi.launch(&ctx);
        let words = args(&spec);
        assert!(!words.contains(&"--approve".to_owned()));
        assert!(!words.contains(&"--append-system-prompt".to_owned()));
        assert!(
            words
                .windows(2)
                .any(|w| w == ["--model", "lmstudio/google/gemma-4-e2b"])
        );
        assert!(words.windows(2).any(|w| w == ["--name", "reviewer"]));
        assert_eq!(env_of(&spec, "OPENROUTER_API_KEY").as_deref(), Some("k"));
        let extension = String::from_utf8(spec.files[0].1.clone()).unwrap();
        assert!(extension.starts_with(
            r#"const CONFIG = {"hook":"/data/cli/bin/connecto-hook","inbox":false,"lmstudio":"http://localhost:1235"};"#
        ));
    }

    #[test]
    fn a_terminal_with_no_model_of_its_own_starts_on_the_default_one() {
        let mut ctx = launch_ctx();
        ctx.pi.default_model = Some("together/some-model".to_owned());
        let words = args(&Pi.launch(&ctx));
        assert!(
            words
                .windows(2)
                .any(|w| w == ["--model", "together/some-model"])
        );
        ctx.pi.default_model = None;
        assert!(!args(&Pi.launch(&ctx)).contains(&"--model".to_owned()));
    }

    #[test]
    fn system_config_leaves_pis_own_folder_in_place() {
        let mut ctx = launch_ctx();
        ctx.system_config = true;
        assert_eq!(env_of(&Pi.launch(&ctx), "PI_CODING_AGENT_DIR"), None);
    }

    #[test]
    fn status_asks_the_version_and_an_answer_means_ready() {
        let spec = Pi.status(&login_ctx());
        assert_eq!(args(&spec), ["--version"]);
        assert_eq!(spec.cwd, PathBuf::from("/run/tmp"));
        assert_eq!(env_of(&spec, "CONNECTO_HOOK_TOKEN"), None);
        assert!(matches!(
            Pi.parse_status("1.5.2\n"),
            AuthState::LoggedIn { .. }
        ));
        assert_eq!(Pi.parse_status(""), AuthState::Unknown);
        assert_eq!(
            Pi.parse_status("env: node: No such file"),
            AuthState::Unknown
        );
    }

    fn hook_ctx() -> HookCtx {
        HookCtx {
            profile: "/profile".into(),
            hooks: true,
        }
    }

    #[test]
    fn the_extensions_payloads_become_events() {
        let started = br#"{"hook_event_name":"SessionStart","session_id":"s1","transcript_path":"/profile/sessions/x/s1.jsonl"}"#;
        assert_eq!(
            Pi.parse_hook(started, &hook_ctx()).unwrap(),
            [ToolEvent::Started {
                tool_session: EventText::new("s1".to_owned()),
                turn_log: Some(EventText::new("/profile/sessions/x/s1.jsonl".to_owned())),
            }]
        );
        let prompt = br#"{"hook_event_name":"UserPromptSubmit","prompt":"hello"}"#;
        assert_eq!(
            Pi.parse_hook(prompt, &hook_ctx()).unwrap(),
            [ToolEvent::PromptSubmitted {
                prompt: EventText::new("hello".to_owned()),
                turn: None,
            }]
        );
        let stop = br#"{"hook_event_name":"Stop","last_assistant_message":"done"}"#;
        assert_eq!(
            Pi.parse_hook(stop, &hook_ctx()).unwrap(),
            [ToolEvent::TurnEnded {
                input: Vec::new(),
                input_total: 0,
                reply: Reply::Text(EventText::reply("done".to_owned())),
                lifecycle_known: true,
                background: Vec::new(),
                thread: None,
                turn: None,
                failure: None,
                continued: false,
            }]
        );
        // A turn the provider failed ends with the failure, and no reply.
        let failed = br#"{"hook_event_name":"StopFailure","error":"provider_error"}"#;
        assert!(matches!(
            Pi.parse_hook(failed, &hook_ctx()).unwrap().as_slice(),
            [ToolEvent::TurnEnded {
                reply: Reply::None,
                failure: Some(_),
                ..
            }]
        ));
        let waiting = br#"{"hook_event_name":"Notification","notification_type":"ui_prompt"}"#;
        assert_eq!(
            Pi.parse_hook(waiting, &hook_ctx()).unwrap(),
            [ToolEvent::NeedsUser]
        );
    }

    #[test]
    fn the_session_file_marks_prompts_final_replies_and_interrupts() {
        let user = r#"{"type":"message","id":"a","message":{"role":"user","content":[{"type":"text","text":"say hi"}]}}"#;
        assert_eq!(
            Pi.read_log(user, true),
            LogMark::Prompt {
                text: "say hi".to_owned()
            }
        );
        let reply = |stop: &str| {
            format!(
                r#"{{"type":"message","id":"b","message":{{"role":"assistant","content":[],"stopReason":"{stop}"}}}}"#
            )
        };
        assert_eq!(Pi.read_log(&reply("stop"), true), LogMark::FinalReply);
        assert_eq!(Pi.read_log(&reply("error"), true), LogMark::FinalReply);
        assert_eq!(Pi.read_log(&reply("toolUse"), true), LogMark::Other);
        assert_eq!(
            Pi.read_log(&reply("aborted"), true),
            LogMark::Interrupt { turn: None }
        );
        for other in [
            r#"{"type":"model_change","provider":"lmstudio"}"#,
            r#"{"type":"message","message":{"role":"toolResult","content":[]}}"#,
            r#"{"type":"message","message":{"role":"system","content":""}}"#,
            "not json",
        ] {
            assert_eq!(Pi.read_log(other, true), LogMark::Other, "{other}");
        }
        assert_eq!(Pi.read_log(&reply("stop"), false), LogMark::Other);
    }

    #[test]
    fn only_a_session_file_under_the_profile_is_tailed() {
        assert!(
            Pi.turn_log(Path::new("/profile/sessions/p/s.jsonl"), &hook_ctx())
                .is_some()
        );
        assert!(Pi.turn_log(Path::new("/etc/passwd"), &hook_ctx()).is_none());
    }

    #[test]
    fn the_ack_form_drops_the_whitespace_around_a_prompt() {
        assert_eq!(Pi.ack_form("  a\tb \n"), "a\tb");
    }
}
