//! A host project's Project Settings (packages, tools, install status) and
//! Agent Token Optimization, as controls the window draws like this
//! machine's. Each shows one project, under the id the window knows it by;
//! what it shows is the host's latest answer, fetched in the background while
//! the sheet is open, and each change is the host's to make (its guard records
//! it, and Pause refuses it).

use std::sync::Arc;
use std::time::Duration;

use connecto_core::tokens::ProjectTokenView;
use connecto_core::{
    InstallState, PackagesControl, PackagesView, ProjectSettings, TokenControl, TokenView,
};

use crate::project_cache::{PanelKind, PanelValue};
use crate::project_panels::ProjectAsk;
use crate::reconnect::HostHandle;

/// How long an answer is shown before it is asked for again: sooner while
/// the host is working on something.
const FRESH: Duration = Duration::from_secs(5);
const FRESH_BUSY: Duration = Duration::from_secs(1);

const NOT_CONNECTED: &str = "Not connected to that machine.";

/// One host project's package settings.
pub struct RemotePackages {
    host: Arc<HostHandle>,
    project: String,
    shown_as: String,
}

impl RemotePackages {
    #[must_use]
    pub const fn new(host: Arc<HostHandle>, project: String, shown_as: String) -> Self {
        Self {
            host,
            project,
            shown_as,
        }
    }

    fn command(&self, ask: ProjectAsk) -> Result<(), String> {
        if !self.host.is_connected() {
            return Err(NOT_CONNECTED.to_owned());
        }
        self.host.project_command(&self.project, ask);
        Ok(())
    }
}

impl PackagesControl for RemotePackages {
    fn view(&self) -> Arc<PackagesView> {
        let busy = matches!(
            self.host.peek_panel(&self.project, PanelKind::Settings).0,
            Some(PanelValue::Settings(answer)) if answer
                .project
                .as_ref()
                .is_some_and(|p| p.status.state == InstallState::Working)
        );
        let fresh = if busy { FRESH_BUSY } else { FRESH };
        let (value, error) = self.host.panel(&self.project, PanelKind::Settings, fresh);
        let Some(PanelValue::Settings(answer)) = value else {
            return Arc::new(PackagesView {
                last_error: error,
                ..PackagesView::default()
            });
        };
        let mut project = answer.project.clone();
        if let Some(project) = &mut project {
            project.project_id.clone_from(&self.shown_as);
        }
        Arc::new(PackagesView {
            packages: answer.packages.clone(),
            default_package: answer.default_package.clone(),
            projects: project.into_iter().collect(),
            last_error: error.or_else(|| answer.last_error.clone()),
            tools_one_or_all: answer.tools_one_or_all,
            ..PackagesView::default()
        })
    }

    fn set_project_settings(
        &self,
        project_id: &str,
        settings: ProjectSettings,
    ) -> Result<(), String> {
        let _ = project_id;
        self.command(ProjectAsk::SetSettings(settings))
    }

    fn latest_change_refused(&self, project_id: &str) -> bool {
        let _ = project_id;
        self.host.latest_save_refused(&self.project)
    }

    fn refresh_project(&self, project_id: &str) {
        let _ = project_id;
        let _ = self.command(ProjectAsk::Refresh);
    }

    fn check_requirements(&self, package: &str) -> Result<(), String> {
        self.command(ProjectAsk::CheckRequirements {
            package: package.to_owned(),
        })
    }

    fn install_requirement(
        &self,
        project_id: &str,
        package: &str,
        requirement: &str,
        command: &str,
    ) -> Result<(), String> {
        let _ = project_id;
        self.command(ProjectAsk::InstallRequirement {
            package: package.to_owned(),
            requirement: requirement.to_owned(),
            command: command.to_owned(),
        })
    }
}

/// One host project's Agent Token Optimization.
pub struct RemoteTokens {
    host: Arc<HostHandle>,
    project: String,
    shown_as: String,
}

impl RemoteTokens {
    #[must_use]
    pub const fn new(host: Arc<HostHandle>, project: String, shown_as: String) -> Self {
        Self {
            host,
            project,
            shown_as,
        }
    }

