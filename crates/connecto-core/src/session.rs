//! The contract between the UI and a running terminal session.
//!
//! Every method must return promptly: they are called on the UI thread, which
//! may never block on a child process, a lock holder or the network.

use crate::{GridSize, TermSnapshot};

/// Lifecycle of the child process, as far as it is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Running,
    /// The child ended. `signal` describes the signal when it was killed by one.
    Exited {
        code: u32,
        signal: Option<String>,
    },
    /// The child's fate could not be determined. Never shown as idle or exited.
    Unknown {
        reason: String,
    },
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Exited {
                signal: Some(signal),
                ..
            } => write!(f, "terminated: {signal}"),
            Self::Exited { code, .. } => write!(f, "exited with code {code}"),
            Self::Unknown { reason } => write!(f, "status unknown: {reason}"),
        }
    }
}

/// Result of asking for a fresh snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotOutcome {
    /// The snapshot was refreshed.
    Updated,
    /// Nothing changed since this snapshot was taken.
    Unchanged,
    /// The emulator is busy. Keep the last frame and poll again next frame.
    Contended,
    /// The emulator failed and will not produce more frames.
    Failed,
}

/// Result of a resize request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeOutcome {
    Applied,
    /// The emulator is busy. Retry on the next frame.
    Retry,
    /// The session can no longer be resized.
    Failed,
}

/// Why input was not accepted. The UI shows this; it never waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InputRefused {
    #[error("session not accepting input: input queue is full")]
    QueueFull,
    #[error("session not accepting input: session has ended")]
    Closed,
}

/// A terminal session as the UI sees it.
pub trait TerminalSession: Send + Sync {
    /// Refresh `out` if the screen changed. Never blocks.
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome;

    /// Queue bytes for the child. Never blocks.
    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused>;

    /// Apply a new size to the emulator and the PTY. Never blocks.
    fn resize(&self, size: GridSize) -> ResizeOutcome;

    /// Scroll the history by whole lines; positive moves back in time.
    /// Returns false when the emulator was busy and nothing happened.
    fn scroll(&self, lines: i32) -> bool;

    /// Current child status.
    fn status(&self) -> SessionStatus;

    /// Resize on behalf of a remote peer that has the size (see
    /// [`TerminalSession::set_remote_size_control`]). A session with no such
    /// notion treats it as an ordinary resize.
    fn resize_remote(&self, size: GridSize) -> ResizeOutcome {
        self.resize(size)
    }

    /// Whether this terminal's window should send mouse events to it when the
    /// program has mouse reporting on. A remote terminal does; a local one still
    /// selects text with the mouse.
    fn forwards_mouse(&self) -> bool {
        false
    }

    /// A remote peer takes (`true`) or gives back (`false`) the terminal's
    /// size. While it has it, [`TerminalSession::resize`] only remembers the
    /// local view's wish and changes nothing; on giving it back, the size
    /// becomes the last local wish. Never blocks for long.
    fn set_remote_size_control(&self, on: bool) {
        let _ = on;
    }
}

/// Sees a session's output on the reader thread, in the same step as the
/// emulator, so a remote host can number the bytes the screen has taken.
///
/// `feed` must call `emulator_feed(bytes)` exactly once, and return quickly.
pub trait OutputTap: Send + Sync {
    fn feed(&self, bytes: &[u8], emulator_feed: &mut dyn FnMut(&[u8]));
}

/// Makes the tap for a terminal's next session. Remote hosting is the only
/// implementation; the launcher asks it once per session, so a restart gets a
/// fresh numbering that matches its fresh screen.
pub trait TapFactory: Send + Sync {
    /// `key` names the terminal as `<project id>/<terminal id>`.
    fn tap_for(&self, key: &str) -> std::sync::Arc<dyn OutputTap>;
}
