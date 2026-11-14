//! The adapter contract.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use connecto_core::{AgentMode, AuthState, Readiness, ToolEvent};

use crate::platform::{Flavor, comparable};

/// `path`, if it sits under `profile`, with no `..` in it. A tool reports
/// where its turn log is; only one inside its own profile is read.
pub(crate) fn under_profile(path: &Path, profile: &Path) -> Option<PathBuf> {
    let clean = path
        .components()
        .all(|part| !matches!(part, std::path::Component::ParentDir));
    (clean && comparable(path).starts_with(comparable(profile))).then(|| path.to_owned())
}

/// How to start one process. The caller decides whether to sandbox it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    /// Files to write first, all under the run folder.
    pub files: Vec<(PathBuf, Vec<u8>)>,
}

/// What a terminal gets so its agent can use the bridge (section 5.15).
#[derive(Debug, Clone)]
pub struct BridgeLaunch {
    /// The bridge socket.
    pub socket: PathBuf,
    /// The terminal's own id, for the agent's reading only. Never trusted.
    pub terminal: String,
    /// `<data>/cli/bin`, first on `PATH`. It holds no model CLI.
    pub bin_dir: PathBuf,
    /// The launch text. Absent when the bridge is off for this terminal.
    pub text: Option<String>,
    /// Whether the hooks are registered. Off means no hooks and no context.
    pub hooks: bool,
    /// Codex: the `hooks.state={...}` value that trusts the hooks, read during
    /// preflight. Hooks are never registered without it.
    pub trust: Option<String>,
}

/// A hidden utility session (a short model call under Connecto's control):
/// the model and effort to pass, with tools off and no bypass flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtilityLaunch {
    pub model: String,
    pub effort: String,
}

/// Everything a contained session launch needs. All paths are real paths.
#[derive(Debug, Clone)]
pub struct LaunchCtx {
    pub entry: PathBuf,
    pub hook_binary: PathBuf,
    pub project: PathBuf,
    pub profile: PathBuf,
    /// The tool's own configuration folder is used (`CLAUDE_CONFIG_DIR` /
    /// `CODEX_HOME` are not set); `profile` then names that real folder.
    pub system_config: bool,
    pub session_tmp: PathBuf,
    /// Not writable by the session.
    pub run: PathBuf,
    pub home: PathBuf,
    /// Variables copied from the parent, already filtered.
    pub inherited: Vec<(OsString, OsString)>,
    pub session_id: String,
    /// The tool session to pick up again (Claude `--resume`, Codex `resume`)
    /// instead of starting a new one. Only ever a value the tool itself
    /// reported, checked by the caller.
    pub resume: Option<String>,
    pub hook_socket: PathBuf,
    pub hook_token: String,
    pub bridge: BridgeLaunch,
    /// The first message of a NEW conversation, as the tool's positional
    /// prompt. Ignored when resuming and for a utility session.
    pub initial_prompt: Option<String>,
    /// The name the user gave this terminal. Claude shows it as the session's
    /// name (`--name`: the prompt box, the resume picker, the terminal title).
    pub session_name: Option<String>,
    /// The model this terminal was started with (`connecto add --model`).
    /// `None` leaves the tool's own default. Not used by a utility session,
    /// which names its own.
    pub model: Option<String>,
    /// A short first message for a RESUMED conversation (a note that the
    /// instructions changed). Ignored for a new one.
    pub resume_note: Option<String>,
    /// Set for a utility session. Claude only.
    pub utility: Option<UtilityLaunch>,
    /// Whether the tool asks before it acts. Not used by a utility session,
    /// which never runs with the bypass.
    pub agent_mode: AgentMode,
    /// Base URL of a local fake model.
    #[cfg(feature = "test-endpoints")]
    pub fake_model: Option<String>,
}

/// What a login or status command needs. It has no hook socket and no project,
/// so those cannot leak into the login path.
#[derive(Debug, Clone)]
pub struct LoginCtx {
    pub entry: PathBuf,
    pub profile: PathBuf,
    /// As [`LaunchCtx::system_config`].
    pub system_config: bool,
    /// This attempt's run folder, which no session can write.
    pub run: PathBuf,
    pub session_tmp: PathBuf,
    pub home: PathBuf,
    pub inherited: Vec<(OsString, OsString)>,
}

/// What parsing a hook frame may consult.
#[derive(Debug, Clone)]
pub struct HookCtx {
    pub profile: PathBuf,
    /// The terminal runs with hooks, so the main thread is told by its pinned
    /// id. Without them, a Codex `notify` is checked against the rollout files.
    pub hooks: bool,
}

