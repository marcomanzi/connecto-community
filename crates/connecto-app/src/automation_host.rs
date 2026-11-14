//! The automations engine's view of the running app: the workspace.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use connecto_automation::{Host, ProjectInfo, TermInfo};
use connecto_core::{ToolKind, WorkspaceControl};
use connecto_workspace::Workspace;

pub struct WorkspaceHost {
    pub workspace: Arc<Workspace>,
}

fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

impl WorkspaceHost {
    fn control(&self, project: &str) -> Option<Arc<dyn connecto_core::TerminalsControl>> {
        self.workspace
            .view()
            .projects
            .iter()
            .find(|p| p.id == project)
            .and_then(|p| p.terminals.clone())
    }
}

impl Host for WorkspaceHost {
    fn terminals(&self) -> Vec<TermInfo> {
        let view = self.workspace.view();
        let mut out = Vec::new();
        for project in &view.projects {
            let Some(control) = &project.terminals else {
                continue;
            };
            let path = PathBuf::from(&project.path);
            for entry in control
                .view()
                .terminals
                .iter()
                // Automations read and type into agents; a shell is left alone.
                .filter(|entry| entry.tool.is_agent())
            {
                out.push(TermInfo {
                    project_id: project.id.clone(),
                    project_name: project.name.clone(),
                    project_path: path.clone(),
                    id: entry.id.clone(),
                    name: entry.name.clone(),
                    tool: entry.tool,
                    state: entry.state,
                });
            }
        }
        out
    }

    fn screen(&self, project: &str, terminal: &str) -> Option<String> {
        let relay = self.workspace.relay(project)?;
        // Only the bottom: the engine reads every terminal on every tick.
        Some(
            relay
                .pane(terminal)?
                .session()?
                .text_tail(connecto_automation::SCREEN_ROWS),
        )
    }

    fn paused(&self) -> bool {
        self.workspace.pause_switch().is_paused()
    }

    fn keys(&self, project: &str, terminal: &str, keys: &[String]) -> Result<(), String> {
        self.workspace
            .relay(project)
            .ok_or("that project is closed")?
            .automation_keys(terminal, keys, "automation")
    }

    fn open_project(&self, dir: &Path) -> Result<(), String> {
        self.workspace.open_project(dir)
    }

    fn project_at(&self, dir: &Path) -> Option<ProjectInfo> {
        let dir = real(dir);
        let view = self.workspace.view();
        let project = view
            .projects
            .iter()
            .find(|p| real(Path::new(&p.path)) == dir)?;
        let terminals = project.terminals.as_ref().map(|t| {
            t.view()
                .terminals
                .iter()
                .map(|e| (e.id.clone(), e.tool))
                .collect()
        });
        Some(ProjectInfo {
            id: project.id.clone(),
            ready: terminals.is_some(),
            terminals: terminals.unwrap_or_default(),
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
        let control = self.control(project).ok_or("that project is not ready")?;
        control.add_with(tool, name, prompt, resume, resume_note)
    }

    fn terminal_session(&self, project: &str, terminal: &str) -> Option<String> {
        self.control(project)?.session_of(terminal)
    }

    fn show(&self, project: &str, terminal: Option<&str>) {
        self.workspace.activate(project);
        if let Some(terminal) = terminal {
            self.workspace.set_active_terminal(project, terminal);
        }
    }

    fn saved_root(&self) -> Option<PathBuf> {
        self.workspace.automations_root()
    }

    fn save_root(&self, root: Option<PathBuf>) -> Result<(), String> {
        self.workspace.set_automations_root(root)
    }
}
