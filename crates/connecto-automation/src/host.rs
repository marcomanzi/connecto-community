//! What the engine needs from the running app, behind a trait so tests can
//! stand in for it. [`WorkspaceHost`] is the real one.

use std::path::{Path, PathBuf};

use connecto_core::{TermActivity, ToolKind};

/// How many rows from the bottom [`Host::screen`] needs to give. The engine
/// uses the last 60 lines that are not blank or borders, so this leaves room.
pub const SCREEN_ROWS: usize = 200;

/// One terminal of one project.
#[derive(Debug, Clone)]
pub struct TermInfo {
    pub project_id: String,
    pub project_name: String,
    pub project_path: PathBuf,
    pub id: String,
    pub name: Option<String>,
    pub tool: ToolKind,
    pub state: TermActivity,
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: String,
    /// Its terminals are available.
    pub ready: bool,
    pub terminals: Vec<(String, ToolKind)>,
}

pub trait Host: Send + Sync {
    fn terminals(&self) -> Vec<TermInfo>;
    /// The bottom of the terminal's text: its last [`SCREEN_ROWS`] rows or
    /// more, always read the same way (the engine compares hashes of it). The
    /// engine looks at this every tick, so it should not build the whole
    /// scrollback. May wait on a lock.
    fn screen(&self, project: &str, terminal: &str) -> Option<String>;
    fn paused(&self) -> bool;
    /// Type keys as the bridge `keys` op does. Blocks up to 30 s.
    fn keys(&self, project: &str, terminal: &str, keys: &[String]) -> Result<(), String>;
    fn open_project(&self, dir: &Path) -> Result<(), String>;
    fn project_at(&self, dir: &Path) -> Option<ProjectInfo>;
    /// Add a terminal and return the id it was given. `prompt` is the first message of a new conversation;
    /// `resume` picks that tool session up again (the prompt then applies only
    /// if the resume fails). `resume_note` is a short first message into the
    /// resumed conversation.
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
    /// The saved Automations folder.
    fn saved_root(&self) -> Option<PathBuf>;
    /// Save the Automations folder (and let it be a project).
    fn save_root(&self, root: Option<PathBuf>) -> Result<(), String>;
}
