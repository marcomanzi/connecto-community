//! Whose model providers a terminal uses, and what Settings shows about the
//! ones Claude Code can be given.
//!
//! The app-wide providers are set in Settings > Agents and apply to every
//! project whose tool uses the System configuration. A project whose tool is
//! Isolated can have providers of its own instead (Project settings > Tools):
//! then that project's terminals of that tool use those, and only those.
//!
//! Claude Code reaches Together AI's models through TogetherLink, Together's
//! own launcher (`togetherlink claude …`). It runs on macOS and Linux only.

/// Whose providers: the app's, or one project's (by its id).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ProviderScope {
    /// Settings > Agents: for every project that has none of its own.
    #[default]
    Global,
    /// One project's own, by project id.
    Project(String),
}

impl ProviderScope {
    /// The project's id, for a project's own providers.
    #[must_use]
    pub fn project(&self) -> Option<&str> {
        match self {
            Self::Global => None,
            Self::Project(id) => Some(id),
        }
    }
}

/// Whether Claude Code can be given other model providers on this system.
/// TogetherLink runs on macOS and Linux only, so Windows offers none.
pub const CLAUDE_PROVIDERS_SUPPORTED: bool = cfg!(not(windows));

/// How TogetherLink is installed (Together's own installer; it installs Bun
/// when it is missing, which needs `unzip` on Linux). Connecto runs the same
/// script when the person agrees, and shows this for doing it by hand.
pub const TOGETHERLINK_INSTALL: &str = "curl -fsSL https://link.together.ai/install | bash";

/// Where TogetherLink's installer is downloaded from.
pub const TOGETHERLINK_INSTALLER_URL: &str = "https://link.together.ai/install";

/// Where TogetherLink's documentation is.
pub const TOGETHERLINK_DOCS: &str = "docs.together.ai/docs/togetherlink";

/// Where Together AI's keys are made.
pub const TOGETHER_KEYS_PAGE: &str = "api.together.ai/settings/api-keys";

/// Whether TogetherLink is on this computer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TogetherLinkState {
    /// Not looked for yet.
    #[default]
    Checking,
    /// Not found where its installer puts it.
    Missing,
    /// Being installed: which step.
    Installing(String),
    /// Found.
    Installed,
}

/// What Settings shows about the providers Claude terminals are given. Never
/// a key itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClaudeProvidersView {
    /// A Together AI key is saved: Claude terminals start through TogetherLink.
    pub together_key: bool,
    /// The secret store could not be read: whether a key is saved is unknown.
    pub keys_unreadable: bool,
    /// For a project: it uses providers of its own instead of the app-wide
    /// ones. Always true app-wide.
    pub own: bool,
    /// Whether TogetherLink is on this computer.
    pub togetherlink: TogetherLinkState,
    /// TogetherLink has a key saved of its own (`togetherlink configure`).
    /// It uses that one before the one given at launch.
    pub togetherlink_own_key: bool,
    /// A key was just saved and TogetherLink is missing: the person is asked
    /// whether to install it now.
    pub install_offered: bool,
    /// Why the last install of TogetherLink did not finish, in plain words.
    pub install_error: Option<String>,
}

impl ClaudeProvidersView {
    /// Claude terminals go through TogetherLink, but it is not installed:
    /// they cannot start until it is.
    #[must_use]
    pub fn blocked(&self) -> bool {
        self.together_key
            && matches!(
                self.togetherlink,
                TogetherLinkState::Missing | TogetherLinkState::Installing(_)
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_key_without_togetherlink_blocks_the_terminals() {
        let mut view = ClaudeProvidersView {
            together_key: true,
            togetherlink: TogetherLinkState::Missing,
            ..ClaudeProvidersView::default()
        };
        assert!(view.blocked());
        view.togetherlink = TogetherLinkState::Installing("Downloading…".into());
        assert!(view.blocked());
        view.togetherlink = TogetherLinkState::Installed;
        assert!(!view.blocked());
        view.together_key = false;
        view.togetherlink = TogetherLinkState::Missing;
        assert!(!view.blocked());
    }

    #[test]
    fn only_a_project_scope_names_a_project() {
        assert_eq!(ProviderScope::Global.project(), None);
        assert_eq!(ProviderScope::Project("p1".into()).project(), Some("p1"));
    }
}
