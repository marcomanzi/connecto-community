//! The contract between the UI and a pane whose session is managed for it.
//!
//! Every method must return promptly, as for
//! [`TerminalSession`](crate::TerminalSession). Slow work happens elsewhere and
//! shows up here as a stage.

use std::sync::Arc;

/// Result of a tool's own status command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    LoggedIn {
        method: String,
    },
    LoggedOut,
    /// The check failed. This does not mean logged out.
    Unknown,
}

/// Where a managed pane is in its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneStage {
    /// No project chosen yet.
    Idle,
    /// Preflight is running. `step` says what it is doing.
    Preparing { step: String },
    /// The tool cannot start until the user logs in.
    NeedsLogin { auth: AuthState },
    /// The tool's login runs in this pane.
    LoggingIn,
    /// The session exists. It may since have exited.
    Running,
    /// The user stopped it.
    Stopped,
    /// Preflight or launch refused the session.
    Failed { reason: String },
}

impl PaneStage {
    /// Whether the user can ask for a login or a start now.
    #[must_use]
    pub const fn accepts_commands(&self) -> bool {
        matches!(
            self,
            Self::NeedsLogin { .. } | Self::Failed { .. } | Self::Stopped
        )
    }
}

/// User commands on a managed pane.
pub trait PaneControl: Send + Sync {
    /// The current stage. Never blocks.
    fn stage(&self) -> PaneStage;

    /// Run the tool's login in this pane. Never blocks.
    fn request_login(&self);

    /// Run preflight and start the session. Never blocks.
    fn request_start(&self);

    /// Stop whatever runs in this pane, a login included. Never blocks.
    fn request_stop(&self);

    /// Stop the session and start a new attempt. Never blocks.
    fn request_restart(&self);

    /// Whether a process is running in this pane now.
    fn is_live(&self) -> bool;

    /// What the user must keep in view, one line each. Never blocks.
    fn warnings(&self) -> Arc<Vec<String>>;

    /// Hook frames refused since the session started.
    fn rejected_hook_frames(&self) -> u64;
}
