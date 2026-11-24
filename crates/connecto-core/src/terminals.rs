//! The contract between the UI and the terminal registry.
//!
//! The registry owns up to [`MAX_TERMINALS`] terminals, their ids and the saved
//! list. The UI sees only this: a view it reads each frame, and two requests.
//! Every method returns promptly and touches no disk (ADR 0011, ADR 0014).

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{PaneControl, TerminalSession};

/// Terminals one project may have open at once, stopped ones included. Closed
/// terminals live in the history and do not count (project-tree-ui A2).
pub const MAX_TERMINALS: usize = 16;

/// The largest file sent to a remote host's terminal (dropped or pasted on
/// it), and the most files in one drop. A host from before this limit takes
/// 10 MiB (`connecto_net::wire::LEGACY_UPLOAD_BYTES`); a project chat
/// message's attachments keep their own, smaller limit.
pub const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_UPLOAD_FILES: usize = 4;

/// The longest name a user or agent may give a terminal or a project, in
/// characters. Longer names are refused, never cut.
pub const MAX_NAME_CHARS: usize = 64;

/// What a terminal runs. The five agents, and a plain shell. Further kinds
/// (another agent CLI, say) are added here, and the compiler then points at every
/// place that has to decide what the new kind does; [`ToolKind::ALL`] and the
/// capability methods below are the only other things to extend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolKind {
    Claude,
    Codex,
    /// The Pi coding agent: any provider's models (OpenRouter, Together AI, a
    /// local LM Studio server), chosen inside Pi.
    Pi,
    /// Google's Antigravity CLI (`agy`), the successor of Gemini CLI.
    Antigravity,
    /// OpenCode (`opencode`): any provider's models, signed in to inside
    /// OpenCode (`/connect`).
    OpenCode,
    /// The user's login shell: no turns, no hooks, no login, no launch mode.
    Shell,
}

impl ToolKind {
    /// Every kind, in the order the UI offers them.
    pub const ALL: [Self; 6] = [
        Self::Claude,
        Self::Codex,
        Self::Pi,
        Self::Antigravity,
        Self::OpenCode,
        Self::Shell,
    ];

    /// The lowercase name, which is also the first terminal's id.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Antigravity => "antigravity",
            Self::OpenCode => "opencode",
            Self::Shell => "shell",
        }
    }

    /// The kind with this name, as `connecto add <name>` and saved lists spell it.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// A word for a menu or a message.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Pi => "Pi",
            Self::Antigravity => "Antigravity",
            Self::OpenCode => "OpenCode",
            Self::Shell => "Shell",
        }
    }

    /// Whether this is an agent CLI: it has turns that begin and end, hooks that
    /// report them, a login, a launch mode, a conversation to resume, and
    /// instructions about the `connecto` command pasted into it. A shell has none.
    #[must_use]
    pub const fn is_agent(self) -> bool {
        !matches!(self, Self::Shell)
    }

    /// The models offered for a new terminal of this kind when the tool keeps
    /// no list of its own: what each CLI's `--model` takes, newest first. Pi's
    /// come from its providers, OpenCode's from its own favorite and recent
    /// models, and Codex's from its model cache when there is one, so this is
    /// Codex's fallback only. The one place to update when a
    /// CLI changes its models. It is only offered: `--model` takes any name a
    /// launch accepts, and the tool's own default is always offered too.
    /// Checked on 2026-11-19 against Claude Code's aliases, Codex 0.166.0's
    /// model cache and `agy models`.
    #[must_use]
    pub const fn suggested_models(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["opus", "sonnet", "haiku", "fable", "opusplan"],
            Self::Codex => &[
                "gpt-6.2-sol",
                "gpt-6.2-astra",
                CODEX_TERRA,
                "gpt-6.1-sol",
                "gpt-6-astra",
            ],
            Self::Antigravity => &[
                "gemini-4-argon-high",
                "gemini-4-argon-medium",
                "gemini-3.8-flash-high",
                "gemini-3.8-flash-medium",
                "gemini-3.1-pro-high",
            ],
            Self::Pi | Self::OpenCode | Self::Shell => &[],
        }
    }
}