/// What one line of a tool's turn log says. The log is the tool's own record
/// of a session: Claude's transcript or Codex's rollout file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogMark {
    /// The turn was interrupted. Codex names the turn.
    Interrupt {
        turn: Option<String>,
    },
    /// Codex `task_complete`. `reply` is `None` when the line was too long to
    /// read whole.
    TurnComplete {
        turn: Option<String>,
        reply: Option<String>,
    },
    /// Codex `task_started`: a turn began. `turn` is the id its `task_complete`
    /// carries.
    TurnStarted {
        turn: Option<String>,
    },
    /// Claude: a user prompt. `text` is its start, to match a hook's prompt.
    Prompt {
        text: String,
    },
    /// Claude: an assistant record that ends a turn.
    FinalReply,
    Other,
}

/// What a prompt that starts a Claude turn by itself says about its cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continuation {
    /// The background task it reports, when it names one.
    pub task: Option<String>,
}

/// How a tool behaves, whatever its version. Connecto does not pin CLI
/// versions: the bridge is on for every version, relying on the CLIs staying
/// backward compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The tool acknowledges a turn when it starts, not only when it ends.
    pub immediate_ack: bool,
}

/// Why a hook frame could not be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AdapterError {
    #[error("the payload is not valid JSON")]
    NotJson,
    #[error("the payload lacks a required field: {0}")]
    MissingField(&'static str),
}

/// One tool.
pub trait ToolAdapter: Send + Sync {
    /// A contained session.
    fn launch(&self, ctx: &LaunchCtx) -> LaunchSpec;

    /// The tool's interactive login. It runs outside the sandbox, so its
    /// working directory is the run folder and never a session-writable one.
    fn login(&self, ctx: &LoginCtx) -> LaunchSpec;

    /// The tool's status command, to run inside the sandbox.
    fn status(&self, ctx: &LoginCtx) -> LaunchSpec;

    /// Keep only the login state and method from a status command's output.
    fn parse_status(&self, output: &str) -> AuthState;

    /// Events in one hook frame. An empty list means nothing of interest.
    fn parse_hook(&self, payload: &[u8], ctx: &HookCtx) -> Result<Vec<ToolEvent>, AdapterError>;

    /// A soft signal from the screen. Warn with it; never treat it as an ack.
    fn readiness(&self, screen: &str) -> Readiness;

    /// What sending to a target that may not be ready means for this tool.
    fn send_anyway_text(&self) -> &'static str;

    /// `text` without what this tool may rewrite on submission. Compare a
    /// pasted message with what the tool reports only in this form, on both
    /// sides: only what was measured on the real CLI belongs here, because
    /// everything removed stops counting as a difference in an ack.
    fn ack_form(&self, text: &str) -> String;

    /// What `version` may be used for.
    fn capabilities(&self, _version: &str) -> Capabilities;

    /// The turn log to tail, from the path a hook reported. Only a path under
    /// the terminal's own profile is used.
    fn turn_log(&self, reported: &Path, ctx: &HookCtx) -> Option<PathBuf>;

    /// What one line of the turn log says. `whole` is false when the line was
    /// too long and only its start is given.
    fn read_log(&self, line: &str, whole: bool) -> LogMark;

    /// Whether `prompt` is a turn the tool started by itself.
    fn continuation(&self, prompt: &str) -> Option<Continuation>;
}

/// The environment every tool process starts from.
///
/// What else a Windows tool needs (`USERPROFILE`, `APPDATA`, `LOCALAPPDATA`,
/// `SystemRoot`, `ComSpec`, `PATHEXT`, `PSModulePath`, …) is copied from the
/// parent by the session layer into `inherited`; it is not set here.
pub(crate) fn base_env(
    flavor: Flavor,
    home: &Path,
    tmp: &Path,
    inherited: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let mut env = inherited.to_vec();
    match flavor {
        Flavor::Unix => {
            env.push(("HOME".into(), home.into()));
            env.push(("TERM".into(), "xterm-256color".into()));
            env.push(("COLORTERM".into(), "truecolor".into()));
            env.push(("TMPDIR".into(), tmp.into()));
            // zsh ignores TMPDIR for heredocs and uses /tmp/zsh, which sessions cannot write.
            env.push(("TMPPREFIX".into(), tmp.join("zsh").into()));
        }
        Flavor::Windows => {
            // Git Bash (Claude's Bash tool) and Codex read HOME; Windows
            // programs keep their temporary files under TEMP and TMP, which
            // replace the inherited ones so they stay in the session folder.
            // TMPDIR is for the Unix tools under Git Bash. No TMPPREFIX: no zsh.
            set_var(&mut env, "HOME", home.into());
            set_var(&mut env, "TERM", "xterm-256color".into());
            set_var(&mut env, "COLORTERM", "truecolor".into());
            set_var(&mut env, "TMPDIR", tmp.into());
            set_var(&mut env, "TEMP", tmp.into());
            set_var(&mut env, "TMP", tmp.into());
        }
    }
    env
}

