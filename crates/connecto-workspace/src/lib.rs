//! The workspace: the open projects, each with its own relay and terminal
//! registry, and the one Pause switch over all of them (project-tree-ui R1, R2,
//! R12).
//!
//! Every [`WorkspaceControl`] method returns promptly. The disk work (reading
//! and writing `workspace.json`, starting a project's relay, tearing one down)
//! happens on a worker thread; the UI reads a view that thread publishes.
//! Switching the shown project changes the view and nothing else: it never
//! starts, stops or types into anything.

mod platform;
mod saved;
mod settings;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use connecto_audit::Audit;
use connecto_core::{
    MAX_NAME_CHARS, PauseSwitch, ProjectView, RecentProject, UtilitySettings, WorkspaceControl,
    WorkspaceView,
};
use connecto_session::launch::{LaunchExtras, Layout, project_id, validated};
use connecto_session::{Relay, RelayConfig, SIGKILL_GRACE, Wake, wait_for_sweeps};
use connecto_store::Tool;
use crossbeam_channel::{Receiver, Sender, bounded};

use saved::{Disk, Loaded, MAX_RECENTS, Saved, SavedProject};

/// Commands waiting for the worker.
const QUEUE: usize = 256;
/// Extra time for the kernel to reap children after SIGKILL.
const REAP_MARGIN: Duration = Duration::from_secs(1);
const AUDIT_FLUSH: Duration = Duration::from_secs(2);

/// What starts a workspace.
pub struct WorkspaceConfig {
    pub layout: Layout,
    pub extras: LaunchExtras,
    /// The one audit log. Every project's records go to it, each naming its
    /// project (A5).
    pub audit: Audit,
    pub wake: Wake,
    /// One terminal of each tool, running, when a project has no saved list.
    pub default_tools: Vec<Tool>,
}

/// What the worker and the callers share.
struct Inner {
    view: ArcSwap<WorkspaceView>,
    /// Every open project's relay, by project id. The worker writes it; the
    /// app's shutdown reads it.
    relays: Mutex<Vec<(String, Arc<Relay>)>>,
    pause: Arc<PauseSwitch>,
    /// The utility model and effort, read by every project's relay.
    utility: Arc<ArcSwap<UtilitySettings>>,
    /// How each tool is launched. Mirrored into the launcher, which every way
    /// of starting an agent goes through.
    launch_modes: Arc<ArcSwap<connecto_core::LaunchModes>>,
    /// Secrets in the system store (the saved setting; used from the next
    /// launch).
    secrets_in_system_store: AtomicBool,
    /// The Automations folder (the saved setting).
    automations_root: Mutex<Option<PathBuf>>,
    /// Connecto Home (the saved setting).
    connecto_home: Mutex<Option<PathBuf>>,
    /// The settings file could not be read, so it is never written this run.
    settings_read_only: bool,
    closing: AtomicBool,
    /// Teardowns of closed projects still running.
    tearing: AtomicUsize,
    /// Relays being started right now. A shutdown waits for them, so none is
    /// installed after it has looked at the open ones.
    setups: AtomicUsize,
}

impl Inner {
    fn root(&self) -> Option<PathBuf> {
        self.automations_root
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn home(&self) -> Option<PathBuf> {
        self.connecto_home
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The one folder that may be a project although it would be refused
    /// otherwise: Connecto Home (which holds the Automations folder), or, for
    /// an install from before Home, the Automations folder itself.
    fn allowed(&self) -> Option<PathBuf> {
        self.home().or_else(|| self.root())
    }

    fn relays(&self) -> Vec<Arc<Relay>> {
        self.relays
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, relay)| Arc::clone(relay))
            .collect()
    }
}

enum Cmd {
    /// A folder, as the user gave it.
    Open(PathBuf),
    Activate(String),
    Close(String),
    Rename(String, Option<String>),
    Move(String, usize),
    Expanded(String, bool),
    Collapsed(bool),
    ActiveTerminal(String, String),
    ClearRecents,
    /// Write the utility settings the caller already published.
    SaveSettings,
}

/// The workspace the UI and the app hold.
pub struct Workspace {
    inner: Arc<Inner>,
    tx: Sender<Cmd>,
    /// Where this workspace keeps its state: the key of its registration.
    data_root: PathBuf,
}