    fn command(&self, ask: ProjectAsk) -> Result<(), String> {
        if !self.host.is_connected() {
            return Err(NOT_CONNECTED.to_owned());
        }
        self.host.project_command(&self.project, ask);
        Ok(())
    }
}

impl TokenControl for RemoteTokens {
    fn view(&self) -> Arc<TokenView> {
        let busy = matches!(
            self.host.peek_panel(&self.project, PanelKind::Tokens).0,
            Some(PanelValue::Tokens(answer)) if answer.project.as_ref().is_some_and(|p| p.busy.is_some())
        );
        let fresh = if busy { FRESH_BUSY } else { FRESH };
        let (value, error) = self.host.panel(&self.project, PanelKind::Tokens, fresh);
        let (available, project) = match value {
            Some(PanelValue::Tokens(answer)) => (answer.available, answer.project.clone()),
            _ => (true, None),
        };
        // A refusal is the project's error, so the tab says it where its own
        // errors show.
        let project = match (project, error) {
            (Some(mut project), error) => {
                project.project_id.clone_from(&self.shown_as);
                if error.is_some() {
                    project.error = error;
                }
                Some(project)
            }
            (None, Some(error)) => Some(ProjectTokenView {
                project_id: self.shown_as.clone(),
                error: Some(error),
                ..ProjectTokenView::default()
            }),
            (None, None) => None,
        };
        Arc::new(TokenView {
            projects: project.into_iter().collect(),
            available,
        })
    }

    fn analyze(&self, project_id: &str) -> Result<(), String> {
        let _ = project_id;
        self.command(ProjectAsk::TokensAnalyze)
    }

    fn apply(
        &self,
        project_id: &str,
        plugins_off: Vec<String>,
        skills_off: Vec<String>,
    ) -> Result<(), String> {
        let _ = project_id;
        self.command(ProjectAsk::TokensApply {
            plugins: plugins_off,
            skills: skills_off,
        })
    }

    fn undo(&self, project_id: &str) -> Result<(), String> {
        let _ = project_id;
        self.command(ProjectAsk::TokensUndo)
    }

    fn set_autocompact(&self, project_id: &str, tokens: Option<u32>) -> Result<(), String> {
        let _ = project_id;
        self.command(ProjectAsk::TokensAutocompact(tokens))
    }
}

/// How long a host project's operations are shown before they are asked for
/// again while the tab is open: as often as this machine reads its own.
const OPS_FRESH: Duration = Duration::from_secs(1);

/// What the tab says while the first answer is on its way.
const OPS_LOADING: &str = "Asking that machine for this project's operations…";

/// What the tab says for a host too old to share its operations.
pub const OPS_OLD_HOST: &str = "The Connecto on that machine is too old to share this project's operations. Update it there to see, approve and run them from here.";

/// One host project's operations (ADR 0031): viewed, approved and run on
/// the host, which checks each action as it checks its own person's. An
/// approval or a run names the hash of the files this machine was sent and
/// shows (see [`crate::project_panels`]), so a change to them on the host
/// refuses it.
pub struct RemoteOps {
    host: Arc<HostHandle>,
    project: String,
}

impl RemoteOps {
    #[must_use]
    pub const fn new(host: Arc<HostHandle>, project: String) -> Self {
        Self { host, project }
    }
}

/// Whether `error` says the host does not know the ask: it is older.
pub(crate) fn old_host(error: &str) -> bool {
    error == crate::project_panels::UNKNOWN_ASK
        || error
            == crate::project_cache::wording(&crate::client::ClientError::Denied(
                crate::wire::DenyReason::Protocol,
            ))
}

