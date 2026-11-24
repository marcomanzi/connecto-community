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

/// What a utility session may do, as far as its CLI can be held to it. Never a
/// bypass or permission-skipping flag, whatever the access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UtilityAccess {
    /// As few tools as the CLI allows (titles): Claude has none at all.
    #[default]
    Minimal,
    /// Read and search the project's files, nothing that writes, edits or runs
    /// a command where the CLI can say so (project memory). Best effort per CLI.
    ReadOnly,
}

/// A hidden utility session (a short model call under Connecto's control,
/// working in the project folder): the model and effort to pass, and what the
/// session may do. Any adapter whose [`ToolAdapter::utility`] is `Some` takes
/// one in [`LaunchCtx::utility`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtilityLaunch {
    /// `None` leaves the CLI's own default model.
    pub model: Option<String>,
    pub effort: String,
    pub access: UtilityAccess,
}

/// What a tool's own session log says about the turn that answers a prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoggedTurn {
    /// A user record holds exactly the prompt that was sent
    /// ([`UtilityTool::prompt_matches`]).
    pub prompt_whole: bool,
    /// Characters of the longest user record seen (to say how much arrived).
    pub prompt_chars: usize,
    /// The text of the turn's final response (not the commentary of earlier
    /// steps that used tools), when there is any.
    pub reply: Option<String>,
    /// The log marks that response as the end of the turn.
    pub complete: bool,
    /// The turn ended in an error the tool reported (network, API).
    pub error: Option<String>,
}

/// Whether what a tool reports as submitted is the whole prompt that was sent:
/// the same length, give or take what the composer rewrites (a paste wrapper,
/// whitespace), and the same ending.
#[must_use]
pub fn prompt_arrived_whole(sent: &str, submitted: &str) -> bool {
    let norm = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let (sent, got) = (norm(sent), norm(submitted));
    let end: String = {
        let chars: Vec<char> = sent.chars().collect();
        chars[chars.len().saturating_sub(40)..].iter().collect()
    };
    got.contains(&end) && got.chars().count() * 100 >= sent.chars().count() * 95
}

/// Whether `submitted` is exactly the prompt that was `sent`, once whitespace
/// runs are made single spaces (a composer may rewrite tabs and line ends and
/// drop trailing space). Nothing may be missing or added. For memory runs,
/// which need the whole prompt; [`prompt_arrived_whole`] is the looser check
/// titles have always used.
#[must_use]
pub fn prompt_matches_exact(sent: &str, submitted: &str) -> bool {
    sent.split_whitespace().eq(submitted.split_whitespace())
}

/// The small surface a CLI implements to serve utility sessions (titles,
/// project memory, any later use). The launch itself is
/// [`ToolAdapter::launch`] with [`LaunchCtx::utility`] set; this reads the
/// session that launch starts. Nothing in the callers is specific to a CLI.
pub trait UtilityTool: Send + Sync {
    /// The composer is up and empty: the prompt may be pasted now.
    fn ready(&self, screen: &str) -> bool;

    /// A screen that waits for a person (trust, login, first-run setup),
    /// named for the user. A hidden session never answers one.
    fn blocked(&self, screen: &str) -> Option<String>;

    /// The CLI's folder-trust (or approval) screen for the project: the CLI
    /// has not been trusted there yet. The reason tells the user what to do.
    /// A hidden session never answers it, and Connecto never edits the CLI's
    /// configuration to skip it.
    fn untrusted(&self, screen: &str) -> Option<String>;

    /// Whether the tool reports a submitted prompt by hook in a utility session
    /// (`ToolEvent::PromptSubmitted`). When not, [`UtilityTool::submitted`]
    /// reads it from the screen.
    fn prompt_hook(&self) -> bool;

    /// What the live screen says about the composer.
    fn composer(&self, screen: &str) -> ComposerState;