fn busy() -> String {
    "busy: try again".to_owned()
}

/// Connecto Home as the settings file holds it. For the first-launch setup,
/// which runs before any workspace: the same file and the same reader as the
/// Settings screens.
#[must_use]
pub fn saved_connecto_home(data_root: &std::path::Path) -> Option<PathBuf> {
    match settings::Disk::new(data_root).load() {
        settings::Loaded::Read(saved) => saved.connecto_home,
        _ => None,
    }
}

/// Save Connecto Home (and the automations folder inside it) in the settings
/// file, as choosing it in Settings does. The caller has checked the folder.
///
/// # Errors
/// When the settings file cannot be read or written.
pub fn save_connecto_home(
    data_root: &std::path::Path,
    home: &std::path::Path,
) -> Result<(), String> {
    let disk = settings::Disk::new(data_root);
    let mut saved = match disk.load() {
        settings::Loaded::Read(saved) => saved,
        settings::Loaded::Bad { warning } | settings::Loaded::ReadOnly { warning } => {
            return Err(warning);
        }
    };
    saved.connecto_home = Some(home.to_owned());
    saved.automations_root = Some(home.join("automations"));
    disk.write(&saved)
        .map_err(|err| format!("the settings could not be saved: {err}"))
}

/// Why a setting is refused when the settings file could not be read.
const SETTINGS_READ_ONLY: &str =
    "the saved settings could not be read at startup, so settings are not saved this run";

/// A name as a person gave it, or why it is refused. Blank clears it.
fn clean_name(name: &str) -> Result<Option<String>, String> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("a name is at most {MAX_NAME_CHARS} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err("a name is one line of plain text".to_owned());
    }
    Ok(Some(name.to_owned()))
}

impl Workspace {
    /// Read the saved workspace and start restoring it: every saved project
    /// shows in the sidebar at once, and its relay and terminals start on the
    /// worker thread. Returns promptly.
    pub fn start(config: WorkspaceConfig) -> std::io::Result<Self> {
        let pause = Arc::new(PauseSwitch::default());
        let data_root = config.layout.data_root.clone();
        let (saved_settings, settings_warning, settings_read_only) =
            match settings::Disk::new(&config.layout.data_root).load() {
                settings::Loaded::Read(settings) => (settings, None, false),
                settings::Loaded::Bad { warning } => {
                    (settings::Settings::default(), Some(warning), false)
                }
                settings::Loaded::ReadOnly { warning } => {
                    (settings::Settings::default(), Some(warning), true)
                }
            };
        // Before any project is restored: the folder may be one that is
        // otherwise refused.
        // This workspace's own registration, from what it saved: none clears
        // any older one for the same data folder.
        connecto_session::project_folder::allow_root(
            &config.layout.data_root,
            saved_settings
                .connecto_home
                .clone()
                .or_else(|| saved_settings.automations_root.clone()),
        );
        // Before any terminal is restored, so the relaunch at app start
        // honours the saved choice.
        connecto_session::launch::set_launch_modes(saved_settings.launch_modes);
        let inner = Arc::new(Inner {
            view: ArcSwap::from_pointee(WorkspaceView::default()),
            relays: Mutex::new(Vec::new()),
            pause,
            utility: Arc::new(ArcSwap::from_pointee(saved_settings.utility)),
            launch_modes: Arc::new(ArcSwap::from_pointee(saved_settings.launch_modes)),
            secrets_in_system_store: AtomicBool::new(saved_settings.secrets_in_system_store),
            automations_root: Mutex::new(saved_settings.automations_root),
            connecto_home: Mutex::new(saved_settings.connecto_home),
            settings_read_only,
            closing: AtomicBool::new(false),
            tearing: AtomicUsize::new(0),
            setups: AtomicUsize::new(0),
        });
        let disk = Disk::new(&config.layout.data_root);
        let (saved, warning, read_only) = match disk.load() {
            Loaded::Read(saved) => (saved, None, false),
            Loaded::Bad { warning } => (Saved::default(), Some(warning), false),
            Loaded::ReadOnly { warning } => (Saved::default(), Some(warning), true),
        };
        // Both files may be in trouble: the user is told about each.
        let warning = match (warning, settings_warning) {
            (Some(workspace), Some(settings)) => Some(format!("{workspace} {settings}")),
            (workspace, settings) => workspace.or(settings),
        };
        let mut state =
            State::restored(saved, Arc::clone(&inner), config, disk, warning, read_only);
        state.publish();
        let (tx, rx) = bounded(QUEUE);
        thread::Builder::new()
            .name("workspace".into())
            .spawn(move || state.run(&rx))?;
        Ok(Self {
            inner,
            tx,
            data_root,
        })
    }