/// Codex's GPT Terra model, as Codex 0.167 lists it: also the model an
/// Agentic Works job runs on when Codex stands in for Antigravity (ADR 0041).
pub const CODEX_TERRA: &str = "gpt-6.1-terra";

/// Whether the terminal list on disk matches the one in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveState {
    /// The last list written is the list in memory.
    Saved,
    /// A change of `desired` could not be written yet. What survives a
    /// relaunch is the last list that was written, and a retry runs every 30 s.
    Unsaved,
    /// The list file could not be read, so it is never written this run. Add
    /// and remove are refused. Starts and stops work and are held in memory.
    ReadOnly,
}

/// Background work running at least this long is shown as long-running.
pub const LONG_RUNNING: Duration = Duration::from_secs(15 * 60);

/// Work an agent left running after its turn ended: a Claude background
/// shell, subagent, monitor or workflow. While any runs, the terminal's turn
/// has not finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundWork {
    /// The tool's own task id.
    pub id: String,
    /// `shell`, `subagent`, `monitor`, `workflow`, ...; `task` when not said.
    pub kind: String,
    /// What it runs: a shell's command, otherwise its description. Redacted,
    /// on one line, and cut.
    pub label: String,
    /// What the agent said the work is for (Claude's own description of a
    /// background shell, say), when it said something other than the label.
    /// Redacted, on one line, and cut.
    pub description: Option<String>,
    /// When Connecto first saw it running: the turn end that first listed it.
    pub since: Instant,
}

impl BackgroundWork {
    /// A background shell: the only kind Connecto can stop.
    #[must_use]
    pub fn is_shell(&self) -> bool {
        self.kind == "shell"
    }

    #[must_use]
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.since)
    }
}

/// A running time, short: `42 s`, `3 min`, `2 h 20 min`.
#[must_use]
pub fn short_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else {
        format!("{} h {} min", secs / 3600, secs % 3600 / 60)
    }
}

/// What a terminal is doing, for its status dot. Derived by the registry from
/// signals it already has; the UI never infers it from the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TermActivity {
    /// In the list but never started this run.
    #[default]
    NotStarted,
    /// Preflight or launch is running.
    Starting,
    /// The tool cannot start until the user logs in, or its login runs.
    NeedsLogin,
    /// A turn is running, or background work continues after one.
    Working,
    /// The tool asked the user something (the bridge's `needs-user`). Cleared by
    /// the next accepted prompt, the next turn start, or the next user input.
    NeedsUser,
    /// At its prompt.
    Idle,
    /// At its prompt, but it was in the middle of a turn when the app closed
    /// and has not been given a new one since. Cleared by the next turn.
    Interrupted,
    /// A child terminal whose last `--ask` request from its parent has ended
    /// and which is now idle. Only children reach this state.
    Done,
    /// The process ended, or the user stopped it.
    Exited,
    /// Preflight or launch refused it.
    Failed,
    /// A plain shell that is running. Nothing is known about what it is doing:
    /// it has no turns and sends no events, so it is never working, finished or
    /// waiting for the user. Last in the order, because the wire numbers the
    /// states by position.
    Plain,
}

/// What a peer is told when a close would also close a terminal it was not sent.
pub const CLOSE_NOT_ALL_LISTED: &str =
    "that terminal has a child terminal you cannot see, so nothing was closed";

