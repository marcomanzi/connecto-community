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

/// Id of the built-in Game Dev package (Godot), ADR 0037.
pub const GAME_DEV_PACKAGE_ID: &str = "game-dev";

/// The id the built-in package had under the product's earlier name. Settings,
/// locks and `extends` written then still name it.
pub const LEGACY_BUILTIN_PACKAGE_ID: &str = "magneto";

/// A package id as the current build names it: the earlier built-in id is the
/// current one; any other id is returned as given.
#[must_use]
pub fn current_package_id(id: &str) -> &str {
    if id == LEGACY_BUILTIN_PACKAGE_ID {
        BUILTIN_PACKAGE_ID
    } else {
        id
    }
}

/// Which tools a project uses: one or more of the coding CLIs, never none.
/// Restricts the new-terminal chooser and which tool folders a package
/// installs (Claude: `.claude`; Codex: `.codex` and `.agents`; Pi,
/// Antigravity and OpenCode: `.agents`, where their skills are).
///
/// The constants name the sets older builds could save: one tool, or
/// [`ProjectTools::All`]. Any other set is built with
/// [`ProjectTools::from_tools`] or [`ProjectTools::with`].
///
/// Saved as two keys (see `connecto_session::project_settings`): `tools`, a
/// word an older build understands ([`ProjectTools::legacy_word`]: the tool's
/// word for one tool, `all` for every other set, a superset that keeps such a
/// build working), and `tool_list`, the exact set ([`ProjectTools::words`]).
/// A reader takes `tool_list` while `tools` still says what this set would
/// write; when an older build has changed `tools` since, `tools` wins.
///
/// A known limit: an older build keeps `tool_list` (it keeps keys it does
/// not know) and cannot tell it apart. If it changes Codex + Pi to Claude
/// and then back to all before a newer build reads the file, the file is
/// the same as one it never changed (`all`, `["codex","pi"]`), and Codex +
/// Pi comes back. Choosing all again in the newer build fixes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectTools(u8);

#[allow(non_upper_case_globals)]
impl ProjectTools {
    pub const Claude: Self = Self(1);
    pub const Codex: Self = Self(1 << 1);
    pub const Pi: Self = Self(1 << 2);
    pub const Antigravity: Self = Self(1 << 3);
    pub const OpenCode: Self = Self(1 << 4);
    /// Every agent. Saved as `all`; a file from before Pi says `both`.
    pub const All: Self = Self(0b1_1111);

    /// The coding CLIs a project can choose, Claude first: the order every
    /// list of them uses.
    pub const CHOICES: [ToolKind; 5] = [
        ToolKind::Claude,
        ToolKind::Codex,
        ToolKind::Pi,
        ToolKind::Antigravity,
        ToolKind::OpenCode,
    ];

    const fn bit(tool: ToolKind) -> u8 {
        match tool {
            ToolKind::Claude => Self::Claude.0,
            ToolKind::Codex => Self::Codex.0,
            ToolKind::Pi => Self::Pi.0,
            ToolKind::Antigravity => Self::Antigravity.0,
            ToolKind::OpenCode => Self::OpenCode.0,
            ToolKind::Shell => 0,
        }
    }