    fn send(&self, command: Cmd) -> Result<(), String> {
        self.tx.try_send(command).map_err(|_| busy())
    }

    fn known(&self, project: &str) -> Result<(), String> {
        let view = self.inner.view.load();
        if view.projects.iter().any(|open| open.id == project) {
            Ok(())
        } else {
            Err("no such project".to_owned())
        }
    }

    /// Say the app is closing, before anything is stopped: the registries do
    /// not save the exits that follow, no new project opens, and agents are
    /// paused. Safe to repeat.
    pub fn begin_shutdown(&self) {
        self.inner.closing.store(true, Ordering::SeqCst);
        for relay in self.inner.relays() {
            relay.begin_shutdown();
        }
        self.inner.pause.pause();
    }

    /// Every pane of every open project.
    #[must_use]
    pub fn panes(&self) -> Vec<Arc<connecto_session::ManagedPane>> {
        self.inner
            .relays()
            .iter()
            .flat_map(|relay| relay.panes())
            .collect()
    }

    /// Pause, stop the actors of every project, and wait for them and then the
    /// audit log, each up to `timeout`. False if any did not finish. Call
    /// [`Workspace::begin_shutdown`] first and stop the terminals before this.
    pub fn shut_down(&self, timeout: Duration) -> bool {
        self.begin_shutdown();
        let mut clean = true;
        // A relay still being started is installed, then stopped by its own
        // setup (it sees the shutdown), before the open ones are looked at.
        let deadline = Instant::now() + timeout;
        while self.inner.setups.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                clean = false;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        for relay in self.inner.relays() {
            clean &= relay.shut_down(timeout);
        }
        // Closed projects still being torn down hold the app up too.
        while self.inner.tearing.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                clean = false;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // Dropping the relays removes their bridge sockets.
        self.inner
            .relays
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        clean
    }

    /// The saved Automations folder.
    #[must_use]
    pub fn automations_root(&self) -> Option<PathBuf> {
        self.inner.root()
    }

    /// Save the Automations folder, and allow it to be a project. The caller
    /// has checked the folder.
    pub fn set_automations_root(&self, root: Option<PathBuf>) -> Result<(), String> {
        self.set_folder(&self.inner.automations_root, root)
    }

    /// Put `value` in one of the saved folder settings and queue the save. A
    /// refusal leaves the setting as it was: the caller undoes its own work
    /// then, and a value left behind would be saved by the next change.
    fn set_folder(
        &self,
        slot: &Mutex<Option<PathBuf>>,
        value: Option<PathBuf>,
    ) -> Result<(), String> {
        if self.inner.settings_read_only {
            return Err(SETTINGS_READ_ONLY.to_owned());
        }
        {
            // Held over the send, so the worker saves the new value and
            // nobody reads one that is about to be taken back.
            let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
            let previous = std::mem::replace(&mut *slot, value);
            if let Err(why) = self.send(Cmd::SaveSettings) {
                *slot = previous;
                return Err(why);
            }
        }
        connecto_session::project_folder::allow_root(&self.data_root, self.inner.allowed());
        Ok(())
    }

    /// The saved Connecto Home.
    #[must_use]
    pub fn connecto_home(&self) -> Option<PathBuf> {
        self.inner.home()
    }

    /// Save Connecto Home, and allow it (with everything in it) to be a
    /// project. The caller has checked the folder.
    pub fn set_connecto_home(&self, home: Option<PathBuf>) -> Result<(), String> {
        self.set_folder(&self.inner.connecto_home, home)
    }

    /// A project's relay, by id. For tests.
    #[doc(hidden)]
    #[must_use]
    pub fn relay(&self, project: &str) -> Option<Arc<Relay>> {
        self.inner
            .relays
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(id, _)| id == project)
            .map(|(_, relay)| Arc::clone(relay))
    }
}

