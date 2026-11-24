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
mod project_files;
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
    MAX_NAME_CHARS, PauseSwitch, ProjectGroup, ProjectView, RecentProject, UtilitySettings,
    WorkspaceControl, WorkspaceView,
};
use connecto_session::launch::{LaunchExtras, Layout, project_id, validated};
use connecto_session::project_folder::within_protected;
use connecto_session::{
    ProjectDirectory, ProjectEntry, Relay, RelayConfig, SIGKILL_GRACE, Wake, wait_for_sweeps,
};
use connecto_store::Tool;
use crossbeam_channel::{Receiver, Sender, bounded};

use saved::{Disk, Loaded, MAX_RECENTS, Saved, SavedProject};

/// Delete confirmations open at once whose folders are held open.
const MAX_PLANS: usize = 8;
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
    /// This launch is the new version an update's hand-over started: the
    /// projects restored now bring back every terminal that was running when
    /// the app quit, idle ones included (ADR 0030). Projects opened later
    /// restore as usual.
    pub after_update: bool,
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
    /// Confirmed deletions not yet handed to their teardown (queued, or in
    /// their checks). A shutdown waits for them, and one that finishes its
    /// checks after the shutdown began is refused.
    deleting: AtomicUsize,
    /// Relays being started right now. A shutdown waits for them, so none is
    /// installed after it has looked at the open ones.
    setups: AtomicUsize,
    /// Run by Stop Everything after the pause: what else must stop (memory
    /// runs, spec §2.4).
    stop_hooks: Mutex<Vec<StopHook>>,
    /// The projects every relay's bridge may reach with `--project` (ADR
    /// 0035): the sidebar's, open, and the recent ones, closed.
    directory: Arc<ProjectDirectory>,
    /// Where this workspace keeps its data, and the home folder: what a
    /// deletion must never reach.
    layout: Layout,
    /// Where deleted project folders go.
    bin: Mutex<project_files::Bin>,
    /// The open projects that were opened through a link somewhere in their
    /// path, by id: never deleted.
    linked: Mutex<std::collections::HashSet<String>>,
    /// Folders being deleted or cloned into, until the outcome is known.
    /// Two holds never overlap (one inside the other), and nothing inside a
    /// folder being deleted opens.
    reserved: Mutex<Vec<(PathBuf, Hold)>>,
    /// The worker's own queue, for work that ends on other threads (a
    /// deletion's outcome). Taken when the [`Workspace`] is dropped, so the
    /// worker then sees its queue close and ends.
    tx: Mutex<Option<Sender<Cmd>>>,
    /// The folders shown in open delete confirmations, held open by plan
    /// token until the deletion takes them or the dialog gives them back.
    plans: Mutex<Vec<(u64, project_files::Held)>>,
    next_plan: std::sync::atomic::AtomicU64,
    /// Clones under way: cancelled and waited for when Connecto closes.
    clones: Mutex<Vec<Arc<dyn connecto_core::project_files::ProjectTask>>>,
    /// Run while a clone is being admitted, for tests.
    clone_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Run on the worker before a deletion's checks, for tests.
    delete_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Run at the end of every teardown, for tests: it may wait, and says
    /// whether the teardown counts as stopped.
    teardown_hook: Mutex<Option<TeardownHook>>,
}

/// What a reserved folder is held for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    Delete,
    Clone,
}

/// See [`Workspace::on_teardown`].
pub type TeardownHook = Arc<dyn Fn() -> bool + Send + Sync>;

/// Something Stop Everything also stops.
pub type StopHook = Arc<dyn Fn() + Send + Sync>;

impl Inner {
    /// The folder of an open project as the person sees it, and what a
    /// deletion of it must never reach.
    fn deletion_guards(&self, project: &str) -> Result<(PathBuf, project_files::Guards), String> {
        let view = self.view.load_full();
        let open = view
            .projects
            .iter()
            .find(|open| open.id == project)
            .ok_or("no such project")?;
        let path = PathBuf::from(&open.path);
        let others = view
            .projects
            .iter()
            .filter(|other| other.id != project)
            .map(|other| PathBuf::from(&other.path))
            .chain(
                view.recents
                    .iter()
                    .map(|recent| PathBuf::from(&recent.path)),
            )
            .filter(|other| !(within_protected(other, &path) && within_protected(&path, other)))
            .collect();
        let connecto: Vec<PathBuf> = [self.home(), self.root()]
            .into_iter()
            .flatten()
            .chain(connecto_itself())
            .collect();
        let linked = self
            .linked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(project);
        let internal = [
            self.root(),
            self.home().map(|home| home.join(AGENTIC_JOBS_FOLDER)),
        ]
        .into_iter()
        .flatten()
        .collect();
        Ok((
            path,
            project_files::Guards {
                home: self.layout.home.clone(),
                data_root: self.layout.data_root.clone(),
                connecto,
                internal,
                others,
                linked,
            },
        ))
    }

