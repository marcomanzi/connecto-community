//! What the terminals hand to the memory engine (spec §5.3), and the
//! non-blocking way in.
//!
//! The session registry (`connecto-session::memory_capture`) observes three
//! things per Claude or Codex terminal of a project: an accepted prompt, the
//! end of a turn and the end of the session. The app turns each into a
//! [`Captured`] and offers it to [`CaptureHandle::offer`], which never waits:
//! when the bounded channel is full the event is counted in
//! [`CaptureHandle::dropped`] (shown in `status` and the UI as
//! `Backlog::dropped_on_full`) and the terminal goes on.
//!
//! Every turn is captured, whatever it did: there is no filter on diffs,
//! commits or tool use (spec §5.10). What becomes of an event (redaction,
//! deduplication, the internal-event rule, evidence kinds, git observations)
//! is [`crate::ingest`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use connecto_core::memory::Provider;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

/// Events waiting between the terminals and the engine.
pub const CAPTURE_QUEUE: usize = 1024;

/// How a turn's reply arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedReply {
    Text(String),
    /// The turn reported no text.
    None,
    /// There was a reply, too large for the hook: read it from the log.
    Dropped,
}

/// Who wrote a prompt, as the registry knows it. Only [`Author::User`] is
/// ever a user statement; everything else, unknown included, is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    /// Typed or pasted in the app's window, with nothing else typed.
    User,
    /// Typed by Connecto for another agent or an automation.
    Agent,
    /// Connecto's own notice (internal).
    Connecto,
    Unknown,
}

impl Author {
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Connecto => "connecto",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        [Self::User, Self::Agent, Self::Connecto, Self::Unknown]
            .into_iter()
            .find(|a| a.word() == word)
    }
}

/// What happened in a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Happened {
    /// The tool accepted a prompt.
    Prompt {
        text: String,
        author: Author,
        /// The hook cut it: the text is not the whole prompt.
        truncated: bool,
    },
    /// A turn ended.
    TurnEnd { reply: CapturedReply },
    /// The session exited or its terminal was closed. `how` is a short word
    /// from the registry ("exited: 0", "the terminal closed").
    SessionEnd { how: String },
}

/// One observation from one terminal of one project. The text is raw: it is
/// redacted by the engine before anything is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub project_id: String,
    /// The Connecto terminal id.
    pub terminal: String,
    pub provider: Provider,
    /// Claude's session id or Codex's thread id, once known.
    pub tool_session: Option<String>,
    /// Unix milliseconds the terminal's current session started: tells two
    /// runs of the same tool session apart.
    pub session_start_ms: u64,
    /// Codex's turn id, else the prompt's ordinal in this session (`#3`).
    pub turn: String,
    /// The tool's own session log, when known.
    pub log: Option<PathBuf>,
    pub what: Happened,
}

/// The sending end, for the app's sink. Cheap to clone, never blocks.
#[derive(Clone)]
pub struct CaptureHandle {
    tx: Sender<Captured>,
    dropped: Arc<AtomicU64>,
}

impl CaptureHandle {
    /// A handle and the receiving end, with room for `capacity` events.
    #[must_use]
    pub fn channel(capacity: usize) -> (Self, Receiver<Captured>) {
        let (tx, rx) = bounded(capacity.max(1));
        (
            Self {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    /// Hand an event over without waiting. False when it was dropped (the
    /// queue was full, or the engine has stopped); a full queue is counted.
    pub fn offer(&self, event: Captured) -> bool {
        match self.tx.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Events dropped because the queue was full, since the engine started.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Count an event dropped further on (a project's own queue was full).
    pub(crate) fn count_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(n: u32) -> Captured {
        Captured {
            project_id: "p".to_owned(),
            terminal: "t".to_owned(),
            provider: Provider::Claude,
            tool_session: None,
            session_start_ms: 0,
            turn: format!("#{n}"),
            log: None,
            what: Happened::Prompt {
                text: "hi".to_owned(),
                author: Author::User,
                truncated: false,
            },
        }
    }

    #[test]
    fn a_full_queue_counts_and_never_blocks() {
        let (handle, rx) = CaptureHandle::channel(2);
        assert!(handle.offer(event(1)));
        assert!(handle.offer(event(2)));
        let started = std::time::Instant::now();
        assert!(!handle.offer(event(3)));
        assert!(!handle.offer(event(4)));
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        assert_eq!(handle.dropped(), 2);
        assert_eq!(rx.try_iter().count(), 2);
        drop(rx);
        // A stopped engine: not counted as a full queue.
        assert!(!handle.offer(event(5)));
        assert_eq!(handle.dropped(), 2);
    }
}