/// Windows: set `name`, replacing every inherited spelling of it (names are
/// not case-sensitive there), so the child gets exactly one value.
fn set_var(env: &mut Vec<(OsString, OsString)>, name: &str, value: OsString) {
    env.retain(|(key, _)| !key.to_string_lossy().eq_ignore_ascii_case(name));
    env.push((name.into(), value));
}

/// The value of `name` in `env`, as Windows looks it up (any case).
pub(crate) fn windows_var<'a>(
    env: &'a [(OsString, OsString)],
    name: &str,
) -> Option<&'a std::ffi::OsStr> {
    env.iter()
        .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_os_str())
}

/// The hook channel's variables.
pub(crate) fn hook_env(ctx: &LaunchCtx) -> Vec<(OsString, OsString)> {
    vec![
        (
            "CONNECTO_HOOK_SOCK".into(),
            ctx.hook_socket.clone().into_os_string(),
        ),
        ("CONNECTO_HOOK_TOKEN".into(), ctx.hook_token.clone().into()),
        (
            "CONNECTO_BRIDGE_SOCK".into(),
            ctx.bridge.socket.clone().into_os_string(),
        ),
        (
            "CONNECTO_TERMINAL".into(),
            ctx.bridge.terminal.clone().into(),
        ),
    ]
}