impl WorkspaceControl for Workspace {
    fn view(&self) -> Arc<WorkspaceView> {
        self.inner.view.load_full()
    }

    fn open_project(&self, path: &Path) -> Result<(), String> {
        if self.inner.closing.load(Ordering::SeqCst) {
            return Err("Connecto is closing".to_owned());
        }
        // Checked on the worker: a slow or network folder must not hold the
        // window. A refusal shows in the view's `last_error`.
        self.send(Cmd::Open(path.to_owned()))
    }

    fn activate(&self, project: &str) {
        let _ = self.send(Cmd::Activate(project.to_owned()));
    }

    fn close_project(&self, project: &str) -> Result<(), String> {
        self.known(project)?;
        self.send(Cmd::Close(project.to_owned()))
    }

    fn rename_project(&self, project: &str, name: Option<String>) -> Result<(), String> {
        let name = name.map_or(Ok(None), |name| clean_name(&name))?;
        self.known(project)?;
        self.send(Cmd::Rename(project.to_owned(), name))
    }

    fn move_project(&self, project: &str, index: usize) {
        let _ = self.send(Cmd::Move(project.to_owned(), index));
    }

    fn set_expanded(&self, project: &str, expanded: bool) {
        let _ = self.send(Cmd::Expanded(project.to_owned(), expanded));
    }

    fn set_sidebar_collapsed(&self, collapsed: bool) {
        let _ = self.send(Cmd::Collapsed(collapsed));
    }

    fn set_active_terminal(&self, project: &str, terminal: &str) {
        let _ = self.send(Cmd::ActiveTerminal(project.to_owned(), terminal.to_owned()));
    }

    fn clear_recents(&self) {
        let _ = self.send(Cmd::ClearRecents);
    }

    fn utility_settings(&self) -> UtilitySettings {
        **self.inner.utility.load()
    }

    fn set_utility_settings(&self, settings: UtilitySettings) {
        self.inner.utility.store(Arc::new(settings));
        let _ = self.send(Cmd::SaveSettings);
    }

    fn launch_modes(&self) -> connecto_core::LaunchModes {
        **self.inner.launch_modes.load()
    }

    fn set_launch_modes(&self, modes: connecto_core::LaunchModes) {
        self.inner.launch_modes.store(Arc::new(modes));
        // The launcher reads it for the next start or restart; a running
        // terminal keeps what it was launched with.
        connecto_session::launch::set_launch_modes(modes);
        let _ = self.send(Cmd::SaveSettings);
    }

    fn secrets_in_system_store(&self) -> bool {
        self.inner.secrets_in_system_store.load(Ordering::SeqCst)
    }

    fn set_secrets_in_system_store(&self, on: bool) {
        self.inner
            .secrets_in_system_store
            .store(on, Ordering::SeqCst);
        let _ = self.send(Cmd::SaveSettings);
    }

    fn pause_switch(&self) -> Arc<PauseSwitch> {
        Arc::clone(&self.inner.pause)
    }

    /// Pause first, so nothing an agent does can start a terminal behind the
    /// stop, then stop every terminal of every project.
    fn stop_everything(&self) {
        self.inner.pause.pause();
        for relay in self.inner.relays() {
            relay.stop_terminals();
        }
    }
}

/// One open project, as the worker keeps it.
struct Project {
    id: String,
    path: PathBuf,
    /// The user's name for it.
    custom: Option<String>,
    relay: Option<Arc<Relay>>,
    error: Option<String>,
    expanded: bool,
    active_terminal: Option<String>,
}