/// One terminal as the sidebar tree draws it.
#[derive(Clone)]
pub struct TerminalEntry {
    /// Stable, never reused within a project. Agents address terminals by it,
    /// so a rename never changes it.
    pub id: String,
    /// The name the user or the starting agent gave it. `None` shows the id.
    pub name: Option<String>,
    /// The id of the terminal whose agent started this one with `connecto add`,
    /// flattened to the top-level ancestor so the tree has one nesting level.
    /// `None` for a terminal the user opened.
    pub parent: Option<String>,
    /// The terminal that actually asked, when it differs from `parent` (a
    /// grandchild), for the "asked by" label. `None` otherwise.
    pub asked_by: Option<String>,
    pub tool: ToolKind,
    /// The status dot.
    pub state: TermActivity,
    /// A turn is running now. The dot also shows Working while the tool sits at
    /// its prompt with background work still going (a background shell or
    /// subagent); that is not a turn. A restart to update waits only for this.
    pub in_turn: bool,
    /// The background work running while the tool sits at its prompt after a
    /// turn, oldest first. Empty otherwise, and for a remote terminal.
    pub background: Vec<BackgroundWork>,
    /// Turns that ended since the app started. The UI compares it with the value
    /// it last saw to show "finished, not seen".
    pub finished_turns: u64,
    pub session: Arc<dyn TerminalSession>,
    /// Absent for a plain shell under the `frame-capture` seam.
    pub control: Option<Arc<dyn PaneControl>>,
    /// What the saved list says: whether this terminal starts at the next launch.
    pub desired_running: bool,
    /// The id to show when `id` is not the one its own machine uses: a remote
    /// terminal's id on its host (`claude-12`), where `id` is the wire's
    /// number. `None` shows `id`.
    pub shown_id: Option<String>,
    /// Counts what agents did to this terminal: typed text, keys, start, close.
    /// The tab strip shows an activity mark when it moves. It never changes
    /// which tab is visible.
    pub activity: u64,
}

impl TerminalEntry {
    /// The id its own machine gives it: `shown_id` when set, else `id`.
    pub fn display_id(&self) -> &str {
        self.shown_id.as_deref().unwrap_or(&self.id)
    }

    /// The terminal whose agent started this one, which it belongs to: the
    /// one that asked, or the parent when that is the same terminal or the
    /// asker is gone. `None` for a terminal the user opened.
    pub fn owner(&self) -> Option<&str> {
        self.asked_by.as_deref().or(self.parent.as_deref())
    }
}

impl fmt::Debug for TerminalEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TerminalEntry")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("parent", &self.parent)
            .field("tool", &self.tool)
            .field("state", &self.state)
            .field("desired_running", &self.desired_running)
            .field("shown_id", &self.shown_id)
            .field("activity", &self.activity)
            .finish_non_exhaustive()
    }
}

/// A closed terminal, as the history sheet lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Unique within the project. Not a terminal id.
    pub id: String,
    /// The id the terminal had while it was open.
    pub terminal_id: String,
    pub name: Option<String>,
    pub tool: ToolKind,
    /// The parent's id at close, if it was a child.
    pub parent: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub closed_at_ms: u64,
    /// The first task it was given (the first `--ask` or prompt), redacted and
    /// cut to a few hundred characters. For search and the list row.
    pub task: Option<String>,
    /// The start of its last reply, redacted and cut the same way.
    pub last_reply: Option<String>,
    /// Whether Continue can resume the tool's session (a session id was kept).
    pub resumable: bool,
}

/// A saved transcript the user asked to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub history_id: String,
    /// Redacted plain text of the scrollback at close.
    pub text: Arc<String>,
}

/// Everything the sidebar tree draws for one project.
#[derive(Clone, Debug)]
pub struct TerminalsView {
    /// In list order. Children follow their parent's position rules in the UI,
    /// not here.
    pub terminals: Vec<TerminalEntry>,
    /// Newest first. Loaded when the project opens.
    pub history: Arc<Vec<HistoryEntry>>,
    /// The transcript last asked for with [`TerminalsControl::request_transcript`],
    /// once loaded.
    pub transcript: Option<Transcript>,
    pub save: SaveState,
    /// The latest failure of an add or remove the UI asked for, until the next
    /// request or a successful save.
    pub last_error: Option<String>,
    /// A standing warning, such as an unreadable list that fell back to the
    /// default two. Empty when there is none.
    pub warning: Option<String>,
}

impl TerminalsView {
    /// Whether another terminal would stay within the cap.
    #[must_use]
    pub fn can_add(&self) -> bool {
        self.terminals.len() < MAX_TERMINALS && self.save != SaveState::ReadOnly
    }
}

/// The models another machine offers for a new terminal of one tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteModels {
    /// The model it starts on when none is chosen, when known.
    pub default: Option<String>,
    /// The models to offer, in the host's order. Empty: only the default.
    pub models: Vec<String>,
}

