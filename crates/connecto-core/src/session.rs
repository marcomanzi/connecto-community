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

/// A change to the text selection. Cells are `(row, col)` on the screen as it
/// is shown now; the session anchors them to the text, so the selection stays
/// on it while the view scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectOp {
    /// A drag began on this cell. Nothing is selected until it moves.
    Start {
        row: u16,
        col: u16,
    },
    /// The drag is over this cell: everything from the start to it, both
    /// included, is selected.
    Extend {
        row: u16,
        col: u16,
    },
    /// Every cell of the screen as shown.
    All,
    Clear,
}

/// The selected text, read without waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionText {
    /// Nothing is selected, or the selected text has gone (it scrolled out of
    /// history or was rewritten).
    Nothing,
    Text(String),
    /// The emulator is busy. Ask again on the next frame.
    Busy,
}

/// Why input was not accepted. The UI shows this; it never waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InputRefused {
    #[error("session not accepting input: input queue is full")]
    QueueFull,
    #[error("session not accepting input: session has ended")]
    Closed,
    /// Connecto is typing into the terminal: the bytes would be held back
    /// and written later, so they were refused ([`TerminalSession::send_input_if`]).
    #[error("session not accepting input now: Connecto is typing into it")]
    Held,
    /// What the write was conditional on no longer holds: the terminal took
    /// other input, restarted, or its input line is not as expected
    /// ([`TerminalSession::send_input_if`]).
    #[error("session not accepting input: the terminal changed since it was read")]
    Stale,
}

/// How many writes a terminal's input has taken in one run of its program:
/// `epoch` tells the runs apart, so a restart is another generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputGeneration {
    pub epoch: u64,
    pub writes: u64,
}

impl InputGeneration {
    /// The generation after one more write.
    #[must_use]
    pub const fn next(self) -> Self {
        Self {
            epoch: self.epoch,
            writes: self.writes + 1,
        }
    }
}

/// What must still hold when a write goes in ([`TerminalSession::send_input_if`]),
/// checked under the terminal's input lock, together with the write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputCondition {
    /// No other write since this generation: nobody pressed a key, Connecto
    /// typed nothing, the program did not restart.
    pub generation: Option<InputGeneration>,
    /// The agent's input line holds exactly this, known from every key typed
    /// into it and drawn so on the screen.
    pub composer: Option<String>,
    /// The agent's input line holds exactly this as every key typed into it
    /// says (the draft tracker), however it is drawn: for an Enter right
    /// after the writer's own paste into a line proven empty, on the
    /// generation right after it (nothing else typed since), where a
    /// collapsed or wrapped drawing cannot be read back.
    pub typed: Option<String>,
    /// For `composer: Some("")` only (an empty line), from a tool whose input
    /// box is not read beyond its first row: the empty line rests on every
    /// key typed into it (the draft tracker) and on the tool not drawing a
    /// draft. For a call's own prompts, whose Enter then goes in only on the
    /// generation right after the paste (ADR 0034).
    pub unread_box_ok: bool,
    /// The screen is still at this generation (a snapshot's `generation`):
    /// nothing drawn since it was read, checked with the screen held
    /// through the enqueue.
    pub output: Option<u64>,
}

/// Recent submitted prompts and completed replies for the project chat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatHistory {
    pub revision: u64,
    /// True for a user prompt, false for a completed assistant reply.
    pub messages: Vec<(bool, String)>,
}

pub const APP_SESSION: &str = "app-session";

/// The folder under Connecto's data folder that holds, per project
/// (`<this>/<project id>`), the files a terminal is given by path: chat
/// uploads, and images pasted or dropped as bytes. Agents that confine their
/// reads are given read access to it.
pub const ATTACHMENTS_DIR: &str = "chat-attachments";

/// Keeps a file a terminal was given only as bytes (an image pasted from the
/// clipboard), so the terminal can be given its path instead.
pub trait PastedFiles: Send + Sync {
    /// Save `bytes`, named after `name`, for this machine's project
    /// `project` (its workspace id). The path written, which the caller
    /// pastes. May take a moment: never called on the UI thread.
    ///
    /// # Errors
    /// Why it was not saved, in plain words.
    fn save(&self, project: &str, name: &str, bytes: &[u8]) -> Result<std::path::PathBuf, String>;
}

/// A terminal session as the UI sees it.
pub trait TerminalSession: Send + Sync {
    fn chat_history(&self) -> Option<std::sync::Arc<ChatHistory>> {
        None
    }
    /// The last received snapshot for display during an attachment retry.
    fn cached_chat_history(&self) -> Option<std::sync::Arc<ChatHistory>> {
        None
    }
    /// A problem reported by the attached peer stream, for the chat view.
    fn chat_error(&self) -> Option<String> {
        None
    }
    /// Moves each time the session follows its terminal over a new link (a
    /// remote terminal reattached, as after a restart on its host), so work
    /// begun for one attachment can tell it is no longer current. Always 0
    /// for a session with no link.
    fn attachment_generation(&self) -> u64 {
        0
    }
    /// Whether the host serving this chat accepts files.
    fn chat_attachments_supported(&self) -> bool {
        false
    }

