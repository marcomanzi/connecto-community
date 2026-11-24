//! The contract between the UI and the workspace: the open projects in the
//! sidebar, the recent projects in the File menu, and the brakes that cover
//! every project (project-tree-ui R1, R2, R9).
//!
//! Every method returns promptly and touches no disk. A failure shows in
//! [`WorkspaceView::last_error`] or comes back as `Err` at once.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use crate::{PauseSwitch, TerminalsControl};

/// One open project in the sidebar.
#[derive(Clone)]
pub struct ProjectView {
    /// Stable for the folder, the same id the data folder uses
    /// (`projects/<id>/`).
    pub id: String,
    /// The user's name for it, or the folder name, with the parent folder added
    /// when two open projects share a folder name.
    pub name: String,
    /// The folder, for the tooltip and the File menu.
    pub path: String,
    /// Its terminal registry. `None` while it is being set up, or if setup
    /// failed (then `error` says why).
    pub terminals: Option<Arc<dyn TerminalsControl>>,
    /// Why the project could not be opened this run, such as a folder that no
    /// longer exists.
    pub error: Option<String>,
    /// Whether its terminals show in the tree.
    pub expanded: bool,
    /// The terminal last shown in this project, restored at launch.
    pub active_terminal: Option<String>,
    /// The id of the sidebar group it is in ([`WorkspaceView::groups`]), if any.
    pub group: Option<String>,
}

impl fmt::Debug for ProjectView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectView")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("path", &self.path)
            .field("ready", &self.terminals.is_some())
            .field("error", &self.error)
            .field("expanded", &self.expanded)
            .field("active_terminal", &self.active_terminal)
            .field("group", &self.group)
            .finish()
    }
}

/// A named group of projects in the sidebar. Only a label: a project in a
/// group runs as any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectGroup {
    /// Stable while the group exists (`group-1`, `group-2` ...).
    pub id: String,
    pub name: String,
}

/// A project in File > Open Recent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentProject {
    pub path: String,
    pub name: String,
}

/// Everything the sidebar and the menus draw.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceView {
    /// In sidebar order.
    pub projects: Vec<ProjectView>,
    /// The sidebar's project groups, in the order they are shown.
    pub groups: Vec<ProjectGroup>,
    /// The project shown, by id.
    pub active: Option<String>,
    /// Newest first, at most ten. Open projects are included.
    pub recents: Vec<RecentProject>,
    pub sidebar_collapsed: bool,
    /// The latest failure of a request, until the next request.
    pub last_error: Option<String>,
    /// A standing warning, such as an unreadable `workspace.json` kept aside.
    pub warning: Option<String>,
}

const GROUPS_UNAVAILABLE: &str = "Project groups are not available here.";

/// User commands on the workspace. Each returns promptly and is persisted by
/// the workspace, so a relaunch restores it.
pub trait WorkspaceControl: Send + Sync {
    /// The latest state. Never blocks.
    fn view(&self) -> Arc<WorkspaceView>;

    /// Open a folder as a project, or show it if it is already open. It is
    /// added to the sidebar and the recents, becomes active, and its saved
    /// terminals start. `Err` names a refused folder (home, Connecto's data...).
    fn open_project(&self, path: &Path) -> Result<(), String>;

    /// Show this project. Nothing starts or stops.
    fn activate(&self, project: &str);

    /// Stop every terminal of the project and take it out of the sidebar. It
    /// stays in the recents, and its terminal list and history stay on disk.
    fn close_project(&self, project: &str) -> Result<(), String>;

    /// Set or clear (`None`, back to the folder name) a project's name.
    fn rename_project(&self, project: &str, name: Option<String>) -> Result<(), String>;

    /// Move a project to `index` in the sidebar.
    fn move_project(&self, project: &str, index: usize);

    fn set_expanded(&self, project: &str, expanded: bool);

    /// Make a group named `name` (one line, not empty). It shows at once,
    /// empty; projects are put in it with [`Self::set_project_group`].
    fn create_group(&self, name: &str) -> Result<(), String> {
        let _ = name;
        Err(GROUPS_UNAVAILABLE.to_owned())
    }

    fn rename_group(&self, group: &str, name: &str) -> Result<(), String> {
        let _ = (group, name);
        Err(GROUPS_UNAVAILABLE.to_owned())
    }