    const fn tool_word(tool: ToolKind) -> &'static str {
        match tool {
            ToolKind::Claude => "claude",
            ToolKind::Codex => "codex",
            ToolKind::Pi => "pi",
            ToolKind::Antigravity => "antigravity",
            ToolKind::OpenCode => "opencode",
            ToolKind::Shell => "shell",
        }
    }

    /// The set of these tools; `None` when there is none (a shell is not one).
    #[must_use]
    pub fn from_tools(tools: impl IntoIterator<Item = ToolKind>) -> Option<Self> {
        let bits = tools.into_iter().fold(0, |bits, t| bits | Self::bit(t));
        (bits != 0).then_some(Self(bits))
    }

    /// This set with `tool` turned on or off; `None` when that would leave
    /// none.
    #[must_use]
    pub const fn with(self, tool: ToolKind, on: bool) -> Option<Self> {
        let bits = if on {
            self.0 | Self::bit(tool)
        } else {
            self.0 & !Self::bit(tool)
        };
        if bits == 0 { None } else { Some(Self(bits)) }
    }

    #[must_use]
    pub const fn allows(self, tool: ToolKind) -> bool {
        self.0 & Self::bit(tool) != 0
    }

    /// Every tool there is.
    #[must_use]
    pub const fn is_all(self) -> bool {
        self.0 == Self::All.0
    }

    /// How many tools are in the set.
    #[must_use]
    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Never: a set always has a tool. For the `len` convention.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The tools to offer, Claude first.
    #[must_use]
    pub fn tools(self) -> Vec<ToolKind> {
        Self::CHOICES
            .into_iter()
            .filter(|t| self.allows(*t))
            .collect()
    }

    /// The word an older build reads in `tools`: the tool's own for a set of
    /// one, else `all` (a superset, so such a build still offers and installs
    /// every tool chosen here).
    #[must_use]
    pub fn legacy_word(self) -> &'static str {
        match self.tools().as_slice() {
            [one] => Self::tool_word(*one),
            _ => "all",
        }
    }

    /// The set an older build saved as `word` (one tool, `all`, or `both`,
    /// what `all` was called when there were two agents).
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        if word == "all" || word == "both" {
            return Some(Self::All);
        }
        Self::CHOICES
            .into_iter()
            .find(|t| Self::tool_word(*t) == word)
            .map(|t| Self(Self::bit(t)))
    }

    /// Each tool's word, Claude first: what `tool_list` holds.
    #[must_use]
    pub fn words(self) -> Vec<&'static str> {
        self.tools().into_iter().map(Self::tool_word).collect()
    }

    /// The set these words name. `None` when one of them is not a tool's
    /// word (a newer build's tool: the caller falls back to `tools`) or there
    /// are none.
    #[must_use]
    pub fn from_words<'a>(words: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let mut bits = 0;
        for word in words {
            let tool = Self::CHOICES
                .into_iter()
                .find(|t| Self::tool_word(*t) == word)?;
            bits |= Self::bit(tool);
        }
        (bits != 0).then_some(Self(bits))
    }

    /// The set saved as `word` (`tools`) and `list` (`tool_list`, when
    /// there is one): the list while `word` is still what the list's set
    /// writes, else (no list, an unknown tool in it, or an older build has
    /// changed `tools` since) the word. `None` when the word is unknown and
    /// there is no usable list.
    #[must_use]
    pub fn from_saved<'a>(
        word: Option<&str>,
        list: Option<impl IntoIterator<Item = &'a str>>,
    ) -> Option<Self> {
        let listed = list.and_then(Self::from_words);
        match (word, listed) {
            (Some(word), Some(set)) if set.legacy_word() == word => Some(set),
            (Some(word), _) => Self::from_word(word).or(listed),
            (None, listed) => listed,
        }
    }
}

impl ProjectTools {
    /// What a peer or build from before sets of tools means by `sent` (one
    /// tool or all, as it was shown `self.legacy_word()`): when it is the
    /// word this set shows such a peer, it left the tools alone and the set
    /// stays; else it chose `sent`.
    #[must_use]
    pub fn from_older_peer(self, sent: Self) -> Self {
        if self.legacy_word() == sent.legacy_word() {
            self
        } else {
            sent
        }
    }
}