impl connecto_core::ops::OpsControl for RemoteOps {
    /// `shown_as` is the id the window knows the project by.
    fn view(&self, shown_as: &str) -> connecto_core::ops::OpsView {
        use connecto_core::ops::OpsView;
        let (value, error) = self.host.panel(&self.project, PanelKind::Ops, OPS_FRESH);
        if error.as_deref().is_some_and(old_host) {
            return OpsView {
                project: shown_as.to_owned(),
                unavailable: Some(OPS_OLD_HOST.to_owned()),
                ..OpsView::default()
            };
        }
        match value {
            Some(PanelValue::Ops(view)) => {
                let mut view = (*view).clone();
                view.project = shown_as.to_owned();
                // A refused action, or a host that stopped answering, is
                // said where the tab says what an action came to.
                if let Some(error) = error {
                    view.notice = Some((error, true));
                }
                view
            }
            _ => OpsView {
                project: shown_as.to_owned(),
                unavailable: Some(error.unwrap_or_else(|| OPS_LOADING.to_owned())),
                ..OpsView::default()
            },
        }
    }

    fn submit(&self, project: String, action: connecto_core::ops::OpsAction) {
        let _ = project;
        self.host
            .project_command(&self.project, ProjectAsk::OpsAct(action));
    }
}

/// One host project's functional memory (ADR 0033): its status, from the
/// host's `functional-memory` answer, and Update now, asked of the host.
/// Whether it updates by itself is set on the host only.
pub struct RemoteFunctionalMemory {
    host: Arc<HostHandle>,
    project: String,
}

impl RemoteFunctionalMemory {
    #[must_use]
    pub const fn new(host: Arc<HostHandle>, project: String) -> Self {
        Self { host, project }
    }
}

impl connecto_core::functional_memory::FunctionalMemoryControl for RemoteFunctionalMemory {
    fn status(&self, _project_id: &str) -> connecto_core::functional_memory::FmStatus {
        use connecto_core::functional_memory::{FmState, FmStatus};
        let busy = matches!(
            self.host.peek_panel(&self.project, PanelKind::FunctionalMemory).0,
            Some(PanelValue::FunctionalMemory(status))
                if matches!(status.state, FmState::Running | FmState::Waiting)
        );
        let fresh = if busy { FRESH_BUSY } else { FRESH };
        let (value, error) = self
            .host
            .panel(&self.project, PanelKind::FunctionalMemory, fresh);
        let mut status = match value {
            Some(PanelValue::FunctionalMemory(status)) => (*status).clone(),
            // Not "not built": nothing is known until the host answers.
            _ => FmStatus {
                state: FmState::Unknown,
                message: (error.is_none()).then(|| "Asking that machine for it…".to_owned()),
                ..FmStatus::default()
            },
        };
        // A refusal (or a host from before functional memory) is said where
        // the card says why.
        if let Some(error) = error {
            status.message = Some(error);
        }
        status
    }

    fn update_now(&self, _project_id: &str) -> Result<(), String> {
        if !self.host.is_connected() {
            return Err(NOT_CONNECTED.to_owned());
        }
        self.host
            .project_command(&self.project, ProjectAsk::FunctionalMemoryUpdate);
        Ok(())
    }

    fn set_auto(&self, _project_id: &str, on: bool) -> Result<(), String> {
        self.set(Some(on), None)
    }

    fn set_model(
        &self,
        _project_id: &str,
        model: Option<connecto_core::functional_memory::FmModel>,
    ) -> Result<(), String> {
        self.set(None, Some(model))
    }
}

impl RemoteFunctionalMemory {
    /// A change, made by the host as its window makes it (its CLIs decide
    /// which models may be chosen); a refusal shows as the card's message.
    fn set(
        &self,
        auto: Option<bool>,
        model: Option<Option<connecto_core::functional_memory::FmModel>>,
    ) -> Result<(), String> {
        if !self.host.is_connected() {
            return Err(NOT_CONNECTED.to_owned());
        }
        self.host.project_command(
            &self.project,
            ProjectAsk::FunctionalMemorySet { auto, model },
        );
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A host from before operations were shared refuses the ask in words
    /// this side knows, so the tab says to update it rather than an error.
    #[test]
    fn a_host_too_old_for_operations_is_told_apart() {
        let refused = ProjectAsk::decode(br#"{"ask":"some-later-ask"}"#).unwrap_err();
        assert!(old_host(&refused));
        assert!(old_host(&crate::project_cache::wording(
            &crate::client::ClientError::Denied(crate::wire::DenyReason::Protocol)
        )));
        assert!(!old_host(
            "That machine paused remote actions. Try again when it resumes."
        ));
    }
}
