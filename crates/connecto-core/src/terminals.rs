//! The contract between the UI and the terminal registry.
//!
//! The registry owns up to [`MAX_TERMINALS`] terminals, their ids and the saved
//! list. The UI sees only this: a view it reads each frame, and two requests.
//! Every method returns promptly and touches no disk (ADR 0011, ADR 0014).

use std::fmt;
use std::sync::Arc;

use crate::{PaneControl, TerminalSession};

/// Terminals one project may have open at once, stopped ones included. Closed
/// terminals live in the history and do not count (project-tree-ui A2).
pub const MAX_TERMINALS: usize = 16;

/// The longest name a user or agent may give a terminal or a project, in
/// characters. Longer names are refused, never cut.
pub const MAX_NAME_CHARS: usize = 64;

/// What a terminal runs. The two agents, and a plain shell. Further kinds
/// (another agent CLI, say) are added here, and the compiler then points at every
/// place that has to decide what the new kind does; [`ToolKind::ALL`] and the
/// capability methods below are the only other things to extend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolKind {
    Claude,
    Codex,
    /// The user's login shell: no turns, no hooks, no login, no launch mode.
    Shell,
}

impl ToolKind {
    /// Every kind, in the order the UI offers them.
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Shell];

    /// The lowercase name, which is also the first terminal's id.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
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
}

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
    /// Turns that ended since the app started. The UI compares it with the value
    /// it last saw to show "finished, not seen".
    pub finished_turns: u64,
    pub session: Arc<dyn TerminalSession>,
    /// Absent for a plain shell under the `frame-capture` seam.
    pub control: Option<Arc<dyn PaneControl>>,
    /// What the saved list says: whether this terminal starts at the next launch.
    pub desired_running: bool,
    /// Counts what agents did to this terminal: typed text, keys, start, close.
    /// The tab strip shows an activity mark when it moves. It never changes
    /// which tab is visible.
    pub activity: u64,
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

/// User commands on the terminal list. Every method returns promptly: a
/// failure shows in [`TerminalsView::last_error`], not as a wait.
pub trait TerminalsControl: Send + Sync {
    /// The latest list. Never blocks.
    fn view(&self) -> Arc<TerminalsView>;

    /// Ask for a new top-level terminal of `tool`, started at once, with an
    /// optional name. `Err` means the request was refused at once, with the
    /// reason. A later failure shows in the view.
    fn add(&self, tool: ToolKind, name: Option<String>) -> Result<(), String>;

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

    /// Open a new top-level terminal that resumes a closed one's tool session
    /// (`claude --resume <id>`, `codex resume <id>`), with its name. The history
    /// entry stays.
    fn continue_from(&self, history_id: &str) -> Result<(), String>;

    /// Load a saved transcript off the UI thread. It appears in
    /// [`TerminalsView::transcript`].
    fn request_transcript(&self, history_id: &str) -> Result<(), String>;

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
    }

    #[test]
    fn only_a_shell_is_not_an_agent() {
        assert!(ToolKind::Claude.is_agent() && ToolKind::Codex.is_agent());
        assert!(!ToolKind::Shell.is_agent());
    }
}