    /// Whether `submitted` (as a hook or the log reports it) is exactly the
    /// prompt that was `sent`, after removing what this tool wraps a prompt in.
    fn prompt_matches(&self, sent: &str, submitted: &str) -> bool;

    /// The session log of a utility run, from what the start hook reported or
    /// the thread whose turn ended. Only a path inside the profile.
    fn log_path(
        &self,
        reported: Option<&Path>,
        thread: Option<&str>,
        ctx: &HookCtx,
    ) -> Option<PathBuf>;

    /// What the log's text says about the turn that answers `sent`.
    fn read_turn(&self, log: &str, sent: &str) -> LoggedTurn;

    /// A reply that is the tool reporting an error, not model output
    /// (Claude's `API Error: …`). The reason, when it is one.
    fn reply_error(&self, reply: &str) -> Option<String>;
}

/// Everything a contained session launch needs. All paths are real paths.
#[derive(Debug, Clone)]
pub struct LaunchCtx {
    pub entry: PathBuf,
    /// The CLI's version as the store probed it (`--version`), when known.
    /// Hooks a version may not know are registered only when it is.
    pub cli_version: Option<String>,
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
    /// The built-in package's always-on instructions (the end-of-task status
    /// recap, compiled into Connecto), when the project has that package.
    /// Pi, Antigravity and OpenCode get them after the bridge text, by the
    /// same option; Codex through Connecto's `SessionStart` hook, the one
    /// channel that adds to its configured `developer_instructions` (so not
    /// without the hooks). Claude reads its copy from `.claude/rules`. `None`
    /// for a utility session.
    pub package_text: Option<String>,
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
    pub effort: Option<String>,
    /// A short first message for a RESUMED conversation (a note that the
    /// instructions changed). Ignored for a new one.
    pub resume_note: Option<String>,
    /// Set for a utility session, on a tool whose [`ToolAdapter::utility`]
    /// is `Some`.
    pub utility: Option<UtilityLaunch>,
    /// Whether the tool asks before it acts. Not used by a utility session,
    /// which never runs with the bypass.
    pub agent_mode: AgentMode,
    /// The model providers a Pi terminal is given. Pi only.
    pub pi: crate::pi::PiProviders,
    /// How a Claude terminal reaches its model: through TogetherLink, or as
    /// usual. Claude only; a utility session ignores it.
    pub claude: crate::claude::ClaudeProviders,
    /// What a Claude terminal leaves out to spend fewer tokens (the project's
    /// Agent Token Optimization). Claude only; a utility session ignores it.
    pub claude_cuts: crate::claude::ClaudeCuts,
    /// Base URL of a local fake model.
    #[cfg(feature = "test-endpoints")]
    pub fake_model: Option<String>,
}

impl LaunchCtx {
    /// What Pi, Antigravity and OpenCode are given as instructions at launch:
    /// the bridge text, then the package instructions.
    #[must_use]
    pub fn launch_text(&self) -> Option<String> {
        match (&self.bridge.text, &self.package_text) {
            (Some(bridge), Some(package)) => Some(format!("{bridge}\n\n{package}")),
            (Some(text), None) | (None, Some(text)) => Some(text.clone()),
            (None, None) => None,
        }
    }
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
    /// Claude: the main conversation at work (an assistant record that does
    /// not end the turn, or a tool result): a turn is running.
    Activity,
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

    /// How to read a utility session of this tool, or `None` when it has no
    /// utility launch.
    fn utility(&self) -> Option<&dyn UtilityTool> {
        None
    }

    /// How this tool's input line takes keys, for following what the user
    /// has typed and not sent.
    fn draft_keys(&self) -> DraftKeys;