/// User commands on the terminal list. Every method returns promptly: a
/// failure shows in [`TerminalsView::last_error`], not as a wait.
pub trait TerminalsControl: Send + Sync {
    /// The latest list. Never blocks.
    fn view(&self) -> Arc<TerminalsView>;

    /// Ask for a new top-level terminal of `tool`, started at once, with an
    /// optional name. `Err` means the request was refused at once, with the
    /// reason. A later failure shows in the view.
    fn add(&self, tool: ToolKind, name: Option<String>) -> Result<(), String>;

    /// As [`TerminalsControl::add`], on a model of the person's choosing
    /// (Pi: `provider/model`). `None` is the tool's own default.
    fn add_on_model(
        &self,
        tool: ToolKind,
        name: Option<String>,
        model: Option<String>,
    ) -> Result<(), String> {
        let _ = model;
        self.add(tool, name)
    }

    /// The models a new `tool` terminal is offered when another machine
    /// starts it (a remote host's project), as that machine says. `None` for
    /// this machine's projects, whose models come from this machine's
    /// providers. Never blocks: empty until the host has answered.
    fn remote_models(&self, tool: ToolKind) -> Option<RemoteModels> {
        let _ = tool;
        None
    }

    /// Whether a new `tool` terminal can be started where these terminals
    /// run. Always on this machine; a remote host from before a tool existed
    /// would start another one in its place, so it is not offered there.
    fn offers(&self, tool: ToolKind) -> bool {
        let _ = tool;
        true
    }

    /// Which agents [`TerminalsControl::offers`] is not known yet: a remote
    /// host's project settings (its Tools setting) are still on their way.
    /// The agents are shown but cannot be chosen until they are here. Never
    /// on this machine, nor once a host has said it cannot give them.
    fn agents_loading(&self) -> bool {
        false
    }

    /// The file at `path` as terminal `id` printed it, read from the machine
    /// it runs on (a remote host's): that machine's full path to it, and its
    /// bytes. This machine's terminals read their files directly, so by
    /// default there is none. It waits for the whole file: never call it
    /// from the UI thread.
    ///
    /// # Errors
    /// Why not, as the window shows it.
    fn fetch_file(&self, id: &str, path: &str) -> Result<(String, Vec<u8>), String> {
        let _ = (id, path);
        Err("this terminal's files are on this computer".to_owned())
    }

    /// Send a file dropped or pasted on terminal `id` to the machine it runs
    /// on (a remote host's), named after `name`: the path the host saved it
    /// at, to type into that terminal. At most [`MAX_UPLOAD_BYTES`]. This
    /// machine's terminals read dropped files where they are, so by default
    /// there is none. It waits for the whole transfer: never call it from the
    /// UI thread.
    ///
    /// # Errors
    /// Why not, as the window shows it.
    fn upload_file(&self, id: &str, name: &str, bytes: &[u8]) -> Result<String, String> {
        let _ = (id, name, bytes);
        Err("this terminal reads files on this computer".to_owned())
    }

    /// Close a terminal: stop it if it runs, save its transcript and session id
    /// to the history, and take it out of the list. Its children are closed
    /// with it, each saved to the history too. A project may end with no terminals. Same reporting as
    /// [`TerminalsControl::add`].
    /// As [`TerminalsControl::add`], but it returns the id of the terminal it
    /// made, so a caller can tie that exact terminal to something. It waits
    /// for the registry, so never call it from the UI thread. For a terminal
    /// that opens with `prompt`
    /// as its first message, or that picks the tool session `resume` up again
    /// (the prompt is then used only if the resume fails and a new
    /// conversation begins). `resume_note` is a short first message sent into
    /// the resumed conversation.
    fn add_with(
        &self,
        tool: ToolKind,
        name: Option<String>,
        prompt: Option<String>,
        resume: Option<String>,
        resume_note: Option<String>,
    ) -> Result<String, String> {
        let _ = (tool, name, prompt, resume, resume_note);
        Err("not available".to_owned())
    }