impl Default for ProjectTools {
    fn default() -> Self {
        Self::All
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

/// The settings of one project. Defaults for a new project: `All`, `System`,
/// `System`, `true`, no packages. A project that already existed keeps what it
/// had: see `connecto_session::project_settings::pin_existing_projects`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSettings {
    pub tools: ProjectTools,
    pub claude_config: ConfigMode,
    pub codex_config: ConfigMode,
    /// Pi: the user's own `~/.pi/agent`, or a per-project profile
    /// (`PI_CODING_AGENT_DIR`).
    pub pi_config: ConfigMode,
    /// OpenCode: the user's own folders (`~/.config/opencode`,
    /// `~/.local/share/opencode`, `~/.local/state/opencode`), or a
    /// per-project profile that holds all three (`XDG_CONFIG_HOME`,
    /// `XDG_DATA_HOME`, `XDG_STATE_HOME` point into it).
    pub opencode_config: ConfigMode,
    /// Apply the default package (when one is set) to this project.
    pub use_default_package: bool,
    /// Extra packages by id, installed in this order after the default one.
    pub packages: Vec<String>,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            tools: ProjectTools::All,
            claude_config: ConfigMode::System,
            codex_config: ConfigMode::System,
            pi_config: ConfigMode::System,
            opencode_config: ConfigMode::System,
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
    /// The tools it needs on the machine the packages run on, and whether
    /// each is there (ADR 0037).
    pub requires: Vec<RequirementView>,
}

impl PackageItem {
    /// The tools it needs that are not there (or not checked yet).
    pub fn unmet(&self) -> impl Iterator<Item = &RequirementView> {
        self.requires.iter().filter(|r| !r.state.met())
    }
}

/// Whether a tool a package needs is on the machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RequirementState {
    /// Being looked for (also before the first look).
    #[default]
    Checking,
    /// There; its version when one was asked for and could be read.
    Found {
        version: Option<String>,
    },
    Missing,
    /// There, but not a version the package can use. `wanted` says which
    /// will do (`4.7 or later, before 5`).
    WrongVersion {
        version: String,
        wanted: String,
    },
    /// There, but its version could not be read (it did not start, failed,
    /// or printed none), and only some versions will do.
    NoVersion {
        wanted: String,
    },
}

impl RequirementState {
    #[must_use]
    pub const fn met(&self) -> bool {
        matches!(self, Self::Found { .. })
    }
}

/// One tool a package needs, as the dialog shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequirementView {
    /// Unique in its package; what [`PackagesControl::install_requirement`]
    /// names.
    pub id: String,
    pub name: String,
    pub why: String,
    pub state: RequirementState,
    /// The command Connecto offers to run, in a shell terminal of the
    /// project, on this system. Never set for a `licence` tool.
    pub install: Option<String>,
    /// What to do by hand.
    pub manual: String,
    /// Where to read more or download it.
    pub url: Option<String>,
    /// Getting it means accepting a licence or EULA, or signing in: the user
    /// does that themselves.
    pub licence: bool,
}

/// Why `settings` cannot be saved for `project_id`: a package it adds (or
/// the default package it turns on) needs a tool that is not there. Packages
/// the project already had are not checked again, so a tool removed later does
/// not lock the project's settings. `None` when nothing stands in the way.
#[must_use]
pub fn requirements_refusal(
    view: &PackagesView,
    project_id: &str,
    settings: &ProjectSettings,
) -> Option<String> {
    let before = view.project(project_id).map(|p| &p.settings);
    let mut added: Vec<&str> = settings
        .packages
        .iter()
        .filter(|id| before.is_none_or(|b| !b.packages.contains(id)))
        .map(String::as_str)
        .collect();
    if settings.use_default_package
        && before.is_none_or(|b| !b.use_default_package)
        && let Some(default) = &view.default_package
    {
        added.push(default);
    }
    added.iter().find_map(|id| package_refusal(view, id))
}