    /// The confirmed folder, checked again just before the deletion: still
    /// allowed, still the directory the confirmation showed and holds open,
    /// with a usable Trash, and not being deleted already. Disk work, so on
    /// the worker, never the window's thread.
    fn confirm_delete(
        &self,
        plan: &connecto_core::project_files::DeletePlan,
        held: &project_files::Held,
    ) -> Result<project_files::Checked, String> {
        let hook = self
            .delete_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook();
        }
        let (path, guards) = self.deletion_guards(&plan.project)?;
        let checked = project_files::refusal(&path, &guards)?;
        // The confirmation was for one directory, held open since: this must
        // still be it.
        if checked.identity != held.identity
            || checked.identity[..] != plan.identity[..]
            || platform::shown(&checked.real) != plan.path
        {
            return Err(
                "The project's folder changed since it was looked at, so nothing was deleted. Ask again to see what is there now."
                    .to_owned(),
            );
        }
        self.bin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .name(&checked.real)?;
        if self.reserved_for(&checked.real).is_some() {
            return Err("The project is already being deleted.".to_owned());
        }
        Ok(checked)
    }

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

    /// The folder being deleted that `path` is in, if any.
    fn reserved_for(&self, path: &Path) -> Option<PathBuf> {
        self.reserved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(folder, hold)| *hold == Hold::Delete && within_protected(path, folder))
            .map(|(folder, _)| folder.clone())
    }

    /// Hold `folder` for a deletion or a clone, unless a hold already covers
    /// it, or it covers one. Why not, when it is refused.
    fn reserve(&self, folder: &Path, hold: Hold) -> Result<(), String> {
        let mut held = self.reserved.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((other, kind)) = held
            .iter()
            .find(|(other, _)| within_protected(other, folder) || within_protected(folder, other))
        {
            return Err(format!(
                "{} is being {} now. Try again once that has finished.",
                other.display(),
                match kind {
                    Hold::Delete => "deleted",
                    Hold::Clone => "cloned into",
                }
            ));
        }
        held.push((folder.to_owned(), hold));
        Ok(())
    }

    fn release(&self, folder: &Path) {
        self.reserved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(held, _)| held != folder);
    }

    /// The clones still running (finished ones are dropped from the list).
    fn running_clones(&self) -> Vec<Arc<dyn connecto_core::project_files::ProjectTask>> {
        let mut clones = self.clones.lock().unwrap_or_else(PoisonError::into_inner);
        clones.retain(|clone| clone.state().is_running());
        clones.clone()
    }

    fn sender(&self) -> Option<Sender<Cmd>> {
        self.tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
    NewGroup(String),
    RenameGroup(String, String),
    DeleteGroup(String),
    SetGroup(String, Option<String>),
    /// Write the utility settings the caller already published.
    SaveSettings,
    /// Close a project and move its folder (already checked) to the trash
    /// once its terminals have stopped.
    /// Its checks run here first, before anything stops.
    Delete {
        plan: connecto_core::project_files::DeletePlan,
        /// The folder and its parent, held open since the confirmation.
        held: project_files::Held,
        task: Arc<project_files::Task>,
        /// Counted in [`Inner::deleting`] until dropped: once the deletion
        /// is refused, or its teardown (counted in `tearing`) has begun.
        admitted: project_files::OnDrop,
    },
    /// A deleted project's folder is gone: out of the recents.
    Forget {
        id: String,
        path: String,
    },
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

/// The folder in Connecto Home where Agentic Works opens its setup terminals.
pub const AGENTIC_JOBS_FOLDER: &str = "agentic jobs";
/// The sidebar group Connecto puts its own projects in (the Automations
/// folder and the agentic jobs folder).
pub const INTERNAL_GROUP: &str = "Connecto Internal";

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
            deleting: AtomicUsize::new(0),
            setups: AtomicUsize::new(0),
            stop_hooks: Mutex::new(Vec::new()),
            directory: Arc::new(ProjectDirectory::default()),
            layout: config.layout.clone(),
            bin: Mutex::new(project_files::Bin::System),
            linked: Mutex::default(),
            reserved: Mutex::default(),
            tx: Mutex::default(),
            plans: Mutex::default(),
            next_plan: std::sync::atomic::AtomicU64::new(1),
            clones: Mutex::default(),
            clone_hook: Mutex::default(),
            delete_hook: Mutex::default(),
            teardown_hook: Mutex::default(),
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
        *inner.tx.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx.clone());
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

    fn known_group(&self, group: &str) -> Result<(), String> {
        if self.view().groups.iter().any(|g| g.id == group) {
            Ok(())
        } else {
            Err("no such group".to_owned())
        }
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
        // A clone under way stops, and removes its staging folder.
        for clone in self.inner.running_clones() {
            clone.cancel();
        }
    }

    /// The app quits for an update (ADR 0030): in every open project, the
    /// terminals in a turn now are saved as cut by it, so the next launch
    /// carries them on. Call it before [`Workspace::begin_shutdown`]. False
    /// when a project's list could not be saved in `wait`.
    pub fn mark_cut_by_update(&self, wait: Duration) -> bool {
        // Every project at once, so none waits on another while its agents
        // go on (a turn ending after its mark would be carried on for nothing).
        let relays = self.inner.relays();
        thread::scope(|scope| {
            let marks: Vec<_> = relays
                .iter()
                .map(|relay| scope.spawn(move || relay.mark_cut_by_update(wait)))
                .collect();
            // The scope joins any mark `all` stops looking at: every one runs.
            marks.into_iter().all(|mark| mark.join().unwrap_or(false))
        })
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
        // Clones cancelled above: git stopped and its folder cleaned up.
        while !self.inner.running_clones().is_empty() {
            if Instant::now() >= deadline {
                clean = false;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
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
        // A confirmed deletion still in its checks: refused once they end
        // (it sees `closing`), or counted in `tearing` before it lets go.
        while self.inner.deleting.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                clean = false;
                break;
            }
            thread::sleep(Duration::from_millis(20));
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

    /// Have Stop Everything also run `hook`, after the pause and before the
    /// terminals stop. It must return promptly.
    pub fn on_stop_everything(&self, hook: StopHook) {
        self.inner
            .stop_hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(hook);
    }

    /// Send deleted project folders to `bin` instead of the system's trash.
    /// For tests, which must not fill the person's Trash.
    #[doc(hidden)]
    pub fn use_trash_folder(&self, bin: PathBuf) {
        *self
            .inner
            .bin
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = project_files::Bin::Folder(bin);
    }

    /// Run `hook` at the end of every closed project's teardown; it may hold
    /// the teardown, and `false` makes it count as not stopped. For tests.
    #[doc(hidden)]
    pub fn on_teardown(&self, hook: TeardownHook) {
        *self
            .inner
            .teardown_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// Run `hook` at the start of each clone's disk work (registered, the
    /// lock released, before its folders are looked at). For tests.
    #[doc(hidden)]
    pub fn on_clone_admission(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .inner
            .clone_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// Run `hook` on the worker before each deletion's checks (a stalled
    /// volume). For tests.
    #[doc(hidden)]
    pub fn on_delete_check(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .inner
            .delete_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// How many delete confirmations hold their folder open now. For tests.
    #[doc(hidden)]
    #[must_use]
    pub fn held_plans(&self) -> usize {
        self.inner
            .plans
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    fn bin(&self) -> project_files::Bin {
        self.inner
            .bin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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

impl Drop for Workspace {
    /// The worker's own sender goes, so its queue closes once nothing else
    /// holds one and the worker ends.
    fn drop(&mut self) {
        self.inner
            .tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
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

    fn clone_project(
        &self,
        url: &str,
        parent: &Path,
    ) -> Result<connecto_core::project_files::Cloning, String> {
        use connecto_core::project_files::TaskState;
        // The address alone, at once (no disk work): a bad one is refused
        // here, the rest is said by the task.
        let name = project_files::repo_name(url)?;
        // Registered under the registry's lock, with no disk work in it: a
        // shutdown sets `closing` before it takes the lock, so a clone is
        // either refused, or registered before the shutdown looks (and then
        // cancelled and waited for, within the shutdown's time limit).
        let task = project_files::Task::new("Checking the folder…", true);
        {
            let mut clones = self
                .inner
                .clones
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if self.inner.closing.load(Ordering::SeqCst) {
                return Err("Connecto is closing".to_owned());
            }
            clones.retain(|clone| clone.state().is_running());
            clones.push(Arc::clone(&task) as Arc<dyn connecto_core::project_files::ProjectTask>);
        }
        // The disk work (a slow or stalled volume included) runs on a thread
        // of its own: the window gets the task at once, keeps drawing, and
        // its Cancel reaches the task whatever the volume does.
        let spawned = thread::Builder::new()
            .name("clone-admission".into())
            .spawn({
                let inner = Arc::clone(&self.inner);
                let (url, parent, task) = (url.to_owned(), parent.to_owned(), Arc::clone(&task));
                move || {
                    if let Err(why) = admit_clone(&inner, &url, &parent, &task) {
                        task.set(if task.cancelled() {
                            TaskState::Cancelled
                        } else {
                            TaskState::Failed(why)
                        });
                    }
                }
            });
        if let Err(err) = spawned {
            task.set(TaskState::Failed(format!(
                "The clone could not start: {err}"
            )));
            return Err(format!("The clone could not start: {err}"));
        }
        Ok(connecto_core::project_files::Cloning {
            target: parent.join(name),
            task,
        })
    }

    fn plan_delete(
        &self,
        project: &str,
    ) -> Result<connecto_core::project_files::DeletePlan, String> {
        let (path, guards) = self.inner.deletion_guards(project)?;
        let checked = project_files::refusal(&path, &guards)?;
        // Held open from now: its identity cannot pass to another folder
        // while the confirmation shows it.
        let held = project_files::Held::open(&checked.real)
            .filter(|held| held.identity == checked.identity)
            .ok_or("The project's folder changed while it was being looked at. Ask again.")?;
        let token = self.inner.next_plan.fetch_add(1, Ordering::SeqCst);
        let plan = project_files::plan(project, &checked, &self.bin(), token)?;
        let mut plans = self
            .inner
            .plans
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A few confirmations at most; the oldest lets go first.
        if plans.len() >= MAX_PLANS {
            plans.remove(0);
        }
        plans.push((token, held));
        Ok(plan)
    }

    fn release_delete_plan(&self, plan: &connecto_core::project_files::DeletePlan) {
        self.inner
            .plans
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(token, _)| *token != plan.token);
    }

    fn delete_project(
        &self,
        plan: &connecto_core::project_files::DeletePlan,
    ) -> Result<Arc<dyn connecto_core::project_files::ProjectTask>, String> {
        if self.inner.closing.load(Ordering::SeqCst) {
            return Err("Connecto is closing".to_owned());
        }
        if !self
            .view()
            .projects
            .iter()
            .any(|open| open.id == plan.project)
        {
            return Err("no such project".to_owned());
        }
        // Taken from the confirmation, still open: the worker checks the
        // folder against it. Nothing here touches the disk, so a stalled
        // volume never holds the window.
        let held = {
            let mut plans = self
                .inner
                .plans
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let at = plans
                .iter()
                .position(|(token, _)| *token == plan.token)
                .ok_or("This confirmation has expired. Ask again to see what is there now.")?;
            plans.remove(at).1
        };
        let task = project_files::Task::new("Checking the folder…", false);
        // Counted before it is queued: a shutdown from now on waits for it.
        self.inner.deleting.fetch_add(1, Ordering::SeqCst);
        let admitted = project_files::OnDrop::new({
            let inner = Arc::clone(&self.inner);
            move || {
                inner.deleting.fetch_sub(1, Ordering::SeqCst);
            }
        });
        let sent = self.send(Cmd::Delete {
            plan: plan.clone(),
            held,
            task: Arc::clone(&task),
            admitted,
        });
        sent?;
        Ok(task)
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

    fn create_group(&self, name: &str) -> Result<(), String> {
        let name = clean_name(name)?.ok_or("a group needs a name")?;
        if self.view().groups.len() >= saved::MAX_GROUPS {
            return Err(format!("at most {} groups", saved::MAX_GROUPS));
        }
        self.send(Cmd::NewGroup(name))
    }

    fn rename_group(&self, group: &str, name: &str) -> Result<(), String> {
        let name = clean_name(name)?.ok_or("a group needs a name")?;
        self.known_group(group)?;
        self.send(Cmd::RenameGroup(group.to_owned(), name))
    }

    fn delete_group(&self, group: &str) -> Result<(), String> {
        self.known_group(group)?;
        self.send(Cmd::DeleteGroup(group.to_owned()))
    }

    fn set_project_group(&self, project: &str, group: Option<&str>) -> Result<(), String> {
        self.known(project)?;
        if let Some(group) = group {
            self.known_group(group)?;
        }
        self.send(Cmd::SetGroup(project.to_owned(), group.map(str::to_owned)))
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
        let hooks = self
            .inner
            .stop_hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for hook in hooks {
            hook();
        }
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
    group: Option<String>,
    /// Opened through a link somewhere in its path: never deleted.
    linked: bool,
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
    /// Set while the projects saved at the update's quit are restored, then
    /// dropped: see [`WorkspaceConfig::after_update`].
    after_update: bool,
    disk: Disk,
    settings_disk: settings::Disk,
    projects: Vec<Project>,
    /// The project shown, by id.
    active: Option<String>,
    recents: Vec<RecentProject>,
    sidebar_collapsed: bool,
    groups: Vec<ProjectGroup>,
    last_error: Option<String>,
    warning: Option<String>,
    /// The saved file could not be read, so it is never written this run.
    read_only: bool,
    /// Recents that were open through a link, by path.
    linked_recents: std::collections::HashSet<String>,
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
                let became_link = project_files::through_link(&path);
                Project {
                    id: project_id(&path),
                    path,
                    custom: project.name.clone(),
                    relay: None,
                    error: None,
                    expanded: project.expanded,
                    active_terminal: project.active_terminal.clone(),
                    group: project.group.clone(),
                    // A saved folder that became a link while Connecto was
                    // closed: seen before anything resolves it away.
                    linked: project.linked || became_link,
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
            after_update: config.after_update,
            settings_disk,
            disk,
            projects,
            active,
            recents: saved.recents,
            sidebar_collapsed: saved.sidebar_collapsed,
            groups: saved.groups,
            last_error: None,
            warning,
            read_only,
            linked_recents: saved.linked_recents.into_iter().collect(),
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
        // Only the projects open at the update's quit: one opened, or set up
        // again, from now on restores as usual.
        self.after_update = false;
        for command in rx {
            if self.inner.closing.load(Ordering::SeqCst) {
                // What is under way still gets an outcome.
                match command {
                    Cmd::Delete { task, .. } => {
                        task.set(connecto_core::project_files::TaskState::Failed(
                            "Connecto is closing, so nothing was deleted.".to_owned(),
                        ))
                    }
                    Cmd::Forget { id, path } => self.forget(&id, &path),
                    _ => {}
                }
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
            Cmd::Close(id) => self.close(&id, None),
            Cmd::Delete {
                plan,
                held,
                task,
                admitted,
            } => {
                use connecto_core::project_files::TaskState;
                match self.inner.confirm_delete(&plan, &held) {
                    // Still counted in `deleting`: a shutdown that began
                    // after this look waits until the teardown is counted.
                    Ok(_) if self.inner.closing.load(Ordering::SeqCst) => {
                        task.set(TaskState::Failed(
                            "Connecto is closing, so nothing was deleted.".to_owned(),
                        ))
                    }
                    Ok(checked) => {
                        task.set(TaskState::Running(
                            "Stopping the project's terminals…".to_owned(),
                        ));
                        self.delete(&plan.project, checked, held, task);
                    }
                    Err(why) => task.set(TaskState::Failed(why)),
                }
                drop(admitted);
            }
            Cmd::Forget { id, path } => self.forget(&id, &path),
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
            Cmd::NewGroup(name) => {
                if self.new_group(name).is_some() {
                    self.save();
                }
            }
            Cmd::RenameGroup(id, name) => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    group.name = name;
                    self.save();
                }
            }
            Cmd::DeleteGroup(id) => {
                self.groups.retain(|g| g.id != id);
                for project in &mut self.projects {
                    if project.group.as_deref() == Some(id.as_str()) {
                        project.group = None;
                    }
                }
                self.save();
            }
            Cmd::SetGroup(id, group) => {
                let known = group
                    .as_ref()
                    .is_none_or(|group| self.groups.iter().any(|g| g.id == *group));
                if let Some(project) = self.projects.iter_mut().find(|project| project.id == id)
                    && known
                {
                    project.group = group;
                    self.save();
                }
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
    /// Add a group; its id, or None when there are already as many as allowed.
    fn new_group(&mut self, name: String) -> Option<String> {
        if self.groups.len() >= saved::MAX_GROUPS {
            return None;
        }
        // One more than the highest in use, so an id is never given to two
        // groups that are both remembered somewhere.
        let next = self
            .groups
            .iter()
            .filter_map(|g| g.id.strip_prefix("group-")?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let id = format!("group-{next}");
        self.groups.push(ProjectGroup {
            id: id.clone(),
            name,
        });
        Some(id)
    }

    /// Connecto's own folders (Automations, agentic jobs) go in the
    /// Connecto Internal group when they have none, made if missing. A project
    /// the user put in another group stays there.
    fn place_internal(&mut self, index: usize) {
        let Some(project) = self.projects.get(index) else {
            return;
        };
        if project.group.is_some() {
            return;
        }
        let internal = [
            self.inner.root(),
            self.inner.home().map(|home| home.join(AGENTIC_JOBS_FOLDER)),
        ];
        let is_internal = internal
            .into_iter()
            .flatten()
            .filter_map(|dir| std::fs::canonicalize(dir).ok())
            .any(|dir| dir == project.path);
        if !is_internal {
            return;
        }
        let group = match self.groups.iter().find(|g| g.name == INTERNAL_GROUP) {
            Some(group) => Some(group.id.clone()),
            None => self.new_group(INTERNAL_GROUP.to_owned()),
        };
        if let (Some(group), Some(project)) = (group, self.projects.get_mut(index)) {
            project.group = Some(group);
        }
    }

    fn open(&mut self, path: PathBuf) {
        // How it was opened, before the link is resolved away.
        let linked = project_files::through_link(&path);
        let path = match validated(&self.layout, &path) {
            Ok(real) => real,
            Err(err) => {
                self.last_error = Some(err.to_string());
                return;
            }
        };
        if let Some(folder) = self.inner.reserved_for(&path) {
            self.last_error = Some(format!(
                "{} is being deleted, so it cannot be opened now.",
                folder.display()
            ));
            return;
        }
        // Opened from the recents after it was open through a link.
        let linked = linked || self.linked_recents.contains(&platform::shown(&path));
        let id = project_id(&path);
        let index = match self.projects.iter().position(|project| project.id == id) {
            Some(index) => {
                self.projects[index].linked |= linked;
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
                    group: None,
                    linked,
                });
                let index = self.projects.len() - 1;
                // In the sidebar at once, while its terminals start.
                self.publish();
                self.setup(index);
                index
            }
        };
        self.place_internal(index);
        let Some(project) = self.projects.get(index) else {
            return;
        };
        self.active = Some(project.id.clone());
        let recent = RecentProject {
            path: platform::shown(&project.path),
            name: project.custom.clone().unwrap_or_else(|| project.folder()),
        };
        if project.linked {
            self.linked_recents.insert(recent.path.clone());
        }
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
            self.projects[index].linked = true;
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
            after_update: self.after_update,
        });
        match started {
            Ok(relay) => {
                relay.join_directory(&self.inner.directory);
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

    /// Close a project, then, on the teardown's thread once its terminals,
    /// their background work and its relay have all stopped, move its folder
    /// to the trash (if it is still the folder confirmed) and remove what
    /// Connecto kept for it. Until the outcome is known the folder is
    /// reserved: nothing in it opens. On any failure the recents and
    /// Connecto's data for it stay, and the task says what happened.
    fn delete(
        &mut self,
        id: &str,
        checked: project_files::Checked,
        held: project_files::Held,
        task: Arc<project_files::Task>,
    ) {
        use connecto_core::project_files::TaskState;
        let Some(project) = self.projects.iter().find(|project| project.id == id) else {
            task.set(TaskState::Failed(
                "The project is no longer open, so nothing was deleted.".to_owned(),
            ));
            return;
        };
        // Checked again here: the project may have moved since the check.
        if project.linked
            || std::fs::canonicalize(&project.path).ok().as_ref() != Some(&checked.real)
        {
            task.set(TaskState::Failed(
                "The project's folder changed since the confirmation, so nothing was deleted."
                    .to_owned(),
            ));
            return;
        }
        let real = checked.real.clone();
        // The live list, not the view the caller saw: a project opened since
        // (queued before this) may be inside.
        if let Some(inside) = self
            .projects
            .iter()
            .find(|other| other.id != id && within_protected(&other.path, &real))
        {
            task.set(TaskState::Failed(format!(
                "{} holds another open project, {}, so nothing was deleted. Close that project first.",
                real.display(),
                inside.path.display()
            )));
            return;
        }
        if let Err(why) = self.inner.reserve(&real, Hold::Delete) {
            task.set(TaskState::Failed(format!("Nothing was deleted: {why}")));
            return;
        }
        let bin = self
            .inner
            .bin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let data = self.layout.data_root.join("projects").join(id);
        let inner = Arc::clone(&self.inner);
        let tx = self.inner.sender();
        let key = id.to_owned();
        let finish: Box<dyn FnOnce(bool) + Send> = Box::new(move |stopped| {
            let outcome = if stopped {
                task.set(TaskState::Running(
                    "Moving the folder to the Trash…".to_owned(),
                ));
                bin.remove(&real, &held)
            } else {
                Err("Its terminals or background work did not all stop, so its folder was not moved to the Trash. Delete it again once they have stopped, or remove it yourself.".to_owned())
            };
            let state = match outcome {
                Ok(()) => {
                    // Its terminal list, history and profiles.
                    project_files::remove_all(&data);
                    if let Some(tx) = tx {
                        let _ = tx.send(Cmd::Forget {
                            id: key,
                            path: platform::shown(&real),
                        });
                    }
                    TaskState::Done
                }
                Err(why) => TaskState::Failed(format!("The project was closed. {why}")),
            };
            inner.release(&real);
            task.set(state);
        });
        self.close(id, Some(finish));
    }

    /// A deleted project's folder is gone: out of the recents.
    fn forget(&mut self, id: &str, path: &str) {
        self.recents
            .retain(|recent| recent.path != path && canonical_id(Path::new(&recent.path)) != id);
        self.linked_recents.remove(path);
        self.save();
    }

    /// Take a project out of the sidebar at once. Its terminals are stopped and
    /// its relay shut down on a thread of their own, so a slow process holds
    /// nothing up. Its list and history stay on disk. `then` runs on that
    /// thread once that is over, told whether everything stopped (`false`
    /// when the thread could not start either).
    fn close(&mut self, id: &str, then: Option<Box<dyn FnOnce(bool) + Send>>) {
        let Some(index) = self.projects.iter().position(|project| project.id == id) else {
            if let Some(then) = then {
                then(false);
            }
            return;
        };
        let project = self.projects.remove(index);
        if project.linked {
            self.linked_recents.insert(platform::shown(&project.path));
        }
        if self.active.as_deref() == Some(id) {
            let next = index.min(self.projects.len().saturating_sub(1));
            self.active = self.projects.get(next).map(|project| project.id.clone());
        }
        self.inner
            .relays
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(open, _)| open != id);
        if let Some(relay) = &project.relay {
            // Before anything is stopped, so the exits are not saved.
            relay.begin_shutdown();
        }
        self.inner.tearing.fetch_add(1, Ordering::SeqCst);
        let inner = Arc::clone(&self.inner);
        let relay = project.relay.clone();
        let then = Arc::new(Mutex::new(then));
        let spawned = thread::Builder::new()
            .name("project-teardown".into())
            .spawn({
                let then = Arc::clone(&then);
                move || {
                    let mut stopped = relay.as_deref().is_none_or(tear_down);
                    let hook = inner
                        .teardown_hook
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    if let Some(hook) = hook {
                        stopped &= hook();
                    }
                    let then = then.lock().unwrap_or_else(PoisonError::into_inner).take();
                    if let Some(then) = then {
                        then(stopped);
                    }
                    inner.tearing.fetch_sub(1, Ordering::SeqCst);
                }
            });
        if spawned.is_err() {
            self.inner.tearing.fetch_sub(1, Ordering::SeqCst);
            if let Some(relay) = &project.relay {
                relay.stop_terminals();
            }
            // Nothing is removed while its terminals may still run in it.
            let then = then.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(then) = then {
                then(false);
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
                    group: project.group.clone(),
                    linked: project.linked,
                })
                .collect(),
            active: self
                .active
                .as_ref()
                .and_then(|id| self.projects.iter().find(|project| project.id == *id))
                .map(|project| project.path.display().to_string()),
            recents: self.recents.clone(),
            linked_recents: self
                .recents
                .iter()
                .filter(|recent| self.linked_recents.contains(&recent.path))
                .map(|recent| recent.path.clone())
                .collect(),
            sidebar_collapsed: self.sidebar_collapsed,
            groups: self.groups.clone(),
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
                group: project.group.clone(),
            })
            .collect();
        self.publish_directory();
        *self
            .inner
            .linked
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = self
            .projects
            .iter()
            .filter(|project| project.linked)
            .map(|project| project.id.clone())
            .collect();
        self.inner.view.store(Arc::new(WorkspaceView {
            projects,
            groups: self.groups.clone(),
            active: self.active.clone(),
            recents: self.recents.clone(),
            sidebar_collapsed: self.sidebar_collapsed,
            last_error: self.last_error.clone(),
            warning: self.warning.clone(),
        }));
        (self.wake)();
    }
}

impl State {
    /// What the bridges may reach across projects: each sidebar project by
    /// the name it shows, with its bridge while it runs, then each recent one
    /// not in the sidebar, closed.
    fn publish_directory(&self) {
        let names = self.display_names();
        let mut entries: Vec<ProjectEntry> = self
            .projects
            .iter()
            .zip(names)
            .map(|(project, name)| ProjectEntry {
                id: project.id.clone(),
                name,
                path: project.path.clone(),
                link: project.relay.as_ref().and_then(|relay| relay.bridge_link()),
            })
            .collect();
        for recent in &self.recents {
            let path = PathBuf::from(&recent.path);
            let id = canonical_id(&path);
            if entries.iter().any(|entry| entry.id == id) {
                continue;
            }
            entries.push(ProjectEntry {
                id,
                name: recent.name.clone(),
                path,
                link: None,
            });
        }
        self.inner.directory.set(entries);
    }
}

/// A project's id from the folder as shown, by its real path, as an open
/// project's is: a shown path (`C:\work\app` on Windows, or a link) and the
/// canonical one (`\\?\C:\work\app`) name the same project. A folder that is
/// gone keeps the id of the path as shown.
fn canonical_id(path: &Path) -> String {
    std::fs::canonicalize(path).map_or_else(|_| project_id(path), |real| project_id(&real))
}

/// The disk work of starting a clone, after it is registered, on its own
/// thread: its destination, its hold, its staging folder and git's thread. A
/// clone cancelled meanwhile (Cancel, or Connecto closing) stops before git
/// starts.
fn admit_clone(
    inner: &Arc<Inner>,
    url: &str,
    parent: &Path,
    task: &Arc<project_files::Task>,
) -> Result<(), String> {
    let hook = inner
        .clone_hook
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
    let target = project_files::clone_target(&inner.layout, url, parent)?;
    // Held while git runs: no deletion of a folder it is in, nor a second
    // clone to the same place. Released when the guard drops, however the
    // clone ends (a thread that never started included).
    inner.reserve(&target, Hold::Clone)?;
    let held = target.clone();
    let guard = project_files::OnDrop::new({
        let inner = Arc::clone(inner);
        move || inner.release(&held)
    });
    if task.cancelled() {
        return Err("The clone was stopped before it started.".to_owned());
    }
    project_files::start_clone(&inner.layout, url, &target, guard, Arc::clone(task)).map(|_| ())
}

/// Stop every terminal of a closed project, wait for the processes and their
/// descendants, and shut its relay down without touching the shared Pause.
/// Whether all of it stopped in time.
fn tear_down(relay: &Relay) -> bool {
    relay.stop_terminals();
    let panes = relay.panes();
    for pane in &panes {
        pane.close();
    }
    let deadline = Instant::now() + SIGKILL_GRACE + REAP_MARGIN;
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let mut stopped = true;
    for pane in &panes {
        if !pane.wait_exit(remaining()) {
            eprintln!("a session did not end before the project's shutdown deadline");
            stopped = false;
        }
    }
    stopped &= wait_for_sweeps(remaining());
    if !relay.close(AUDIT_FLUSH) {
        eprintln!("a project's relay did not shut down cleanly");
        stopped = false;
    }
    for pane in &panes {
        pane.release();
    }
    stopped
}

/// Connecto itself: the running app, and the source checkout it was built
/// from when that is on this machine. A deletion never holds either.
fn connecto_itself() -> Vec<PathBuf> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = std::fs::canonicalize(source)
        .ok()
        .filter(|root| root.join("Cargo.toml").exists());
    let app = std::env::current_exe()
        .ok()
        .and_then(|exe| std::fs::canonicalize(exe).ok());
    source.into_iter().chain(app).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    /// A recent project shown by another path to the same folder (here a
    /// link; on Windows the shown `C:\` path against the canonical one) has
    /// the id the open project has, so it is one project, not two.
    #[cfg(unix)]
    #[test]
    fn a_recent_project_has_the_id_of_its_real_folder() {
        let root = std::env::temp_dir().join(format!("mg-ws-recent-{}", std::process::id()));
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let open = project_id(&std::fs::canonicalize(&real).unwrap());
        assert_eq!(super::canonical_id(&link), open);
        assert_ne!(project_id(&link), open, "the shown path alone hashes apart");
        let gone = root.join("gone");
        assert_eq!(super::canonical_id(&gone), project_id(&gone));
        let _ = std::fs::remove_dir_all(&root);
    }

    use super::*;

    #[test]
    fn stop_everything_pauses_then_runs_its_hooks() {
        let (workspace, _rx) = stalled("stop-hooks", false);
        let seen = Arc::new(Mutex::new(Vec::new()));
        for n in 0..2 {
            let (seen, pause) = (Arc::clone(&seen), workspace.pause_switch());
            workspace.on_stop_everything(Arc::new(move || {
                seen.lock().unwrap().push((n, pause.is_paused()));
            }));
        }
        workspace.stop_everything();
        assert_eq!(*seen.lock().unwrap(), [(0, true), (1, true)]);
    }

    /// A deletion still queued when Connecto starts closing ends with a
    /// reason, never left running; a finished one's Forget still applies.
    #[test]
    fn queued_work_gets_an_outcome_while_closing() {
        use connecto_core::project_files::{ProjectTask, TaskState};
        let root = std::env::temp_dir().join(format!("mg-ws-closing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        let (workspace, _stalled) = stalled("closing", false);
        let inner = Arc::clone(&workspace.inner);
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let config = WorkspaceConfig {
            layout: layout.clone(),
            extras: LaunchExtras::default(),
            audit: Audit::open(&root.join("audit.jsonl")).unwrap(),
            wake: Arc::new(|| {}),
            default_tools: Vec::new(),
            after_update: false,
        };
        let saved = Saved {
            recents: vec![RecentProject {
                path: "/gone/app".into(),
                name: "app".into(),
            }],
            ..Saved::default()
        };
        let mut state = State::restored(
            saved,
            Arc::clone(&inner),
            config,
            Disk::new(&layout.data_root),
            None,
            false,
        );
        inner.closing.store(true, Ordering::SeqCst);
        let (tx, rx) = bounded(4);
        let task = project_files::Task::new("waiting", false);
        std::fs::create_dir_all(root.join("app")).unwrap();
        tx.send(Cmd::Delete {
            plan: connecto_core::project_files::DeletePlan {
                project: "p".into(),
                name: "app".into(),
                path: platform::shown(&root.join("app")),
                contents: String::new(),
                trash: String::new(),
                warnings: Vec::new(),
                identity: Vec::new(),
                token: 1,
            },
            held: project_files::Held::open(&root.join("app")).unwrap(),
            task: Arc::clone(&task),
            admitted: project_files::OnDrop::new(|| {}),
        })
        .unwrap();
        tx.send(Cmd::Forget {
            id: "gone".into(),
            path: "/gone/app".into(),
        })
        .unwrap();
        drop(tx);
        // Returns: the queue closed.
        state.run(&rx);
        assert!(
            matches!(task.state(), TaskState::Failed(why) if why.contains("closing")),
            "{:?}",
            task.state()
        );
        assert!(state.recents.is_empty(), "the Forget applied");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A clone's hold is let go when its guard drops, whether or not the
    /// clone's thread ever ran (a spawn failure drops it unrun).
    #[test]
    fn a_clone_hold_is_released_when_its_guard_drops() {
        let (workspace, _rx) = stalled("hold-guard", false);
        let inner = Arc::clone(&workspace.inner);
        let target = PathBuf::from("/nowhere/app");
        inner.reserve(&target, Hold::Clone).unwrap();
        assert!(inner.reserve(&target, Hold::Delete).is_err());
        let guard = {
            let inner = Arc::clone(&inner);
            let target = target.clone();
            project_files::OnDrop::new(move || inner.release(&target))
        };
        // As in a thread closure that is dropped without running.
        let closure = move || drop(guard);
        drop(closure);
        assert!(inner.reserve(&target, Hold::Delete).is_ok());
    }

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
            deleting: AtomicUsize::new(0),
            setups: AtomicUsize::new(0),
            stop_hooks: Mutex::new(Vec::new()),
            directory: Arc::new(ProjectDirectory::default()),
            layout: Layout {
                data_root: PathBuf::from("/mg-ws-unit"),
                home: PathBuf::from("/mg-ws-unit-home"),
            },
            bin: Mutex::new(project_files::Bin::System),
            linked: Mutex::default(),
            reserved: Mutex::default(),
            tx: Mutex::default(),
            plans: Mutex::default(),
            next_plan: std::sync::atomic::AtomicU64::new(1),
            clones: Mutex::default(),
            clone_hook: Mutex::default(),
            delete_hook: Mutex::default(),
            teardown_hook: Mutex::default(),
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
