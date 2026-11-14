//! The contract between the UI and the packages manager: Connecto Home, the
//! packages in it (and the built-in one), per-project settings, and what is
//! installed into each project.
//!
//! Same rules as [`crate::AutomationsControl`]: the UI reads
//! [`PackagesControl::view`] each frame (a cheap `Arc` clone) and calls the
//! commands below. Every method returns promptly and touches no disk and no
//! network; work happens on the manager's own threads and its result shows in
//! the next view. Argument mistakes come back as `Err(String)` at once; later
//! failures show in [`PackagesView::last_error`] or in a project's
//! [`ProjectPackages::state`].
//!
//! Where commands hook in: `connecto-ui` keeps an `Arc<dyn PackagesControl>`
//! beside its `AutomationsControl`; the Settings sheet (Connecto Home, default
//! package, package list) and the per-project settings sheet call it. The
//! new-terminal chooser reads [`PackagesView::projects`] (or
//! [`PackagesControl::allowed_tools`]) to know which tools to offer.

use std::path::PathBuf;
use std::sync::Arc;

use crate::ToolKind;

/// Id of the built-in package.
pub const BUILTIN_PACKAGE_ID: &str = "connecto";

/// Which tools a project uses. Restricts the new-terminal chooser and which
/// tool folders a package installs (Claude: `.claude`; Codex: `.codex` and
/// `.agents`; Both: all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectTools {
    Claude,
    Codex,
    #[default]
    Both,
}

impl ProjectTools {
    #[must_use]
    pub const fn allows(self, tool: ToolKind) -> bool {
        matches!(
            (self, tool),
            (Self::Both, _) | (Self::Claude, ToolKind::Claude) | (Self::Codex, ToolKind::Codex)
        )
    }

    /// The tools to offer, Claude first.
    #[must_use]
    pub fn tools(self) -> Vec<ToolKind> {
        [ToolKind::Claude, ToolKind::Codex]
            .into_iter()
            .filter(|t| self.allows(*t))
            .collect()
    }

    /// Stable word used in `settings.json`.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Both => "both",
        }
    }

    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        [Self::Claude, Self::Codex, Self::Both]
            .into_iter()
            .find(|t| t.word() == word)
    }
}

/// Where a tool reads its configuration from in a project's terminals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigMode {
    /// A per-project profile (`CLAUDE_CONFIG_DIR` / `CODEX_HOME` are set), with
    /// its own login. An advanced choice; what every project had before the
    /// default became [`ConfigMode::System`].
    Isolated,
    /// The user's own `~/.claude` / `~/.codex`: the variables are not set, so the
    /// tool has the user's usual login and configuration. The default for a new
    /// project.
    #[default]
    System,
}

impl ConfigMode {
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Isolated => "isolated",
            Self::System => "system",
        }
    }

    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        [Self::Isolated, Self::System]
            .into_iter()
            .find(|m| m.word() == word)
    }
}

/// The settings of one project. Defaults for a new project: `Both`, `System`,
/// `System`, `true`, no packages. A project that already existed keeps what it
/// had: see `connecto_session::project_settings::pin_existing_projects`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSettings {
    pub tools: ProjectTools,
    pub claude_config: ConfigMode,
    pub codex_config: ConfigMode,
    /// Apply the default package (when one is set) to this project.
    pub use_default_package: bool,
    /// Extra packages by id, installed in this order after the default one.
    pub packages: Vec<String>,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            tools: ProjectTools::Both,
            claude_config: ConfigMode::System,
            codex_config: ConfigMode::System,
            use_default_package: true,
            packages: Vec::new(),
        }
    }
}

