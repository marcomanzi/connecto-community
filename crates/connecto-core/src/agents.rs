//! The agents Connecto works with (Claude Code, Codex, and later others), as
//! Settings shows them: whether each is on this Mac, whether the user is signed
//! in, and whether it is offered when a terminal is opened. The host does the
//! work (installing, checking); the window draws rows and passes clicks back.

use std::sync::Arc;

use crate::{ProviderScope, ToolKind};

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
    /// The install is waiting for the person: Node.js, which the agent runs
    /// on (Pi), is missing or too old. What is said and asked; the answer is
    /// [`AgentsControl::install_node`] or [`AgentsControl::decline_node`].
    pub node_prompt: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentsView {
    pub rows: Vec<AgentRow>,
    /// The model providers Pi terminals are given.
    pub pi: crate::PiProvidersView,
    /// The model providers Claude terminals are given (Settings > Agents).
    pub claude: crate::ClaudeProvidersView,
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
    /// The person agreed: install Node.js, then go on installing the agent.
    fn install_node(&self, tool: ToolKind) {
        let _ = tool;
    }
    /// The person declined Node.js: the agent is not installed, and the row
    /// says so.
    fn decline_node(&self, tool: ToolKind) {
        let _ = tool;
    }
    /// The window has started the sign-in this agent asked for.
    fn signin_started(&self, tool: ToolKind) {
        let _ = tool;
    }
    /// Save (`Some`) or remove (`None`) the key Pi terminals are given for a
    /// provider, app-wide or for one project. Applies to terminals started
    /// from now on.
    ///
    /// # Errors
    /// Why it was not saved, for the person; never the key.
    fn set_pi_key(
        &self,
        scope: &ProviderScope,
        provider: crate::PiProvider,
        key: Option<String>,
    ) -> Result<(), String> {
        let _ = (scope, provider, key);
        Err("Agents are not available.".to_owned())
    }

    /// Set the address of the local LM Studio server whose models Pi offers.
    /// Empty turns it off.
    ///
    /// # Errors
    /// What is wrong with the address.
    fn set_pi_lmstudio_url(&self, scope: &ProviderScope, url: String) -> Result<(), String> {
        let _ = (scope, url);
        Err("Agents are not available.".to_owned())
    }

    /// Set (`Some`) or clear (`None`) the model a new Pi terminal starts on
    /// when none is chosen for it.
    ///
    /// # Errors
    /// Why it was not saved.
    fn set_pi_default_model(
        &self,
        scope: &ProviderScope,
        model: Option<String>,
    ) -> Result<(), String> {
        let _ = (scope, model);
        Err("Agents are not available.".to_owned())
    }

    /// Pi's providers for `scope`. App-wide, what [`AgentsView::pi`] says.
    fn pi_providers(&self, scope: &ProviderScope) -> crate::PiProvidersView {
        match scope {
            ProviderScope::Global => self.view().pi.clone(),
            ProviderScope::Project(_) => crate::PiProvidersView::default(),
        }
    }

    /// Ask a project's local LM Studio server again whether it answers.
    fn check_pi_local(&self, scope: &ProviderScope) {
        let _ = scope;
    }

    /// The providers Claude terminals are given for `scope`.
    fn claude_providers(&self, scope: &ProviderScope) -> crate::ClaudeProvidersView {
        match scope {
            ProviderScope::Global => self.view().claude.clone(),
            ProviderScope::Project(_) => crate::ClaudeProvidersView::default(),
        }
    }

    /// Save (`Some`) or remove (`None`) the Together AI key Claude terminals
    /// are started with (through TogetherLink).
    ///
    /// # Errors
    /// Why it was not saved, for the person; never the key.
    fn set_claude_together_key(
        &self,
        scope: &ProviderScope,
        key: Option<String>,
    ) -> Result<(), String> {
        let _ = (scope, key);
        Err("Agents are not available.".to_owned())
    }

    /// Look again for TogetherLink on this computer.
    fn check_togetherlink(&self) {}

    /// The person agreed: install TogetherLink with Together's own installer.
    fn install_togetherlink(&self) {}

    /// The person declined the offer to install TogetherLink.
    fn decline_togetherlink(&self) {}

    /// Whether `project`'s terminals of `tool` use providers of the project's
    /// own (when its config is Isolated) instead of the app-wide ones.
    ///
    /// # Errors
    /// Why it was not saved.
    fn set_own_providers(&self, project: &str, tool: ToolKind, own: bool) -> Result<(), String> {
        let _ = (project, tool, own);
        Err("Agents are not available.".to_owned())
    }

    /// Agents switched off in Settings; the chooser leaves them out.
    fn inactive(&self) -> Vec<ToolKind> {
        Vec::new()
    }

    /// The agents not offered for a new terminal in `project`: switched off
    /// in Settings, and Pi when no model provider works for that project
    /// (its own, when its Pi config is Isolated and it has them; else the
    /// app-wide ones).
    fn inactive_for(&self, project: &str) -> Vec<ToolKind> {
        let _ = project;
        self.inactive()
    }

    /// Whose providers `project`'s terminals of `tool` use now.
    fn provider_scope(&self, project: &str, tool: ToolKind) -> ProviderScope {
        let _ = (project, tool);
        ProviderScope::Global
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
