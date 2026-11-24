//! The manager's one thread: Connecto Home, the packages in it, each project's
//! settings, and keeping every open project's installed files current.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use arc_swap::ArcSwap;
use connecto_core::{
    HomeMigration, InstallState, InstalledPackage, PackageItem, PackagesView, ProjectPackages,
    ProjectPackagesView, ProjectSettings, RequirementState, RequirementView, ToolKind,
};
use connecto_hook::package::{self, FILE_NAME, Manifest};
use connecto_hook::requires::{Os, Requirement};
use connecto_session::Wake;
use connecto_session::project_settings;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};

use crate::host::Host;
use crate::package::{
    BUILTIN_ID, GAME_DEV_ID, Package, Seen, Variant, builtin, game_dev, is_builtin, load_dir,
    signature,
};
use crate::state::State;
use crate::{bundle, guide, home, install, session};

pub const TICK: Duration = Duration::from_millis(400);
const OPEN_LIMIT: Duration = Duration::from_secs(30);
const DEFAULTS_WAIT: Duration = Duration::from_secs(3);
const MAX_DEPTH: usize = 8;
pub const NO_HOME: &str = "Set Connecto Home first";
/// How the note about a packages folder that cannot be listed begins.
const UNREADABLE: &str = "the packages folder";

pub enum Cmd {
    SetHome(PathBuf),
    SetDefault(Option<String>),
    Create {
        name: String,
        extends: Option<String>,
    },
    Edit(String),
    Delete(String),
    /// Write the package as one file at the path.
    Export {
        id: String,
        dest: PathBuf,
    },
    /// Add the package in the exported file as a new package.
    Import(PathBuf),
    SetProject(String, ProjectSettings),
    /// As `SetProject`, from a peer from before OpenCode: the project keeps
    /// the OpenCode config it has when this is applied.
    SetProjectKeepingOpenCode(String, ProjectSettings),
    /// As `SetProject`, from a peer from before sets of tools: its one tool
    /// or all is resolved against the project's set when this is applied
    /// (`ProjectTools::from_older_peer`). `SetProjectKeepingOpenCode` does
    /// the same.
    SetProjectKeepingTools(String, ProjectSettings),
    Refresh(String),
    /// Look again for the tools a package needs.
    CheckRequirements(String),
    /// Run a tool's install command in a new shell terminal of the project.
    InstallRequirement {
        project: String,
        package: String,
        requirement: String,
        /// The command the user approved; run only if it is still the one.
        command: String,
    },
    Stop,
}

/// Looks for one tool: [`crate::requires::detect`] outside tests.
/// The second argument is the folders a found program may not be in
/// ([`crate::requires::locate_all`]).
pub type Detector = Arc<dyn Fn(&Requirement, &[PathBuf]) -> RequirementState + Send + Sync>;

/// What is known of the tools one package needs.
struct Checks {
    /// The requirements these states are for; a change starts a new look.
    requires: Vec<Requirement>,
    states: Vec<RequirementState>,
    /// The look whose answer counts (older answers are dropped).
    generation: u64,
}

pub struct Config {
    pub data_root: PathBuf,
    /// The user's home folder (Connecto Home may not be it).
    pub user_home: PathBuf,
    pub host: Arc<dyn Host>,
    pub wake: Wake,
    /// How the tools packages need are looked for; `None`: on this machine.
    pub detector: Option<Detector>,
}

/// One folder in `<home>/packages/`.
struct Entry {
    id: String,
    package: Result<Arc<Package>, String>,
    /// What `connecto package check` finds wrong.
    problems: Vec<String>,
}

struct ProjectState {
    id: String,
    path: PathBuf,
    settings: ProjectSettings,
    status: ProjectPackages,
    dirty: bool,
}

struct Opening {
    id: String,
    name: String,
    prompt: String,
    note: Option<String>,
    recorded: Option<session::Record>,
    started: Instant,
    requested: bool,
    ready_since: Option<Instant>,
}

struct Binding {
    dir: PathBuf,
    project: String,
    terminal: String,
    session: Option<String>,
    guide: Option<u32>,
}

pub struct Core {
    cfg: Config,
    view: Arc<ArcSwap<PackagesView>>,
    home: Option<PathBuf>,
    migration: HomeMigration,
    state: State,
    registry: Vec<Entry>,
    builtins: HashMap<Variant, Arc<Package>>,
    game_dev: Option<Arc<Package>>,
    /// By package id.
    checks: HashMap<String, Checks>,
    checked_tx: Sender<(String, u64, Vec<RequirementState>)>,
    checked_rx: Receiver<(String, u64, Vec<RequirementState>)>,
    detector: Detector,
    /// Numbers each look, so only the latest one's answer counts.
    generation: u64,
    projects: Vec<ProjectState>,
    openings: Vec<Opening>,
    bindings: Vec<Binding>,
    signature: Vec<Seen>,
    unsettled: bool,
    /// How long an install waits for a launch in progress.
    lease_wait: Duration,
    last_error: Option<String>,
    /// What the last export or import did, cleared by the next command.
    notice: Option<String>,
    /// The packages folder could not be listed on the last try. A package
    /// missing from the registry is then unknown, not removed.
    unreadable: bool,
    revision: u64,
    dirty: bool,
    now: Instant,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The longest missing-tool warning. A remote Project Settings keeps at most
/// 512 bytes of a line (`MAX_LINE` in `connecto-net/src/project_panels.rs`,
/// older peers included), and a command, steps or link cut short must never
/// be shown.
const WARNING_MAX: usize = 400;
/// The most of a package id or tool name a warning repeats.
const WARNING_NAME_MAX: usize = 80;

/// `text`, cut to at most `max` bytes on a character boundary.
fn capped(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max - '…'.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// What the project's status says of a tool an installed package needs and
/// the last look did not find. It says how to get it by hand, which works
/// however the package came in (chosen, the default, or extended by
/// another). The first of these that fits in [`WARNING_MAX`]: with this
/// system's install command, with the manual steps, with the link, or plain.
/// The plain one always fits: the names in it are capped.
fn missing_tool_warning(package: &str, req: &Requirement, os: Os) -> String {
    let lead = format!(
        "{} needs {}, which is not installed here in a version it can use.",
        capped(package, WARNING_NAME_MAX),
        capped(&req.name, WARNING_NAME_MAX)
    );
    let after = "Restart Connecto once it is installed, so it looks again.";
    let mut candidates = Vec::new();
    if let Some(command) = req.install_on(os) {
        // The command last, so nothing of the sentence is copied with it.
        candidates.push(format!(
            "{lead} {after} To install it, run in a terminal: {command}"
        ));
    }
    if !req.manual.is_empty() {
        candidates.push(format!("{lead} {after} To install it: {}", req.manual));
    }
    if let Some(url) = &req.url {
        candidates.push(format!("{lead} Install it by hand (see {url}). {after}"));
    }
    candidates
        .into_iter()
        .find(|line| line.len() <= WARNING_MAX)
        .unwrap_or_else(|| {
            format!("{lead} Install it by hand, then restart Connecto so it looks again.")
        })
}

impl Core {
    pub fn new(cfg: Config) -> Self {
        let crate::state::Loaded { state, warning } = State::load(&cfg.data_root);
        let home = cfg
            .host
            .saved_home()
            .map(|p| fs::canonicalize(&p).unwrap_or(p));
        let (checked_tx, checked_rx) = unbounded();
        let detector: Detector = cfg.detector.clone().unwrap_or_else(|| {
            let home = cfg.user_home.clone();
            Arc::new(move |req: &Requirement, untrusted: &[PathBuf]| {
                // Made again for each look, so one after an install sees the
                // PATH as it is now (on Windows, as saved in the registry).
                let search = crate::requires::Search::here(&home);
                crate::requires::detect(req, &search, untrusted, &crate::requires::version_output)
            })
        });
        let mut core = Self {
            view: Arc::new(ArcSwap::from_pointee(PackagesView::default())),
            home,
            migration: HomeMigration::None,
            state,
            registry: Vec::new(),
            builtins: HashMap::new(),
            game_dev: None,
            checks: HashMap::new(),
            checked_tx,
            checked_rx,
            detector,
            generation: 0,
            projects: Vec::new(),
            openings: Vec::new(),
            bindings: Vec::new(),
            signature: Vec::new(),
            unsettled: false,
            lease_wait: Duration::from_secs(5),
            last_error: warning,
            notice: None,
            unreadable: false,
            revision: 0,
            dirty: true,
            now: Instant::now(),
            cfg,
        };
        core.reload_registry();
        core.check_changed_requirements();
        core
    }

    pub fn view(&self) -> Arc<ArcSwap<PackagesView>> {
        Arc::clone(&self.view)
    }

    fn fail(&mut self, why: String) {
        self.notice = None;
        self.last_error = Some(why);
        self.dirty = true;
    }

    fn builtin_for(&mut self, variant: Variant) -> Result<Arc<Package>, String> {
        if let Some(found) = self.builtins.get(&variant) {
            return Ok(Arc::clone(found));
        }
        let package = Arc::new(builtin(variant)?);
        self.builtins.insert(variant, Arc::clone(&package));
        Ok(package)
    }

    fn game_dev_package(&mut self) -> Result<Arc<Package>, String> {
        if let Some(found) = &self.game_dev {
            return Ok(Arc::clone(found));
        }
        let package = Arc::new(game_dev()?);
        self.game_dev = Some(Arc::clone(&package));
        Ok(package)
    }

    /// Every package that loads, with what it needs: the built-in ones, then
    /// the user's.
    fn all_requirements(&mut self) -> Vec<(String, Vec<Requirement>)> {
        let mut out = Vec::new();
        if let Ok(game) = self.game_dev_package() {
            out.push((GAME_DEV_ID.to_owned(), game.manifest.requires.clone()));
        }
        for entry in &self.registry {
            if let Ok(package) = &entry.package {
                out.push((entry.id.clone(), package.manifest.requires.clone()));
            }
        }
        out
    }

    /// Where packages and what they install live: Connecto Home, Connecto's
    /// data, and every open project. A program found there came with a
    /// package (or a project), so it is neither found nor run.
    fn untrusted(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self.home.iter().cloned().collect();
        out.push(self.cfg.data_root.clone());
        out.extend(self.projects.iter().map(|p| p.path.clone()));
        out.extend(self.cfg.host.projects().into_iter().map(|p| p.path));
        out
    }

    /// The package and the packages it extends, in that order.
    fn chain_of(&self, id: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut next = Some(connecto_core::current_package_id(id).to_owned());
        while let Some(id) = next
            .take()
            .filter(|i| !out.contains(i) && out.len() < MAX_DEPTH)
        {
            next = self
                .registry
                .iter()
                .find(|e| e.id == id)
                .and_then(|e| e.package.as_ref().ok())
                .and_then(|p| p.manifest.extends.clone());
            out.push(id);
        }
        out
    }

    /// Look now, on this thread, for the tools of the packages `settings`
    /// adds to the project (or the default package it turns on), with what
    /// they extend: the view's answer may be old. Why one cannot be added,
    /// if it cannot.
    fn verify_added(&mut self, project: &str, settings: &ProjectSettings) -> Option<String> {
        let before = self
            .projects
            .iter()
            .find(|p| p.id == project)
            .map(|p| p.settings.clone());
        let mut added: Vec<String> = settings
            .packages
            .iter()
            .filter(|id| before.as_ref().is_none_or(|b| !b.packages.contains(id)))
            .cloned()
            .collect();
        if settings.use_default_package
            && before.as_ref().is_none_or(|b| !b.use_default_package)
            && let Some(default) = self.state.default_id()
        {
            added.push(default.to_owned());
        }
        self.verify_packages(&added)
    }

    /// Look now, on this thread, for the tools of `packages` and of what
    /// they extend. Why one of them cannot be turned on, if one cannot.
    fn verify_packages(&mut self, packages: &[String]) -> Option<String> {
        let wanted: Vec<String> = packages.iter().flat_map(|id| self.chain_of(id)).collect();
        let all = self.all_requirements();
        let untrusted = self.untrusted();
        for id in wanted {
            let Some((_, requires)) = all.iter().find(|(p, _)| *p == id) else {
                continue;
            };
            let states: Vec<RequirementState> = requires
                .iter()
                .map(|r| (self.detector)(r, &untrusted))
                .collect();
            if let Some(checks) = self.checks.get_mut(&id)
                && checks.requires == *requires
            {
                checks.states.clone_from(&states);
                self.dirty = true;
            }
            if let Some((req, _)) = requires.iter().zip(&states).find(|(_, s)| !s.met()) {
                return Some(format!(
                    "{id} needs {}, which is not installed here in a version it can use, so it was not turned on",
                    req.name
                ));
            }
        }
        None
    }

    /// Start a look for each package whose requirements are new or changed,
    /// and forget the packages that are gone.
    fn check_changed_requirements(&mut self) {
        let all = self.all_requirements();
        self.checks
            .retain(|id, _| all.iter().any(|(known, _)| known == id));
        for (id, requires) in all {
            let same = self.checks.get(&id).is_some_and(|c| c.requires == requires);
            if !same {
                self.start_check(&id, requires);
            }
        }
    }

    /// Look for a package's tools on a thread of its own (a version flag may
    /// take a while); the answer is taken on a later tick.
    fn start_check(&mut self, id: &str, requires: Vec<Requirement>) {
        self.generation += 1;
        let generation = self.generation;
        self.checks.insert(
            id.to_owned(),
            Checks {
                states: vec![RequirementState::Checking; requires.len()],
                requires: requires.clone(),
                generation,
            },
        );
        self.dirty = true;
        if requires.is_empty() {
            return;
        }
        let (tx, detector, id, untrusted) = (
            self.checked_tx.clone(),
            Arc::clone(&self.detector),
            id.to_owned(),
            self.untrusted(),
        );
        let spawned = std::thread::Builder::new()
            .name("package-requires".into())
            .spawn(move || {
                let states = requires.iter().map(|r| detector(r, &untrusted)).collect();
                let _ = tx.send((id, generation, states));
            });
        if let Err(why) = spawned {
            self.fail(format!(
                "the tools packages need could not be looked for: {why}"
            ));
        }
    }

    /// A warning for each tool an installed package needs and the last look
    /// did not find. Adding a package is refused without its tools, but one
    /// already in a project is updated anyway: a new version may need a tool
    /// the old one did not (Game Dev 3 added Blender and FFmpeg).
    fn missing_tools(&self, status: &ProjectPackages) -> Vec<String> {
        let mut out = Vec::new();
        for package in &status.packages {
            if package.installed_version.is_none() {
                continue;
            }
            let Some(checks) = self.checks.get(&package.id) else {
                continue;
            };
            for (req, state) in checks.requires.iter().zip(&checks.states) {
                if !state.met() && *state != RequirementState::Checking {
                    out.push(missing_tool_warning(&package.id, req, Os::CURRENT));
                }
            }
        }
        out
    }

    /// Take the answers of finished looks.
    fn take_checks(&mut self) {
        while let Ok((id, generation, states)) = self.checked_rx.try_recv() {
            if let Some(checks) = self.checks.get_mut(&id)
                && checks.generation == generation
                && checks.states.len() == states.len()
            {
                checks.states = states;
                self.dirty = true;
            }
        }
    }

    fn check_requirements(&mut self, id: &str) {
        let Some((_, requires)) = self.all_requirements().into_iter().find(|(p, _)| p == id) else {
            return self.fail(format!("no such package: {id}"));
        };
        self.start_check(id, requires);
    }

    /// Open a shell terminal in the project that runs the tool's install
    /// command. The command is the package's own, for this system, never one
    /// passed in; a tool that needs a licence has none.
    fn install_requirement(
        &mut self,
        project: &str,
        package: &str,
        requirement: &str,
        approved: &str,
    ) {
        let Some((_, requires)) = self
            .all_requirements()
            .into_iter()
            .find(|(p, _)| p == package)
        else {
            return self.fail(format!("no such package: {package}"));
        };
        let Some(req) = requires.iter().find(|r| r.id == requirement) else {
            return self.fail(format!("{package} needs no tool {requirement}"));
        };
        let Some(command) = req.install_on(Os::CURRENT) else {
            return self.fail(format!(
                "{} has no install command here: follow its manual steps",
                req.name
            ));
        };
        // The package may have changed since the user saw the command: run
        // only the very text they approved.
        if command != approved {
            return self.fail(crate::CHANGED.to_owned());
        }
        if !self.projects.iter().any(|p| p.id == project) {
            return self.fail("no such project".to_owned());
        }
        match self.cfg.host.add_terminal(
            project,
            ToolKind::Shell,
            Some(format!("Install {}", req.name)),
            Some(command.to_owned()),
            None,
            None,
        ) {
            Ok(terminal) => self.cfg.host.show(project, Some(&terminal)),
            Err(why) => self.fail(format!("{} could not be installed: {why}", req.name)),
        }
    }

    /// What the view says of a package's tools.
    fn requirement_views(&self, id: &str, requires: &[Requirement]) -> Vec<RequirementView> {
        let states = self
            .checks
            .get(id)
            .filter(|c| c.requires == requires)
            .map(|c| c.states.clone())
            .unwrap_or_default();
        requires
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let state = states.get(i).cloned().unwrap_or_default();
                crate::requires::view(r, state, Os::CURRENT)
            })
            .collect()
    }