/// One package, as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackageItem {
    /// Folder name under `<home>/packages/`; `"connecto"` for the built-in one.
    /// The id for every command.
    pub id: String,
    pub name: String,
    pub description: String,
    /// Starts at 1; Connecto bumps it by one whenever the content hash changes.
    pub version: u32,
    pub builtin: bool,
    /// The id of the package this one builds on (installed first).
    pub extends: Option<String>,
    pub is_default: bool,
    /// Which tools it has files for (`.claude` / `.codex` or `.agents`).
    pub has_claude: bool,
    pub has_codex: bool,
    /// `package.json` or the layout does not validate: the reasons. The
    /// package is listed (so it can be edited) but is never installed.
    pub errors: Vec<String>,
}

/// How the last install of a package into a project ended.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum InstallState {
    /// No package applies to this project, or nothing was attempted yet.
    #[default]
    None,
    /// Being checked or copied right now.
    Working,
    /// Installed versions equal the available ones; nothing changed on the
    /// latest check.
    UpToDate,
    /// The latest check installed or removed files. Running terminals need a
    /// restart to see them.
    Updated,
    /// The install failed (unreadable package, project folder not writable,
    /// lockfile corrupt ...): the reason. Nothing is half-applied.
    Error(String),
}

/// One package as installed in one project.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstalledPackage {
    pub id: String,
    /// What the lockfile says is installed; `None` if not installed yet.
    pub installed_version: Option<u32>,
    /// What the package offers now; `None` if the package no longer exists.
    pub available_version: Option<u32>,
}

/// What is installed in one project and how it went.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectPackages {
    pub state: InstallState,
    /// In install order: bases before the packages that extend them.
    pub packages: Vec<InstalledPackage>,
    /// Files the install left alone because the user edited them (path
    /// relative to the project). Shown as warnings.
    pub skipped_modified: Vec<String>,
    /// Other warnings (a package that was set but is missing, a `settings.json`
    /// key the user owns and was not replaced ...).
    pub warnings: Vec<String>,
    /// Unix milliseconds of the latest check, if any.
    pub checked_ms: Option<u64>,
    /// Terminals started before the latest update that changed files: they
    /// need a restart.
    pub restart_needed: bool,
}

/// One open project's settings and install status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectPackagesView {
    /// The project id (`ProjectView::id`).
    pub project_id: String,
    pub settings: ProjectSettings,
    pub status: ProjectPackages,
}

/// Result of moving/choosing Connecto Home.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HomeMigration {
    /// Nothing to do (first choice, or the same folder).
    #[default]
    None,
    /// Running on the manager's thread.
    Working,
    /// Existing automations were moved into `<home>/automations/`.
    Done { moved: usize },
    /// Failed; the old location is untouched. The reason.
    Error(String),
}

/// Everything the Settings sheet and the project settings show.
#[derive(Debug, Clone, Default)]
pub struct PackagesView {
    /// Bumped on every change; compare to skip work.
    pub revision: u64,
    /// Connecto Home, once chosen (also the former Automations folder).
    pub home: Option<PathBuf>,
    pub migration: HomeMigration,
    /// The built-in package first, then the user's by name.
    pub packages: Vec<PackageItem>,
    /// The default package's id, if any. Initially [`BUILTIN_PACKAGE_ID`].
    pub default_package: Option<String>,
    /// One entry per open project.
    pub projects: Vec<ProjectPackagesView>,
    /// The latest failure of a background step, cleared by the next success.
    pub last_error: Option<String>,
}

impl PackagesView {
    /// The entry for a project, if it is open.
    #[must_use]
    pub fn project(&self, project_id: &str) -> Option<&ProjectPackagesView> {
        self.projects.iter().find(|p| p.project_id == project_id)
    }

    /// Settings of a project; defaults when it is unknown.
    #[must_use]
    pub fn settings_of(&self, project_id: &str) -> ProjectSettings {
        self.project(project_id)
            .map(|p| p.settings.clone())
            .unwrap_or_default()
    }

    /// The tools the new-terminal chooser offers for a project (both for an
    /// unknown project).
    #[must_use]
    pub fn allowed_tools(&self, project_id: &str) -> Vec<ToolKind> {
        self.settings_of(project_id).tools.tools()
    }
}

