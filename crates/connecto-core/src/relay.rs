//! The vocabulary of the manual relay, shared by the gate, sessions and UI.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::EventText;

/// One attempt of one session. A restart is a new attempt, so anything aimed at
/// the old one is stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionRef {
    /// Index of the session in the window.
    pub session: usize,
    pub attempt: u64,
}

/// Whether a tool can take a message now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    NotReady,
    /// This signal cannot tell.
    Unknown,
}

/// The global pause. The UI thread flips it directly and nothing it does can
/// wait. Every pause starts a new epoch, so a delivery authorized before it
/// can tell, even if the user has since resumed.
#[derive(Debug, Default)]
pub struct PauseSwitch {
    paused: AtomicBool,
    epoch: AtomicU64,
}

impl PauseSwitch {
    /// `paused` is set first and the epoch bumped second. A reader that sees
    /// `paused` false therefore took its epoch before the bump, so a later
    /// comparison catches it. In the other order, a reader could hold the new
    /// epoch with `paused` still false, and keep it valid through a Resume.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

/// How far a delivery has got. Only `Completed` is success: `Written` means
/// bytes reached the terminal and proves nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    Accepted,
    Dispatching,
    Written,
    Acknowledged,
    Completed,
    Denied,
    Suppressed,
    Failed,
    Uncertain,
    /// An agent's text or keys were typed into the target and Enter was
    /// pressed (ADR 0015). Final: there is no delivery to end, and no ack to
    /// wait for. Who typed what is in the row.
    Typed,
}

impl DeliveryState {
    /// Whether this state is final.
    #[must_use]
    pub const fn is_final(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Denied
                | Self::Suppressed
                | Self::Failed
                | Self::Uncertain
                | Self::Typed
        )
    }

    /// What the UI says. `Written` is never shown as sent.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Dispatching => "dispatching",
            Self::Written => "written, unconfirmed",
            Self::Acknowledged => "acknowledged",
            Self::Completed => "completed",
            Self::Denied => "denied",
            Self::Suppressed => "suppressed",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
            Self::Typed => "typed",
        }
    }
}

/// One line of the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    pub id: String,
    /// The session it was sent to.
    pub target: usize,
    pub from: String,
    pub to: String,
    pub state: DeliveryState,
    pub reason: String,
}

/// The latest reply captured from a session. It is shown, and offered to the
/// user. Nothing acts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub text: EventText,
    /// The delivery this answers, when connecto delivered its prompt.
    pub origin: Option<Origin>,
}

/// Where a captured reply sits in a chain of hops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub trace: String,
    pub message: String,
    pub hop: u32,
}

/// One session as the relay panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    /// The terminal's id. Empty while the slot is vacant.
    pub name: String,
    /// The slot has no terminal. `RelayView::sessions` is indexed by slot.
    pub vacant: bool,
    pub attempt: u64,
    pub live: bool,
    pub readiness: Readiness,
    pub capture: Option<Capture>,
}

/// The second click a delivery needs, and what it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    pub target: usize,
    pub text: String,
}

/// Everything the relay panel draws.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayView {
    pub project: Option<String>,
    pub sessions: Vec<SessionView>,
    pub ledger: Vec<LedgerRow>,
    pub confirmation: Option<Confirmation>,
}

/// A click on Send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRequest {
    pub target: usize,
    pub attempt: u64,
    pub body: String,
    /// The user clicked "send anyway".
    pub confirmed: bool,
    /// Which session's captured reply this was taken from, if any.
    pub source: Option<usize>,
    pub origin: Option<Origin>,
}

/// User commands on the relay. Every method returns promptly.
pub trait RelayControl: Send + Sync {
    /// The latest state.
    fn view(&self) -> Arc<RelayView>;

    /// The switch the UI flips directly.
    fn pause_switch(&self) -> Arc<PauseSwitch>;

    /// Exactly the body that would be sent, or why it would be denied.
    fn preview(&self, body: &str) -> Result<String, String>;

    /// Queue a delivery. Its outcome shows in the ledger.
    fn send(&self, request: SendRequest) -> Result<(), String>;

    /// End an open delivery as `Uncertain`, because its turn end may never
    /// come. This closes local state and sends nothing to the session.
    fn end_delivery(&self, target: usize, id: &str) -> Result<(), String>;

    /// Pause, then stop every session.
    fn stop_all(&self);

    /// Use this project, or say why not. Refused while a session is live.
    fn choose_project(&self, path: &std::path::Path) -> Result<(), String>;
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    /// T4: a reader that takes the epoch and then sees `paused` false never
    /// holds an epoch whose pause has not been resumed. In the other order
    /// (epoch bumped first) the reader could hold the new epoch while the flag
    /// was still false, and the admission would stay valid through the Resume.
    #[test]
    fn a_reader_that_sees_not_paused_never_holds_an_unresumed_epoch() {
        use std::sync::atomic::AtomicU64;
        let switch = Arc::new(PauseSwitch::default());
        let resuming = Arc::new(AtomicU64::new(0));
        let writer = {
            let (switch, resuming) = (Arc::clone(&switch), Arc::clone(&resuming));
            thread::spawn(move || {
                for cycle in 1..=50_000_u64 {
                    switch.pause();
                    resuming.store(cycle, Ordering::SeqCst);
                    switch.resume();
                }
            })
        };
        let mut seen_new = 0_u32;
        while !writer.is_finished() {
            let epoch = switch.epoch();
            let paused = switch.is_paused();
            let resumed_started = resuming.load(Ordering::SeqCst);
            if !paused && epoch > 0 && resumed_started < epoch {
                panic!("held epoch {epoch} not paused, but cycle {epoch} had not resumed");
            }
            seen_new += u32::from(epoch > 0);
        }
        let _ = writer.join();
        assert!(seen_new > 0);
    }

    #[test]
    fn pause_sets_the_flag_before_the_epoch() {
        let switch = PauseSwitch::default();
        switch.pause();
        assert!(switch.is_paused());
        assert_eq!(switch.epoch(), 1);
        switch.resume();
        assert!(!switch.is_paused());
        assert_eq!(switch.epoch(), 1);
    }
}