    fn packages_dir(&self) -> Option<PathBuf> {
        self.home.as_deref().map(home::packages_dir)
    }

    // ---- commands -------------------------------------------------------

    pub fn command(&mut self, cmd: Cmd) {
        if !matches!(cmd, Cmd::Stop) {
            self.notice = None;
        }
        match cmd {
            Cmd::SetHome(path) => self.set_home(&path),
            Cmd::SetDefault(id) => self.set_default(id),
            Cmd::Create { name, extends } => self.create(&name, extends),
            Cmd::Edit(id) => self.open_session(&id, false),
            Cmd::Delete(id) => self.delete(&id),
            Cmd::Export { id, dest } => self.export(&id, &dest),
            Cmd::Import(path) => self.import(&path),
            Cmd::SetProject(id, settings) => self.set_project(&id, settings),
            Cmd::SetProjectKeepingOpenCode(id, mut settings) => {
                // Read here, where writes are applied one after another: what
                // is saved now includes every write queued before this one.
                // The project as this worker holds it (set by the last write
                // it applied), else as saved.
                let own = self.settings_now(&id);
                settings.opencode_config = own.opencode_config;
                settings.tools = own.tools.from_older_peer(settings.tools);
                self.set_project(&id, settings);
            }
            Cmd::SetProjectKeepingTools(id, mut settings) => {
                // Read here, as above: after every write queued before it.
                settings.tools = self.settings_now(&id).tools.from_older_peer(settings.tools);
                self.set_project(&id, settings);
            }
            Cmd::Refresh(id) => {
                if let Some(project) = self.projects.iter_mut().find(|p| p.id == id) {
                    project.dirty = true;
                }
            }
            Cmd::CheckRequirements(id) => self.check_requirements(&id),
            Cmd::InstallRequirement {
                project,
                package,
                requirement,
                command,
            } => self.install_requirement(&project, &package, &requirement, &command),
            Cmd::Stop => {}
        }
        self.dirty = true;
    }

    fn set_default(&mut self, id: Option<String>) {
        if let Some(id) = &id
            && !self.installable(id)
        {
            return self.fail(format!(
                "{id} cannot be the default: it has problems or is gone"
            ));
        }
        // The default goes into every project that uses it: look for its
        // tools now, not by the view's answer, which may be old. Refused, the
        // default stays what it was.
        if let Some(id) = &id
            && let Some(why) = self.verify_packages(std::slice::from_ref(id))
        {
            return self.fail(why);
        }
        self.state.default = Some(id);
        self.save_state();
        self.mark_all_dirty();
    }

    fn installable(&self, id: &str) -> bool {
        is_builtin(id)
            || self
                .registry
                .iter()
                .any(|e| e.id == id && e.package.is_ok() && e.problems.is_empty())
    }

    fn save_state(&mut self) {
        if let Err(why) = self.state.save(&self.cfg.data_root) {
            self.fail(format!("package state could not be saved: {why}"));
        }
    }

    fn mark_all_dirty(&mut self) {
        for project in &mut self.projects {
            project.dirty = true;
        }
    }

    /// A project's settings as writes stand now: as this worker holds them
    /// (set by the last write it applied), else as saved.
    fn settings_now(&self, id: &str) -> ProjectSettings {
        self.projects.iter().find(|p| p.id == id).map_or_else(
            || project_settings::load(&self.cfg.data_root, id),
            |p| p.settings.clone(),
        )
    }

    fn set_project(&mut self, id: &str, settings: ProjectSettings) {
        self.sync_projects();
        // The facade refused what the view knew to be missing; this is the
        // last look, whoever asked (this window or a remote one). Refused
        // settings are not saved, and the project's status says why.
        if let Some(why) = self.verify_added(id, &settings) {
            if let Some(project) = self.projects.iter_mut().find(|p| p.id == id) {
                project.status.state = InstallState::Error(why.clone());
            }
            return self.fail(why);
        }
        if let Err(why) = project_settings::save(&self.cfg.data_root, id, &settings) {
            return self.fail(format!("the project's settings could not be saved: {why}"));
        }
        self.sync_projects();
        if let Some(project) = self.projects.iter_mut().find(|p| p.id == id) {
            project.settings = settings;
            project.dirty = true;
        }
    }

    // ---- Connecto Home ---------------------------------------------------

    /// Choose Home (already checked by the facade): move the old Automations
    /// folder's automations into `<home>/automations`, save the choice and point
    /// the automations engine at the new place. Any refusal leaves everything
    /// as it was.
    fn set_home(&mut self, path: &Path) {
        let new_home = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        self.migration = HomeMigration::Working;
        self.publish();
        let previous_home = self.home.clone();
        let target = home::automations_dir(&new_home);
        let old_root = self
            .cfg
            .host
            .automations_root()
            .map(|p| fs::canonicalize(&p).unwrap_or(p));
        let result = self.switch_home(
            &new_home,
            &target,
            old_root.as_deref(),
            previous_home.as_deref(),
        );
        match result {
            Ok(moved) => {
                self.migration = if old_root.as_ref().is_some_and(|old| *old != target) {
                    HomeMigration::Done { moved }
                } else {
                    HomeMigration::None
                };
                self.home = Some(new_home);
                self.last_error = None;
                self.reload_registry();
                self.mark_all_dirty();
            }
            Err(why) => {
                self.migration = HomeMigration::Error(why);
            }
        }
    }

    fn switch_home(
        &mut self,
        new_home: &Path,
        target: &Path,
        old_root: Option<&Path>,
        previous_home: Option<&Path>,
    ) -> Result<usize, String> {
        home::ensure_layout(new_home)?;
        let moved = match old_root {
            Some(old) => home::migrate(old, target)?,
            None => Vec::new(),
        };
        let rollback = |why: String| {
            if let Some(old) = old_root {
                home::undo_migrate(old, target, &moved);
            }
            why
        };
        self.cfg
            .host
            .save_home(Some(new_home.to_owned()))
            .map_err(|why| rollback(format!("Connecto Home could not be saved: {why}")))?;
        if let Err(why) = self.cfg.host.use_automations_root(target.to_owned()) {
            let _ = self.cfg.host.save_home(previous_home.map(Path::to_owned));
            return Err(rollback(format!(
                "the automations folder could not be set: {why}"
            )));
        }
        Ok(moved.len())
    }

    // ---- packages -------------------------------------------------------

