//! What the manager needs from the running app, behind a trait so tests can
//! stand in for it.

use std::path::{Path, PathBuf};

use connecto_core::ToolKind;

/// One open project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    /// `ProjectView::id`.
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: String,
    /// Its terminals are available.
    pub ready: bool,
    pub terminals: Vec<(String, ToolKind)>,
}

pub trait Host: Send + Sync {
    /// The open projects, in sidebar order.
    fn projects(&self) -> Vec<ProjectRef>;
    /// How many of a project's terminals are running now (not stopped).
    fn running_terminals(&self, project: &str) -> usize;
    fn open_project(&self, dir: &Path) -> Result<(), String>;
    fn project_at(&self, dir: &Path) -> Option<ProjectInfo>;
    /// Add a terminal and return its id. `prompt` is the first message of a
    /// new conversation; `resume` picks a tool session up again (the prompt then
    /// applies only if the resume fails); `resume_note` is a short first message
    /// into the resumed conversation.
    fn add_terminal(
        &self,
        project: &str,
        tool: ToolKind,
        name: Option<String>,
        prompt: Option<String>,
        resume: Option<String>,
        resume_note: Option<String>,
    ) -> Result<String, String>;
    /// The tool's own session id for a terminal, once it has reported one.
    fn terminal_session(&self, project: &str, terminal: &str) -> Option<String>;
    /// Make the project, and optionally one of its terminals, the shown one.
    fn show(&self, project: &str, terminal: Option<&str>);
    /// The saved Connecto Home.
    fn saved_home(&self) -> Option<PathBuf>;
    /// Save Connecto Home (and let it, with what is inside it, be a project).
    fn save_home(&self, home: Option<PathBuf>) -> Result<(), String>;
    /// The Automations folder in use.
    fn automations_root(&self) -> Option<PathBuf>;
    /// Make the automations engine use `root` (`<home>/automations`).
    fn use_automations_root(&self, root: PathBuf) -> Result<(), String>;
}