/// Why `package` cannot be turned on now: a tool it, or a package it
/// extends, needs is not there.
#[must_use]
pub fn package_refusal(view: &PackagesView, package: &str) -> Option<String> {
    let mut id = Some(current_package_id(package));
    let mut seen = 0;
    while let Some(current) = id {
        let item = view.packages.iter().find(|p| p.id == current)?;
        if let Some(missing) = item.unmet().next() {
            let what = match &missing.state {
                RequirementState::Checking => "is still being looked for: try again in a moment",
                RequirementState::WrongVersion { .. } => {
                    "is not a version it can use: install one that is"
                }
                RequirementState::NoVersion { .. } => {
                    "did not tell its version: check that it starts"
                }
                _ => "is not installed: install it first",
            };
            return Some(format!(
                "{} needs {}, which {what}",
                item.name, missing.name
            ));
        }
        seen += 1;
        id = item.extends.as_deref().filter(|_| seen < 8);
    }
    None
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
    /// What the last export or import did, cleared by the next command.
    pub notice: Option<String>,
    /// The packages are a remote host's whose Connecto is from before sets
    /// of tools: a project there uses one tool or all of them, and a set of
    /// several would be saved as all. Only those are offered.
    pub tools_one_or_all: bool,
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

    /// Write a user package as one file at `dest` (`<name>.connecto-package`),
    /// to give to someone else. Refused for the built-in package. The result
    /// shows in `view().notice` or `view().last_error`.
    fn export_package(&self, id: &str, dest: PathBuf) -> Result<(), String> {
        let _ = (id, dest);
        Err(UNAVAILABLE.to_owned())
    }

    /// Add the package in an exported file to Connecto Home as a new package
    /// (a new id when the name is taken; its version starts at 1). The result
    /// shows in `view().notice` or `view().last_error`.
    fn import_package(&self, path: PathBuf) -> Result<(), String> {
        let _ = path;
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

    /// As [`PackagesControl::set_project_settings`], for settings from a
    /// peer from before OpenCode, which says nothing of OpenCode's config:
    /// the project keeps the one it has when the write is applied, after
    /// every write queued before it. `settings.opencode_config` is ignored.
    /// Such a peer is from before sets of tools too: `settings.tools` is
    /// read as [`PackagesControl::set_project_settings_keeping_tools`] does.
    fn set_project_settings_keeping_opencode(
        &self,
        project_id: &str,
        settings: ProjectSettings,
    ) -> Result<(), String> {
        let _ = (project_id, settings);
        Err(UNAVAILABLE.to_owned())
    }

    /// As [`PackagesControl::set_project_settings`], for settings from a
    /// peer from before sets of tools, whose `settings.tools` is one tool or
    /// all: resolved against the project's set when the write is applied,
    /// after every write queued before it
    /// ([`ProjectTools::from_older_peer`]).
    fn set_project_settings_keeping_tools(
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

    /// Look again for the tools `package` needs ("I've installed it, check
    /// again"). The result shows in the package's `requires`.
    fn check_requirements(&self, package: &str) -> Result<(), String> {
        let _ = package;
        Err(UNAVAILABLE.to_owned())
    }

    /// Run the install command of one tool `package` needs, in a new shell
    /// terminal of `project_id`, where the user sees it work. Only after the
    /// user confirmed `command`, the text they were shown: refused if the
    /// package's command is not that text any more, and for a tool with no
    /// command for this system (or a `licence` one). Check again once it is
    /// done.
    fn install_requirement(
        &self,
        project_id: &str,
        package: &str,
        requirement: &str,
        command: &str,
    ) -> Result<(), String> {
        let _ = (project_id, package, requirement, command);
        Err(UNAVAILABLE.to_owned())
    }

    /// Whether the last settings save asked of `project_id` was refused
    /// after it was accepted for sending (another machine's settings answer
    /// later): what it set is not coming. A Refresh does not count. This machine's refusals come back at once,
    /// so by default none is.
    fn latest_change_refused(&self, project_id: &str) -> bool {
        let _ = project_id;
        false
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
        assert_eq!(ProjectTools::Pi.tools(), vec![ToolKind::Pi]);
        assert_eq!(
            ProjectTools::Antigravity.tools(),
            vec![ToolKind::Antigravity]
        );
        assert_eq!(ProjectTools::OpenCode.tools(), vec![ToolKind::OpenCode]);
        assert_eq!(ProjectTools::All.tools().len(), 5);
        assert_eq!(NoPackages.allowed_tools("x").len(), 5);
        assert_eq!(ProjectSettings::default().tools, ProjectTools::All);
        // A file from before Pi.
        assert_eq!(ProjectTools::from_word("both"), Some(ProjectTools::All));
    }

    #[test]
    fn words_round_trip() {
        for t in [
            ProjectTools::Claude,
            ProjectTools::Codex,
            ProjectTools::Pi,
            ProjectTools::Antigravity,
            ProjectTools::OpenCode,
            ProjectTools::All,
        ] {
            assert_eq!(ProjectTools::from_word(t.legacy_word()), Some(t));
            assert_eq!(ProjectTools::from_words(t.words()), Some(t));
        }
        for m in [ConfigMode::Isolated, ConfigMode::System] {
            assert_eq!(ConfigMode::from_word(m.word()), Some(m));
        }
    }

    /// Any non-empty subset of the CLIs, never none.
    #[test]
    fn a_project_chooses_any_set_of_tools_but_never_none() {
        let codex_pi = ProjectTools::Codex.with(ToolKind::Pi, true);
        assert_eq!(
            ProjectTools::from_tools([ToolKind::Codex, ToolKind::Pi]),
            codex_pi
        );
        let Some(codex_pi) = codex_pi else {
            unreachable!("two tools are a set")
        };
        assert_eq!(codex_pi.tools(), [ToolKind::Codex, ToolKind::Pi]);
        assert!(!codex_pi.allows(ToolKind::Claude));
        assert_eq!(codex_pi.legacy_word(), "all");
        assert_eq!(codex_pi.words(), ["codex", "pi"]);
        assert_eq!(ProjectTools::from_tools([]), None);
        assert_eq!(ProjectTools::from_tools([ToolKind::Shell]), None);
        assert_eq!(ProjectTools::Pi.with(ToolKind::Pi, false), None);
        assert_eq!(ProjectTools::Pi.with(ToolKind::Codex, true), Some(codex_pi));
        assert_eq!(
            codex_pi.with(ToolKind::Codex, false),
            Some(ProjectTools::Pi)
        );
        assert!(!ProjectTools::Claude.is_all() && ProjectTools::All.is_all());
        assert_eq!(ProjectTools::from_words(["codex", "nope"]), None);
    }

    fn game_dev(state: RequirementState) -> PackageItem {
        PackageItem {
            id: GAME_DEV_PACKAGE_ID.into(),
            name: "Game Dev".into(),
            requires: vec![RequirementView {
                id: "godot".into(),
                name: "Godot 4.7 or later".into(),
                state,
                ..RequirementView::default()
            }],
            ..PackageItem::default()
        }
    }

    fn view_with(item: PackageItem, settings: ProjectSettings) -> PackagesView {
        PackagesView {
            packages: vec![
                item,
                PackageItem {
                    id: "mine".into(),
                    name: "Mine".into(),
                    extends: Some(GAME_DEV_PACKAGE_ID.into()),
                    ..PackageItem::default()
                },
            ],
            projects: vec![ProjectPackagesView {
                project_id: "p".into(),
                settings,
                ..ProjectPackagesView::default()
            }],
            ..PackagesView::default()
        }
    }

    #[test]
    fn a_package_whose_tool_is_missing_cannot_be_added() {
        let start = ProjectSettings {
            use_default_package: false,
            ..ProjectSettings::default()
        };
        let adding = ProjectSettings {
            packages: vec![GAME_DEV_PACKAGE_ID.into()],
            ..start.clone()
        };
        let extending = ProjectSettings {
            packages: vec!["mine".into()],
            ..start.clone()
        };
        for state in [
            RequirementState::Missing,
            RequirementState::Checking,
            RequirementState::WrongVersion {
                version: "5.0".into(),
                wanted: "4.7 or later, before 5".into(),
            },
            RequirementState::NoVersion {
                wanted: "4.7 or later, before 5".into(),
            },
        ] {
            let view = view_with(game_dev(state.clone()), start.clone());
            let why = requirements_refusal(&view, "p", &adding).unwrap_or_default();
            assert!(why.contains("Godot 4.7 or later"), "{why}");
            assert!(
                requirements_refusal(&view, "p", &extending).is_some(),
                "a package that extends it needs the tool too ({state:?})"
            );
        }
        let found = RequirementState::Found {
            version: Some("4.7.2".into()),
        };
        let view = view_with(game_dev(found), start.clone());
        assert_eq!(requirements_refusal(&view, "p", &adding), None);
        // Already there: a tool removed later does not lock the settings.
        let view = view_with(game_dev(RequirementState::Missing), adding.clone());
        let mut other = adding.clone();
        other.tools = ProjectTools::Claude;
        assert_eq!(requirements_refusal(&view, "p", &other), None);
        // Turning on a default package that needs a missing tool.
        let mut view = view_with(game_dev(RequirementState::Missing), start.clone());
        view.default_package = Some(GAME_DEV_PACKAGE_ID.into());
        let on = ProjectSettings {
            use_default_package: true,
            ..start
        };
        assert!(requirements_refusal(&view, "p", &on).is_some());
    }
}