    /// What the screen shows of the input box, or `None` when this tool has no
    /// input box the screen can be read for (a shell). Faint text in the box
    /// (a placeholder or a suggested prompt) is not a draft.
    fn composer(&self, screen: Screen<'_>) -> Option<ComposerView>;
}

/// The hook channel's variables without the bridge's: a utility session gets
/// no bridge socket and no terminal identity.
pub(crate) fn utility_hook_env(ctx: &LaunchCtx) -> Vec<(OsString, OsString)> {
    hook_env(ctx)
        .into_iter()
        .filter(|(name, _)| name != "CONNECTO_BRIDGE_SOCK" && name != "CONNECTO_TERMINAL")
        .collect()
}

/// How a tool's input line takes keys. Connecto follows what the user types
/// into a terminal so it can move an unsent draft out of the way of a message
/// it types itself, and put it back after. Only what is the same in every
/// version is described here; any key whose effect is not certain makes the
/// draft unknown, and an unknown draft is never touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DraftKeys {
    /// The draft can hold newlines (a TUI composer). A shell line cannot: a
    /// newline submits it.
    pub multiline: bool,
    /// ESC CR (Shift+Enter as Connecto sends it) puts a newline in the draft.
    pub esc_enter_newline: bool,
    /// Ctrl-C empties a line that has text (a shell). In a TUI it may instead
    /// interrupt a turn, so what it leaves is not known.
    pub ctrl_c_clears: bool,
    /// A long paste is shown as one placeholder that the cursor moves over and
    /// Backspace may delete whole (Claude's `[Pasted text #1]`, Codex's
    /// `[Pasted Content N chars]`).
    pub collapses_pastes: bool,
}

/// A live screen as a tool's input box is read from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen<'a> {
    /// Every row as drawn.
    pub drawn: &'a str,
    /// The same rows, row for row, with every faint cell drawn as a space:
    /// what a tool dims is a hint (a placeholder, a suggested next prompt,
    /// ghost text of a completion), never what someone typed.
    pub typed: &'a str,
}

impl<'a> Screen<'a> {
    /// A screen known only as text: nothing on it is known to be faint.
    #[must_use]
    pub const fn plain(text: &'a str) -> Self {
        Self {
            drawn: text,
            typed: text,
        }
    }

    /// Every row, trimmed, with its typed text.
    pub(crate) fn rows(self) -> Vec<Row<'a>> {
        let mut typed = self.typed.lines();
        self.drawn
            .lines()
            .map(|drawn| {
                let drawn = drawn.trim();
                Row {
                    drawn,
                    typed: typed.next().map_or(drawn, str::trim),
                }
            })
            .collect()
    }
}

/// One row of a [`Screen`], trimmed: as drawn, and only what was typed on it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Row<'a> {
    pub(crate) drawn: &'a str,
    pub(crate) typed: &'a str,
}

/// What the screen shows of a tool's input box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerView {
    pub state: ComposerState,
    /// The input box's first row of text as drawn, without the prompt glyph
    /// and without what is drawn faint (a hint, ghost text). Claude and Codex
    /// give it whatever the state (a line shaped like the placeholder but
    /// not faint is someone's text); other tools only when `Occupied`.
    pub first_line: String,
    /// The input box's rows after its first, as read from its geometry.
    pub rest: BoxRows,
}

/// What a tool's adapter read of its input box beyond the first row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoxRows {
    /// The box found from its geometry (its borders, the prompt marker at
    /// its column), and what was typed on each of its rows after the first,
    /// trimmed (empty for a row with nothing typed): all of the box is then
    /// `first_line` and these.
    Read(Vec<String>),
    /// The box's rows could not be told apart for certain (a marker or a
    /// border where the box's own cannot be, no end found): nothing about
    /// its contents is proven.
    Ambiguous,
    /// This tool's box is not read beyond its first row.
    NotRead,
}

impl BoxRows {
    /// The rows read, if the box was read and nothing on them is typed.
    #[must_use]
    pub fn all_empty(&self) -> bool {
        matches!(self, Self::Read(rows) if rows.iter().all(String::is_empty))
    }
}

