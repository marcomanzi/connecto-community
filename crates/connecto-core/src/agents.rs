//! The agents Connecto works with (Claude Code, Codex, and later others), as
//! Settings shows them: whether each is on this Mac, whether the user is signed
//! in, and whether it is offered when a terminal is opened. The host does the
//! work (installing, checking); the window draws rows and passes clicks back.

use std::sync::Arc;

use crate::ToolKind;

/// What an agent is doing right now, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentBusy {
    /// Installing from the vendor's official source: the line says which step.
    Installing(String),
    /// Looking at what is installed and signed in.
    Checking,
}

/// One supported agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub tool: ToolKind,
    /// "Claude Code", "Codex".
    pub name: String,
    /// Where it comes from, said plainly ("Anthropic's installer at claude.ai").
    pub source: String,
    pub installed: bool,
    /// What `--version` printed, when installed.
    pub version: Option<String>,
    /// Signed in on this Mac, if it could be told.
    pub signed_in: Option<bool>,
    /// Offered when a terminal is opened. Turning it off hides it; nothing is
    /// uninstalled.
    pub active: bool,
    pub busy: Option<AgentBusy>,
    /// The last install or check that failed, in plain words.
    pub error: Option<String>,
    /// An install just finished and the person should now sign in: the window
    /// opens the sign-in once and acknowledges it.
    pub signin_requested: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentsView {
    pub rows: Vec<AgentRow>,
}

/// The commands the window gives. Cheap; the work runs elsewhere.
pub trait AgentsControl: Send + Sync {
    fn view(&self) -> Arc<AgentsView>;
    /// Install the agent if it is missing, offer it in new terminals, and ask
    /// the window to take the person to sign in.
    fn activate(&self, tool: ToolKind) {
        let _ = tool;
    }
    /// Stop offering the agent in new terminals. Nothing is uninstalled.
    fn deactivate(&self, tool: ToolKind) {
        let _ = tool;
    }
    /// Look again at what is installed and signed in (after a sign-in).
    fn refresh(&self) {}
    /// The window has started the sign-in this agent asked for.
    fn signin_started(&self, tool: ToolKind) {
        let _ = tool;
    }
    /// Agents switched off in Settings; the chooser leaves them out.
    fn inactive(&self) -> Vec<ToolKind> {
        Vec::new()
    }
}

/// The control used when none is wired in.
#[derive(Debug, Default)]
pub struct NoAgents;

impl AgentsControl for NoAgents {
    fn view(&self) -> Arc<AgentsView> {
        Arc::new(AgentsView::default())
    }
}