    /// A new terminal of `tool` named `name`, on `model`, that opens with
    /// `prompt` as its first message; its id. It waits for the registry, so
    /// never call it from the UI thread.
    ///
    /// # Errors
    /// Why not (a model name that is not one, the list full or read-only).
    fn add_prompted(
        &self,
        tool: ToolKind,
        name: String,
        prompt: String,
        model: Option<String>,
    ) -> Result<String, String> {
        let _ = (tool, name, prompt, model);
        Err("not available".to_owned())
    }

    /// Open a fresh project terminal and submit its first work item.
    fn add_work(
        &self,
        _tool: ToolKind,
        _name: String,
        _prompt: String,
        _model: Option<String>,
        _effort: Option<String>,
        _epoch: u64,
    ) -> Result<String, String> {
        Err("Work sessions are unavailable for this project.".into())
    }

    /// The models `tool` itself lists now for a new terminal here, when it
    /// keeps such a list (Codex's model cache). `None`: no list of its own.
    fn offered_models(&self, tool: ToolKind) -> Option<Vec<String>> {
        let _ = tool;
        None
    }

    /// The tool's own session id for a terminal, once it has reported one.
    fn session_of(&self, id: &str) -> Option<String> {
        let _ = id;
        None
    }

    fn close(&self, id: &str) -> Result<(), String>;

    /// Set or clear (`None`) a terminal's name. The id never changes.
    fn rename(&self, id: &str, name: Option<String>) -> Result<(), String>;

    /// Restart an exited or failed terminal in place, same id and name.
    fn restart(&self, id: &str) -> Result<(), String>;

    /// Stop the background shells a terminal's agent left running (SIGTERM,
    /// then SIGKILL after a short grace), or only the one with the task id
    /// `task`. Other background work is not stopped. `Ok` says what was
    /// stopped; `Err` why nothing was. The default refuses.
    fn stop_background(&self, id: &str, task: Option<&str>) -> Result<String, String> {
        let _ = (id, task);
        Err("Stopping background work is not available here.".to_owned())
    }

    /// [`Self::stop_background`], answered only once the shells were looked
    /// for, so `Err` also says why none could be stopped (one that cannot be
    /// told apart from another, or has ended). For a caller that can wait, as
    /// a peer's request does; a window calls [`Self::stop_background`], which
    /// never waits. By default the same call.
    fn stop_background_now(&self, id: &str, task: Option<&str>) -> Result<String, String> {
        self.stop_background(id, task)
    }

    /// Close `id` and every terminal under it, but only if each of them is in
    /// `allowed` (entry ids). The set is worked out where the close executes,
    /// so a terminal added after the caller looked is part of it, and is refused
    /// if it is not allowed; nothing is closed then. The default refuses.
    ///
    /// # Errors
    /// [`CLOSE_NOT_ALL_LISTED`] when some terminal under `id` is not allowed; the
    /// registry's own reason otherwise.
    fn close_within(&self, id: &str, allowed: &[String]) -> Result<(), String> {
        let _ = (id, allowed);
        Err(CLOSE_NOT_ALL_LISTED.to_owned())
    }

    /// Open a new top-level terminal that resumes a closed one's tool session
    /// (`claude --resume <id>`, `codex resume <id>`), with its name. The history
    /// entry stays.
    fn continue_from(&self, history_id: &str) -> Result<(), String>;

    /// Load a saved transcript off the UI thread. It appears in
    /// [`TerminalsView::transcript`].
    fn request_transcript(&self, history_id: &str) -> Result<(), String>;

    /// Keep [`TerminalsView::history`] fresh while the window shows it. A
    /// project on another machine fetches it from there, never waiting; this
    /// machine's history is always loaded, so by default nothing happens.
    fn watch_history(&self) {}

    /// The saved transcript of `history_id`, read for another machine
    /// without changing [`TerminalsView::transcript`], which is the window's.
    /// It waits for the disk, so never call it from the UI thread.
    ///
    /// # Errors
    /// Why not, as the window shows it.
    fn read_transcript(&self, history_id: &str) -> Result<String, String> {
        let _ = history_id;
        Err("transcripts are not read for other machines here".to_owned())
    }