/// A screen's rows as drawn and as typed, each with only its trailing
/// blanks removed: columns are kept, so where a row starts tells a box's
/// own marker or border from typed text that looks like one.
pub(crate) fn raw_rows<'a>(screen: Screen<'a>) -> Vec<(&'a str, &'a str)> {
    let mut typed = screen.typed.lines();
    screen
        .drawn
        .lines()
        .map(|drawn| {
            let drawn = drawn.trim_end();
            (drawn, typed.next().map_or(drawn, str::trim_end))
        })
        .collect()
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
            cli_version: Some("2.1.318".to_owned()),
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
            package_text: None,
            initial_prompt: None,
            session_name: None,
            model: None,
            effort: None,
            resume_note: None,
            utility: None,
            agent_mode: AgentMode::Bypass,
            pi: crate::pi::PiProviders::default(),
            claude: crate::claude::ClaudeProviders::default(),
            claude_cuts: crate::claude::ClaudeCuts::default(),
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
    /// session has (`docs/windows-port.md` 4.8): the helper is an `.exe`, the
    /// hook and bridge channels are named pipes, and the inherited block holds
    /// the Windows system variables.
    pub(crate) fn launch_ctx_windows() -> LaunchCtx {
        let data = r"C:\Users\Jane O'Neil\AppData\Roaming\connecto";
        LaunchCtx {
            cli_version: Some("2.1.318".to_owned()),
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

    /// What a launch hands the model as instructions, read back through each
    /// tool's own option: what Codex's `SessionStart` hook is given (the
    /// bridge text and the recap flag), Pi's extension config, Claude's
    /// `--append-system-prompt`, OpenCode's `instructions` and Antigravity's
    /// launch-text file. Codex's `developer_instructions` and Pi's
    /// `--append-system-prompt` are read too, so a launch that went back to
    /// them (they replace the user's own) still shows here; the test below
    /// refuses them.
    fn given(tool: &str, spec: &LaunchSpec) -> Option<String> {
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let env = |name: &str| {
            spec.env
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        let file = |path: &str| {
            spec.files
                .iter()
                .find(|(p, _)| p.to_string_lossy() == path)
                .map(|(_, body)| String::from_utf8_lossy(body).into_owned())
        };
        let after = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1).cloned())
        };
        match tool {
            "codex" => {
                let parts: Vec<String> = [
                    args.iter()
                        .find_map(|a| a.strip_prefix("developer_instructions=").map(str::to_owned)),
                    env(crate::codex::BRIDGE_TEXT_ENV),
                    // The flag turns on the recap compiled into the hook
                    // helper: it stands for the package text here.
                    (env(crate::codex::RECAP_ENV).as_deref() == Some("1"))
                        .then(|| "PACKAGE RULE".to_owned()),
                ]
                .into_iter()
                .flatten()
                .collect();
                (!parts.is_empty()).then(|| parts.join("\n"))
            }
            "pi" => {
                // The bridge text and the recap in the extension's config,
                // which adds them as prompt sections.
                let config = spec
                    .files
                    .iter()
                    .find(|(p, _)| p.extension().is_some_and(|e| e == "mjs"))
                    .and_then(|(_, body)| {
                        let text = String::from_utf8_lossy(body).into_owned();
                        let line = text.lines().next()?.to_owned();
                        let json = line.strip_prefix("const CONFIG = ")?.strip_suffix(';')?;
                        serde_json::from_str::<serde_json::Value>(json).ok()
                    })
                    .unwrap_or_default();
                let text = |key: &str| config[key].as_str().map(str::to_owned);
                let parts: Vec<String> = [
                    after("--append-system-prompt").and_then(|path| file(&path)),
                    text("bridge"),
                    text("recap"),
                ]
                .into_iter()
                .flatten()
                .collect();
                (!parts.is_empty()).then(|| parts.join("\n"))
            }
            "claude" => after("--append-system-prompt"),
            "opencode" => {
                let config: serde_json::Value =
                    serde_json::from_str(&env("OPENCODE_CONFIG_CONTENT")?).ok()?;
                file(config["instructions"][0].as_str()?)
            }
            "antigravity" => file(&env(crate::antigravity::LAUNCH_TEXT_ENV)?),
            _ => unreachable!(),
        }
    }

    /// The package instructions reach Codex, Pi, Antigravity and OpenCode by
    /// their launch option, after the bridge text or on their own when the
    /// bridge is off, and nothing is passed when the project has none. Claude
    /// is not given them: it reads `.claude/rules` itself. Nothing is written
    /// into the project.
    #[test]
    fn the_package_instructions_reach_every_agent_but_claude_at_launch() {
        let adapters: [(&str, &dyn ToolAdapter); 5] = [
            ("codex", &crate::codex::Codex),
            ("pi", &crate::pi::Pi),
            ("opencode", &crate::opencode::OpenCode),
            ("antigravity", &crate::antigravity::Antigravity),
            ("claude", &crate::claude::Claude),
        ];
        for (tool, adapter) in adapters {
            for bridge in [Some("LAUNCH TEXT"), None] {
                let mut ctx = launch_ctx();
                ctx.bridge.text = bridge.map(str::to_owned);
                ctx.package_text = Some("PACKAGE RULE".to_owned());
                let spec = adapter.launch(&ctx);
                let text = given(tool, &spec).unwrap_or_default();
                assert_eq!(
                    text.contains("PACKAGE RULE"),
                    tool != "claude",
                    "{tool} {bridge:?}: {text}"
                );
                if bridge.is_some() {
                    assert!(text.contains("LAUNCH TEXT"), "{tool}: {text}");
                }
                for (path, _) in &spec.files {
                    assert!(!path.starts_with(&ctx.project), "{tool}: {path:?}");
                }
                // A project without package instructions: as before.
                ctx.package_text = None;
                let text = given(tool, &adapter.launch(&ctx));
                assert_eq!(text.is_some(), bridge.is_some(), "{tool} {bridge:?}");
                assert!(!text.unwrap_or_default().contains("PACKAGE RULE"));
            }
        }
    }

    /// The bridge text adds to the instructions the user configured, on every
    /// agent, new or resumed: no launch uses an option that replaces them
    /// (Codex's `developer_instructions`, Pi's `--append-system-prompt`,
    /// which drops the APPEND_SYSTEM.md Pi would load), and the text still
    /// arrives whole, before the package instructions.
    #[test]
    fn the_bridge_text_never_replaces_the_users_own_instructions() {
        let adapters: [(&str, &dyn ToolAdapter); 5] = [
            ("codex", &crate::codex::Codex),
            ("pi", &crate::pi::Pi),
            ("opencode", &crate::opencode::OpenCode),
            ("antigravity", &crate::antigravity::Antigravity),
            ("claude", &crate::claude::Claude),
        ];
        let bridge = "LAUNCH \"TEXT\"\nwith `connecto` \\ $HOME ünï";
        for (tool, adapter) in adapters {
            for resume in [None, Some("11111111-2222-4333-8444-555555555555")] {
                let mut ctx = launch_ctx();
                ctx.resume = resume.map(str::to_owned);
                ctx.bridge.text = Some(bridge.to_owned());
                ctx.package_text = Some("PACKAGE RULE".to_owned());
                let spec = adapter.launch(&ctx);
                let words: Vec<String> = spec
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                let replacing = match tool {
                    "codex" => words.iter().any(|w| w.contains("developer_instructions")),
                    "pi" => words.iter().any(|w| w == "--append-system-prompt"),
                    _ => false,
                };
                assert!(!replacing, "{tool} {resume:?}: {words:?}");
                let text = given(tool, &spec).unwrap_or_default();
                let at = text.find(bridge);
                assert!(at.is_some(), "{tool} {resume:?}: {text}");
                if tool != "claude" {
                    assert!(at < text.find("PACKAGE RULE"), "{tool} {resume:?}: {text}");
                }
            }
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