    /// Upload one staged file and return its path on the host.
    ///
    /// # Errors
    /// When the session no longer supports file transfer, or transfer fails.
    fn upload_chat_attachment(
        &self,
        _project: &str,
        _token: &str,
        _name: &str,
        _bytes: &[u8],
    ) -> Result<String, String> {
        Err("This host can't receive files. Update Connecto on the host.".to_owned())
    }

    /// Queue input only on an attachment that has served project chat.
    ///
    /// # Errors
    /// When chat is unavailable or input cannot be queued.
    fn send_chat_input(&self, _bytes: Vec<u8>) -> Result<(), InputRefused> {
        Err(InputRefused::Closed)
    }

    /// Refresh `out` if the screen changed. Never blocks.
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome;

    /// The live screen and up to `max_cells` cells of the history above it,
    /// read together (see `Emulator::try_capture`), so a peer shown this
    /// terminal can scroll back too. `flush` ends a synchronized update that is
    /// holding output back instead of answering `Contended`. A session with no
    /// history gives the screen as [`TerminalSession::poll_snapshot`] does and
    /// no history.
    fn poll_capture(
        &self,
        max_cells: usize,
        flush: bool,
        screen: &mut TermSnapshot,
        history: &mut TermSnapshot,
    ) -> SnapshotOutcome {
        let _ = (max_cells, flush);
        *history = TermSnapshot::default();
        self.poll_snapshot(screen)
    }

    /// Queue bytes for the child. Never blocks.
    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused>;

    /// The writes its input has taken so far, in this run of its program.
    /// None for a session that does not count them.
    fn input_generation(&self) -> Option<InputGeneration> {
        None
    }

    /// Writes `bytes` only if `condition` holds at that moment, checked under
    /// the input lock with the write itself, and never holds them back to be
    /// written later (refused while Connecto types into the terminal). For a
    /// write that is only right for what was read just before it. A session
    /// that cannot check refuses.
    ///
    /// # Errors
    /// `Stale` when the condition does not hold, `Held` while Connecto types,
    /// and as `send_input`.
    fn send_input_if(
        &self,
        condition: &InputCondition,
        bytes: Vec<u8>,
    ) -> Result<(), InputRefused> {
        let _ = (condition, bytes);
        Err(InputRefused::Stale)
    }

    /// As [`TerminalSession::send_input`], but only over the attachment
    /// `attachment` (an [`TerminalSession::attachment_generation`]), checked
    /// as the bytes are queued: after a reattachment they are refused
    /// (`Closed`), never sent to what replaced it.
    ///
    /// # Errors
    /// As `send_input`, and `Closed` for another attachment.
    fn send_input_on(&self, bytes: Vec<u8>, attachment: u64) -> Result<(), InputRefused> {
        if self.attachment_generation() != attachment {
            return Err(InputRefused::Closed);
        }
        self.send_input(bytes)
    }

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

    /// The local view takes the size back from a peer that has it (a click on
    /// the terminal in this window): the size becomes the last local wish
    /// until the peer resizes again. Nothing happens when no peer has it.
    fn claim_local_size(&self) {}

    /// The size the terminal was last given (its screen and its PTY),
    /// whoever asked for it. `None` for a session that does not keep it.
    fn applied_size(&self) -> Option<GridSize> {
        None
    }

    /// How many times [`TerminalSession::claim_local_size`] took the size from
    /// a peer. A peer's stream that sees it change has lost the size.
    fn local_size_claims(&self) -> u64 {
        0
    }

    /// Change the selection. Returns false when the emulator was busy and
    /// nothing happened. A session with no screen of its own selects nothing.
    fn select(&self, op: SelectOp) -> bool {
        let _ = op;
        false
    }

    /// The selected text, including any part scrolled out of view. Never
    /// blocks.
    fn selection_text(&self) -> SelectionText {
        SelectionText::Nothing
    }

    /// The selected text as the clean copy reads it
    /// ([`crate::copy_text::clean_copy`]). Never blocks. A session without its
    /// own screen gives the plain text.
    fn clean_selection_text(&self) -> SelectionText {
        self.selection_text()
    }
}

/// Sees a session's output on the reader thread, in the same step as the
/// emulator, so a remote host can number the bytes the screen has taken.
///
/// `feed` must call `emulator_feed(bytes)` exactly once, and return quickly.
pub trait OutputTap: Send + Sync {
    fn feed(&self, bytes: &[u8], emulator_feed: &mut dyn FnMut(&[u8]));

    /// Run `resize`, which resizes the emulator and returns the size it took
    /// (None when it did not), so that no output is fed while it runs: a
    /// remote host marks the size at the byte where the screen took it, and
    /// its peers apply it there. Never waits: while output is being fed it
    /// returns false without calling `resize`, and the caller retries as for
    /// a busy screen. Otherwise it calls `resize` once and returns true.
    fn resize(&self, resize: &mut dyn FnMut() -> Option<GridSize>) -> bool {
        let _ = resize();
        true
    }
}

/// Makes the tap for a terminal's next session. Remote hosting is the only
/// implementation; the launcher asks it once per session, so a restart gets a
/// fresh numbering that matches its fresh screen.
pub trait TapFactory: Send + Sync {
    /// `key` names the terminal as `<project id>/<terminal id>`.
    fn tap_for(&self, key: &str) -> std::sync::Arc<dyn OutputTap>;
}
