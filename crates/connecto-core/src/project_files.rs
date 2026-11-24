//! Making a project from a git repository, and deleting a project together
//! with its folder. The UI asks through [`crate::WorkspaceControl`]; the work
//! runs off the UI thread and is followed through a [`ProjectTask`].

use std::path::PathBuf;
use std::sync::Arc;

/// Where a clone or a deletion stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    /// Still going; the latest progress line (may be empty).
    Running(String),
    /// Finished.
    Done,
    /// Stopped by a failure, said in words for the person.
    Failed(String),
    /// Stopped because the person asked. Nothing is left behind.
    Cancelled,
}

impl TaskState {
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }
}

/// Work that runs off the UI thread. Both methods return at once.
pub trait ProjectTask: Send + Sync {
    fn state(&self) -> TaskState;
    /// Ask it to stop. A clone that stops leaves no folder behind.
    fn cancel(&self);

    /// Where a finished clone was published, by its real path, once done.
    fn published(&self) -> Option<PathBuf> {
        None
    }
}

/// A clone under way.
#[derive(Clone)]
pub struct Cloning {
    /// The folder asked for (the chosen parent and the repository's name),
    /// to show while the clone runs. Not necessarily the real path: once the
    /// task is done, open [`ProjectTask::published`] instead.
    pub target: PathBuf,
    pub task: Arc<dyn ProjectTask>,
}

/// What deleting a project would remove, for the confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletePlan {
    pub project: String,
    /// The name the person types to confirm: the project's folder name.
    pub name: String,
    /// The full path of the folder that goes.
    pub path: String,
    /// What is in it, such as "1,204 files in 87 folders, 31.2 MB".
    pub contents: String,
    /// Where it goes ("the Trash"). A folder with no usable trash is
    /// refused: Connecto never deletes a folder for good.
    pub trash: String,
    /// Work that would be lost, such as uncommitted or unpushed git changes.
    pub warnings: Vec<String>,
    /// Which directory was looked at (its own and its parent's identity on
    /// disk), opaque to the UI. The deletion goes ahead only while the folder
    /// at the path is still this one.
    pub identity: Vec<u64>,
    /// The workspace holds the folder and its parent open for this plan
    /// (so their identities cannot pass to another folder) until it is used
    /// by [`crate::WorkspaceControl::delete_project`] or given back with
    /// [`crate::WorkspaceControl::release_delete_plan`].
    pub token: u64,
}
