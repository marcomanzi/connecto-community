//! The bridge: the `connecto` command's server and the state behind it (ADR 0016).
//!
//! Three rules hold it together. Session actors stay the only writers to a PTY.
//! The hook socket stays one-way: anything that needs an answer goes over the
//! bridge socket. And the bridge never decides on untrusted content: text from
//! terminals is stored and handed over, and generated text holds only ids that
//! Connecto made.

pub(crate) mod render;
pub(crate) mod server;
pub(crate) mod state;

use std::sync::Arc;
use std::time::{Duration, Instant};

use connecto_gate::{Admitted, Denial};
use crossbeam_channel::Sender;

use crate::turn::RequestEnd;

/// How long a handler waits for an outcome before it answers `timeout`.
pub(crate) const HANDLER_WAIT: Duration = Duration::from_secs(30);
/// Depth of an actor's queue of agent commands.
pub(crate) const AGENT_QUEUE: usize = 16;

/// What an actor's write came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Enter was written (or the keys were).
    Typed,
    /// Refused by the Gate, before any paste.
    Refused(Denial),
    /// A Pause came after the paste: the text may be in the input box.
    PausedBeforeEnter,
    /// The target restarted after the text was pasted. Nothing more was typed.
    Restarted,
    /// A wake found its target working and typed nothing.
    Busy,
    /// The target never showed an input box that could take the text, or is
    /// waiting at a screen for a person. Nothing was pasted; `why` says what
    /// the screen shows.
    NotReady { why: String },
    /// The target has no process. `pasted` says whether text had gone in.
    NotRunning { pasted: bool },
    /// The write failed.
    Failed { pasted: bool, why: String },
}

/// Which kind of write an outcome is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Send,
    Keys,
    Wake,
}

/// A write an agent asked for, or Connecto made for it. Each carries the
/// instant it was made, which no queue changes: a text send's deadline runs
/// from there.
pub enum AgentCommand {
    Type {
        id: String,
        source: String,
        text: String,
        admitted: Admitted,
        reply: Sender<WriteOutcome>,
        enqueued: Instant,
    },
    Keys {
        id: String,
        source: String,
        keys: Vec<Vec<u8>>,
        admitted: Admitted,
        reply: Sender<WriteOutcome>,
        enqueued: Instant,
    },
    Wake {
        id: String,
        text: String,
        admitted: Admitted,
        reply: Sender<WriteOutcome>,
        enqueued: Instant,
    },
}

impl AgentCommand {
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Type { id, .. } | Self::Keys { id, .. } | Self::Wake { id, .. } => id,
        }
    }

    /// When the command was made.
    pub(crate) const fn enqueued(&self) -> Instant {
        match self {
            Self::Type { enqueued, .. }
            | Self::Keys { enqueued, .. }
            | Self::Wake { enqueued, .. } => *enqueued,
        }
    }

    pub(crate) const fn kind(&self) -> WriteKind {
        match self {
            Self::Type { .. } => WriteKind::Send,
            Self::Keys { .. } => WriteKind::Keys,
            Self::Wake { .. } => WriteKind::Wake,
        }
    }
}

/// What an actor reports to bridge state through its lossless outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Typing {
        id: String,
        kind: WriteKind,
        outcome: WriteOutcome,
    },
    /// A prompt was accepted.
    Prompt,
    /// A request id this actor typed was seen at prompt submission.
    Bound {
        request: String,
    },
    Ended {
        request: String,
        end: RequestEnd,
    },
    Exited {
        status: String,
    },
}

/// An event that may be lost: a refused send raises a counter instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Began,
    Finished,
    Interrupted,
    NeedsUser,
    LeftWorking,
}

impl Signal {
    pub(crate) const fn word(self) -> Option<&'static str> {
        match self {
            Self::Began => Some("began"),
            Self::Finished => Some("finished"),
            Self::Interrupted => Some("interrupted"),
            Self::NeedsUser => Some("needs-user"),
            Self::LeftWorking => None,
        }
    }
}

/// Who called: found from the process tree, as an id and attempt read from one
/// live session.
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    pub slot: usize,
    pub uid: u64,
    pub id: String,
    pub attempt: u64,
}

/// The sender bridge state listens on.
pub(crate) type StateTx = Sender<state::Msg>;

/// Shared pointer to the state's mailbox, set when the bridge starts.
pub(crate) type StateLink = Arc<std::sync::OnceLock<StateTx>>;