/// `PATH` with the bridge folder first, which holds the `connecto` command and
/// no model CLI.
pub(crate) fn with_bridge_path(
    flavor: Flavor,
    env: &mut Vec<(OsString, OsString)>,
    ctx: &LaunchCtx,
) {
    let is_path = |name: &OsString| match flavor {
        Flavor::Unix => name == "PATH",
        // `Path` is the usual spelling in a Windows environment block.
        Flavor::Windows => name.to_string_lossy().eq_ignore_ascii_case("PATH"),
    };
    let existing = env
        .iter()
        .position(|(name, _)| is_path(name))
        .map(|index| env.remove(index).1);
    let mut path = ctx.bridge.bin_dir.clone().into_os_string();
    if let Some(existing) = existing {
        path.push(flavor.path_separator());
        path.push(existing);
    }
    env.push(("PATH".into(), path));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn launch_ctx() -> LaunchCtx {
        LaunchCtx {
            entry: "/store/tool".into(),
            hook_binary: "/data/cli/bin/connecto-hook".into(),
            project: "/project".into(),
            profile: "/profile".into(),
            system_config: false,
            session_tmp: "/run/tmp".into(),
            run: "/run".into(),
            home: "/home".into(),
            inherited: vec![("PATH".into(), "/usr/bin".into())],
            session_id: "11111111-2222-4333-8444-555555555555".to_owned(),
            resume: None,
            hook_socket: "/private/tmp/mg-501/a.sock".into(),
            hook_token: "token".to_owned(),
            bridge: BridgeLaunch {
                socket: "/private/tmp/mg-501/bridge-1.sock".into(),
                terminal: "claude".to_owned(),
                bin_dir: "/data/cli/bin".into(),
                text: Some("LAUNCH TEXT".to_owned()),
                hooks: true,
                trust: Some(r#"hooks.state={"k"={trusted_hash="sha256:h"}}"#.to_owned()),
            },
            initial_prompt: None,
            session_name: None,
            model: None,
            resume_note: None,
            utility: None,
            agent_mode: AgentMode::Bypass,
            #[cfg(feature = "test-endpoints")]
            fake_model: None,
        }
    }

    pub(crate) fn login_ctx() -> LoginCtx {
        LoginCtx {
            entry: "/store/tool".into(),
            profile: "/profile".into(),
            system_config: false,
            run: "/run".into(),
            session_tmp: "/run/tmp".into(),
            home: "/home".into(),
            inherited: vec![("PATH".into(), "/usr/bin".into())],
        }
    }

    /// The same launch as [`launch_ctx`], with the paths and names a Windows
    /// session has: the helper is an `.exe`, the
    /// hook and bridge channels are named pipes, and the inherited block holds
    /// the Windows system variables.
    pub(crate) fn launch_ctx_windows() -> LaunchCtx {
        let data = r"C:\Users\Jane O'Neil\AppData\Roaming\connecto";
        LaunchCtx {
            entry: format!(r"{data}\cli\claude\2.1.322\claude.exe").into(),
            hook_binary: format!(r"{data}\cli\bin\connecto-hook.exe").into(),
            project: r"C:\Users\Jane O'Neil\proj".into(),
            profile: format!(r"{data}\profiles\p\g1").into(),
            session_tmp: format!(r"{data}\run\r1\tmp").into(),
            run: format!(r"{data}\run\r1").into(),
            home: r"C:\Users\Jane O'Neil".into(),
            inherited: vec![
                ("PATH".into(), r"C:\WINDOWS\system32;C:\WINDOWS".into()),
                ("SystemRoot".into(), r"C:\WINDOWS".into()),
                ("TEMP".into(), r"C:\Users\JANEO~1\AppData\Local\Temp".into()),
                ("TMP".into(), r"C:\Users\JANEO~1\AppData\Local\Temp".into()),
            ],
            hook_socket: r"\\.\pipe\connecto-hook-0123456789abcdef0123456789abcdef".into(),
            bridge: BridgeLaunch {
                socket: r"\\.\pipe\connecto-bridge-0123456789abcdef0123456789abcdef".into(),
                bin_dir: format!(r"{data}\cli\bin").into(),
                ..launch_ctx().bridge
            },
            ..launch_ctx()
        }
    }

    #[test]
    fn only_a_clean_path_under_the_profile_passes() {
        let profile = Path::new("/p");
        assert_eq!(
            under_profile(Path::new("/p/projects/t.jsonl"), profile),
            Some(PathBuf::from("/p/projects/t.jsonl"))
        );
        for bad in [
            "/etc/passwd",
            "/p/../etc/passwd",
            "/pp/t.jsonl",
            "p/t.jsonl",
        ] {
            assert_eq!(under_profile(Path::new(bad), profile), None, "{bad}");
        }
    }

    // Unix paths in the fixture (`Path::join` writes `/t/zsh` only on Unix).
    #[cfg(unix)]
    #[test]
    fn home_comes_from_the_context_and_terminal_identity_is_set() {
        let env = base_env(Flavor::HOST, Path::new("/h"), Path::new("/t"), &[]);
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(get("HOME").as_deref(), Some("/h"));
        assert_eq!(get("TMPDIR").as_deref(), Some("/t"));
        assert_eq!(get("TMPPREFIX").as_deref(), Some("/t/zsh"));
        assert_eq!(get("TERM").as_deref(), Some("xterm-256color"));
    }

    fn names(env: &[(OsString, OsString)]) -> Vec<String> {
        env.iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect()
    }

    /// The Unix shape, on every host: the variables and their order are the
    /// ones macOS has always had.
    #[test]
    fn the_unix_environment_keeps_its_variables_and_their_order() {
        let inherited = [("PATH".into(), "/usr/bin".into())];
        let env = base_env(Flavor::Unix, Path::new("/h"), Path::new("/t"), &inherited);
        assert_eq!(
            names(&env),
            ["PATH", "HOME", "TERM", "COLORTERM", "TMPDIR", "TMPPREFIX"]
        );
    }

    /// The Windows shape, on every host: TEMP and TMP point into the session
    /// folder and replace the inherited ones; no zsh setting.
    #[test]
    fn the_windows_environment_has_one_temp_folder_and_no_zsh_setting() {
        let ctx = launch_ctx_windows();
        let env = base_env(Flavor::Windows, &ctx.home, &ctx.session_tmp, &ctx.inherited);
        let get = |name: &str| {
            let found: Vec<String> = env
                .iter()
                .filter(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
                .map(|(_, value)| value.to_string_lossy().into_owned())
                .collect();
            assert!(found.len() <= 1, "{name} is set {} times", found.len());
            found.into_iter().next()
        };
        let tmp = ctx.session_tmp.to_string_lossy().into_owned();
        for name in ["TEMP", "TMP", "TMPDIR"] {
            assert_eq!(get(name), Some(tmp.clone()), "{name}");
        }
        assert_eq!(get("HOME").as_deref(), Some(r"C:\Users\Jane O'Neil"));
        assert_eq!(get("TERM").as_deref(), Some("xterm-256color"));
        assert_eq!(get("COLORTERM").as_deref(), Some("truecolor"));
        assert_eq!(get("TMPPREFIX"), None);
        // The inherited system variables are kept as they came.
        assert_eq!(get("SystemRoot").as_deref(), Some(r"C:\WINDOWS"));
        assert_eq!(
            windows_var(&env, "SYSTEMROOT"),
            Some(std::ffi::OsStr::new(r"C:\WINDOWS"))
        );
    }

    #[test]
    fn the_bridge_folder_goes_first_on_path_with_the_flavors_separator() {
        let path = |flavor: Flavor, ctx: &LaunchCtx| {
            let mut env = ctx.inherited.clone();
            with_bridge_path(flavor, &mut env, ctx);
            assert_eq!(
                env.iter()
                    .filter(|(n, _)| n.to_string_lossy().eq_ignore_ascii_case("PATH"))
                    .count(),
                1
            );
            env.iter()
                .find(|(name, _)| name == "PATH")
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(
            path(Flavor::Unix, &launch_ctx()).as_deref(),
            Some("/data/cli/bin:/usr/bin")
        );
        let mut windows = launch_ctx_windows();
        assert_eq!(
            path(Flavor::Windows, &windows).as_deref(),
            Some(
                r"C:\Users\Jane O'Neil\AppData\Roaming\connecto\cli\bin;C:\WINDOWS\system32;C:\WINDOWS"
            )
        );
        // Windows spells it `Path` as often as not.
        windows.inherited[0].0 = "Path".into();
        assert_eq!(
            path(Flavor::Windows, &windows).as_deref(),
            Some(
                r"C:\Users\Jane O'Neil\AppData\Roaming\connecto\cli\bin;C:\WINDOWS\system32;C:\WINDOWS"
            )
        );
        // No inherited PATH: the bridge folder alone, on either system.
        windows.inherited.clear();
        assert_eq!(
            path(Flavor::Windows, &windows).as_deref(),
            Some(r"C:\Users\Jane O'Neil\AppData\Roaming\connecto\cli\bin")
        );
    }

    #[cfg(windows)]
    #[test]
    fn home_comes_from_the_context_and_temp_is_the_session_folder_on_windows() {
        let env = base_env(
            Flavor::HOST,
            Path::new(r"C:\h"),
            Path::new(r"C:\t"),
            &[("TEMP".into(), r"C:\elsewhere".into())],
        );
        let get =
            |name: &str| windows_var(&env, name).map(|value| value.to_string_lossy().into_owned());
        assert_eq!(get("HOME").as_deref(), Some(r"C:\h"));
        assert_eq!(get("TEMP").as_deref(), Some(r"C:\t"));
        assert_eq!(get("TMP").as_deref(), Some(r"C:\t"));
        assert_eq!(get("TMPDIR").as_deref(), Some(r"C:\t"));
        assert_eq!(get("TMPPREFIX"), None);
        assert_eq!(get("TERM").as_deref(), Some("xterm-256color"));
    }

    #[cfg(windows)]
    #[test]
    fn a_turn_log_under_a_verbatim_profile_path_is_found() {
        assert_eq!(
            under_profile(
                Path::new(r"C:\data\p\projects\C--x\s.jsonl"),
                Path::new(r"\\?\C:\data\p")
            ),
            Some(PathBuf::from(r"C:\data\p\projects\C--x\s.jsonl"))
        );
        assert_eq!(
            under_profile(
                Path::new(r"C:\data\p\..\q\s.jsonl"),
                Path::new(r"\\?\C:\data\p")
            ),
            None
        );
        assert_eq!(
            under_profile(Path::new(r"C:\data\pq\s.jsonl"), Path::new(r"C:\data\p")),
            None
        );
    }
}

/// What a tool's live screen says about its input box. Read from the screen
/// alone; the composer is the last one drawn at the bottom of the screen, never
/// an older echo higher up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerState {
    /// The empty composer with its footer under it: a paste typed now lands in it.
    Empty,
    /// The tool's status says it is working: the composer is not for typing yet.
    Busy,
    /// Text in the composer, or a choice on its line (a draft, a menu, a prompt):
    /// typing now would join it or answer the prompt.
    Occupied,
    /// No composer yet (booting, or not fully drawn).
    Booting,
}