const UNAVAILABLE: &str = "packages are not available";

pub trait PackagesControl: Send + Sync {
    fn view(&self) -> Arc<PackagesView>;

    /// The tools to offer for a project. Cheap; same as
    /// `view().allowed_tools(project_id)`.
    fn allowed_tools(&self, project_id: &str) -> Vec<ToolKind> {
        self.view().allowed_tools(project_id)
    }

    /// Choose Connecto Home: an existing, writable folder, not the data folder
    /// or the user's home folder. Saved at once; the move of existing
    /// automations and the creation of `automations/` and `packages/` run on
    /// the manager's thread (`view().migration`).
    fn set_home(&self, path: PathBuf) -> Result<(), String> {
        let _ = path;
        Err(UNAVAILABLE.to_owned())
    }

    /// Make `package` the default (applied to projects that use it, at once),
    /// or `None` for no default. Unknown ids are refused.
    fn set_default_package(&self, package: Option<&str>) -> Result<(), String> {
        let _ = package;
        Err(UNAVAILABLE.to_owned())
    }

    /// Make `<home>/packages/<id>/` with a template `package.json` and open a
    /// Claude terminal in Connecto Home briefed to ask which skills and plugins
    /// to include. `name` is checked at once (non-empty, becomes the id;
    /// must not exist); `extends` must be an existing package id.
    /// `Err("Set Connecto Home first")` without a home.
    fn create_package(&self, name: &str, extends: Option<&str>) -> Result<(), String> {
        let _ = (name, extends);
        Err(UNAVAILABLE.to_owned())
    }

    /// Show the editing session of a package, starting one if none is open.
    /// Refused for the built-in package.
    fn edit_package(&self, id: &str) -> Result<(), String> {
        let _ = id;
        Err(UNAVAILABLE.to_owned())
    }

    /// Close its session, remove the folder. Refused for the built-in package.
    /// Projects that listed it keep their installed files until the next
    /// update removes them; if it was the default, the default becomes none.
    fn delete_package(&self, id: &str) -> Result<(), String> {
        let _ = id;
        Err(UNAVAILABLE.to_owned())
    }

    /// Save a project's settings (validated: package ids must exist) and
    /// re-apply packages to it. The project's running terminals keep their
    /// environment until restarted.
    fn set_project_settings(
        &self,
        project_id: &str,
        settings: ProjectSettings,
    ) -> Result<(), String> {
        let _ = (project_id, settings);
        Err(UNAVAILABLE.to_owned())
    }

    /// Check and install now (the same step that runs on project open and on
    /// a version bump). The result shows in the project's `status`.
    fn refresh_project(&self, project_id: &str) {
        let _ = project_id;
    }
}

/// The control used when packages are not wired in: an empty view, and every
/// command refused. Lets `connecto-ui` build and run without a backend.
#[derive(Debug, Default)]
pub struct NoPackages;

impl PackagesControl for NoPackages {
    fn view(&self) -> Arc<PackagesView> {
        Arc::new(PackagesView::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_restrict_chooser() {
        assert_eq!(ProjectTools::Claude.tools(), vec![ToolKind::Claude]);
        assert_eq!(ProjectTools::Codex.tools(), vec![ToolKind::Codex]);
        assert_eq!(ProjectTools::Both.tools().len(), 2);
        assert_eq!(NoPackages.allowed_tools("x").len(), 2);
        assert_eq!(ProjectSettings::default().tools, ProjectTools::Both);
    }

    #[test]
    fn words_round_trip() {
        for t in [
            ProjectTools::Claude,
            ProjectTools::Codex,
            ProjectTools::Both,
        ] {
            assert_eq!(ProjectTools::from_word(t.word()), Some(t));
        }
        for m in [ConfigMode::Isolated, ConfigMode::System] {
            assert_eq!(ConfigMode::from_word(m.word()), Some(m));
        }
    }
}
