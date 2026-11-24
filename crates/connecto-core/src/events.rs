//! What a tool reports about itself, in one vocabulary for all tools.
//!
//! These events update state and nothing else. They have no route to any
//! action: only a user gesture, or the bridge under the Gate, can start one.

use crate::EventText;

/// What a turn end says about the reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Text(EventText),
    /// The turn reported no text.
    None,
    /// There was a reply, and it was over the limit or cut.
    Dropped,
}

impl Reply {
    /// The text, when there is one.
    #[must_use]
    pub const fn text(&self) -> Option<&EventText> {
        match self {
            Self::Text(text) => Some(text),
            Self::None | Self::Dropped => None,
        }
    }
}

/// Claude: one piece of background work a `Stop` lists as still running.
/// Every field but the id is optional: an older Claude sends bare ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundReport {
    pub id: EventText,
    /// `shell`, `subagent`, `monitor`, `workflow`, ...
    pub kind: Option<EventText>,
    pub description: Option<EventText>,
    /// A shell task's command line.
    pub command: Option<EventText>,
}

impl BackgroundReport {
    /// A task known only by its id.
    #[must_use]
    pub const fn id_only(id: EventText) -> Self {
        Self {
            id,
            kind: None,
            description: None,
            command: None,
        }
    }
}

/// A normalized event from a tool's hook channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEvent {
    /// The tool finished starting. `tool_session` is Claude's session id or
    /// Codex's thread id. `turn_log` is the tool's own record of the session.
    Started {
        tool_session: EventText,
        turn_log: Option<EventText>,
    },
    /// The tool names the session it runs in now, with no start of its own:
    /// OpenCode makes its session only when the first prompt is sent. Only
    /// the id a restart resumes changes; the terminal's input and turn are
    /// left as they are.
    SessionNamed { tool_session: EventText },
    /// The tool accepted a prompt. `turn` is Codex's turn id.
    PromptSubmitted {
        prompt: EventText,
        turn: Option<EventText>,
    },
    /// A turn ended.
    TurnEnded {
        /// Codex: the thread's `input-messages` that were kept.
        input: Vec<EventText>,
        /// How many `input-messages` the thread held, before any were dropped
        /// for size. The kept entries are the last of them.
        input_total: usize,
        reply: Reply,
        /// False when the helper could not read the payload: nothing is known
        /// about which turn ended or what still runs.
        lifecycle_known: bool,
        /// Claude: background work still running.
        background: Vec<BackgroundReport>,
        /// Codex: the thread that reported the end.
        thread: Option<EventText>,
        /// Codex: the turn that ended.
        turn: Option<EventText>,
        /// Claude: the turn ended in an API error (`StopFailure`, fired
        /// instead of `Stop`): its error type. The reply is the error text,
        /// never an answer.
        failure: Option<EventText>,
        /// Claude: a `Stop` that follows one another hook blocked
        /// (`stop_hook_active`): the same turn went on and ended again.
        continued: bool,
    },
    /// The tool is asking for the user.
    NeedsUser,
}

impl ToolEvent {
    /// Whether this event closes a turn. These are never dropped for space
    /// while anything else can be.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::TurnEnded { .. })
    }
}
