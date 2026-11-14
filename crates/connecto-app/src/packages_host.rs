//! The packages manager's view of the running app: the workspace and the
//! automations engine.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use connecto_automation::{Automations, Host as _};
use connecto_core::{AutomationsControl, TermActivity, ToolKind, WorkspaceControl};
use connecto_packages::{Host, ProjectInfo, ProjectRef};
use connecto_workspace::Workspace;

use crate::automation_host::WorkspaceHost;

pub struct PackagesHost {
    pub workspace: Arc<Workspace>,
    /// `None` when the automations engine did not start.
    pub automations: Option<Arc<Automations>>,
}

impl PackagesHost {
    fn terminals(&self) -> WorkspaceHost {
        WorkspaceHost {
            workspace: Arc::clone(&self.workspace),
        }
    }
}

impl Host for PackagesHost {
    fn projects(&self) -> Vec<ProjectRef> {
        self.workspace
            .view()
            .projects
            .iter()
            .map(|p| ProjectRef {
                id: p.id.clone(),
                path: PathBuf::from(&p.path),
            })
            .collect()
    }

    fn running_terminals(&self, project: &str) -> usize {
        self.workspace
            .view()
            .projects
            .iter()
            .find(|p| p.id == project)
            .and_then(|p| p.terminals.as_ref())
            .map_or(0, |terminals| {
                terminals
                    .view()
                    .terminals
                    .iter()
                    .filter(|t| {
                        !matches!(
                            t.state,
                            TermActivity::NotStarted | TermActivity::Exited | TermActivity::Failed
                        )
                    })
                    .count()
            })
    }

    fn open_project(&self, dir: &Path) -> Result<(), String> {
        self.workspace.open_project(dir)
    }

    fn project_at(&self, dir: &Path) -> Option<ProjectInfo> {
        let found = self.terminals().project_at(dir)?;
        Some(ProjectInfo {
            id: found.id,
            ready: found.ready,
            terminals: found.terminals,
        })
    }

    fn add_terminal(
        &self,
        project: &str,
        tool: ToolKind,
        name: Option<String>,
        prompt: Option<String>,
        resume: Option<String>,
        resume_note: Option<String>,
    ) -> Result<String, String> {
        self.terminals()
            .add_terminal(project, tool, name, prompt, resume, resume_note)
    }

    fn terminal_session(&self, project: &str, terminal: &str) -> Option<String> {
        self.terminals().terminal_session(project, terminal)
    }

    fn show(&self, project: &str, terminal: Option<&str>) {
        self.terminals().show(project, terminal);
    }

    fn saved_home(&self) -> Option<PathBuf> {
        self.workspace.connecto_home()
    }

    fn save_home(&self, home: Option<PathBuf>) -> Result<(), String> {
        self.workspace.set_connecto_home(home)
    }

    fn automations_root(&self) -> Option<PathBuf> {
        self.workspace.automations_root()
    }

    fn use_automations_root(&self, root: PathBuf) -> Result<(), String> {
        match &self.automations {
            Some(automations) => automations.set_root(root),
            None => self.workspace.set_automations_root(Some(root)),
        }
    }
}