    /// Why the history could not be read or acted on, for the history sheet:
    /// a project on another machine says why that machine did not answer.
    fn history_error(&self) -> Option<String> {
        None
    }

    /// The package settings of a project that another machine keeps, shown
    /// under `shown_as` (the id the window knows the project by). `None` for
    /// this machine's projects, whose settings are this machine's.
    fn remote_packages(&self, shown_as: &str) -> Option<Arc<dyn crate::PackagesControl>> {
        let _ = shown_as;
        None
    }

    /// As [`TerminalsControl::remote_packages`], for Agent Token Optimization.
    fn remote_tokens(&self, shown_as: &str) -> Option<Arc<dyn crate::TokenControl>> {
        let _ = shown_as;
        None
    }

    /// As [`TerminalsControl::remote_packages`], for the project's
    /// operations (ADR 0031): viewed, approved and run on that machine.
    fn remote_ops(&self) -> Option<Arc<dyn crate::ops::OpsControl>> {
        None
    }

    /// As [`TerminalsControl::remote_packages`], for the project's
    /// functional memory (ADR 0033): its status, and Update now asked of
    /// that machine.
    fn remote_functional_memory(
        &self,
    ) -> Option<Arc<dyn crate::functional_memory::FunctionalMemoryControl>> {
        None
    }

    /// Start a hidden titling run for terminal `id`. Never blocks. `Err` means
    /// refused at once (unknown terminal, or a run for it is already in
    /// flight). Progress shows in [`TerminalsControl::utility_runs`]; on success
    /// the terminal is renamed.
    fn start_titling(&self, id: &str) -> Result<(), String> {
        let _ = id;
        Err("utility sessions are not available".to_owned())
    }

    /// This project's latest utility runs, newest first, at most
    /// [`crate::MAX_UTILITY_RUNS`]. Never blocks.
    fn utility_runs(&self) -> Arc<Vec<crate::UtilityRun>> {
        Arc::new(Vec::new())
    }

    /// The hidden session of utility run `run`, as an entry the main area can
    /// show and type into like a terminal (`session` renders, `control` is the
    /// pane). Its id is `utility-<run>`; it is not in [`TerminalsView::terminals`].
    /// Live while the run is going, and at its final screen afterwards, for the
    /// latest few runs. `None` once the session has been closed (older runs, a
    /// closed project, or app restart). Never blocks.
    fn utility_pane(&self, run: u64) -> Option<TerminalEntry> {
        let _ = run;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_name_that_finds_it_again() {
        for kind in ToolKind::ALL {
            assert_eq!(ToolKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(ToolKind::from_name("gemini"), None);
        assert_eq!(ToolKind::Shell.name(), "shell");
        assert_eq!(ToolKind::from_name("opencode"), Some(ToolKind::OpenCode));
    }

    #[test]
    fn only_a_shell_is_not_an_agent() {
        assert!(ToolKind::Claude.is_agent() && ToolKind::Codex.is_agent());
        assert!(ToolKind::Pi.is_agent() && ToolKind::Antigravity.is_agent());
        assert!(ToolKind::OpenCode.is_agent());
        assert!(!ToolKind::Shell.is_agent());
    }

    #[test]
    fn a_running_time_reads_short() {
        assert_eq!(short_age(Duration::from_secs(42)), "42 s");
        assert_eq!(short_age(Duration::from_secs(185)), "3 min");
        assert_eq!(short_age(Duration::from_secs(8400)), "2 h 20 min");
    }

    #[test]
    fn only_a_shell_task_is_a_shell_and_its_age_never_goes_negative() {
        let since = Instant::now();
        let work = |kind: &str| BackgroundWork {
            id: "b1".to_owned(),
            kind: kind.to_owned(),
            label: "sleep 100".to_owned(),
            description: None,
            since,
        };
        assert!(work("shell").is_shell());
        assert!(!work("subagent").is_shell());
        let later = since + Duration::from_secs(5);
        assert_eq!(work("shell").age(later), Duration::from_secs(5));
        let mut ahead = work("shell");
        ahead.since = later;
        assert_eq!(ahead.age(since), Duration::ZERO);
    }
}