    /// Read every package folder again, bump the versions of the ones whose
    /// files changed, and mark the projects for an update if anything changed.
    fn reload_registry(&mut self) {
        let Some(dir) = self.packages_dir() else {
            self.unreadable = false;
            let changed = !self.registry.is_empty();
            self.registry.clear();
            self.signature.clear();
            if changed {
                self.mark_all_dirty();
            }
            return;
        };
        // A folder that cannot be listed (its volume is gone, a permission
        // error) is not an empty one: reading it as empty would uninstall every
        // package from every open project. Only a folder that was never there,
        // with no package on record, counts as empty.
        let listed: std::io::Result<Vec<fs::DirEntry>> =
            fs::read_dir(&dir).and_then(Iterator::collect);
        let entries = match listed {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.state.records.is_empty() => {
                Vec::new()
            }
            Err(e) => {
                let why = format!(
                    "{UNREADABLE} {} could not be read ({e}): installed packages are left as they are",
                    dir.display()
                );
                if !self.unreadable || self.last_error.as_deref() != Some(why.as_str()) {
                    self.unreadable = true;
                    self.last_error = Some(why);
                    self.dirty = true;
                }
                return;
            }
        };
        // Readable again: the projects that could not be checked meanwhile are
        // checked now, and the note about the folder goes.
        let recovered = std::mem::take(&mut self.unreadable);
        if recovered
            && self
                .last_error
                .as_deref()
                .is_some_and(|e| e.starts_with(UNREADABLE))
        {
            self.last_error = None;
        }
        let mut ids: Vec<String> = entries
            .into_iter()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|id| package::valid_id(id) && !is_builtin(id))
            .collect();
        ids.sort();
        let mut registry = Vec::new();
        let mut state_changed = false;
        for id in ids {
            let loaded = load_dir(&dir, &id);
            let entry = match loaded {
                Ok(mut package) => {
                    let (version, changed) =
                        self.state
                            .reconcile(&id, package.manifest.version, &package.hash);
                    state_changed |= changed;
                    if version != package.manifest.version {
                        package.manifest.version = version;
                        let text = package.manifest.to_text();
                        if let Err(why) = crate::fsx::write_atomic(
                            &dir.join(&id),
                            FILE_NAME,
                            text.as_bytes(),
                            None,
                        ) {
                            self.last_error =
                                Some(format!("{id}: the version could not be written: {why}"));
                        }
                    }
                    let (_, report) = package::check_dir(&dir.join(&id));
                    let problems = report
                        .lines()
                        .filter_map(|l| l.strip_prefix("problem: "))
                        .map(str::to_owned)
                        .collect();
                    Entry {
                        id,
                        package: Ok(Arc::new(package)),
                        problems,
                    }
                }
                Err(why) => Entry {
                    id,
                    package: Err(why.clone()),
                    problems: vec![why],
                },
            };
            registry.push(entry);
        }
        let known: Vec<&str> = registry.iter().map(|e| e.id.as_str()).collect();
        let before = self.state.records.len();
        self.state
            .records
            .retain(|id, _| known.contains(&id.as_str()));
        state_changed |= before != self.state.records.len();
        let differs = registry.len() != self.registry.len()
            || registry.iter().zip(&self.registry).any(|(a, b)| {
                a.id != b.id
                    || a.problems != b.problems
                    || match (&a.package, &b.package) {
                        (Ok(x), Ok(y)) => x.hash != y.hash || x.manifest != y.manifest,
                        (Err(x), Err(y)) => x != y,
                        _ => true,
                    }
            });
        self.registry = registry;
        if state_changed {
            self.save_state();
        }
        // After our own write of package.json, so it does not look like a change.
        self.signature = self.scan_signature();
        if differs || recovered {
            self.mark_all_dirty();
            self.dirty = true;
        }
    }

    fn scan_signature(&self) -> Vec<Seen> {
        self.packages_dir()
            .map(|dir| signature(&dir))
            .unwrap_or_default()
    }

    fn create(&mut self, name: &str, extends: Option<String>) {
        let Some(dir) = self.packages_dir() else {
            return self.fail(NO_HOME.to_owned());
        };
        let name = match package::check_name(name) {
            Ok(name) => name.to_owned(),
            Err(why) => return self.fail(why),
        };
        if let Some(base) = &extends
            && !self.installable(base)
        {
            return self.fail(format!(
                "{base} cannot be extended: it has problems or is gone"
            ));
        }
        if let Err(why) = fs::create_dir_all(&dir) {
            return self.fail(format!("could not create the package: {why}"));
        }
        let (id, folder) = match Self::new_folder(&dir, &package::slugify(&name)) {
            Ok(found) => found,
            Err(e) => return self.fail(format!("could not create the package: {e}")),
        };
        let manifest = Manifest {
            name: name.clone(),
            description: String::new(),
            version: 1,
            extends,
            requires: Vec::new(),
        };
        let written =
            crate::fsx::write_atomic(&folder, FILE_NAME, manifest.to_text().as_bytes(), None)
                .and_then(|()| Self::write_guides(&folder, &name, &id));
        if let Err(why) = written {
            let _ = fs::remove_dir_all(&folder);
            return self.fail(format!("could not create the package: {why}"));
        }
        self.reload_registry();
        self.open_session(&id, true);
    }

    /// Make a new, empty package folder named after `base` (`base-2`, `base-3`
    /// ... when it is taken).
    fn new_folder(dir: &Path, base: &str) -> std::io::Result<(String, PathBuf)> {
        let (mut id, mut n) = (base.to_owned(), 2);
        loop {
            let folder = dir.join(&id);
            // Never `builtin`'s id, and `create_dir` fails if it exists, so two
            // creators cannot share a folder. On Windows never a device name
            // either (`nul` is no folder there, `con` is one only on Windows 11).
            if !is_builtin(&id) && crate::platform::portable_name(&id) {
                match fs::create_dir(&folder) {
                    Ok(()) => return Ok((id, folder)),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            id = format!("{base}-{n}");
            n += 1;
        }
    }

    fn export(&mut self, id: &str, dest: &Path) {
        let Some(dir) = self.packages_dir() else {
            return self.fail(NO_HOME.to_owned());
        };
        if is_builtin(id) || !package::valid_id(id) {
            return self.fail("a built-in package cannot be exported".to_owned());
        }
        let bundle = match bundle::read_package(&dir.join(id)) {
            Ok(bundle) => bundle,
            Err(why) => return self.fail(format!("{id} could not be exported: {why}")),
        };
        if let Err(why) = fs::write(dest, bundle::encode(&bundle)) {
            return self.fail(format!("could not write {}: {why}", dest.display()));
        }
        self.last_error = None;
        self.notice = Some(format!(
            "Exported {} to {}",
            bundle.manifest.name,
            dest.display()
        ));
        self.dirty = true;
    }

    fn import(&mut self, path: &Path) {
        let Some(dir) = self.packages_dir() else {
            return self.fail(NO_HOME.to_owned());
        };
        let bundle = match bundle::read_file(path) {
            Ok(bundle) => bundle,
            Err(why) => return self.fail(format!("could not import: {why}")),
        };
        if let Err(why) = fs::create_dir_all(&dir) {
            return self.fail(format!("could not import: {why}"));
        }
        let name = bundle.manifest.name.clone();
        let (id, folder) = match Self::new_folder(&dir, &package::slugify(&name)) {
            Ok(found) => found,
            Err(e) => return self.fail(format!("could not import: {e}")),
        };
        let written = bundle::write_into(&folder, &bundle)
            .and_then(|()| Self::write_guides(&folder, &name, &id));
        if let Err(why) = written {
            let _ = fs::remove_dir_all(&folder);
            return self.fail(format!("could not import: {why}"));
        }
        self.reload_registry();
        self.last_error = None;
        let missing = bundle
            .manifest
            .extends
            .as_deref()
            .filter(|base| !self.installable(base));
        self.notice = Some(match missing {
            Some(base) => format!(
                "Imported {name} as {id}. It extends {base}, which is not here: import that one too."
            ),
            None => format!("Imported {name} as {id}"),
        });
        self.dirty = true;
    }

    fn write_guides(folder: &Path, name: &str, id: &str) -> std::io::Result<()> {
        for claude in [true, false] {
            let file = folder.join(guide::file_name(claude));
            let existing = fs::read_to_string(&file).ok();
            if let Some(text) = guide::apply(existing.as_deref(), &guide::text(name, id)) {
                crate::fsx::write_atomic(folder, guide::file_name(claude), text.as_bytes(), None)?;
            }
        }
        Ok(())
    }

    fn delete(&mut self, id: &str) {
        let Some(dir) = self.packages_dir() else {
            return self.fail(NO_HOME.to_owned());
        };
        if is_builtin(id) || !package::valid_id(id) {
            return self.fail("a built-in package cannot be deleted".to_owned());
        }
        let folder = dir.join(id);
        match fs::symlink_metadata(&folder) {
            Ok(meta) if meta.file_type().is_symlink() => {
                crate::platform::remove_link(&folder, &meta.file_type())
            }
            Ok(_) => fs::remove_dir_all(&folder),
            Err(e) => Err(e),
        }
        .unwrap_or_else(|e| self.last_error = Some(format!("could not delete {id}: {e}")));
        // Its terminal stays open, but it is no longer this package's.
        self.bindings.retain(|b| b.dir != folder);
        self.openings.retain(|o| o.id != id);
        if self.state.default.as_ref().and_then(|d| d.as_deref()) == Some(id) {
            self.state.default = Some(None);
        }
        self.state.records.remove(id);
        self.save_state();
        self.reload_registry();
        self.mark_all_dirty();
    }

    // ---- sessions -------------------------------------------------------

    /// Where the built-in package can be read by the agent: its files, in the
    /// data folder, refreshed when they differ.
    fn builtin_reference(&mut self) -> Option<PathBuf> {
        let package = self.builtin_for(Variant::Both).ok()?;
        let dir = self.cfg.data_root.join("packages").join("builtin-connecto");
        let marker = dir.join(".hash");
        // Of every file here: the package's hash leaves out the files only
        // Windows installs (on macOS the two are the same).
        let hash = crate::package::content_hash(&package.files, &package.exec, None);
        if fs::read_to_string(&marker).ok().as_deref() != Some(hash.as_str()) {
            let _ = fs::remove_dir_all(&dir);
            for (path, bytes) in &package.files {
                let file = dir.join(path);
                fs::create_dir_all(file.parent()?).ok()?;
                fs::write(&file, bytes).ok()?;
                if package.exec.contains(path) {
                    crate::platform::make_executable(&file).ok()?;
                }
            }
            fs::write(&marker, &hash).ok()?;
        }
        Some(dir)
    }

    fn open_session(&mut self, id: &str, new: bool) {
        let Some(root) = self.packages_dir() else {
            return self.fail(NO_HOME.to_owned());
        };
        if is_builtin(id) || !package::valid_id(id) {
            return self.fail("a built-in package cannot be edited".to_owned());
        }
        let folder = root.join(id);
        if !folder.join(FILE_NAME).is_file() {
            return self.fail("no such package".to_owned());
        }
        if self.openings.iter().any(|o| o.id == id) {
            return;
        }
        let name = self
            .registry
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.package.as_ref().ok())
            .map_or_else(|| id.to_owned(), |p| p.manifest.name.clone());
        if let Err(why) = Self::write_guides(&folder, &name, id) {
            return self.fail(format!("the instructions could not be written: {why}"));
        }
        let recorded = session::read(&folder);
        let note = recorded
            .as_ref()
            .filter(|r| r.guide != Some(guide::VERSION))
            .map(|_| {
                format!(
                    "Connecto's package guide was updated; re-read `{id}/CLAUDE.md` before continuing."
                )
            });
        let reference = self.builtin_reference();
        let prompt = authoring_prompt(&root, id, new, reference.as_deref());
        self.openings.push(Opening {
            id: id.to_owned(),
            name: format!("Package: {name}"),
            prompt,
            note,
            recorded,
            started: self.now,
            requested: false,
            ready_since: None,
        });
    }

    fn record_sessions(&mut self) {
        let (Some(home), false) = (self.home.clone(), self.bindings.is_empty()) else {
            return;
        };
        let open: Vec<(String, String)> = self
            .cfg
            .host
            .project_at(&home)
            .map(|p| {
                p.terminals
                    .iter()
                    .map(|(t, _)| (p.id.clone(), t.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.bindings
            .retain(|b| open.contains(&(b.project.clone(), b.terminal.clone())));
        for binding in &mut self.bindings {
            let Some(found) = self
                .cfg
                .host
                .terminal_session(&binding.project, &binding.terminal)
            else {
                continue;
            };
            if binding.session.as_deref() == Some(found.as_str()) {
                continue;
            }
            let record = session::Record {
                terminal: binding.terminal.clone(),
                session: found.clone(),
                guide: binding.guide,
            };
            match session::write(&binding.dir, &record) {
                Ok(()) => binding.session = Some(found),
                Err(why) => {
                    self.last_error = Some(format!("the session id could not be saved: {why}"));
                }
            }
        }
    }

    fn pump_openings(&mut self) {
        self.record_sessions();
        let openings = std::mem::take(&mut self.openings);
        let count = openings.len();
        let mut keep = Vec::new();
        for mut opening in openings {
            if !self.pump_opening(&mut opening) {
                keep.push(opening);
            }
        }
        if keep.len() != count {
            self.dirty = true;
        }
        // A command may have queued one meanwhile (never inside a tick), so merge.
        keep.append(&mut self.openings);
        self.openings = keep;
    }

    /// One step of an opening; true when it is over.
    fn pump_opening(&mut self, opening: &mut Opening) -> bool {
        let (Some(home), now) = (self.home.clone(), self.now) else {
            return true;
        };
        let expired = now.duration_since(opening.started) > OPEN_LIMIT;
        let project = match self.cfg.host.project_at(&home) {
            None => {
                if !opening.requested {
                    if let Err(why) = self.cfg.host.open_project(&home) {
                        self.last_error = Some(format!("could not open the session: {why}"));
                        return true;
                    }
                    opening.requested = true;
                } else if expired {
                    self.last_error = Some("Connecto Home did not open in time".to_owned());
                    return true;
                }
                return false;
            }
            Some(project) if !project.ready => {
                if expired {
                    self.last_error = Some("the session did not start in time".to_owned());
                }
                return expired;
            }
            Some(project) => project,
        };
        let since = *opening.ready_since.get_or_insert(now);
        let folder = home::packages_dir(&home).join(&opening.id);
        let live = |id: &str| project.terminals.iter().any(|(t, _)| t == id);
        // Already tied to a live terminal of this run: focus that one.
        if let Some(binding) = self
            .bindings
            .iter()
            .find(|b| b.dir == folder && b.project == project.id && live(&b.terminal))
        {
            let terminal = binding.terminal.clone();
            self.cfg.host.show(&project.id, Some(&terminal));
            return true;
        }
        // Still open from an earlier run: its recorded session is the one.
        if let Some(recorded) = &opening.recorded
            && let Some((terminal, _)) = project.terminals.iter().find(|(id, _)| {
                self.cfg.host.terminal_session(&project.id, id).as_deref()
                    == Some(recorded.session.as_str())
            })
        {
            self.bindings.push(Binding {
                dir: folder,
                project: project.id.clone(),
                terminal: terminal.clone(),
                session: None,
                guide: recorded.guide,
            });
            self.cfg.host.show(&project.id, Some(terminal));
            return true;
        }
        // Closed (or never recorded): resume that conversation, or begin a new
        // one. The prompt is used only if there is nothing to resume.
        if !project.terminals.is_empty() || now.duration_since(since) >= DEFAULTS_WAIT {
            // The session is a Claude one (its guide is CLAUDE.md, its
            // conversation is resumed): where Connecto Home leaves Claude out,
            // say so rather than start another CLI.
            if !project_settings::load(&self.cfg.data_root, &project.id)
                .tools
                .allows(ToolKind::Claude)
            {
                self.last_error = Some(
                    "Packages are edited in a Claude session, and the Tools setting of the Connecto Home project leaves Claude out. Turn Claude on there to edit a package.".to_owned(),
                );
                return true;
            }
            let resume = opening.recorded.as_ref().map(|r| r.session.clone());
            match self.cfg.host.add_terminal(
                &project.id,
                ToolKind::Claude,
                Some(opening.name.clone()),
                Some(opening.prompt.clone()),
                resume,
                opening.note.clone(),
            ) {
                Ok(terminal) => {
                    self.bindings
                        .retain(|b| !(b.project == project.id && b.terminal == terminal));
                    self.bindings.push(Binding {
                        dir: folder,
                        project: project.id.clone(),
                        terminal: terminal.clone(),
                        session: None,
                        guide: Some(guide::VERSION),
                    });
                    self.cfg.host.show(&project.id, Some(&terminal));
                }
                Err(why) => {
                    self.last_error = Some(format!("could not start the session: {why}"));
                }
            }
            return true;
        }
        if expired {
            self.last_error = Some("the session did not start in time".to_owned());
        }
        expired
    }

    // ---- projects -------------------------------------------------------

    /// Follow the host's open projects: new ones get their saved settings and an
    /// install, closed ones are forgotten.
    fn sync_projects(&mut self) {
        let open = self.cfg.host.projects();
        let before = self.projects.len();
        for gone in self
            .projects
            .iter()
            .filter(|p| !open.iter().any(|o| o.id == p.id))
        {
            connecto_session::install_gate::finish(&self.cfg.data_root, &gone.id);
        }
        self.projects.retain(|p| open.iter().any(|o| o.id == p.id));
        let mut changed = before != self.projects.len();
        for project in &open {
            if let Some(known) = self.projects.iter_mut().find(|p| p.id == project.id) {
                if known.path != project.path {
                    known.path.clone_from(&project.path);
                    known.dirty = true;
                }
                continue;
            }
            self.projects.push(ProjectState {
                id: project.id.clone(),
                path: project.path.clone(),
                settings: project_settings::load(&self.cfg.data_root, &project.id),
                status: ProjectPackages::default(),
                dirty: true,
            });
            changed = true;
        }
        // Same order as the sidebar.
        self.projects
            .sort_by_key(|p| open.iter().position(|o| o.id == p.id));
        self.dirty |= changed;
    }

    /// The packages a project should have, in install order, and why some are
    /// missing. `Err` when a package it wants has problems.
    fn plan(
        &mut self,
        settings: &ProjectSettings,
    ) -> Result<(Vec<Arc<Package>>, Vec<String>), String> {
        let variant = Variant::for_tools(settings.tools);
        let mut wanted: Vec<String> = Vec::new();
        if settings.use_default_package
            && let Some(id) = self.state.default_id()
        {
            wanted.push(id.to_owned());
        }
        wanted.extend(settings.packages.iter().cloned());
        let mut chain: Vec<Arc<Package>> = Vec::new();
        let mut warnings = Vec::new();
        for id in wanted {
            match self.resolve(&id, variant, &mut Vec::new())? {
                Some(found) => {
                    for package in found {
                        if !chain.iter().any(|p| p.id == package.id) {
                            chain.push(package);
                        }
                    }
                }
                None => warnings.push(format!(
                    "package {id} no longer exists: nothing installed for it"
                )),
            }
        }
        Ok((chain, warnings))
    }

    /// A package and what it extends, bases first. `Ok(None)`: it does not exist.
    fn resolve(
        &mut self,
        id: &str,
        variant: Variant,
        stack: &mut Vec<String>,
    ) -> Result<Option<Vec<Arc<Package>>>, String> {
        let id = connecto_core::current_package_id(id);
        if stack.iter().any(|s| s == id) {
            return Err(format!("{id} extends itself through {}", stack.join(" > ")));
        }
        if stack.len() >= MAX_DEPTH {
            return Err(format!("{id}: the chain of extends is too long"));
        }
        let package = if id == BUILTIN_ID {
            self.builtin_for(variant)?
        } else if id == GAME_DEV_ID {
            self.game_dev_package()?
        } else {
            let Some(entry) = self.registry.iter().find(|e| e.id == id) else {
                if self.unreadable {
                    return Err(format!(
                        "package {id} cannot be read: the packages folder is unreadable"
                    ));
                }
                return Ok(None);
            };
            match &entry.package {
                Ok(package) if entry.problems.is_empty() => Arc::clone(package),
                _ => {
                    return Err(format!(
                        "package {id} has problems: {}",
                        entry.problems.join("; ")
                    ));
                }
            }
        };
        let mut chain = Vec::new();
        if let Some(base) = package.manifest.extends.clone() {
            stack.push(id.to_owned());
            let found = self.resolve(&base, variant, stack)?;
            stack.pop();
            match found {
                Some(found) => chain = found,
                None => return Err(format!("{id} extends {base}, which does not exist")),
            }
        }
        chain.push(package);
        Ok(Some(chain))
    }

    /// Check one project and bring its files up to date.
    fn apply_project(&mut self, index: usize) {
        let id = self.projects[index].id.clone();
        // Exclusive: no terminal of the project is between "permit" and "spawned"
        // while its files are written, and none starts until the lease is
        // dropped (on every way out, an error too). If a launch holds the
        // project for too long, the project stays dirty and is tried next tick.
        let Some(_lease) =
            connecto_session::install_gate::lease(&self.cfg.data_root, &id, self.lease_wait)
        else {
            return;
        };
        self.apply_project_files(index);
    }

    fn apply_project_files(&mut self, index: usize) {
        let (id, path, settings) = {
            let p = &mut self.projects[index];
            p.dirty = false;
            (p.id.clone(), p.path.clone(), p.settings.clone())
        };
        let under_home = self.home.as_ref().is_some_and(|h| path.starts_with(h));
        let mut status = ProjectPackages {
            checked_ms: Some(now_ms()),
            ..ProjectPackages::default()
        };
        if under_home {
            // Connecto Home is where packages are made, not a project that uses them.
            self.projects[index].status = status;
            return;
        }
        let planned = self.plan(&settings);
        let (chain, warnings) = match planned {
            Ok(planned) => planned,
            Err(why) => {
                status.state = InstallState::Error(why);
                self.finish_project(index, status);
                return;
            }
        };
        status.warnings = warnings;
        if !path.is_dir() {
            status.state = InstallState::Error("the project folder is missing".to_owned());
            self.finish_project(index, status);
            return;
        }
        let running = self.cfg.host.running_terminals(&id);
        match install::apply(&path, &chain, settings.tools) {
            Ok(out) => {
                status.state = if out.installed.is_empty() && out.skipped_modified.is_empty() {
                    InstallState::None
                } else if out.changed {
                    InstallState::Updated
                } else {
                    InstallState::UpToDate
                };
                status.packages = chain
                    .iter()
                    .map(|p| InstalledPackage {
                        id: p.id.clone(),
                        installed_version: out
                            .installed
                            .iter()
                            .find(|i| i.id == p.id)
                            .map(|i| i.version),
                        available_version: Some(p.manifest.version),
                    })
                    .collect();
                status.skipped_modified = out.skipped_modified;
                status.warnings.extend(out.warnings);
                // A project that keeps these files in git gets the update as
                // one small commit of its own.
                if out.changed {
                    let paths: Vec<String> = out
                        .written
                        .iter()
                        .chain(&out.removed)
                        .chain(&out.bookkeeping)
                        .cloned()
                        .collect();
                    if let Err(why) = crate::commit::commit(&path, &paths) {
                        status.warnings.push(format!(
                            "the package files were updated but not committed: {why}"
                        ));
                    }
                }
                status.restart_needed = (out.changed && running > 0)
                    || (self.projects[index].status.restart_needed && running > 0);
            }
            Err(why) => {
                status.state = InstallState::Error(why);
                status.packages = install::read_lock(&path)
                    .map(|lock| {
                        lock.packages
                            .iter()
                            .map(|l| InstalledPackage {
                                id: l.id.clone(),
                                installed_version: Some(l.version),
                                available_version: chain
                                    .iter()
                                    .find(|p| p.id == l.id)
                                    .map(|p| p.manifest.version),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
        }
        self.finish_project(index, status);
    }

    fn finish_project(&mut self, index: usize, status: ProjectPackages) {
        self.projects[index].status = status;
        self.dirty = true;
    }

    // ---- the tick and the view ------------------------------------------

    pub fn tick(&mut self, now: Instant) {
        self.now = now;
        self.take_checks();
        self.sync_projects();
        // A package being edited settles for a tick before it counts, so a
        // half-written skill does not take files out of projects.
        let signature = self.scan_signature();
        if signature != self.signature {
            self.signature = signature;
            self.unsettled = true;
        } else if self.unsettled {
            self.unsettled = false;
            self.reload_registry();
        }
        // A package added, changed or removed by any step since the last tick.
        self.check_changed_requirements();
        for index in 0..self.projects.len() {
            if self.projects[index].dirty {
                self.apply_project(index);
            } else if connecto_session::install_gate::is_pending(
                &self.cfg.data_root,
                &self.projects[index].id,
            ) {
                // Held again by a project set up anew; nothing to install.
                connecto_session::install_gate::finish(
                    &self.cfg.data_root,
                    &self.projects[index].id,
                );
            } else if self.projects[index].status.restart_needed
                && self.cfg.host.running_terminals(&self.projects[index].id) == 0
            {
                self.projects[index].status.restart_needed = false;
                self.dirty = true;
            }
        }
        self.pump_openings();
        if self.dirty {
            self.publish();
        }
    }

    pub fn publish(&mut self) {
        self.dirty = false;
        self.revision += 1;
        let default = self.state.default_id().map(str::to_owned);
        let mut packages = Vec::new();
        if let Ok(b) = self.builtin_for(Variant::Both) {
            packages.push(PackageItem {
                id: BUILTIN_ID.to_owned(),
                name: b.manifest.name.clone(),
                description: b.manifest.description.clone(),
                version: b.manifest.version,
                builtin: true,
                extends: None,
                is_default: default.as_deref() == Some(BUILTIN_ID),
                has_claude: true,
                has_codex: true,
                errors: Vec::new(),
                requires: Vec::new(),
            });
        }
        if let Ok(game) = self.game_dev_package() {
            packages.push(PackageItem {
                id: GAME_DEV_ID.to_owned(),
                name: game.manifest.name.clone(),
                description: game.manifest.description.clone(),
                version: game.manifest.version,
                builtin: true,
                extends: None,
                is_default: default.as_deref() == Some(GAME_DEV_ID),
                has_claude: game.has_claude(),
                has_codex: game.has_codex(),
                errors: Vec::new(),
                requires: self.requirement_views(GAME_DEV_ID, &game.manifest.requires),
            });
        }
        let mut users: Vec<PackageItem> = self
            .registry
            .iter()
            .map(|e| match &e.package {
                Ok(p) => PackageItem {
                    id: e.id.clone(),
                    name: p.manifest.name.clone(),
                    description: p.manifest.description.clone(),
                    version: p.manifest.version,
                    builtin: false,
                    extends: p.manifest.extends.clone(),
                    is_default: default.as_deref() == Some(e.id.as_str()),
                    has_claude: p.has_claude(),
                    has_codex: p.has_codex(),
                    errors: e.problems.clone(),
                    requires: self.requirement_views(&e.id, &p.manifest.requires),
                },
                Err(why) => PackageItem {
                    id: e.id.clone(),
                    name: e.id.clone(),
                    description: String::new(),
                    version: 0,
                    errors: vec![why.clone()],
                    ..PackageItem::default()
                },
            })
            .collect();
        users.sort_by_key(|p| p.name.to_lowercase());
        packages.extend(users);
        let view = PackagesView {
            revision: self.revision,
            home: self.home.clone(),
            migration: self.migration.clone(),
            packages,
            default_package: default,
            projects: self
                .projects
                .iter()
                .map(|p| {
                    let mut status = p.status.clone();
                    status.warnings.extend(self.missing_tools(&status));
                    ProjectPackagesView {
                        project_id: p.id.clone(),
                        settings: p.settings.clone(),
                        status,
                    }
                })
                .collect(),
            last_error: self.last_error.clone(),
            notice: self.notice.clone(),
            tools_one_or_all: false,
        };
        self.view.store(Arc::new(view));
        (self.cfg.wake)();
    }
}

/// What a new authoring terminal is told first. It names the session's one
/// objective, this package, and nothing else: no list of the user's projects
/// or folders to survey. Sources are the ones the user names ("make it from
/// project X").
fn authoring_prompt(root: &Path, id: &str, new: bool, reference: Option<&Path>) -> String {
    let verb = if new {
        "authoring the new"
    } else {
        "editing the"
    };
    let task = format!(
        "You are {verb} Connecto package `{id}` at `{}/{id}/`.",
        root.display()
    );
    let reference = reference.map_or_else(String::new, |dir| {
        format!(
            " The built-in `connecto` package (read-only, to copy from or extend) is at `{}`.",
            dir.display()
        )
    });
    let origin = guide::ORIGIN;
    let (read, ask, record) = if new {
        (
            String::new(),
            "what the package is for and which skills, agents, commands and plugins to include",
            format!(
                "Before you finish, write `{id}/{origin}`: what I asked for, the sources (repository \
                 URLs with the ref or commit or the date fetched, and the paths taken), what you \
                 adapted or left out and why, and how to refresh the package from its sources."
            ),
        )
    } else {
        (
            format!(
                " and then `{id}/{origin}` (how this package was made: its request, sources and \
                 choices)"
            ),
            "what to change",
            format!(
                "If the work changes what `{id}/{origin}` says (a new source, a newer ref, different \
                 choices), bring it up to date before you finish. If it is missing, write one from \
                 what the package holds and what I tell you; never invent sources."
            ),
        )
    };
    format!(
        "{task} Read `{id}/CLAUDE.md` first (it says how a package is laid out){read}, then ask me \
         {ask}. Do not look through my projects or folders on your own: use the sources I name \
         (for example \"make it from project X\": then read that project's `.claude`, `.codex` and \
         `.agents` and offer what is there; ask me for its path if you cannot tell).{reference} \
         Write only inside `{id}/`, never edit `version` in package.json, and run \
         `connecto package check {id}` after every edit. {record}"
    )
}

/// Turns the install gate off when dropped.
struct Release(PathBuf);

impl Drop for Release {
    fn drop(&mut self) {
        connecto_session::install_gate::disable(&self.0);
    }
}

/// The loop for the manager's thread.
pub fn run(mut core: Core, rx: &Receiver<Cmd>) {
    core.publish();
    let data_root = core.cfg.data_root.clone();
    // However the loop ends, no terminal stays held for a manager that is gone.
    let _release = Release(data_root);
    loop {
        match rx.recv_timeout(TICK) {
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(cmd) => core.command(cmd),
            Err(RecvTimeoutError::Timeout) => {}
        }
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, Cmd::Stop) {
                return;
            }
            core.command(cmd);
        }
        core.tick(Instant::now());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::too_many_lines)]
mod tests {
    use std::sync::Mutex;

    use connecto_core::ProjectTools;

    use super::*;
    use crate::host::{ProjectInfo, ProjectRef};

    #[derive(Default)]
    struct HostState {
        projects: Vec<ProjectRef>,
        running: usize,
        opened: Vec<PathBuf>,
        info: Option<ProjectInfo>,
        added: Vec<(String, Option<String>, Option<String>)>,
        saved_home: Option<PathBuf>,
        fail_use_root: bool,
        automations_root: Option<PathBuf>,
        used_root: Option<PathBuf>,
        shown: Vec<String>,
        /// The tools (requirement ids) the fake search finds.
        found: Vec<String>,
        /// The tools of added terminals: `added` keeps their names.
        added_tools: Vec<ToolKind>,
    }

    /// Finds what the host's `found` lists, and nothing else.
    fn fake_detector(host: &Arc<FakeHost>) -> Detector {
        let host = Arc::clone(host);
        Arc::new(move |req: &Requirement, _: &[PathBuf]| {
            if host.with(|s| s.found.contains(&req.id)) {
                RequirementState::Found {
                    version: Some("4.7.2".into()),
                }
            } else {
                RequirementState::Missing
            }
        })
    }

    #[derive(Default)]
    struct FakeHost(Mutex<HostState>);

    impl FakeHost {
        fn with<T>(&self, f: impl FnOnce(&mut HostState) -> T) -> T {
            f(&mut self.0.lock().unwrap())
        }
    }

    impl Host for FakeHost {
        fn projects(&self) -> Vec<ProjectRef> {
            self.with(|s| s.projects.clone())
        }
        fn running_terminals(&self, _: &str) -> usize {
            self.with(|s| s.running)
        }
        fn open_project(&self, dir: &Path) -> Result<(), String> {
            self.with(|s| {
                s.opened.push(dir.to_owned());
                s.info = Some(ProjectInfo {
                    id: "home-project".to_owned(),
                    ready: true,
                    terminals: vec![("claude".to_owned(), ToolKind::Claude)],
                });
            });
            Ok(())
        }
        fn project_at(&self, _: &Path) -> Option<ProjectInfo> {
            self.with(|s| s.info.clone())
        }
        fn add_terminal(
            &self,
            project: &str,
            tool: ToolKind,
            name: Option<String>,
            prompt: Option<String>,
            _: Option<String>,
            _: Option<String>,
        ) -> Result<String, String> {
            self.with(|s| {
                s.added.push((project.to_owned(), name, prompt));
                s.added_tools.push(tool);
                if let Some(info) = &mut s.info {
                    info.terminals
                        .push(("claude-2".to_owned(), ToolKind::Claude));
                }
            });
            Ok("claude-2".to_owned())
        }
        fn terminal_session(&self, _: &str, terminal: &str) -> Option<String> {
            (terminal == "claude-2").then(|| "sess-1".to_owned())
        }
        fn show(&self, project: &str, _: Option<&str>) {
            self.with(|s| s.shown.push(project.to_owned()));
        }
        fn saved_home(&self) -> Option<PathBuf> {
            self.with(|s| s.saved_home.clone())
        }
        fn save_home(&self, home: Option<PathBuf>) -> Result<(), String> {
            self.with(|s| s.saved_home = home);
            Ok(())
        }
        fn automations_root(&self) -> Option<PathBuf> {
            self.with(|s| s.automations_root.clone())
        }
        fn use_automations_root(&self, root: PathBuf) -> Result<(), String> {
            if self.with(|s| s.fail_use_root) {
                return Err("refused".to_owned());
            }
            self.with(|s| {
                s.used_root = Some(root.clone());
                s.automations_root = Some(root);
            });
            Ok(())
        }
    }

    struct Rig {
        core: Core,
        host: Arc<FakeHost>,
        base: PathBuf,
        home: PathBuf,
        data: PathBuf,
    }

    impl Rig {
        fn new(name: &str) -> Self {
            let base = fs::canonicalize({
                let b =
                    std::env::temp_dir().join(format!("mg-pkgeng-{name}-{}", std::process::id()));
                let _ = fs::remove_dir_all(&b);
                fs::create_dir_all(&b).unwrap();
                b
            })
            .unwrap();
            let (home, data) = (base.join("home"), base.join("data"));
            fs::create_dir_all(home.join("packages")).unwrap();
            fs::create_dir_all(home.join("automations")).unwrap();
            let host = Arc::new(FakeHost::default());
            host.with(|s| s.saved_home = Some(home.clone()));
            let core = Core::new(Config {
                data_root: data.clone(),
                user_home: base.join("user"),
                host: Arc::clone(&host) as Arc<dyn Host>,
                wake: Arc::new(|| {}),
                detector: Some(fake_detector(&host)),
            });
            Self {
                core,
                host,
                base,
                home,
                data,
            }
        }

        fn project(&mut self, name: &str) -> (String, PathBuf) {
            let path = self.base.join(name);
            fs::create_dir_all(&path).unwrap();
            let id = connecto_session::launch::project_id(&path);
            self.host.with(|s| {
                s.projects.push(ProjectRef {
                    id: id.clone(),
                    path: path.clone(),
                });
            });
            (id, path)
        }

        /// A package change settles over two ticks.
        fn settle(&mut self) {
            self.core.tick(Instant::now());
            self.core.tick(Instant::now());
        }

        fn write_package(&self, id: &str, manifest: &str, files: &[(&str, &str)]) {
            let dir = self.home.join("packages").join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("package.json"), manifest).unwrap();
            for (path, text) in files {
                let file = dir.join(path);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, text).unwrap();
            }
        }

        fn view(&mut self) -> Arc<PackagesView> {
            self.core.publish();
            self.core.view().load_full()
        }

        fn status(&mut self, id: &str) -> ProjectPackages {
            self.view().project(id).unwrap().status.clone()
        }
    }

    fn set(id: &str, settings: ProjectSettings) -> Cmd {
        Cmd::SetProject(id.to_owned(), settings)
    }

    #[test]
    fn the_builtin_package_is_listed_first_and_is_the_default() {
        let mut r = Rig::new("builtin");
        let view = r.view();
        let first = &view.packages[0];
        assert!(first.builtin && first.is_default && first.id == "connecto");
        assert!(first.has_claude && first.has_codex);
        assert_eq!(view.default_package.as_deref(), Some("connecto"));
        assert_eq!(view.home.as_deref(), Some(r.home.as_path()));
    }

    #[test]
    fn a_package_change_bumps_its_version_by_one_and_an_untouched_one_does_not() {
        let mut r = Rig::new("version");
        r.write_package(
            "team",
            r#"{"name":"Team"}"#,
            &[(".claude/commands/go.md", "go")],
        );
        r.settle();
        let version = |r: &mut Rig| {
            r.view()
                .packages
                .iter()
                .find(|p| p.id == "team")
                .unwrap()
                .version
        };
        assert_eq!(version(&mut r), 1);
        r.settle();
        assert_eq!(version(&mut r), 1, "no change, no bump");
        fs::write(
            r.home.join("packages/team/.claude/commands/go.md"),
            "go faster",
        )
        .unwrap();
        r.settle();
        assert_eq!(version(&mut r), 2);
        let text = fs::read_to_string(r.home.join("packages/team/package.json")).unwrap();
        assert!(text.contains("\"version\": 2"), "{text}");
        r.settle();
        assert_eq!(version(&mut r), 2, "writing package.json is not a change");
        // A new file counts, a guide, origin record or session file does not.
        fs::write(r.home.join("packages/team/CLAUDE.md"), "guide").unwrap();
        fs::write(r.home.join("packages/team/ORIGIN.md"), "from a repo").unwrap();
        fs::write(r.home.join("packages/team/.session.json"), "{}").unwrap();
        r.settle();
        assert_eq!(version(&mut r), 2);
        fs::write(r.home.join("packages/team/.claude/commands/new.md"), "n").unwrap();
        r.settle();
        assert_eq!(version(&mut r), 3);
    }

    #[test]
    fn the_default_package_installs_into_an_open_project_by_its_tools() {
        let mut r = Rig::new("default");
        let (id, path) = r.project("proj");
        r.core.tick(Instant::now());
        let status = r.status(&id);
        assert_eq!(status.state, InstallState::Updated);
        assert_eq!(status.packages[0].id, "connecto");
        assert_eq!(status.packages[0].installed_version, Some(10));
        // Version 10: the second opinion, and teams that take their agents
        // from `connecto agents --roles` (ADR 0045).
        assert!(
            path.join(".claude/skills/second-opinion/SKILL.md")
                .is_file()
        );
        assert!(
            path.join(".agents/skills/second-opinion/SKILL.md")
                .is_file()
        );
        assert!(path.join(".claude/skills/dev-team/SKILL.md").is_file());
        assert!(path.join(".agents/skills/dev-team/SKILL.md").is_file());
        // Version 9: Claude ends a task with the status recap from
        // `.claude/rules` (the others get it at launch). The project's own
        // instruction files are not touched.
        let rule = fs::read_to_string(path.join(".claude/rules/connecto-status.md")).unwrap();
        assert!(rule.contains("**Waiting on user**"), "{rule}");
        assert!(!path.join("AGENTS.md").exists() && !path.join("CLAUDE.md").exists());
        // Version 8: the Agentic Works guide knows several jobs at once, for
        // Claude and for the agents that read `.agents`.
        for tool in [".claude", ".agents"] {
            let guide =
                fs::read_to_string(path.join(tool).join("skills/agentic-works/SKILL.md")).unwrap();
            assert!(
                guide.contains("connecto works parallel")
                    && guide.contains("### Several jobs at once"),
                "{tool}"
            );
        }
        assert!(path.join(".connecto/packages.lock").is_file());
        // Claude only: the Claude form of the package, and the Codex files go.
        r.core.command(set(
            &id,
            ProjectSettings {
                tools: ProjectTools::Claude,
                ..ProjectSettings::default()
            },
        ));
        r.core.tick(Instant::now());
        assert!(!path.join(".agents").exists() && !path.join(".codex").exists());
        assert!(path.join(".claude/rules/connecto-status.md").is_file());
        let skill = fs::read_to_string(path.join(".claude/skills/dev-team/SKILL.md")).unwrap();
        assert!(skill.contains("connecto add claude"));
        let settings = fs::read_to_string(path.join(".claude/settings.json")).unwrap();
        assert!(!settings.contains("codex@openai-codex"));
        // Opting out of the default takes everything out again.
        r.core.command(set(
            &id,
            ProjectSettings {
                use_default_package: false,
                ..ProjectSettings::default()
            },
        ));
        r.core.tick(Instant::now());
        assert!(!path.join(".claude").exists() && !path.join(".connecto").exists());
        assert_eq!(r.status(&id).state, InstallState::None);
    }

    /// Two writes queued one after the other: this machine makes OpenCode
    /// isolated, then a peer from before OpenCode changes only the packages.
    /// The second keeps the isolation the first saved, though it was made
    /// from settings read before the first was applied.
    #[test]
    fn an_older_peers_queued_write_keeps_the_opencode_config_written_before_it() {
        let mut r = Rig::new("keep-opencode");
        let (id, _) = r.project("proj");
        r.core.tick(Instant::now());
        let before = r.core.view().load().settings_of(&id);
        assert_eq!(before.opencode_config, connecto_core::ConfigMode::System);
        let isolated = ProjectSettings {
            opencode_config: connecto_core::ConfigMode::Isolated,
            ..before.clone()
        };
        // The older peer's settings: as it read them, before the first write.
        let older = ProjectSettings {
            use_default_package: false,
            ..before
        };
        r.core.command(set(&id, isolated));
        r.core
            .command(Cmd::SetProjectKeepingOpenCode(id.clone(), older));
        let saved = connecto_session::project_settings::load(&r.data, &id);
        assert_eq!(saved.opencode_config, connecto_core::ConfigMode::Isolated);
        assert!(!saved.use_default_package, "the peer's change is kept");
        r.core.publish();
        assert_eq!(
            r.core.view().load().settings_of(&id).opencode_config,
            connecto_core::ConfigMode::Isolated
        );
    }

    /// Two writes queued one after the other: this machine chooses Codex and
    /// Pi, then a peer from before sets of tools, which was shown All, changes
    /// only the packages. The second keeps Codex and Pi, though it was made
    /// before the first was applied; one that chose a tool sets that tool.
    #[test]
    fn an_older_peers_queued_write_keeps_the_set_of_tools_written_before_it() {
        use connecto_core::ToolKind;
        let mut r = Rig::new("keep-tools");
        let (id, _) = r.project("proj");
        r.core.tick(Instant::now());
        let before = r.core.view().load().settings_of(&id);
        assert_eq!(before.tools, ProjectTools::All);
        let codex_pi = ProjectTools::from_tools([ToolKind::Codex, ToolKind::Pi]).unwrap();
        let older = ProjectSettings {
            use_default_package: false,
            ..before.clone()
        };
        r.core.command(set(
            &id,
            ProjectSettings {
                tools: codex_pi,
                ..before
            },
        ));
        r.core
            .command(Cmd::SetProjectKeepingTools(id.clone(), older.clone()));
        let saved = connecto_session::project_settings::load(&r.data, &id);
        assert_eq!(saved.tools, codex_pi);
        assert!(!saved.use_default_package, "the peer's change is kept");
        // From before OpenCode too.
        r.core
            .command(Cmd::SetProjectKeepingOpenCode(id.clone(), older.clone()));
        assert_eq!(
            connecto_session::project_settings::load(&r.data, &id).tools,
            codex_pi
        );
        // A peer that chose Claude alone.
        r.core.command(Cmd::SetProjectKeepingTools(
            id.clone(),
            ProjectSettings {
                tools: ProjectTools::Claude,
                ..older
            },
        ));
        assert_eq!(
            connecto_session::project_settings::load(&r.data, &id).tools,
            ProjectTools::Claude
        );
    }

    #[test]
    fn settings_persist_and_a_new_manager_reads_them() {
        let mut r = Rig::new("persist");
        let (id, _) = r.project("proj");
        r.core.tick(Instant::now());
        let chosen = ProjectSettings {
            tools: ProjectTools::Codex,
            claude_config: connecto_core::ConfigMode::System,
            codex_config: connecto_core::ConfigMode::System,
            pi_config: connecto_core::ConfigMode::System,
            opencode_config: connecto_core::ConfigMode::Isolated,
            use_default_package: false,
            packages: vec![],
        };
        r.core.command(set(&id, chosen.clone()));
        assert_eq!(
            connecto_session::project_settings::load(&r.data, &id),
            chosen
        );
        let fresh = Core::new(Config {
            data_root: r.data.clone(),
            user_home: r.base.join("user"),
            host: Arc::clone(&r.host) as Arc<dyn Host>,
            wake: Arc::new(|| {}),
            detector: None,
        });
        let mut fresh = fresh;
        fresh.tick(Instant::now());
        fresh.publish();
        assert_eq!(fresh.view().load().settings_of(&id), chosen);
        assert_eq!(
            fresh.view().load().allowed_tools(&id),
            vec![ToolKind::Codex]
        );
    }

    #[test]
    fn the_view_shows_submitted_settings_exactly() {
        let mut r = Rig::new("exact");
        r.write_package("b", r#"{"name":"B"}"#, &[(".claude/b.md", "b")]);
        r.write_package("a", r#"{"name":"A"}"#, &[(".claude/a.md", "a")]);
        r.settle();
        let (id, _) = r.project("proj");
        let sent = ProjectSettings {
            packages: vec!["b".into(), "a".into(), "b".into()],
            ..ProjectSettings::default()
        };
        r.core.command(set(&id, sent.clone()));
        r.core.tick(Instant::now());
        assert_eq!(r.view().settings_of(&id), sent);
    }

    #[test]
    fn extends_installs_the_base_first_and_a_broken_chain_is_an_error() {
        let mut r = Rig::new("extends");
        r.write_package(
            "team",
            r#"{"name":"Team","extends":"connecto"}"#,
            &[
                (".claude/commands/mine.md", "mine"),
                (
                    ".claude/skills/dev-team/SKILL.md",
                    "---\nname: dev-team\ndescription: mine\n---\nmy dev team",
                ),
            ],
        );
        r.settle();
        let (id, path) = r.project("proj");
        r.core.command(set(
            &id,
            ProjectSettings {
                use_default_package: false,
                packages: vec!["team".to_owned()],
                ..ProjectSettings::default()
            },
        ));
        r.core.tick(Instant::now());
        let status = r.status(&id);
        let ids: Vec<_> = status.packages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["connecto", "team"], "{status:?}");
        assert!(
            path.join(".claude/skills/brainstorm-team/SKILL.md")
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(path.join(".claude/skills/dev-team/SKILL.md")).unwrap(),
            "---\nname: dev-team\ndescription: mine\n---\nmy dev team",
            "the extending package's file wins"
        );
        // Default and an extending package do not install the base twice.
        r.core.command(set(
            &id,
            ProjectSettings {
                packages: vec!["team".to_owned()],
                ..ProjectSettings::default()
            },
        ));
        r.core.tick(Instant::now());
        assert_eq!(r.status(&id).packages.len(), 2);
        // A cycle and a missing base are errors, with nothing half-applied.
        r.write_package(
            "a",
            r#"{"name":"A","extends":"b"}"#,
            &[(".claude/a.md", "a")],
        );
        r.write_package(
            "b",
            r#"{"name":"B","extends":"a"}"#,
            &[(".claude/b.md", "b")],
        );
        r.settle();
        r.core.command(set(
            &id,
            ProjectSettings {
                use_default_package: false,
                packages: vec!["a".to_owned()],
                ..ProjectSettings::default()
            },
        ));
        r.core.tick(Instant::now());
        assert!(matches!(r.status(&id).state, InstallState::Error(_)));
        assert!(!path.join(".claude/a.md").exists());
        assert!(
            path.join(".claude/commands/mine.md").is_file(),
            "left as it was"
        );
    }

    #[test]
    fn a_version_bump_updates_every_open_project_that_uses_it() {
        let mut r = Rig::new("bump");
        r.write_package(
            "team",
            r#"{"name":"Team"}"#,
            &[(".claude/commands/go.md", "go")],
        );
        r.settle();
        let (a, pa) = r.project("a");
        let (b, pb) = r.project("b");
        for id in [&a, &b] {
            r.core.command(set(
                id,
                ProjectSettings {
                    use_default_package: false,
                    packages: vec!["team".to_owned()],
                    ..ProjectSettings::default()
                },
            ));
        }
        r.core.tick(Instant::now());
        assert_eq!(r.status(&a).packages[0].installed_version, Some(1));
        r.host.with(|s| s.running = 1);
        fs::write(r.home.join("packages/team/.claude/commands/go.md"), "go v2").unwrap();
        r.settle();
        for (id, path) in [(&a, &pa), (&b, &pb)] {
            let status = r.status(id);
            assert_eq!(status.packages[0].installed_version, Some(2), "{status:?}");
            assert_eq!(status.state, InstallState::Updated);
            assert!(status.restart_needed, "running terminals must restart");
            assert_eq!(
                fs::read_to_string(path.join(".claude/commands/go.md")).unwrap(),
                "go v2"
            );
        }
        // Once nothing runs, the restart note clears.
        r.host.with(|s| s.running = 0);
        r.core.tick(Instant::now());
        assert!(!r.status(&a).restart_needed);
    }

    #[test]
    fn a_package_with_problems_is_listed_but_not_installed_or_default() {
        let mut r = Rig::new("problems");
        r.write_package("empty", r#"{"name":"Empty"}"#, &[]);
        r.write_package("broken", "{", &[]);
        r.settle();
        let view = r.view();
        let empty = view.packages.iter().find(|p| p.id == "empty").unwrap();
        assert!(!empty.errors.is_empty());
        assert!(
            !view
                .packages
                .iter()
                .find(|p| p.id == "broken")
                .unwrap()
                .errors
                .is_empty()
        );
        r.core.command(Cmd::SetDefault(Some("empty".to_owned())));
        assert_eq!(r.view().default_package.as_deref(), Some("connecto"));
        assert!(r.view().last_error.is_some());
        let (id, _) = r.project("proj");
        r.core.command(set(
            &id,
            ProjectSettings {
                packages: vec!["empty".to_owned()],
                ..ProjectSettings::default()
            },
        ));
        r.core.tick(Instant::now());
        assert!(matches!(r.status(&id).state, InstallState::Error(_)));
    }

    #[test]
    fn the_default_can_be_changed_cleared_and_falls_away_with_its_package() {
        let mut r = Rig::new("default-change");
        r.write_package("team", r#"{"name":"Team"}"#, &[(".claude/a.md", "a")]);
        r.settle();
        r.core.command(Cmd::SetDefault(Some("team".to_owned())));
        let view = r.view();
        assert_eq!(view.default_package.as_deref(), Some("team"));
        assert!(
            view.packages
                .iter()
                .find(|p| p.id == "team")
                .unwrap()
                .is_default
        );
        assert!(!view.packages[0].is_default);
        r.core.command(Cmd::SetDefault(None));
        assert_eq!(r.view().default_package, None);
        assert_eq!(State::load(&r.data).state.default_id(), None);
        r.core.command(Cmd::SetDefault(Some("team".to_owned())));
        r.core.command(Cmd::Delete("team".to_owned()));
        let view = r.view();
        assert!(!r.home.join("packages/team").exists());
        assert_eq!(view.default_package, None, "the default became none");
        assert!(view.packages.iter().all(|p| p.id != "team"));
        r.core.command(Cmd::Delete("connecto".to_owned()));
        assert!(r.view().last_error.is_some(), "the built-in one stays");
    }

    /// `nul` is the null device on Windows, not a folder: such a package gets
    /// the next free name.
    #[cfg(windows)]
    #[test]
    fn a_package_named_after_a_windows_device_gets_another_folder() {
        let mut r = Rig::new("device");
        r.core.command(Cmd::Create {
            name: "NUL".to_owned(),
            extends: None,
        });
        assert!(r.view().last_error.is_none(), "{:?}", r.view().last_error);
        assert!(r.home.join("packages/nul-2/package.json").is_file());
    }

    #[test]
    fn a_package_is_not_edited_where_connecto_home_leaves_claude_out() {
        use connecto_core::ToolKind;
        let mut r = Rig::new("create-no-claude");
        let mut settings = project_settings::load(&r.data, "home-project");
        settings.tools = ProjectTools::from_tools([ToolKind::Codex, ToolKind::Pi]).unwrap();
        project_settings::save(&r.data, "home-project", &settings).unwrap();
        r.core.command(Cmd::Create {
            name: "My Team".to_owned(),
            extends: None,
        });
        r.core.tick(Instant::now());
        r.core.tick(Instant::now());
        let added = r.host.with(|s| s.added.clone());
        assert!(added.is_empty(), "no session started: {added:?}");
        r.core.publish();
        let error = r.core.view().load().last_error.clone().unwrap_or_default();
        assert!(error.contains("leaves Claude out"), "{error}");
    }

    #[test]
    fn creating_a_package_writes_it_and_opens_a_briefed_claude_session_in_home() {
        let mut r = Rig::new("create");
        let (_, other) = r.project("some-project");
        r.core.command(Cmd::Create {
            name: "My Team".to_owned(),
            extends: Some("connecto".to_owned()),
        });
        let dir = r.home.join("packages/my-team");
        let manifest = fs::read_to_string(dir.join("package.json")).unwrap();
        assert!(
            manifest.contains("\"extends\": \"connecto\"") && manifest.contains("\"version\": 1")
        );
        assert!(dir.join("CLAUDE.md").is_file() && dir.join("AGENTS.md").is_file());
        // The session: Home is opened as a project, then a terminal is added.
        r.core.tick(Instant::now());
        r.core.tick(Instant::now());
        let (opened, added, shown) = r
            .host
            .with(|s| (s.opened.clone(), s.added.clone(), s.shown.clone()));
        assert_eq!(opened, [r.home.clone()]);
        assert_eq!(added.len(), 1, "{added:?}");
        let (project, name, prompt) = &added[0];
        assert_eq!(project, "home-project");
        assert_eq!(name.as_deref(), Some("Package: My Team"));
        let prompt = prompt.as_deref().unwrap();
        assert!(prompt.contains("connecto package check my-team"));
        assert!(
            !prompt.contains(&other.display().to_string()),
            "the open projects are not listed: the session only knows its objective"
        );
        assert!(prompt.contains("Do not look through my projects"));
        assert!(
            prompt.contains("builtin-connecto"),
            "points at the built-in package"
        );
        assert_eq!(shown, ["home-project"]);
        assert!(
            r.data
                .join("packages/builtin-connecto/.claude/skills/dev-team/SKILL.md")
                .is_file()
        );
        // The tool session id is recorded beside the package for a later resume.
        r.core.tick(Instant::now());
        let record = session::read(&dir).unwrap();
        assert_eq!(
            (record.terminal.as_str(), record.session.as_str()),
            ("claude-2", "sess-1")
        );
        assert_eq!(record.guide, Some(guide::VERSION));
        // A second package with the same name gets its own folder.
        r.core.command(Cmd::Create {
            name: "My Team".to_owned(),
            extends: None,
        });
        assert!(r.home.join("packages/my-team-2/package.json").is_file());
        // Not allowed: a missing base, a bad name.
        r.core.command(Cmd::Create {
            name: "X".to_owned(),
            extends: Some("nope".to_owned()),
        });
        assert!(r.view().last_error.is_some());
        assert!(!r.home.join("packages/x").exists());
    }

    #[test]
    fn a_package_exported_to_a_file_imports_as_a_new_package() {
        let mut r = Rig::new("export");
        r.write_package(
            "team",
            r#"{"name":"Team","description":"Tools","version":3}"#,
            &[(
                ".claude/skills/a/SKILL.md",
                "---\nname: a\ndescription: a\n---\n",
            )],
        );
        fs::write(r.home.join("packages/team/ORIGIN.md"), "# Origin\n").unwrap();
        r.settle();
        let file = r.home.join("team.connecto-package");
        r.core.command(Cmd::Export {
            id: "team".to_owned(),
            dest: file.clone(),
        });
        assert!(
            r.view()
                .notice
                .clone()
                .unwrap()
                .starts_with("Exported Team")
        );
        let text = fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains("connecto-packages:begin"),
            "no guide: {text}"
        );
        r.core.command(Cmd::Import(file));
        let view = r.view();
        assert!(view.last_error.is_none(), "{:?}", view.last_error);
        assert_eq!(view.notice.as_deref(), Some("Imported Team as team-2"));
        let copy = r.home.join("packages/team-2");
        assert_eq!(
            fs::read_to_string(copy.join(".claude/skills/a/SKILL.md")).unwrap(),
            "---\nname: a\ndescription: a\n---\n"
        );
        assert_eq!(
            fs::read_to_string(copy.join("ORIGIN.md")).unwrap(),
            "# Origin\n"
        );
        assert!(
            copy.join("CLAUDE.md").is_file(),
            "the guide is written again"
        );
        let item = view.packages.iter().find(|p| p.id == "team-2").unwrap();
        assert_eq!((item.version, item.description.as_str()), (1, "Tools"));
        // Not a package file: refused, nothing added.
        let junk = r.home.join("junk.connecto-package");
        fs::write(&junk, "{}").unwrap();
        r.core.command(Cmd::Import(junk));
        assert!(
            r.view()
                .last_error
                .clone()
                .unwrap()
                .contains("not a Connecto package")
        );
        assert!(!r.home.join("packages/team-3").exists());
    }

    #[test]
    fn a_new_package_is_told_to_write_its_origin_and_an_edit_to_read_and_keep_it() {
        let root = Path::new("/home/packages");
        let made = authoring_prompt(root, "team", true, None);
        assert!(
            made.contains("Before you finish, write `team/ORIGIN.md`"),
            "{made}"
        );
        assert!(!made.contains("never invent"), "{made}");
        let edit = authoring_prompt(root, "team", false, None);
        assert!(
            edit.contains("Read `team/CLAUDE.md` first (it says how a package is laid out) and then `team/ORIGIN.md`"),
            "{edit}"
        );
        assert!(edit.contains("bring it up to date") && edit.contains("never invent sources"));
    }

    #[test]
    fn editing_resumes_the_recorded_session_when_it_is_still_open() {
        let mut r = Rig::new("edit");
        r.write_package("team", r#"{"name":"Team"}"#, &[(".claude/a.md", "a")]);
        r.settle();
        let dir = r.home.join("packages/team");
        session::write(
            &dir,
            &session::Record {
                terminal: "claude-2".into(),
                session: "sess-1".into(),
                guide: Some(guide::VERSION),
            },
        )
        .unwrap();
        r.host.with(|s| {
            s.info = Some(ProjectInfo {
                id: "home-project".into(),
                ready: true,
                terminals: vec![("claude-2".into(), ToolKind::Claude)],
            });
        });
        r.core.command(Cmd::Edit("team".to_owned()));
        r.core.tick(Instant::now());
        assert!(
            r.host.with(|s| s.added.is_empty()),
            "the open terminal is shown instead"
        );
        assert_eq!(r.host.with(|s| s.shown.clone()), ["home-project"]);
        assert!(r.core.command_is_noop_for_builtin());
    }

    #[test]
    fn the_home_project_itself_gets_no_packages() {
        let mut r = Rig::new("homeproj");
        let home = r.home.clone();
        let id = connecto_session::launch::project_id(&home);
        r.host.with(|s| {
            s.projects.push(ProjectRef {
                id: id.clone(),
                path: home.clone(),
            })
        });
        r.core.tick(Instant::now());
        assert_eq!(r.status(&id).state, InstallState::None);
        assert!(!home.join(".claude").exists());
    }

    #[test]
    fn choosing_a_home_moves_the_old_automations_in_and_points_the_engine_at_it() {
        let mut r = Rig::new("sethome");
        let old = r.base.join("old-automations");
        fs::create_dir_all(old.join("say-yes")).unwrap();
        fs::write(old.join("say-yes/automation.json"), "{}").unwrap();
        r.host.with(|s| {
            s.saved_home = None;
            s.automations_root = Some(old.clone());
        });
        let new_home = r.base.join("connecto-home");
        fs::create_dir_all(&new_home).unwrap();
        r.core.home = None;
        r.core.command(Cmd::SetHome(new_home.clone()));
        let view = r.view();
        assert_eq!(view.migration, HomeMigration::Done { moved: 1 });
        assert_eq!(view.home.as_deref(), Some(new_home.as_path()));
        assert!(
            new_home
                .join("automations/say-yes/automation.json")
                .is_file()
        );
        assert!(!old.join("say-yes").exists());
        assert!(new_home.join("packages").is_dir());
        assert_eq!(
            r.host.with(|s| s.saved_home.clone()),
            Some(new_home.clone())
        );
        assert_eq!(
            r.host.with(|s| s.used_root.clone()),
            Some(new_home.join("automations"))
        );
        // Packages now come from the new home.
        r.write_package("later", r#"{"name":"Later"}"#, &[(".claude/a.md", "a")]);
        let _ = fs::rename(
            r.home.join("packages/later"),
            new_home.join("packages/later"),
        );
        r.settle();
        assert!(r.view().packages.iter().any(|p| p.id == "later"));
    }

    #[test]
    fn a_conflicting_move_is_refused_and_leaves_home_and_folders_untouched() {
        let mut r = Rig::new("conflict");
        let old = r.base.join("old-automations");
        let new_home = r.base.join("connecto-home");
        for dir in [old.join("say-yes"), new_home.join("automations/say-yes")] {
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("automation.json"), "{}").unwrap();
        }
        r.host.with(|s| {
            s.saved_home = None;
            s.automations_root = Some(old.clone());
        });
        r.core.home = None;
        r.core.command(Cmd::SetHome(new_home.clone()));
        let view = r.view();
        let why = match &view.migration {
            HomeMigration::Error(why) => why.clone(),
            other => format!("not an error: {other:?}"),
        };
        assert!(!why.starts_with("not an error"), "{why}");
        assert!(why.contains("say-yes"), "{why}");
        assert!(view.home.is_none(), "Home was not changed");
        assert_eq!(r.host.with(|s| s.saved_home.clone()), None);
        assert_eq!(r.host.with(|s| s.used_root.clone()), None);
        assert!(old.join("say-yes/automation.json").is_file());
    }

    #[test]
    fn a_failed_home_switch_puts_back_only_what_it_moved() {
        let mut r = Rig::new("rollback");
        let old = r.base.join("old-automations");
        let new_home = r.base.join("connecto-home");
        fs::create_dir_all(old.join("moved")).unwrap();
        fs::write(old.join("moved/automation.json"), "{}").unwrap();
        fs::create_dir_all(new_home.join("automations/already")).unwrap();
        fs::write(new_home.join("automations/already/automation.json"), "{}").unwrap();
        r.host.with(|s| {
            s.saved_home = None;
            s.automations_root = Some(old.clone());
            s.fail_use_root = true;
        });
        r.core.home = None;
        r.core.command(Cmd::SetHome(new_home.clone()));
        assert!(matches!(r.view().migration, HomeMigration::Error(_)));
        assert!(old.join("moved/automation.json").is_file(), "put back");
        assert!(
            new_home
                .join("automations/already/automation.json")
                .is_file(),
            "what was already there stays"
        );
        assert!(!old.join("already").exists());
        assert_eq!(r.host.with(|s| s.saved_home.clone()), None);
    }

    /// A package folder that is a link (a junction on Windows, a symlink
    /// elsewhere) is deleted as a link: what it points to stays.
    #[test]
    fn a_linked_package_folder_is_deleted_as_a_link() {
        let mut r = Rig::new("linkdel");
        let away = r.base.join("away");
        fs::create_dir_all(away.join(".claude")).unwrap();
        fs::write(away.join("package.json"), r#"{"name":"Away"}"#).unwrap();
        fs::write(away.join(".claude/a.md"), "a").unwrap();
        crate::platform::link_dir(&away, &r.home.join("packages/away")).unwrap();
        r.settle();
        r.core.command(Cmd::Delete("away".to_owned()));
        assert!(r.view().last_error.is_none(), "{:?}", r.view().last_error);
        assert!(fs::symlink_metadata(r.home.join("packages/away")).is_err());
        assert!(away.join(".claude/a.md").is_file(), "the target stays");
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_manifest_is_not_followed_or_rewritten_and_a_linked_guide_is_replaced() {
        let mut r = Rig::new("links");
        let outside = r.base.join("outside.json");
        fs::write(&outside, r#"{"name":"Out","version":1}"#).unwrap();
        let dir = r.home.join("packages/linked");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(dir.join(".claude/a.md"), "a").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("package.json")).unwrap();
        r.settle();
        let view = r.view();
        let item = view.packages.iter().find(|p| p.id == "linked").unwrap();
        assert!(!item.errors.is_empty());
        fs::write(dir.join(".claude/a.md"), "changed").unwrap();
        r.settle();
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            r#"{"name":"Out","version":1}"#,
            "the link target is never rewritten"
        );
        // A real package whose guide file is a link: the link is replaced.
        r.write_package("real", r#"{"name":"Real"}"#, &[(".claude/a.md", "a")]);
        let guide_target = r.base.join("guide-target.md");
        fs::write(&guide_target, "precious").unwrap();
        std::os::unix::fs::symlink(&guide_target, r.home.join("packages/real/CLAUDE.md")).unwrap();
        r.settle();
        r.core.command(Cmd::Edit("real".to_owned()));
        assert_eq!(fs::read_to_string(&guide_target).unwrap(), "precious");
        assert!(
            fs::read_to_string(r.home.join("packages/real/CLAUDE.md"))
                .unwrap()
                .contains("connecto-packages:begin")
        );
    }

    #[test]
    fn a_project_is_held_at_the_install_gate_until_its_packages_are_installed() {
        let mut r = Rig::new("gate");
        connecto_session::install_gate::enable(&r.data);
        r.core.lease_wait = Duration::from_millis(20);
        let (id, path) = r.project("proj");
        // The workspace sets the project up first and holds its terminals.
        connecto_session::install_gate::expect(&r.data, &id);
        assert!(
            connecto_session::install_gate::permit(&r.data, &id, Duration::from_millis(10))
                .is_none()
        );
        r.core.tick(Instant::now());
        assert!(
            path.join(".claude/skills/dev-team/SKILL.md").is_file(),
            "installed before the gate opened"
        );
        assert!(
            connecto_session::install_gate::permit(&r.data, &id, Duration::from_millis(10))
                .is_some()
        );
        // A launch in progress holds the install back, which stays queued.
        connecto_session::install_gate::expect(&r.data, &id);
        let launch =
            connecto_session::install_gate::permit(&r.data, "other", Duration::from_millis(10));
        drop(launch);
        r.core.command(Cmd::Refresh(id.clone()));
        let hold = {
            // A permit taken while the project is not pending: the install must wait.
            connecto_session::install_gate::finish(&r.data, &id);
            connecto_session::install_gate::permit(&r.data, &id, Duration::from_millis(10)).unwrap()
        };
        r.core.tick(Instant::now());
        assert!(
            r.core.projects[0].dirty,
            "no install while a launch is between permit and spawn"
        );
        drop(hold);
        r.core.tick(Instant::now());
        assert!(!r.core.projects[0].dirty);
        // A project set up anew with nothing to install is let go on the next tick.
        connecto_session::install_gate::expect(&r.data, &id);
        r.core.tick(Instant::now());
        assert!(!connecto_session::install_gate::is_pending(&r.data, &id));
        // An error releases it too.
        r.core.command(set(
            &id,
            ProjectSettings {
                packages: vec!["missing-but-valid".to_owned()],
                ..ProjectSettings::default()
            },
        ));
        connecto_session::install_gate::expect(&r.data, &id);
        r.core.tick(Instant::now());
        assert!(!connecto_session::install_gate::is_pending(&r.data, &id));
        connecto_session::install_gate::disable(&r.data);
    }

    /// A project that has the package `team` installed, and the rig for it.
    fn rig_with_team_installed(name: &str) -> (Rig, String, PathBuf) {
        let mut r = Rig::new(name);
        r.write_package(
            "team",
            r#"{"name":"Team"}"#,
            &[(".claude/commands/go.md", "go")],
        );
        r.settle();
        let (id, path) = r.project("proj");
        r.core.command(set(
            &id,
            ProjectSettings {
                use_default_package: false,
                packages: vec!["team".to_owned()],
                ..ProjectSettings::default()
            },
        ));
        r.core.tick(Instant::now());
        assert!(path.join(".claude/commands/go.md").is_file());
        (r, id, path)
    }

    #[test]
    fn a_packages_folder_that_cannot_be_listed_uninstalls_nothing_and_recovers() {
        let (mut r, id, path) = rig_with_team_installed("unlistable");
        let (packages, away) = (r.home.join("packages"), r.home.join("packages-away"));
        // Twice: a file where the folder was, then no folder at all.
        for as_file in [true, false] {
            fs::rename(&packages, &away).unwrap();
            if as_file {
                fs::write(&packages, "not a folder").unwrap();
            }
            r.settle();
            r.core.command(Cmd::Refresh(id.clone()));
            r.core.tick(Instant::now());
            assert!(
                path.join(".claude/commands/go.md").is_file(),
                "as_file {as_file}: nothing is uninstalled"
            );
            assert!(path.join(".connecto/packages.lock").is_file());
            let view = r.view();
            let error = view.last_error.clone().unwrap_or_default();
            assert!(error.contains("could not be read"), "{error}");
            assert!(
                view.packages.iter().any(|p| p.id == "team"),
                "the list is kept"
            );
            // The project keeps what it has, from the packages as last read.
            let status = r.status(&id);
            assert_eq!(status.state, InstallState::UpToDate, "{status:?}");
            assert_eq!(status.packages[0].installed_version, Some(1));
            // A package that was never read is unknown, not gone.
            assert!(
                r.core
                    .resolve("other", Variant::Both, &mut Vec::new())
                    .is_err()
            );
            // Readable again: the project is checked and the note goes.
            if as_file {
                fs::remove_file(&packages).unwrap();
            }
            fs::rename(&away, &packages).unwrap();
            r.settle();
            r.core.tick(Instant::now());
            let view = r.view();
            assert_eq!(view.last_error, None);
            let status = r.status(&id);
            assert_eq!(status.state, InstallState::UpToDate, "{status:?}");
            assert_eq!(status.packages[0].installed_version, Some(1));
            assert!(path.join(".claude/commands/go.md").is_file());
        }
    }

    #[test]
    fn a_damaged_state_file_installs_no_default_package_until_the_user_chooses() {
        let mut r = Rig::new("badstate");
        fs::create_dir_all(r.data.join("packages")).unwrap();
        fs::write(crate::state::path(&r.data), "{ damaged").unwrap();
        let (id, path) = r.project("proj");
        let mut core = Core::new(Config {
            data_root: r.data.clone(),
            user_home: r.base.join("user"),
            host: Arc::clone(&r.host) as Arc<dyn Host>,
            wake: Arc::new(|| {}),
            detector: None,
        });
        core.tick(Instant::now());
        core.publish();
        let view = core.view().load_full();
        assert_eq!(view.default_package, None);
        assert!(
            view.last_error
                .as_deref()
                .is_some_and(|e| e.contains("state.json.bad")),
            "{:?}",
            view.last_error
        );
        assert!(
            !path.join(".claude").exists(),
            "the built-in package is not installed"
        );
        assert_eq!(
            fs::read_to_string(r.data.join("packages/state.json.bad")).unwrap(),
            "{ damaged"
        );
        // The user's choice is taken and saved as usual.
        core.command(Cmd::SetDefault(Some("connecto".to_owned())));
        core.tick(Instant::now());
        assert!(path.join(".claude/skills/dev-team/SKILL.md").is_file());
        assert_eq!(
            State::load(&r.data).state.default_id(),
            Some("connecto"),
            "{id}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_script_in_a_package_reaches_the_project_executable() {
        use std::os::unix::fs::PermissionsExt;
        let mut r = Rig::new("exec");
        let (_, path) = r.project("proj");
        r.core.tick(Instant::now());
        let mode = |file: &Path| fs::metadata(file).unwrap().permissions().mode();
        assert_eq!(
            mode(&path.join(".claude/scripts/statusline.sh")) & 0o111,
            0o111,
            "the built-in status line script can be run"
        );
        assert_eq!(mode(&path.join(".claude/settings.json")) & 0o111, 0);
        // A chmod in a user's package is a new version and reaches the project.
        let (mut r, _, path) = rig_with_team_installed("exec-chmod");
        let installed = path.join(".claude/commands/go.md");
        assert_eq!(mode(&installed) & 0o111, 0);
        fs::set_permissions(
            r.home.join("packages/team/.claude/commands/go.md"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        r.settle();
        assert_eq!(mode(&installed) & 0o111, 0o111);
        let team = r.view().packages.iter().find(|p| p.id == "team").cloned();
        assert_eq!(team.map(|p| p.version), Some(2));
    }

    #[test]
    fn a_fresh_home_with_no_old_folder_just_sets_up_the_layout() {
        let mut r = Rig::new("fresh");
        let new_home = r.base.join("fresh-home");
        fs::create_dir_all(&new_home).unwrap();
        r.host.with(|s| s.saved_home = None);
        r.core.home = None;
        r.core.command(Cmd::SetHome(new_home.clone()));
        let view = r.view();
        assert_eq!(view.migration, HomeMigration::None);
        assert!(new_home.join("automations").is_dir() && new_home.join("packages").is_dir());
        assert_eq!(
            r.host.with(|s| s.used_root.clone()),
            Some(new_home.join("automations"))
        );
    }

    impl Core {
        fn command_is_noop_for_builtin(&mut self) -> bool {
            self.command(Cmd::Edit("connecto".to_owned()));
            self.last_error.is_some()
        }
    }

    // ---- tools packages need (ADR 0037) ----------------------------------

    const SKILL: &str = "---\nname: s\ndescription: does s\n---\nBody\n";

    const NEEDS_TOOL: &str = r#"{"name":"Needs Tool","requires":[
        {"id":"tool","name":"The Tool","detect":{"programs":["the-tool"]},
         "install":{"macos":"brew install the-tool","windows":"winget install the-tool","linux":"apt install the-tool"},
         "manual":"Get it from its site."},
        {"id":"paid","name":"Paid Editor","licence":true,"detect":{"programs":["paid"]},
         "manual":"Buy it and accept its licence yourself."}]}"#;

    /// Ticks until the package's tools are no longer being looked for.
    fn checked(r: &mut Rig, id: &str) -> Vec<RequirementView> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            r.core.tick(Instant::now());
            let item = r
                .view()
                .packages
                .iter()
                .find(|p| p.id == id)
                .cloned()
                .unwrap();
            if item
                .requires
                .iter()
                .all(|q| q.state != RequirementState::Checking)
                || Instant::now() > deadline
            {
                return item.requires;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_tools_a_package_needs_are_looked_for_and_looked_for_again() {
        let mut r = Rig::new("req-look");
        r.write_package("needs", NEEDS_TOOL, &[(".claude/skills/s/SKILL.md", SKILL)]);
        r.settle();
        let requires = checked(&mut r, "needs");
        assert_eq!(requires.len(), 2);
        assert_eq!(requires[0].state, RequirementState::Missing);
        assert!(
            requires[0]
                .install
                .as_deref()
                .is_some_and(|c| c.contains("the-tool"))
        );
        // A tool that needs a licence is never offered as a command.
        assert!(requires[1].licence && requires[1].install.is_none());
        // Installed meanwhile: found once the user asks to check again.
        r.host.with(|s| s.found.push("tool".into()));
        assert_eq!(checked(&mut r, "needs")[0].state, RequirementState::Missing);
        r.core.command(Cmd::CheckRequirements("needs".into()));
        assert!(checked(&mut r, "needs")[0].state.met());
    }

    #[test]
    fn a_tool_is_installed_in_a_shell_terminal_with_the_packages_own_command() {
        let mut r = Rig::new("req-install");
        r.write_package("needs", NEEDS_TOOL, &[(".claude/skills/s/SKILL.md", SKILL)]);
        r.settle();
        let (project, _) = r.project("p");
        r.settle();
        let expected = match Os::CURRENT {
            Os::Macos => "brew install the-tool",
            Os::Windows => "winget install the-tool",
            Os::Linux => "apt install the-tool",
        };
        // The package changed after the user saw its command: what they
        // approved is not what would run, so nothing runs.
        r.core.command(Cmd::InstallRequirement {
            project: project.clone(),
            package: "needs".into(),
            requirement: "tool".into(),
            command: "brew install something-else".into(),
        });
        assert_eq!(r.core.last_error.as_deref(), Some(crate::CHANGED));
        assert!(r.host.with(|s| s.added.is_empty()));
        r.core.command(Cmd::InstallRequirement {
            project: project.clone(),
            package: "needs".into(),
            requirement: "tool".into(),
            command: expected.into(),
        });
        let (added, tools) = r.host.with(|s| (s.added.clone(), s.added_tools.clone()));
        assert_eq!(
            added,
            vec![(
                project.clone(),
                Some("Install The Tool".to_owned()),
                Some(expected.to_owned())
            )]
        );
        assert_eq!(tools, vec![ToolKind::Shell]);
        // Never for a licence tool, nor for a tool the package does not name.
        for requirement in ["paid", "other"] {
            r.core.last_error = None;
            r.core.command(Cmd::InstallRequirement {
                project: project.clone(),
                package: "needs".into(),
                requirement: requirement.into(),
                command: expected.into(),
            });
            assert!(r.core.last_error.is_some(), "{requirement}");
        }
        assert_eq!(r.host.with(|s| s.added.len()), 1);
    }

    #[test]
    fn game_dev_is_built_in_needs_its_tools_and_installs_its_skills() {
        let mut r = Rig::new("req-gamedev");
        let (project, path) = r.project("game");
        r.settle();
        let item = r
            .view()
            .packages
            .iter()
            .find(|p| p.id == GAME_DEV_ID)
            .cloned()
            .unwrap();
        assert!(item.builtin && !item.is_default && item.errors.is_empty());
        assert_eq!(
            item.requires
                .iter()
                .map(|q| q.id.as_str())
                .collect::<Vec<_>>(),
            vec!["godot", "blender", "ffmpeg"]
        );
        // Like the other built-in package, it is not the user's to change.
        r.core.last_error = None;
        r.core.command(Cmd::Delete(GAME_DEV_ID.into()));
        assert!(r.core.last_error.is_some());
        // A folder of that name in Connecto Home is not read.
        r.write_package(
            GAME_DEV_ID,
            r#"{"name":"Mine"}"#,
            &[(".claude/skills/s/SKILL.md", SKILL)],
        );
        r.settle();
        assert_eq!(
            r.view()
                .packages
                .iter()
                .filter(|p| p.id == GAME_DEV_ID)
                .count(),
            1
        );
        let game = ProjectSettings {
            use_default_package: false,
            packages: vec![GAME_DEV_ID.into()],
            ..ProjectSettings::default()
        };
        // Without Godot the engine itself refuses, whoever asked.
        r.core
            .command(Cmd::SetProject(project.clone(), game.clone()));
        r.settle();
        assert!(matches!(r.status(&project).state, InstallState::Error(_)));
        assert!(!path.join(".claude/skills/godot-game").exists());
        assert!(
            project_settings::load(&r.data, &project)
                .packages
                .is_empty(),
            "not saved"
        );
        // Godot alone is not enough: the art needs Blender and the reviews
        // FFmpeg.
        r.host.with(|s| s.found.push("godot".into()));
        r.core
            .command(Cmd::SetProject(project.clone(), game.clone()));
        r.settle();
        assert!(matches!(r.status(&project).state, InstallState::Error(_)));
        assert!(!path.join(".claude/skills/godot-game").exists());
        r.host
            .with(|s| s.found.extend(["blender".into(), "ffmpeg".into()]));
        r.core.command(Cmd::SetProject(project.clone(), game));
        r.settle();
        assert_eq!(r.status(&project).state, InstallState::Updated);
        for skill in ["game-design", "godot-game", "game-playtest"] {
            assert!(
                path.join(format!(".claude/skills/{skill}/SKILL.md"))
                    .is_file(),
                "{skill}"
            );
            assert!(
                path.join(format!(".agents/skills/{skill}/SKILL.md"))
                    .is_file(),
                "{skill}"
            );
        }
    }

    /// A project that turned Game Dev on when it needed Godot only (version 2)
    /// is updated to version 3 anyway, and told which new tools are missing
    /// until they are installed.
    #[test]
    fn an_updated_package_warns_of_the_tools_it_now_needs() {
        let mut r = Rig::new("req-upgrade");
        r.host.with(|s| s.found.push("godot".into()));
        let (project, path) = r.project("game");
        let game = ProjectSettings {
            use_default_package: false,
            packages: vec![GAME_DEV_ID.into()],
            ..ProjectSettings::default()
        };
        project_settings::save(&r.data, &project, &game).unwrap();
        r.settle();
        checked(&mut r, GAME_DEV_ID);
        let status = r.status(&project);
        assert_eq!(status.state, InstallState::Updated);
        assert!(path.join(".claude/skills/godot-game/SKILL.md").is_file());
        let missing: Vec<&String> = status
            .warnings
            .iter()
            .filter(|w| w.starts_with("game-dev needs"))
            .collect();
        assert_eq!(missing.len(), 2, "{:?}", status.warnings);
        assert!(missing[0].contains("Blender") && missing[1].contains("FFmpeg"));
        // It says how to install each one by hand, which works however the
        // package came in (chosen, the default, or extended by another).
        let game_dev = crate::package::game_dev().unwrap();
        for (warning, req) in missing.iter().zip(&game_dev.manifest.requires[1..]) {
            assert_eq!(
                **warning,
                missing_tool_warning(GAME_DEV_ID, req, Os::CURRENT)
            );
            assert!(warning.contains("Restart Connecto"), "{warning}");
            assert!(!warning.contains("remove"), "{warning}");
        }
        // Installed meanwhile and looked for again: the warnings go.
        r.host
            .with(|s| s.found.extend(["blender".into(), "ffmpeg".into()]));
        r.core.command(Cmd::CheckRequirements(GAME_DEV_ID.into()));
        checked(&mut r, GAME_DEV_ID);
        let status = r.status(&project);
        assert!(
            status
                .warnings
                .iter()
                .all(|w| !w.starts_with("game-dev needs")),
            "{:?}",
            status.warnings
        );
    }

    /// A remote Project Settings keeps 512 bytes of a line (older peers
    /// too), so every missing-tool warning of a built-in package fits, on
    /// every system, and one too long for its command says where to read
    /// how instead of showing a command cut short.
    #[test]
    fn missing_tool_warnings_fit_a_remote_line() {
        let mut packages = vec![crate::package::game_dev().unwrap()];
        for variant in [Variant::Claude, Variant::Codex, Variant::Both] {
            packages.push(crate::package::builtin(variant).unwrap());
        }
        let mut seen = 0;
        for package in &packages {
            for req in &package.manifest.requires {
                for os in Os::ALL {
                    let warning = missing_tool_warning(&package.id, req, os);
                    assert!(warning.len() <= 512, "{} bytes: {warning}", warning.len());
                    if let Some(command) = req.install_on(os)
                        && warning.contains("run in a terminal:")
                    {
                        assert!(warning.ends_with(command), "{warning}");
                    }
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, 9, "Game Dev's three tools on three systems");
        // Packages from elsewhere: manual steps only, a 500-character link,
        // a 600-character name. Each warning fits and shows nothing cut.
        let manual_only = Requirement {
            id: "tool".into(),
            name: "Tool".into(),
            manual: "Download Tool from its website and put it on your PATH.".into(),
            ..Requirement::default()
        };
        let warning = missing_tool_warning("pkg", &manual_only, Os::Linux);
        assert!(
            warning.ends_with(
                "To install it: Download Tool from its website and put it on your PATH."
            ),
            "{warning}"
        );
        let long_url = format!("https://example.com/{}", "a".repeat(480));
        let link_only = Requirement {
            manual: "x".repeat(500),
            url: Some(long_url.clone()),
            ..manual_only.clone()
        };
        let warning = missing_tool_warning("pkg", &link_only, Os::Linux);
        assert!(
            !warning.contains("https://") && !warning.contains("xxx"),
            "{warning}"
        );
        assert!(warning.ends_with("Install it by hand, then restart Connecto so it looks again."));
        let long_name = Requirement {
            name: "é".repeat(300),
            ..link_only.clone()
        };
        for req in [&manual_only, &link_only, &long_name] {
            for os in Os::ALL {
                let warning = missing_tool_warning(&"p".repeat(64), req, os);
                assert!(warning.len() <= WARNING_MAX, "{} bytes", warning.len());
            }
        }
        // Linux's FFmpeg command is too long to show whole: a pointer instead.
        let ffmpeg = &packages[0].manifest.requires[2];
        let linux = missing_tool_warning(GAME_DEV_ID, ffmpeg, Os::Linux);
        assert!(
            !linux.contains("sudo") && linux.contains("https://ffmpeg.org/download.html"),
            "{linux}"
        );
        assert!(
            missing_tool_warning(GAME_DEV_ID, ffmpeg, Os::Macos).ends_with("brew install ffmpeg")
        );
    }

    /// The facade, as the window and a remote viewer use it.
    fn facade(r: &Rig) -> crate::Packages {
        crate::Packages::with(Config {
            data_root: r.data.clone(),
            user_home: r.base.join("user"),
            host: Arc::clone(&r.host) as Arc<dyn Host>,
            wake: Arc::new(|| {}),
            detector: Some(fake_detector(&r.host)),
        })
        .unwrap()
    }

    fn wait_view(packages: &crate::Packages, done: impl Fn(&PackagesView) -> bool) {
        use connecto_core::PackagesControl as _;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&packages.view()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_package_whose_tool_was_refused_cannot_be_enabled() {
        use connecto_core::PackagesControl as _;
        let mut r = Rig::new("req-refused");
        r.write_package("needs", NEEDS_TOOL, &[(".claude/skills/s/SKILL.md", SKILL)]);
        let (project, path) = r.project("game");
        let packages = facade(&r);
        let tool_is = |met: bool| {
            let project = project.clone();
            move |v: &PackagesView| {
                v.project(&project).is_some()
                    && v.packages.iter().any(|p| {
                        p.id == "needs"
                            && p.requires[0].state.met() == met
                            && p.requires[0].state != RequirementState::Checking
                    })
            }
        };
        wait_view(&packages, tool_is(false));
        let add = ProjectSettings {
            use_default_package: false,
            packages: vec!["needs".into()],
            ..ProjectSettings::default()
        };
        // The user did not install the tool (or refused to): not enabled.
        let why = packages
            .set_project_settings(&project, add.clone())
            .unwrap_err();
        assert!(why.contains("The Tool"), "{why}");
        let why = packages.set_default_package(Some("needs")).unwrap_err();
        assert!(why.contains("The Tool"), "{why}");
        std::thread::sleep(Duration::from_millis(300));
        let view = packages.view();
        assert!(view.project(&project).unwrap().settings.packages.is_empty());
        assert!(!path.join(".claude/skills/s").exists());
        // Installed, then checked again: now it can be added.
        r.host
            .with(|s| s.found.extend(["tool".to_owned(), "paid".to_owned()]));
        packages.check_requirements("needs").unwrap();
        wait_view(&packages, tool_is(true));
        // Found earlier, gone by the time it is added: the view still says
        // found, but the engine looks again and refuses.
        r.host.with(|s| s.found.clear());
        packages
            .set_project_settings(&project, add.clone())
            .unwrap();
        wait_view(&packages, |v| {
            v.project(&project)
                .is_some_and(|p| matches!(p.status.state, InstallState::Error(_)))
        });
        assert!(
            packages
                .view()
                .project(&project)
                .unwrap()
                .settings
                .packages
                .is_empty()
        );
        r.host
            .with(|s| s.found.extend(["tool".to_owned(), "paid".to_owned()]));
        // The engine's look also updated the view: check again first.
        packages.check_requirements("needs").unwrap();
        wait_view(&packages, tool_is(true));
        packages.set_project_settings(&project, add).unwrap();
        wait_view(&packages, |v| {
            v.project(&project)
                .is_some_and(|p| p.settings.packages == ["needs"])
        });
        assert_eq!(
            packages.view().project(&project).unwrap().settings.packages,
            ["needs"]
        );
        // A tool with no command here is refused before it reaches the engine.
        assert!(
            packages
                .install_requirement(&project, "needs", "paid", "x")
                .is_err()
        );
        packages.stop();
    }

    #[cfg(unix)]
    #[test]
    fn a_package_cannot_make_detection_run_a_program_it_brought_along() {
        use std::os::unix::fs::PermissionsExt as _;
        let r = Rig::new("req-own-probe");
        let folder = r.home.join("packages/evil");
        let probe = folder.join(".claude/probe");
        let marker = r.base.join("ran");
        let manifest = format!(
            r#"{{"name":"Evil","requires":[{{"id":"t","name":"T","manual":"m",
                "detect":{{"paths":{{"macos":["{p}"],"linux":["{p}"]}},
                           "version":{{"flag":"--version","min":"1.0"}}}}}}]}}"#,
            p = probe.display()
        );
        r.write_package(
            "evil",
            &manifest,
            &[
                (".claude/skills/s/SKILL.md", SKILL),
                (
                    ".claude/probe",
                    &format!("#!/bin/sh\ntouch '{}'\necho 9.9\n", marker.display()),
                ),
            ],
        );
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        // The real search, as the app uses it.
        let mut core = Core::new(Config {
            data_root: r.data.clone(),
            user_home: r.base.join("user"),
            host: Arc::clone(&r.host) as Arc<dyn Host>,
            wake: Arc::new(|| {}),
            detector: None,
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let state = loop {
            core.tick(Instant::now());
            core.publish();
            let view = core.view().load_full();
            let state = view
                .packages
                .iter()
                .find(|p| p.id == "evil")
                .map(|p| p.requires[0].state.clone());
            if state
                .as_ref()
                .is_some_and(|s| *s != RequirementState::Checking)
                || Instant::now() > deadline
            {
                break state;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(state, Some(RequirementState::Missing));
        assert!(!marker.exists(), "the package's own program was run");
    }

    #[test]
    fn a_default_whose_tool_went_away_is_refused_and_the_old_default_stays() {
        let mut r = Rig::new("req-default");
        r.write_package("needs", NEEDS_TOOL, &[(".claude/skills/s/SKILL.md", SKILL)]);
        r.host
            .with(|s| s.found.extend(["tool".to_owned(), "paid".to_owned()]));
        r.settle();
        let item = checked(&mut r, "needs");
        assert!(item.iter().all(|q| q.state.met()), "found at first");
        // Gone before it is made the default; the view still says found.
        r.host.with(|s| s.found.clear());
        r.core.command(Cmd::SetDefault(Some("needs".into())));
        assert!(
            r.core
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("The Tool"))
        );
        assert_eq!(r.view().default_package.as_deref(), Some(BUILTIN_ID));
    }
}