impl Project {
    fn folder(&self) -> String {
        self.path.file_name().map_or_else(
            || platform::shown(&self.path),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    fn parent_folder(&self) -> Option<String> {
        self.path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
    }
}

struct State {
    inner: Arc<Inner>,
    layout: Layout,
    extras: LaunchExtras,
    audit: Audit,
    wake: Wake,
    default_tools: Vec<Tool>,
    disk: Disk,
    settings_disk: settings::Disk,
    projects: Vec<Project>,
    /// The project shown, by id.
    active: Option<String>,
    recents: Vec<RecentProject>,
    sidebar_collapsed: bool,
    last_error: Option<String>,
    warning: Option<String>,
    /// The saved file could not be read, so it is never written this run.
    read_only: bool,
}

impl State {
    fn restored(
        saved: Saved,
        inner: Arc<Inner>,
        config: WorkspaceConfig,
        disk: Disk,
        warning: Option<String>,
        read_only: bool,
    ) -> Self {
        let projects: Vec<Project> = saved
            .projects
            .iter()
            .map(|project| {
                let path = PathBuf::from(&project.path);
                Project {
                    id: project_id(&path),
                    path,
                    custom: project.name.clone(),
                    relay: None,
                    error: None,
                    expanded: project.expanded,
                    active_terminal: project.active_terminal.clone(),
                }
            })
            .collect();
        let active = saved
            .active
            .as_deref()
            .map(|path| project_id(Path::new(path)))
            .filter(|id| projects.iter().any(|project| project.id == *id))
            .or_else(|| projects.first().map(|project| project.id.clone()));
        let settings_disk = settings::Disk::new(&config.layout.data_root);
        Self {
            inner,
            layout: config.layout,
            extras: config.extras,
            audit: config.audit,
            wake: config.wake,
            default_tools: config.default_tools,
            settings_disk,
            disk,
            projects,
            active,
            recents: saved.recents,
            sidebar_collapsed: saved.sidebar_collapsed,
            last_error: None,
            warning,
            read_only,
        }
    }

    fn run(&mut self, rx: &Receiver<Cmd>) {
        // Restore: every saved project starts, in the sidebar's order.
        self.dedupe();
        let mut index = 0;
        while index < self.projects.len() {
            if self.inner.closing.load(Ordering::SeqCst) {
                break;
            }
            let before = self.projects.len();
            self.setup(index);
            // A setup may take its project out (two paths, one folder): the
            // next one then sits at the same index.
            if self.projects.len() == before {
                index += 1;
            }
            self.publish();
        }
        for command in rx {
            if self.inner.closing.load(Ordering::SeqCst) {
                continue;
            }
            self.last_error = None;
            self.handle(command);
            self.publish();
        }
    }

    /// Two saved paths to one folder are one project: the first stays. Ids
    /// follow the real path, and the shown project follows its id.
    fn dedupe(&mut self) {
        let mut seen: Vec<String> = Vec::new();
        let mut renamed: Vec<(String, String)> = Vec::new();
        self.projects.retain_mut(|project| {
            let real =
                std::fs::canonicalize(&project.path).unwrap_or_else(|_| project.path.clone());
            let id = project_id(&real);
            if seen.contains(&id) {
                return false;
            }
            seen.push(id.clone());
            if id != project.id {
                renamed.push((std::mem::replace(&mut project.id, id), project.id.clone()));
                project.path = real;
            }
            true
        });
        if let Some(active) = &mut self.active
            && let Some((_, new)) = renamed.iter().find(|(old, _)| old == active)
        {
            active.clone_from(new);
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| !self.projects.iter().any(|project| project.id == *active))
        {
            self.active = self.projects.first().map(|project| project.id.clone());
        }
    }

    fn handle(&mut self, command: Cmd) {
        match command {
            Cmd::Open(path) => self.open(path),
            Cmd::Activate(id) => {
                if self.projects.iter().any(|project| project.id == id) {
                    self.active = Some(id);
                    self.save();
                }
            }
            Cmd::Close(id) => self.close(&id),
            Cmd::Rename(id, name) => {
                if let Some(project) = self.projects.iter_mut().find(|project| project.id == id) {
                    project.custom = name;
                    self.refresh_recent_names();
                    self.save();
                }
            }
            Cmd::Move(id, index) => {
                if let Some(from) = self.projects.iter().position(|project| project.id == id) {
                    let project = self.projects.remove(from);
                    self.projects
                        .insert(index.min(self.projects.len()), project);
                    self.save();
                }
            }
            Cmd::Expanded(id, expanded) => {
                if let Some(project) = self.projects.iter_mut().find(|project| project.id == id) {
                    project.expanded = expanded;
                    self.save();
                }
            }
            Cmd::Collapsed(collapsed) => {
                self.sidebar_collapsed = collapsed;
                self.save();
            }
            Cmd::ActiveTerminal(id, terminal) => {
                if let Some(project) = self.projects.iter_mut().find(|project| project.id == id)
                    && project.active_terminal.as_deref() != Some(terminal.as_str())
                {
                    project.active_terminal = Some(terminal);
                    self.save();
                }
            }
            Cmd::ClearRecents => {
                self.recents.clear();
                self.save();
            }
            Cmd::SaveSettings if self.inner.settings_read_only => {
                self.last_error = Some(format!(
                    "The change is for this run only: {SETTINGS_READ_ONLY}."
                ));
                self.publish();
            }
            Cmd::SaveSettings => {
                // The latest value, so a burst of changes writes the last one.
                let current = settings::Settings {
                    utility: **self.inner.utility.load(),
                    automations_root: self.inner.root(),
                    connecto_home: self.inner.home(),
                    launch_modes: **self.inner.launch_modes.load(),
                    secrets_in_system_store: self
                        .inner
                        .secrets_in_system_store
                        .load(Ordering::SeqCst),
                };
                if let Err(err) = self.settings_disk.write(&current) {
                    self.last_error = Some(format!("The settings could not be saved: {err}"));
                    self.publish();
                }
            }
        }
    }

    /// Open a folder, or show it if it is already open. A project whose setup
    /// failed earlier is tried again.
    fn open(&mut self, path: PathBuf) {
        let path = match validated(&self.layout, &path) {
            Ok(real) => real,
            Err(err) => {
                self.last_error = Some(err.to_string());
                return;
            }
        };
        let id = project_id(&path);
        let index = match self.projects.iter().position(|project| project.id == id) {
            Some(index) => {
                if self.projects[index].relay.is_none() {
                    self.projects[index].path = path;
                    self.projects[index].error = None;
                    self.setup(index);
                }
                index
            }
            None => {
                self.projects.push(Project {
                    id: id.clone(),
                    path,
                    custom: None,
                    relay: None,
                    error: None,
                    expanded: true,
                    active_terminal: None,
                });
                let index = self.projects.len() - 1;
                // In the sidebar at once, while its terminals start.
                self.publish();
                self.setup(index);
                index
            }
        };
        let Some(project) = self.projects.get(index) else {
            return;
        };
        self.active = Some(project.id.clone());
        let recent = RecentProject {
            path: platform::shown(&project.path),
            name: project.custom.clone().unwrap_or_else(|| project.folder()),
        };
        self.recents.retain(|old| old.path != recent.path);
        self.recents.insert(0, recent);
        self.recents.truncate(MAX_RECENTS);
        self.save();
    }

    /// Check the folder again and start the project's relay. A failure is kept
    /// on the project, which stays in the sidebar.
    fn setup(&mut self, index: usize) {
        let path = self.projects[index].path.clone();
        let real = match validated(&self.layout, &path) {
            Ok(real) => real,
            Err(err) => {
                self.projects[index].error = Some(err.to_string());
                return;
            }
        };
        let id = project_id(&real);
        // Before any terminal of the project can start: the packages manager
        // lets them go once it has installed or updated the project's packages.
        connecto_session::install_gate::expect(&self.layout.data_root, &id);
        if id != self.projects[index].id {
            // The folder moved under a link since it was saved.
            if self
                .projects
                .iter()
                .enumerate()
                .any(|(other, project)| other != index && project.id == id)
            {
                let gone = self.projects.remove(index);
                if self.active.as_deref() == Some(gone.id.as_str()) {
                    self.active = Some(id);
                }
                return;
            }
            if self.active.as_deref() == Some(self.projects[index].id.as_str()) {
                self.active = Some(id.clone());
            }
            self.projects[index].id.clone_from(&id);
        }
        self.projects[index].path.clone_from(&real);
        if self.inner.closing.load(Ordering::SeqCst) {
            return;
        }
        self.inner.setups.fetch_add(1, Ordering::SeqCst);
        let started = Relay::start(RelayConfig {
            layout: self.layout.clone(),
            project: Some(real),
            extras: self.extras.clone(),
            audit: self.audit.clone(),
            wake: Arc::clone(&self.wake),
            default_tools: self.default_tools.clone(),
            pause: Some(Arc::clone(&self.inner.pause)),
            utility_settings: Some(Arc::clone(&self.inner.utility)),
            auto_title: true,
        });
        match started {
            Ok(relay) => {
                let relay = Arc::new(relay);
                let late = {
                    let mut relays = self
                        .inner
                        .relays
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    // Decided under the lock, after the shutdown flag is set
                    // and before the shutdown looks at the list.
                    let late = self.inner.closing.load(Ordering::SeqCst);
                    if !late {
                        relays.push((id, Arc::clone(&relay)));
                    }
                    late
                };
                if late {
                    relay.begin_shutdown();
                    tear_down(&relay);
                } else {
                    self.projects[index].relay = Some(relay);
                    self.projects[index].error = None;
                }
            }
            Err(err) => {
                self.projects[index].error =
                    Some(format!("The project could not be started: {err}"));
            }
        }
        self.inner.setups.fetch_sub(1, Ordering::SeqCst);
    }

    /// Take a project out of the sidebar at once. Its terminals are stopped and
    /// its relay shut down on a thread of their own, so a slow process holds
    /// nothing up. Its list and history stay on disk.
    fn close(&mut self, id: &str) {
        let Some(index) = self.projects.iter().position(|project| project.id == id) else {
            return;
        };
        let project = self.projects.remove(index);
        if self.active.as_deref() == Some(id) {
            let next = index.min(self.projects.len().saturating_sub(1));
            self.active = self.projects.get(next).map(|project| project.id.clone());
        }
        self.inner
            .relays
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(open, _)| open != id);
        if let Some(relay) = project.relay {
            // Before anything is stopped, so the exits are not saved.
            relay.begin_shutdown();
            self.inner.tearing.fetch_add(1, Ordering::SeqCst);
            let inner = Arc::clone(&self.inner);
            let spawned = thread::Builder::new()
                .name("project-teardown".into())
                .spawn({
                    let relay = Arc::clone(&relay);
                    move || {
                        tear_down(&relay);
                        inner.tearing.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            if spawned.is_err() {
                self.inner.tearing.fetch_sub(1, Ordering::SeqCst);
                relay.stop_terminals();
            }
        }
        self.save();
    }

    /// The name a recent project shows follows a rename of the open one.
    fn refresh_recent_names(&mut self) {
        for project in &self.projects {
            let path = platform::shown(&project.path);
            if let Some(recent) = self.recents.iter_mut().find(|recent| recent.path == path) {
                recent.name = project.custom.clone().unwrap_or_else(|| project.folder());
            }
        }
    }

    fn save(&mut self) {
        if self.read_only {
            return;
        }
        let saved = Saved {
            projects: self
                .projects
                .iter()
                .map(|project| SavedProject {
                    path: project.path.display().to_string(),
                    name: project.custom.clone(),
                    expanded: project.expanded,
                    active_terminal: project.active_terminal.clone(),
                })
                .collect(),
            active: self
                .active
                .as_ref()
                .and_then(|id| self.projects.iter().find(|project| project.id == *id))
                .map(|project| project.path.display().to_string()),
            recents: self.recents.clone(),
            sidebar_collapsed: self.sidebar_collapsed,
        };
        if let Err(err) = self.disk.write(&saved) {
            self.last_error = Some(format!(
                "The workspace could not be saved ({err}). Open projects may not come back after a relaunch."
            ));
        }
    }

    /// The name each project shows: the user's, else the folder's, with the
    /// parent folder added when two open projects would show the same one.
    fn display_names(&self) -> Vec<String> {
        self.projects
            .iter()
            .map(|project| {
                if let Some(name) = &project.custom {
                    return name.clone();
                }
                let folder = project.folder();
                let twin = self.projects.iter().any(|other| {
                    other.id != project.id && other.custom.is_none() && other.folder() == folder
                });
                match (twin, project.parent_folder()) {
                    (true, Some(parent)) => format!("{folder} ({parent})"),
                    _ => folder,
                }
            })
            .collect()
    }

    fn publish(&self) {
        let names = self.display_names();
        let projects = self
            .projects
            .iter()
            .zip(names)
            .map(|(project, name)| ProjectView {
                id: project.id.clone(),
                name,
                path: platform::shown(&project.path),
                terminals: project.relay.as_ref().map(|relay| relay.terminals()),
                error: project.error.clone(),
                expanded: project.expanded,
                active_terminal: project.active_terminal.clone(),
            })
            .collect();
        self.inner.view.store(Arc::new(WorkspaceView {
            projects,
            active: self.active.clone(),
            recents: self.recents.clone(),
            sidebar_collapsed: self.sidebar_collapsed,
            last_error: self.last_error.clone(),
            warning: self.warning.clone(),
        }));
        (self.wake)();
    }
}

/// Stop every terminal of a closed project, wait for the processes and their
/// descendants, and shut its relay down without touching the shared Pause.
fn tear_down(relay: &Relay) {
    relay.stop_terminals();
    let panes = relay.panes();
    for pane in &panes {
        pane.close();
    }
    let deadline = Instant::now() + SIGKILL_GRACE + REAP_MARGIN;
    let remaining = || deadline.saturating_duration_since(Instant::now());
    for pane in &panes {
        if !pane.wait_exit(remaining()) {
            eprintln!("a session did not end before the project's shutdown deadline");
        }
    }
    let _ = wait_for_sweeps(remaining());
    if !relay.close(AUDIT_FLUSH) {
        eprintln!("a project's relay did not shut down cleanly");
    }
    for pane in &panes {
        pane.release();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A workspace whose worker never takes a command, with a full queue.
    fn stalled(name: &str, read_only: bool) -> (Workspace, Receiver<Cmd>) {
        let (tx, rx) = bounded(1);
        tx.try_send(Cmd::ClearRecents).unwrap();
        let inner = Arc::new(Inner {
            view: ArcSwap::from_pointee(WorkspaceView::default()),
            relays: Mutex::new(Vec::new()),
            pause: Arc::new(PauseSwitch::default()),
            utility: Arc::new(ArcSwap::from_pointee(UtilitySettings::default())),
            launch_modes: Arc::new(ArcSwap::from_pointee(connecto_core::LaunchModes::default())),
            secrets_in_system_store: AtomicBool::new(false),
            automations_root: Mutex::new(Some(PathBuf::from("/old/automations"))),
            connecto_home: Mutex::new(Some(PathBuf::from("/old/home"))),
            settings_read_only: read_only,
            closing: AtomicBool::new(false),
            tearing: AtomicUsize::new(0),
            setups: AtomicUsize::new(0),
        });
        let data_root = PathBuf::from(format!("/mg-ws-unit-{name}-{}", std::process::id()));
        connecto_session::project_folder::allow_root(&data_root, inner.allowed());
        (
            Workspace {
                inner,
                tx,
                data_root,
            },
            rx,
        )
    }

    #[test]
    fn a_folder_setting_refused_by_a_full_queue_is_left_as_it_was() {
        let (ws, rx) = stalled("busy", false);
        let registered = || connecto_session::project_folder::allowed_root(&ws.data_root);
        let err = ws
            .set_connecto_home(Some(PathBuf::from("/new/home")))
            .unwrap_err();
        assert!(err.contains("busy"), "{err}");
        assert_eq!(ws.connecto_home(), Some(PathBuf::from("/old/home")));
        assert_eq!(registered(), Some(PathBuf::from("/old/home")));
        assert!(ws.set_automations_root(None).is_err());
        assert_eq!(
            ws.automations_root(),
            Some(PathBuf::from("/old/automations"))
        );
        // With room in the queue the change is taken and the folder allowed.
        assert!(matches!(rx.try_recv(), Ok(Cmd::ClearRecents)));
        ws.set_connecto_home(Some(PathBuf::from("/new/home")))
            .unwrap();
        assert_eq!(ws.connecto_home(), Some(PathBuf::from("/new/home")));
        assert_eq!(registered(), Some(PathBuf::from("/new/home")));
        assert!(matches!(rx.try_recv(), Ok(Cmd::SaveSettings)));
        connecto_session::project_folder::allow_root(&ws.data_root, None);
    }

    #[test]
    fn folder_settings_are_refused_while_the_settings_file_is_read_only() {
        let (ws, rx) = stalled("readonly", true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::ClearRecents)));
        let err = ws
            .set_connecto_home(Some(PathBuf::from("/new/home")))
            .unwrap_err();
        assert!(err.contains("not saved this run"), "{err}");
        assert_eq!(ws.connecto_home(), Some(PathBuf::from("/old/home")));
        assert!(rx.try_recv().is_err(), "nothing was queued");
        connecto_session::project_folder::allow_root(&ws.data_root, None);
    }
}
