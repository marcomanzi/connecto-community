//! The secrets of paired hosts' projects, beside this machine's: each kept
//! on its own machine and changed there (ADR 0027, ADR 0017). Settings >
//! Secrets lists them under the id the window knows each project by, and a
//! change to one goes to its machine, never to this one's vault.

use std::sync::Arc;

use connecto_core::{ProviderScope, RemoteControl, SecretAsk, VaultControl, VaultView};

use crate::app::{is_remote_id, remote_project_id};
use crate::vault_ui::ProjectName;

/// One host project's secrets, as Settings shows it.
pub(crate) struct RemoteVault {
    pub(crate) project: ProjectName,
    pub(crate) vault: Arc<dyn VaultControl>,
}

/// The connected hosts' projects whose secrets can be kept from here.
pub(crate) fn remote_vaults(remote: &dyn RemoteControl) -> Vec<RemoteVault> {
    let view = remote.view();
    let mut out = Vec::new();
    for host in view.hosts.iter().filter(|h| {
        matches!(
            h.link,
            connecto_core::HostLink::Direct
                | connecto_core::HostLink::Relayed
                | connecto_core::HostLink::Connected
        )
    }) {
        for project in remote.projects(&host.name) {
            let id = remote_project_id(&host.name, &project.id);
            if let Some(vault) = remote.project_vault(&host.name, &project.id, &id) {
                out.push(RemoteVault {
                    project: ProjectName {
                        id,
                        name: format!("{} on {}", project.name, host.name),
                        elsewhere: Some(host.name.clone()),
                    },
                    vault,
                });
            }
        }
    }
    out
}

/// This machine's secrets with those of `remotes`: each remote problem is
/// said with its project's name.
pub(crate) fn merged_view(local: &VaultView, remotes: &[RemoteVault]) -> VaultView {
    let mut view = local.clone();
    let mut problems: Vec<String> = local.problem.iter().cloned().collect();
    for remote in remotes {
        let theirs = remote.vault.view();
        view.entries.extend(theirs.entries.iter().cloned());
        view.pages.extend(theirs.pages.iter().cloned());
        if let Some(why) = &theirs.problem {
            problems.push(format!("{}: {why}", remote.project.name));
        }
    }
    view.problem = (!problems.is_empty()).then(|| problems.join("\n"));
    view
}

/// The vault that keeps `scope`: a remote project's machine, or this one.
///
/// # Errors
/// A remote project whose machine is not connected now.
pub(crate) fn vault_for(
    local: &Arc<dyn VaultControl>,
    remotes: &[RemoteVault],
    scope: &ProviderScope,
) -> Result<Arc<dyn VaultControl>, String> {
    match scope.project() {
        Some(id) if is_remote_id(id) => remotes
            .iter()
            .find(|r| r.project.id == id)
            .map(|r| Arc::clone(&r.vault))
            .ok_or_else(|| "That machine is not connected now.".to_owned()),
        _ => Ok(Arc::clone(local)),
    }
}

/// A request waiting for the person, from this machine's terminal or a
/// paired host's.
#[derive(Clone)]
pub(crate) struct WaitingAsk {
    /// Its project as the window knows it.
    pub(crate) ask: SecretAsk,
    /// The host, the project's id there, and how the window names it; `None`
    /// for this machine's.
    pub(crate) remote: Option<(String, String, ProjectName)>,
}

impl WaitingAsk {
    pub(crate) fn host(&self) -> Option<&str> {
        self.remote.as_ref().map(|(host, ..)| host.as_str())
    }

    /// The vault that answers it.
    pub(crate) fn vault(
        &self,
        local: &Arc<dyn VaultControl>,
        remote: &dyn RemoteControl,
    ) -> Option<Arc<dyn VaultControl>> {
        match &self.remote {
            None => Some(Arc::clone(local)),
            Some((host, project, name)) => remote.project_vault(host, project, &name.id),
        }
    }
}

/// This machine's requests first, then the paired hosts', oldest first.
pub(crate) fn waiting_asks(local: &VaultView, remote: &dyn RemoteControl) -> Vec<WaitingAsk> {
    let mut out: Vec<WaitingAsk> = local
        .asks
        .iter()
        .map(|ask| WaitingAsk {
            ask: ask.clone(),
            remote: None,
        })
        .collect();
    for waiting in remote.secret_asks() {
        let id = remote_project_id(&waiting.host, &waiting.project);
        let mut ask = waiting.ask;
        ask.project = Some(id.clone());
        out.push(WaitingAsk {
            ask,
            remote: Some((
                waiting.host.clone(),
                waiting.project,
                ProjectName {
                    id,
                    name: format!("{} on {}", waiting.project_name, waiting.host),
                    elsewhere: Some(waiting.host),
                },
            )),
        });
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A remote project's secret goes to its machine; when that machine is
    /// not connected it is refused, never kept here instead.
    #[test]
    fn a_remote_projects_secret_never_falls_back_to_this_machine() {
        let local: Arc<dyn VaultControl> = Arc::new(connecto_core::NoVault);
        let remote_id = remote_project_id("build box", "p-web");
        let refused = vault_for(&local, &[], &ProviderScope::Project(remote_id)).err();
        assert_eq!(
            refused.as_deref(),
            Some("That machine is not connected now.")
        );
        assert!(vault_for(&local, &[], &ProviderScope::Global).is_ok());
        assert!(vault_for(&local, &[], &ProviderScope::Project("abc".into())).is_ok());
    }
}