    /// Remove a group. Its projects stay open, with no group.
    fn delete_group(&self, group: &str) -> Result<(), String> {
        let _ = group;
        Err(GROUPS_UNAVAILABLE.to_owned())
    }

    /// Put a project in a group, or take it out of its group (`None`).
    fn set_project_group(&self, project: &str, group: Option<&str>) -> Result<(), String> {
        let _ = (project, group);
        Err(GROUPS_UNAVAILABLE.to_owned())
    }

    fn set_sidebar_collapsed(&self, collapsed: bool);

    /// Remember which terminal the project shows.
    fn set_active_terminal(&self, project: &str, terminal: &str);

    fn clear_recents(&self);

    /// One switch for every project's agents. Flipping it never blocks.
    fn pause_switch(&self) -> Arc<PauseSwitch>;

    /// Pause, then stop every terminal of every project. Nothing is closed.
    fn stop_everything(&self);

    /// The app-wide utility model and effort. Never blocks.
    fn utility_settings(&self) -> crate::UtilitySettings {
        crate::UtilitySettings::default()
    }

    /// Change them. Applies to later runs; saved off the UI thread.
    fn set_utility_settings(&self, settings: crate::UtilitySettings) {
        let _ = settings;
    }

    /// How each tool is launched with respect to its permission prompts.
    fn launch_modes(&self) -> crate::LaunchModes {
        crate::LaunchModes::default()
    }

    /// Change them. Applies to every launch and restart from now on; a running
    /// terminal keeps what it was launched with. Saved off the UI thread.
    fn set_launch_modes(&self, modes: crate::LaunchModes) {
        let _ = modes;
    }

    /// Whether secrets are kept in the system store (the macOS Keychain,
    /// Windows Credential Manager) rather than Connecto's secrets file, as
    /// saved. The store in use is the one chosen when this run started.
    fn secrets_in_system_store(&self) -> bool {
        false
    }

    /// Change it. Takes effect at the next launch, which moves the secrets
    /// across; saved off the UI thread.
    fn set_secrets_in_system_store(&self, on: bool) {
        let _ = on;
    }

    /// Clone the git repository at `url` into a new folder inside `parent`,
    /// named after the repository, with the system's git (its credential
    /// helpers and SSH agent; Connecto never asks for a password). The folder
    /// must not exist yet. Returns at once, before any disk work (a refusal
    /// found then ends the task as failed); once the task is done, open
    /// [`crate::project_files::ProjectTask::published`], the folder it was
    /// published to by its real path.
    fn clone_project(
        &self,
        url: &str,
        parent: &Path,
    ) -> Result<crate::project_files::Cloning, String> {
        let _ = (url, parent);
        Err(PROJECT_FILES_UNAVAILABLE.to_owned())
    }

    /// What deleting `project` would remove, or why it is refused (home, a
    /// system folder, a folder holding other projects or Connecto's data, a
    /// link). Unlike the rest, this reads the disk (git status, a count of the
    /// files): call it off the UI thread.
    fn plan_delete(&self, project: &str) -> Result<crate::project_files::DeletePlan, String> {
        let _ = project;
        Err(PROJECT_FILES_UNAVAILABLE.to_owned())
    }

    /// Close the project `plan` was made for, stop its terminals and
    /// background work, then move its folder to the system's trash and forget
    /// it: out of the recents, its terminal list and history gone. Refused
    /// when the folder is no longer the one `plan` describes, and for the
    /// refusals of [`Self::plan_delete`]. Nothing is ever deleted for good.
    /// Returns at once, without touching the disk: the folder's checks run
    /// before anything stops, and a refusal they find ends the task as
    /// failed. The plan's hold on the folder goes with the task.
    fn delete_project(
        &self,
        plan: &crate::project_files::DeletePlan,
    ) -> Result<Arc<dyn crate::project_files::ProjectTask>, String> {
        let _ = plan;
        Err(PROJECT_FILES_UNAVAILABLE.to_owned())
    }

    /// The confirmation for `plan` closed without a deletion: what the
    /// workspace held open for it is let go. Returns at once.
    fn release_delete_plan(&self, plan: &crate::project_files::DeletePlan) {
        let _ = plan;
    }
}

const PROJECT_FILES_UNAVAILABLE: &str = "This is not available here.";
