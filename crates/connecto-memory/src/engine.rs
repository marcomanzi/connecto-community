//! The memory engine (spec §5.1-§5.4, D3): one per app, serving every open
//! project. It implements [`MemoryControl`] for the UI and [`MemoryBridge`]
//! for `connecto memory`, takes captured events through a
//! [`CaptureHandle`], and runs the development cycle.
//!
//! # Threads (ADR 0002: threads and bounded `crossbeam` channels)
//!
//! - **supervisor** (one): routes captured events to their project, follows
//!   the open projects ([`Host::projects`]), applies settings changes and
//!   deletions, refreshes provider capabilities.
//! - per project with memory **enabled**:
//!   - the **writer** ([`crate::Writer`]), owner of the only write
//!     connection;
//!   - **control**: ingest ([`crate::ingest`]), UI reads, exports, and the
//!     project's part of the view;
//!   - **develop**: the development cycle ([`crate::develop`]), one run at a
//!     time.
//!
//! Nothing here runs on the UI thread: every [`MemoryControl`] method checks
//! its arguments, updates memory, posts to a bounded channel and returns.
//! [`MemoryControl::view`] is an `ArcSwap` load.
//!
//! Disabling memory stops capture and the cycles for the project and keeps
//! its data; deleting removes only the known files of its memory folder.
//!
//! # Extension points for the next tasks
//!
//! The scheduler and the dream cycle (T8, T9) and the initial build (T9b)
//! plug in at the methods in the `impl Engine` block marked *Extension
//! points*: [`Engine::dream_now`], [`Engine::revert_version`],
//! [`Engine::plan_bootstrap_for`], [`Engine::bootstrap_for`],
//! [`Engine::cancel_bootstrap_for`], [`Engine::resume_bootstrap_for`]. Each
//! answers [`NOT_YET`] today. A dream or build run belongs on a thread of
//! its own per project (like develop), using the same [`Live`] writer handle,
//! the same cancel registration ([`Engine::cancel_runs`] reaches it), and the
//! same pause rules; `next_dream_ms` and `skipped` in the view are filled by
//! [`Engine::project_view`] from what the scheduler stores.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use connecto_core::memory::{
    AskEntry, AskRequest, AskState, BootstrapOptions, BootstrapState, BootstrapView, Cycle,
    CycleState, ExportFormat, ExportOutcome, MAX_READS, MAX_RUNS, MemoryBridge, MemoryCaller,
    MemoryCommand, MemoryControl, MemorySettings, MemoryView, ModelChoice, NOT_ENABLED,
    ProjectMemoryView, Provider, ProviderCapability, ReadAnswer, ReadRequest, ReadResult,
    ReadState, Room, RunEntry, RunOutcome, StatementRefusal, StatementResult, StatementState,
    UserStatement, VersionEntry,
};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, select};

use crate::bootstrap::{BootstrapSlot, Start};
use crate::capture::{CAPTURE_QUEUE, CaptureHandle, Captured};
use crate::develop::{Context, Develop, Step};
use crate::dream::{self, DreamReport, DreamSlot, PublicationGate, Trigger};
use crate::error::Result;
use crate::host::Host;
use crate::ingest::Ingest;
use crate::lock;
use crate::read::{Counts, Reader, SearchQuery};
use crate::scheduler::{self, Check};
use crate::store::MemoryPaths;
use crate::writer::{WRITER_QUEUE, Writer, WriterHandle};

/// What an extension point answers until its task lands.
pub const NOT_YET: &str = "not available yet in this build";

/// Commands waiting for one project's control thread.
const CONTROL_QUEUE: usize = 256;
const DEVELOP_QUEUE: usize = 16;
/// How often the supervisor looks at the open projects.
const SYNC_EVERY: Duration = Duration::from_secs(1);
/// How often provider capabilities are asked again.
const CAPABILITY_EVERY: Duration = Duration::from_secs(300);
/// The develop thread looks at its queue at least this often.
const DEVELOP_TICK: Duration = Duration::from_millis(500);
/// The control thread refreshes the view (backlog ages) at least this often.
const CONTROL_TICK: Duration = Duration::from_secs(5);
const VERSIONS_SHOWN: usize = 50;

/// What starts an engine.
pub struct EngineConfig {
    /// The app's data folder: memory lives in
    /// `<data>/projects/<id>/memory/`.
    pub data_root: PathBuf,
    pub host: Arc<dyn Host>,
}

/// The engine. Cheap to clone; [`Engine::shutdown`] ends every thread.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

enum Op {
    Settings(String, MemorySettings),
    Delete(String),
    Stop,
}

enum Cmd {
    Capture(Box<Captured>),
    Read(u64, ReadRequest),
    Export(ExportFormat),
    /// A statement from the Connecto window ([`crate::user_statement`]).
    Statement(Box<crate::user_statement::Job>),
    Refresh,
}

enum DevMsg {
    Wake,
    Proposed,
    Activity,
}

struct Inner {
    data_root: PathBuf,
    host: Arc<dyn Host>,
    capture: CaptureHandle,
    ops: Sender<Op>,
    projects: Mutex<BTreeMap<String, Arc<Project>>>,
    view: ArcSwap<MemoryView>,
    revision: AtomicU64,
    capabilities: Mutex<Vec<ProviderCapability>>,
    stopping: AtomicBool,
    supervisor: Mutex<Option<JoinHandle<()>>>,
    next_read: AtomicU64,
    next_statement: AtomicU64,
    next_ask: AtomicU64,
    /// Bumped by [`Engine::cancel_runs`]: a run registered under an older
    /// value is cancelled.
    cancel_gen: AtomicU64,
    /// The dream scheduler (T8): one thread for every project.
    scheduler: Mutex<Option<JoinHandle<()>>>,
    /// Asks the scheduler to look now.
    nudge: Sender<()>,
}

/// One open project.
struct Project {
    id: String,
    path: PathBuf,
    paths: MemoryPaths,
    settings: Mutex<MemorySettings>,
    live: Mutex<Option<Live>>,
    state: Mutex<State>,
    /// The latest version announced to each terminal.
    announced: Mutex<BTreeMap<String, u64>>,
    /// The cancel flag of the development run in flight.
    run_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    dev_running: AtomicBool,
    /// The dream in flight, the publication gate, the schedule's view (T8,
    /// T9).
    dream: Arc<DreamSlot>,
    /// The initial build in flight and the latest plan (T9b).
    bootstrap: Arc<BootstrapSlot>,
}

#[derive(Default)]
struct State {
    data: Data,
    reads: Vec<ReadResult>,
    last_export: Option<ExportOutcome>,
    last_error: Option<String>,
    /// The user's statements, newest first ([`crate::user_statement`]).
    statements: Vec<StatementResult>,
    /// The latest dream's or revert's report, as the view shows it.
    last_report: Option<Arc<connecto_core::memory::DreamReport>>,
    /// Why git is not (fully) used, as found where git was asked anyway:
    /// at start, after a dream, after an initial build.
    git_note: Option<String>,
    /// The user's questions to the memory, oldest first ([`crate::ask`]).
    asks: Vec<AskEntry>,
    /// The cancel flag of each ask in flight, by id.
    ask_cancels: Vec<(u64, Arc<AtomicBool>)>,
}

/// What the store says, for the view.
#[derive(Default, Clone)]
struct Data {
    version: u64,
    counts: Counts,
    considered_through_ms: Option<u64>,
    backlog: connecto_core::memory::Backlog,
    runs: Arc<Vec<RunEntry>>,
    versions: Arc<Vec<VersionEntry>>,
    bootstrap: BootstrapView,
}

/// A live project and the ways into it.
type LiveSenders = (Arc<Project>, WriterHandle, Sender<Cmd>, Sender<DevMsg>);

/// A project whose memory runs.
struct Live {
    writer: Option<Writer>,
    handle: WriterHandle,
    control: Sender<Cmd>,
    develop: Sender<DevMsg>,
    threads: Vec<JoinHandle<()>>,
}

impl Live {
    fn senders(&self) -> (WriterHandle, Sender<Cmd>, Sender<DevMsg>) {
        (
            self.handle.clone(),
            self.control.clone(),
            self.develop.clone(),
        )
    }
}

/// Whether `id` is safe as one folder name under `projects/`.
fn safe_project_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn project_paths(data_root: &Path, id: &str) -> MemoryPaths {
    MemoryPaths::under(&data_root.join("projects").join(id))
}

/// Models and efforts offered for a provider.
fn offers(provider: Provider) -> (Vec<String>, Vec<String>) {
    let words = |w: &[&str]| w.iter().map(|s| (*s).to_owned()).collect();
    match provider {
        Provider::Claude => (
            words(&["haiku", "sonnet", "opus", "default"]),
            words(&["low", "medium", "high", "xhigh", "max"]),
        ),
        Provider::Codex => (
            words(&["default"]),
            words(&["low", "medium", "high", "xhigh"]),
        ),
    }
}

/// Why an answer that came while Connecto was paused is not shown.
const ASK_PAUSED: &str = "Connecto was paused while it answered: ask again";

/// Cancel every ask of `project` in flight (Stop Everything, memory off,
/// the project closing, the app closing).
fn cancel_asks(project: &Project) {
    for (_, flag) in &lock(&project.state).ask_cancels {
        flag.store(true, Ordering::SeqCst);
    }
}

/// Keep at most [`connecto_core::memory::MAX_ASKS`] asks, dropping the
/// oldest finished ones (a pending one is never dropped).
fn trim_asks(asks: &mut Vec<AskEntry>) {
    while asks.len() > connecto_core::memory::MAX_ASKS {
        match asks.iter().position(|a| !a.state.is_pending()) {
            Some(i) => {
                asks.remove(i);
            }
            None => break,
        }
    }
}

fn check_choice(choice: &ModelChoice) -> std::result::Result<(), String> {
    if choice.is_valid() {
        Ok(())
    } else {
        Err("the model or effort is not a valid word".to_owned())
    }
}

/// Remove the known files of a project's memory folder, by name: the
/// database and its side files, and exports this engine writes
/// (`exports/memory-<ms>.json`, `exports/memory-<ms>/<room>.md`). Folders
/// are removed only when empty; nothing is removed recursively, and nothing
/// outside `<data>/projects/<id>/memory/` is touched.
///
/// No link is followed below `data_root`: on Unix every folder from
/// `projects` down is opened relative to its parent with `O_NOFOLLOW`, and
/// every file is unlinked by name relative to its folder, so a link at any
/// level (`projects/<id>`, `memory`, `exports`, an export folder) refuses
/// the deletion or is removed as a link, never traversed. Elsewhere each
/// level is checked not to be a link before it is used. Returns what was
/// removed.
pub fn delete_memory_files(data_root: &Path, project_id: &str) -> io::Result<Vec<PathBuf>> {
    if !safe_project_id(project_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a project id",
        ));
    }
    delete_files::run(data_root, project_id)
}

/// Whether this platform deletes a project's memory files
/// ([`delete_memory_files`]); elsewhere the deletion is refused with
/// [`unsupported_message`].
pub const DELETION_SUPPORTED: bool = cfg!(unix);

/// The refusal where deleting memory files is not supported: `Unsupported`,
/// naming the folder to remove by hand.
#[must_use]
pub fn deletion_refusal(data_root: &Path, project_id: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        unsupported_message(&project_paths(data_root, project_id).dir),
    )
}

/// What the user is told where deleting memory is not supported: the folder
/// to remove by hand.
#[must_use]
pub fn unsupported_message(dir: &Path) -> String {
    format!(
        "Deleting project memory is not supported on this system yet: Connecto cannot \
         delete these files here without the risk of following a link out of the memory \
         folder. Memory is turned off for this project and its files are closed. To \
         delete it, remove this folder yourself: {}",
        dir.display()
    )
}

/// The file names [`delete_memory_files`] removes.
mod delete_files {
    use super::{Room, io};
    use std::path::{Path, PathBuf};

    pub(super) const DB_FILES: [&str; 4] = [
        "memory.db",
        "memory.db-wal",
        "memory.db-shm",
        "memory.db-journal",
    ];

    pub(super) fn is_export(name: &str, suffix: &str) -> bool {
        name.strip_prefix("memory-")
            .and_then(|rest| rest.strip_suffix(suffix))
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    }

    pub(super) fn room_files() -> Vec<String> {
        Room::ALL
            .iter()
            .filter(|r| r.holds_memories())
            .map(|r| format!("{}.md", r.word()))
            .collect()
    }

    #[cfg(unix)]
    pub(super) fn run(data_root: &Path, project_id: &str) -> io::Result<Vec<PathBuf>> {
        use nix::dir::{Dir, Type};
        use nix::errno::Errno;
        use nix::fcntl::OFlag;
        use nix::sys::stat::Mode;
        use nix::unistd::{UnlinkatFlags, unlinkat};
        use std::os::fd::AsRawFd;

        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let err = |e: Errno| io::Error::from_raw_os_error(e as i32);
        // A folder relative to its parent; `None` when it does not exist. A
        // link (or anything not a folder) is an error: nothing is deleted.
        let sub = |parent: &Dir, name: &str| -> io::Result<Option<Dir>> {
            match Dir::openat(Some(parent.as_raw_fd()), name, flags, Mode::empty()) {
                Ok(d) => Ok(Some(d)),
                Err(Errno::ENOENT) => Ok(None),
                Err(Errno::ELOOP | Errno::ENOTDIR) => Err(io::Error::other(format!(
                    "{name} is a link or not a folder: nothing was deleted"
                ))),
                Err(e) => Err(err(e)),
            }
        };
        // A name in a folder, unlinked relative to it: a link is removed as
        // a link; a folder is left alone.
        let unlink = |dir: &Dir, name: &str, at: &Path, removed: &mut Vec<PathBuf>| match unlinkat(
            Some(dir.as_raw_fd()),
            name,
            UnlinkatFlags::NoRemoveDir,
        ) {
            Ok(()) => {
                removed.push(at.join(name));
                Ok(())
            }
            Err(Errno::ENOENT | Errno::EISDIR | Errno::EPERM) => Ok(()),
            Err(e) => Err(err(e)),
        };
        let rmdir = |dir: &Dir, name: &str, at: &Path, removed: &mut Vec<PathBuf>| {
            if unlinkat(Some(dir.as_raw_fd()), name, UnlinkatFlags::RemoveDir).is_ok() {
                removed.push(at.join(name));
            }
        };

        let mut removed = Vec::new();
        let Ok(root) = Dir::open(
            data_root,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) else {
            return Ok(removed);
        };
        let Some(projects) = sub(&root, "projects")? else {
            return Ok(removed);
        };
        let Some(project) = sub(&projects, project_id)? else {
            return Ok(removed);
        };
        let Some(memory) = sub(&project, "memory")? else {
            return Ok(removed);
        };
        let at = data_root.join("projects").join(project_id).join("memory");
        for name in DB_FILES {
            unlink(&memory, name, &at, &mut removed)?;
        }
        if let Some(mut exports) = sub(&memory, "exports")? {
            let ex_at = at.join("exports");
            let mut files = Vec::new();
            let mut folders = Vec::new();
            for entry in exports.iter().flatten() {
                let Ok(name) = entry.file_name().to_str() else {
                    continue;
                };
                match entry.file_type() {
                    Some(Type::File) if is_export(name, ".json") => files.push(name.to_owned()),
                    Some(Type::Directory) if is_export(name, "") => folders.push(name.to_owned()),
                    _ => {}
                }
            }
            for name in files {
                unlink(&exports, &name, &ex_at, &mut removed)?;
            }
            for name in folders {
                // Opened without following: a folder swapped for a link since
                // the listing is refused.
                if let Some(folder) = sub(&exports, &name)? {
                    let inner = ex_at.join(&name);
                    for room in room_files() {
                        unlink(&folder, &room, &inner, &mut removed)?;
                    }
                    drop(folder);
                    rmdir(&exports, &name, &ex_at, &mut removed);
                }
            }
            drop(exports);
            rmdir(&memory, "exports", &at, &mut removed);
        }
        drop(memory);
        rmdir(
            &project,
            "memory",
            &data_root.join("projects").join(project_id),
            &mut removed,
        );
        Ok(removed)
    }

    /// Off Unix there is no link-proof deletion through safe APIs here
    /// (handle-relative deletion with reparse-point protection): the
    /// deletion is refused rather than risk following a link or junction
    /// out of the memory folder. Memory is still turned off and closed.
    /// Compiled (and tested) on every platform; only the alias below is
    /// platform-gated.
    #[cfg_attr(unix, allow(dead_code))]
    pub(super) fn run_refused(data_root: &Path, project_id: &str) -> io::Result<Vec<PathBuf>> {
        Err(super::deletion_refusal(data_root, project_id))
    }

    #[cfg(not(unix))]
    pub(super) use run_refused as run;

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The branch used off Unix, run here: refused, nothing touched.
        #[test]
        fn the_refused_branch_names_the_folder_and_touches_nothing() {
            let e = run_refused(Path::new("/nowhere/data"), "p1")
                .err()
                .map(|e| (e.kind(), e.to_string()));
            let (kind, text) = e.unwrap_or((io::ErrorKind::Other, String::new()));
            assert_eq!(kind, io::ErrorKind::Unsupported);
            assert!(text.contains("p1"));
        }
    }
}

impl Engine {
    /// Start the supervisor. Projects are picked up from
    /// [`Host::projects`] within a second.
    pub fn start(config: EngineConfig) -> io::Result<Self> {
        let (capture, capture_rx) = CaptureHandle::channel(CAPTURE_QUEUE);
        let (ops, ops_rx) = bounded(64);
        let (nudge, nudge_rx) = bounded(1);
        let inner = Arc::new(Inner {
            data_root: config.data_root,
            host: config.host,
            capture,
            ops,
            projects: Mutex::new(BTreeMap::new()),
            view: ArcSwap::from_pointee(MemoryView::default()),
            revision: AtomicU64::new(0),
            capabilities: Mutex::new(Vec::new()),
            stopping: AtomicBool::new(false),
            supervisor: Mutex::new(None),
            next_read: AtomicU64::new(1),
            next_statement: AtomicU64::new(1),
            next_ask: AtomicU64::new(1),
            cancel_gen: AtomicU64::new(0),
            scheduler: Mutex::new(None),
            nudge,
        });
        let engine = Self {
            inner: Arc::clone(&inner),
        };
        let worker = engine.clone();
        let thread = std::thread::Builder::new()
            .name("memory-supervisor".into())
            .spawn(move || worker.supervise(&capture_rx, &ops_rx))?;
        *lock(&inner.supervisor) = Some(thread);
        let worker = engine.clone();
        let thread = std::thread::Builder::new()
            .name("memory-scheduler".into())
            .spawn(move || worker.schedule_loop(&nudge_rx))?;
        *lock(&inner.scheduler) = Some(thread);
        Ok(engine)
    }

    /// Where terminals' events go in. Never blocks.
    #[must_use]
    pub fn capture(&self) -> CaptureHandle {
        self.inner.capture.clone()
    }

    /// Stop Everything: cancel every memory run in flight, in every project.
    /// The queue is kept; nothing starts while the app is paused.
    pub fn cancel_runs(&self) {
        self.inner.cancel_gen.fetch_add(1, Ordering::SeqCst);
        for project in self.projects() {
            if let Some(flag) = lock(&project.run_cancel).as_ref() {
                flag.store(true, Ordering::SeqCst);
            }
            cancel_asks(&project);
            project.dream.cancel();
            // Closing the app leaves the build to resume at the next start.
            if self.inner.stopping.load(Ordering::SeqCst) {
                project.bootstrap.halt();
            } else {
                project.bootstrap.cancel();
            }
        }
        self.inner.host.cancel_runs();
    }

    /// Closing the app: cancel the runs, stop every project's threads and
    /// the supervisor. State on disk stays consistent (every write is a
    /// transaction). Idempotent.
    pub fn shutdown(&self) {
        if self.inner.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        self.cancel_runs();
        let _ = self.inner.ops.send(Op::Stop);
        // No lock is held across a join: the threads publish through them.
        let supervisor = lock(&self.inner.supervisor).take();
        if let Some(thread) = supervisor {
            let _ = thread.join();
        }
        let scheduler = lock(&self.inner.scheduler).take();
        if let Some(thread) = scheduler {
            let _ = thread.join();
        }
        for project in self.projects() {
            self.stop_live(&project);
        }
        self.publish();
        // What the host keeps for the projects (private git folders) goes.
        self.inner.host.shutdown();
    }

    fn projects(&self) -> Vec<Arc<Project>> {
        lock(&self.inner.projects).values().cloned().collect()
    }

    fn project(&self, id: &str) -> Option<Arc<Project>> {
        lock(&self.inner.projects).get(id).cloned()
    }

    // ---- the supervisor

    fn supervise(&self, captures: &Receiver<Captured>, ops: &Receiver<Op>) {
        let mut last_sync: Option<Instant> = None;
        let mut last_caps: Option<Instant> = None;
        loop {
            if last_sync.is_none_or(|t| t.elapsed() >= SYNC_EVERY) {
                self.sync_projects();
                last_sync = Some(Instant::now());
            }
            if last_caps.is_none_or(|t| t.elapsed() >= CAPABILITY_EVERY) {
                self.refresh_capabilities();
                last_caps = Some(Instant::now());
            }
            select! {
                recv(captures) -> c => match c {
                    Ok(c) => self.route(c),
                    Err(_) => break,
                },
                recv(ops) -> op => match op {
                    Ok(Op::Settings(id, settings)) => self.apply_settings(&id, settings),
                    Ok(Op::Delete(id)) => self.delete(&id),
                    Ok(Op::Stop) | Err(_) => break,
                },
                default(Duration::from_millis(250)) => {}
            }
        }
    }

    fn route(&self, c: Captured) {
        let Some(project) = self.project(&c.project_id) else {
            return;
        };
        if !lock(&project.settings).enabled {
            return;
        }
        let control = lock(&project.live).as_ref().map(|l| l.control.clone());
        let sent = control.map(|control| control.try_send(Cmd::Capture(Box::new(c))));
        if let Some(Err(crossbeam_channel::TrySendError::Full(_))) = sent {
            self.inner.capture.count_dropped();
        }
    }

    fn sync_projects(&self) {
        let open = self.inner.host.projects();
        let mut changed = false;
        let known: Vec<String> = lock(&self.inner.projects).keys().cloned().collect();
        for gone in known.iter().filter(|id| !open.iter().any(|p| &p.id == *id)) {
            // Removed under the lock, stopped after it is released: the
            // project's threads publish through the same lock while they end.
            let removed = lock(&self.inner.projects).remove(gone);
            if let Some(project) = removed {
                self.stop_live(&project);
                changed = true;
            }
        }
        for p in open {
            if !safe_project_id(&p.id) || lock(&self.inner.projects).contains_key(&p.id) {
                continue;
            }
            let settings = self.inner.host.load_settings(&p.id).clamped();
            let project = Arc::new(Project {
                paths: project_paths(&self.inner.data_root, &p.id),
                id: p.id.clone(),
                path: p.path,
                settings: Mutex::new(settings.clone()),
                live: Mutex::new(None),
                state: Mutex::new(State::default()),
                announced: Mutex::new(BTreeMap::new()),
                run_cancel: Arc::new(Mutex::new(None)),
                dev_running: AtomicBool::new(false),
                dream: Arc::default(),
                bootstrap: Arc::default(),
            });
            lock(&self.inner.projects).insert(p.id.clone(), Arc::clone(&project));
            if settings.enabled {
                self.start_live(&project);
            }
            changed = true;
        }
        if changed {
            self.publish();
        }
    }

    fn refresh_capabilities(&self) {
        let caps = Provider::ALL
            .iter()
            .map(|p| {
                let capability = self
                    .inner
                    .host
                    .capability(&ModelChoice::new(*p, "default", "low"));
                let (models, efforts) = if capability.is_available() {
                    offers(*p)
                } else {
                    (Vec::new(), Vec::new())
                };
                ProviderCapability {
                    provider: *p,
                    capability,
                    models,
                    efforts,
                }
            })
            .collect();
        *lock(&self.inner.capabilities) = caps;
        self.publish();
    }

    fn apply_settings(&self, id: &str, settings: MemorySettings) {
        let Some(project) = self.project(id) else {
            return;
        };
        if let Err(e) = self.inner.host.save_settings(id, &settings) {
            lock(&project.state).last_error = Some(format!("the settings were not saved: {e}"));
        }
        *lock(&project.settings) = settings.clone();
        let running = lock(&project.live).is_some();
        if settings.enabled && !running {
            self.start_live(&project);
        } else if !settings.enabled && running {
            self.stop_live(&project);
        } else {
            let develop = lock(&project.live).as_ref().map(|l| l.develop.clone());
            if let Some(develop) = develop {
                let _ = develop.try_send(DevMsg::Wake);
            }
        }
        self.publish();
    }

    fn delete(&self, id: &str) {
        let Some(project) = self.project(id) else {
            return;
        };
        self.stop_live(&project);
        let mut settings = lock(&project.settings).clone();
        settings.enabled = false;
        if let Err(e) = self.inner.host.save_settings(id, &settings) {
            lock(&project.state).last_error = Some(format!("the settings were not saved: {e}"));
        }
        *lock(&project.settings) = settings;
        let result = delete_memory_files(&self.inner.data_root, id);
        {
            let mut state = lock(&project.state);
            state.data = Data::default();
            state.reads.clear();
            state.last_export = None;
            state.last_error = result.err().map(|e| {
                if e.kind() == io::ErrorKind::Unsupported {
                    e.to_string()
                } else {
                    format!("the memory could not be fully deleted: {e}")
                }
            });
        }
        lock(&project.announced).clear();
        self.publish();
    }

    fn start_live(&self, project: &Arc<Project>) {
        self.inner.host.project_started(&project.path);
        let writer = match Writer::spawn(&project.paths.db(), &project.id, WRITER_QUEUE) {
            Ok(w) => w,
            Err(e) => {
                lock(&project.state).last_error =
                    Some(format!("the memory could not be opened: {e}"));
                return;
            }
        };
        let handle = writer.handle();
        let (control_tx, control_rx) = bounded(CONTROL_QUEUE);
        let (dev_tx, dev_rx) = bounded(DEVELOP_QUEUE);
        let mut threads = Vec::new();
        let (engine, p, h, d) = (
            self.clone(),
            Arc::clone(project),
            handle.clone(),
            dev_tx.clone(),
        );
        match std::thread::Builder::new()
            .name(format!("memory-control-{}", project.id))
            .spawn(move || engine.control_loop(&p, &h, &control_rx, &d))
        {
            Ok(t) => threads.push(t),
            Err(e) => {
                lock(&project.state).last_error = Some(format!("memory did not start: {e}"));
                return;
            }
        }
        let (engine, p, h) = (self.clone(), Arc::clone(project), handle.clone());
        match std::thread::Builder::new()
            .name(format!("memory-develop-{}", project.id))
            .spawn(move || engine.develop_loop(&p, &h, &dev_rx))
        {
            Ok(t) => threads.push(t),
            Err(e) => {
                lock(&project.state).last_error = Some(format!("memory did not start: {e}"));
            }
        }
        *lock(&project.live) = Some(Live {
            writer: Some(writer),
            handle,
            control: control_tx,
            develop: dev_tx,
            threads,
        });
    }

    /// Stop a project's threads (its run first) and close its store.
    fn stop_live(&self, project: &Project) {
        let Some(mut live) = lock(&project.live).take() else {
            return;
        };
        if let Some(flag) = lock(&project.run_cancel).as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
        cancel_asks(project);
        // The dream (or revert) ends before its writer does.
        project.dream.stop();
        // So does the initial build, left to resume when memory is back on.
        project.bootstrap.close();
        let threads = std::mem::take(&mut live.threads);
        let writer = live.writer.take();
        // Closing the channels ends both loops.
        drop(live);
        for t in threads {
            let _ = t.join();
        }
        drop(writer);
        // Only now, no worker of this project left to use it: what the host
        // keeps for the project (private git folders) goes, and stays gone
        // until the project starts again.
        self.inner.host.project_stopped(&project.path);
        project.dev_running.store(false, Ordering::SeqCst);
        // A statement still queued was never recorded: say so, finally.
        let mut state = lock(&project.state);
        for r in &mut state.statements {
            if !r.state.is_final() {
                r.state = StatementState::Refused(StatementRefusal::Disabled);
            }
        }
    }

    // ---- per-project threads

    fn control_loop(
        &self,
        project: &Arc<Project>,
        writer: &WriterHandle,
        rx: &Receiver<Cmd>,
        develop: &Sender<DevMsg>,
    ) {
        let host = self.inner.host.as_ref();
        let now = host.now().utc_ms;
        // A run found running was cut short by the app stopping: its batch
        // goes back to the queue (no attempt used).
        if let Err(e) = writer.call(move |s| s.interrupt_running(now)) {
            lock(&project.state).last_error = Some(e.to_string());
        }
        let mut ingest = Ingest::new(project.path.clone());
        if let Err(e) = ingest.baseline(writer, host) {
            lock(&project.state).last_error = Some(format!("git: {e}"));
        }
        self.note_git(project);
        let reader = Reader::open(&project.paths.db(), &project.id).ok();
        self.refresh(project, writer);
        // An initial build the app stopped while it was queued or running
        // goes on (after the runs it had in flight were marked interrupted).
        if matches!(
            writer.call(|s| crate::bootstrap::resumes_at_start(s)),
            Ok(Ok(true))
        ) {
            let _ = self.launch_bootstrap(project, writer.clone(), Start::Resume);
        }
        loop {
            let first = match rx.recv_timeout(CONTROL_TICK) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let mut wake = false;
            for cmd in first.into_iter().chain(rx.try_iter()) {
                match cmd {
                    Cmd::Capture(c) => match ingest.ingest(writer, host, *c) {
                        Ok(done) => {
                            if !done.queued.is_empty() {
                                wake = true;
                            }
                        }
                        Err(e) => {
                            lock(&project.state).last_error =
                                Some(format!("an event was not stored: {e}"));
                        }
                    },
                    Cmd::Read(id, request) => {
                        let brief = matches!(request, ReadRequest::Brief)
                            .then(|| (self.project_view(project), host.now().utc_ms));
                        let state = answer(reader.as_ref(), &request, brief.as_ref());
                        let mut st = lock(&project.state);
                        if let Some(r) = st.reads.iter_mut().find(|r| r.id == id) {
                            r.state = state;
                        }
                    }
                    Cmd::Export(format) => {
                        let at = host.now().utc_ms;
                        let dir = project.paths.exports();
                        let result = writer
                            .call(move |s| crate::export::export(&s.read(), &dir, format, at))
                            .and_then(|r| r)
                            .map_err(|e| e.to_string());
                        lock(&project.state).last_export = Some(ExportOutcome {
                            format,
                            at_ms: at,
                            result,
                        });
                    }
                    Cmd::Statement(job) => self.record_statement(project, writer, *job),
                    Cmd::Refresh => {}
                }
            }
            if wake {
                let _ = develop.try_send(DevMsg::Activity);
            }
            self.refresh(project, writer);
        }
    }

    fn develop_loop(&self, project: &Arc<Project>, writer: &WriterHandle, rx: &Receiver<DevMsg>) {
        let mut develop = Develop::new();
        loop {
            match rx.recv_timeout(DEVELOP_TICK) {
                Ok(DevMsg::Proposed) => develop.close_window(),
                Ok(DevMsg::Activity) => develop.activity(),
                Ok(DevMsg::Wake) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            for msg in rx.try_iter() {
                match msg {
                    DevMsg::Proposed => develop.close_window(),
                    DevMsg::Activity => develop.activity(),
                    DevMsg::Wake => {}
                }
            }
            if self.inner.stopping.load(Ordering::SeqCst) {
                break;
            }
            let settings = lock(&project.settings).clone();
            if !settings.enabled {
                continue;
            }
            // Batches back to back while there is work, one run at a time.
            loop {
                let generation = self.inner.cancel_gen.load(Ordering::SeqCst);
                let cancel = Arc::new(AtomicBool::new(false));
                *lock(&project.run_cancel) = Some(Arc::clone(&cancel));
                if self.inner.cancel_gen.load(Ordering::SeqCst) != generation {
                    cancel.store(true, Ordering::SeqCst);
                }
                project.dev_running.store(true, Ordering::SeqCst);
                let cx = Context {
                    host: Arc::clone(&self.inner.host),
                    writer,
                    project_id: &project.id,
                    settings: &settings.development,
                    cancel: Arc::clone(&cancel),
                };
                let step = develop.step(&cx);
                project.dev_running.store(false, Ordering::SeqCst);
                *lock(&project.run_cancel) = None;
                match step {
                    Ok(Step::Ran { .. }) => {
                        lock(&project.state).last_error = None;
                        self.refresh(project, writer);
                        if self.inner.stopping.load(Ordering::SeqCst)
                            || lock(&project.live).is_none()
                        {
                            break;
                        }
                    }
                    Ok(Step::Misconfigured { needed }) => {
                        lock(&project.state).last_error = Some(format!(
                            "development cycle: the input limit is below what one prompt \
                             needs; raise it to at least {needed} characters"
                        ));
                        self.publish();
                        break;
                    }
                    Ok(_) => break,
                    Err(e) => {
                        lock(&project.state).last_error = Some(format!("development cycle: {e}"));
                        self.publish();
                        break;
                    }
                }
            }
        }
    }

    /// Read the store's part of the view and publish.
    fn refresh(&self, project: &Project, writer: &WriterHandle) {
        let now = self.inner.host.now().utc_ms;
        let data = writer.call(move |s| -> Result<Data> {
            let read = s.read();
            let bootstrap = crate::bootstrap::view(s)?;
            Ok(Data {
                version: read.version()?,
                counts: read.counts()?,
                considered_through_ms: s.considered_through_ms()?,
                backlog: s.backlog(now)?,
                runs: Arc::new(s.runs(None, MAX_RUNS)?),
                versions: Arc::new(read.versions(VERSIONS_SHOWN)?),
                bootstrap,
            })
        });
        match data {
            Ok(Ok(data)) => lock(&project.state).data = data,
            Ok(Err(e)) | Err(e) => lock(&project.state).last_error = Some(e.to_string()),
        }
        self.publish();
    }

    fn project_view(&self, project: &Project) -> ProjectMemoryView {
        let settings = lock(&project.settings).clone();
        let schedule = project.dream.schedule_view();
        let state = lock(&project.state);
        let data = &state.data;
        let mut backlog = data.backlog.clone();
        backlog.dropped_on_full = self.inner.capture.dropped();
        let cycles = Cycle::ALL
            .iter()
            .map(|cycle| {
                let last = data.runs.iter().find(|r| r.cycle == *cycle).cloned();
                let running = last.as_ref().is_some_and(|r| r.outcome.is_running())
                    || (*cycle == Cycle::Development
                        && project.dev_running.load(Ordering::SeqCst)
                        && last
                            .as_ref()
                            .is_some_and(|r| r.outcome == RunOutcome::Running));
                CycleState {
                    cycle: *cycle,
                    running,
                    last,
                }
            })
            .collect();
        ProjectMemoryView {
            project_id: project.id.clone(),
            enabled: settings.enabled,
            settings,
            version: data.version,
            rooms: data.counts.rooms.clone(),
            totals: data.counts.totals,
            open_annotations: data.counts.open_annotations,
            open_contradictions: data.counts.open_contradictions,
            considered_through_ms: data.considered_through_ms,
            backlog,
            cycles,
            next_dream_ms: schedule.0,
            skipped: schedule.1,
            bootstrap: {
                let mut b = data.bootstrap.clone();
                (b.plan, b.planning) = project.bootstrap.plan();
                b
            },
            runs: Arc::clone(&data.runs),
            versions: Arc::clone(&data.versions),
            reads: state.reads.clone(),
            statements: state.statements.clone(),
            asks: state.asks.clone(),
            last_export: state.last_export.clone(),
            last_report: state.last_report.clone(),
            git_note: state.git_note.clone(),
            last_error: state.last_error.clone(),
        }
    }

    fn publish(&self) {
        let projects = self
            .projects()
            .iter()
            .map(|p| self.project_view(p))
            .collect();
        let view = MemoryView {
            revision: self.inner.revision.fetch_add(1, Ordering::SeqCst) + 1,
            projects,
            capabilities: lock(&self.inner.capabilities).clone(),
        };
        self.inner.view.store(Arc::new(view));
        self.inner.host.wake();
    }

    fn live_senders(&self, project_id: &str) -> std::result::Result<LiveSenders, String> {
        let project = self
            .project(project_id)
            .ok_or_else(|| "that project is not open".to_owned())?;
        let senders = lock(&project.live)
            .as_ref()
            .map(Live::senders)
            .ok_or_else(|| "memory is off for this project".to_owned())?;
        Ok((project, senders.0, senders.1, senders.2))
    }

    // ---- Extension points (T8 scheduler, T9 dream, T9b initial build).
    //
    // Each is the one place its feature plugs in; each answers `NOT_YET`
    // until then.

    /// "Dream now" (T9). Refused while paused, while memory is off, and
    /// while a dream, a revert or an initial build holds the project's
    /// publication gate.
    pub fn dream_now(&self, project_id: &str) -> std::result::Result<(), String> {
        let (project, ..) = self.live_senders(project_id)?;
        if !lock(&project.settings).enabled {
            return Err("memory is off for this project".to_owned());
        }
        if self.inner.host.paused() {
            return Err("paused".to_owned());
        }
        self.start_dream(&project, Trigger::Now)
    }

    /// Undo a published version (T9), as a new version, from the UI only.
    /// Annotations that arrived after it are untouched; claims the restored
    /// state no longer supports are lowered and listed in
    /// [`Engine::dream_report`].
    pub fn revert_version(
        &self,
        project_id: &str,
        version: u64,
    ) -> std::result::Result<(), String> {
        let (project, writer, ..) = self.live_senders(project_id)?;
        if version == 0 {
            return Err("no such version".to_owned());
        }
        let guard = project
            .dream
            .gate()
            .try_acquire(Cycle::Dream)
            .ok_or_else(|| {
                "a dream or an initial build is running; try again when it ends".to_owned()
            })?;
        let now = self.inner.host.now().utc_ms;
        let (engine, p) = (self.clone(), Arc::clone(&project));
        project
            .dream
            .spawn(format!("memory-revert-{}", project.id), false, move |_| {
                let result = writer.call(move |s| s.revert(version, now)).and_then(|r| r);
                drop(guard);
                match result {
                    Ok(r) => {
                        let report = DreamReport {
                            reverted: Some((r.reverted, r.version)),
                            lowered: r.lowered,
                            ..DreamReport::default()
                        };
                        engine.keep_report(&p, &report);
                        p.dream.set_report(report);
                        lock(&p.state).last_error = None;
                    }
                    Err(e) => {
                        lock(&p.state).last_error =
                            Some(format!("version {version} was not undone: {e}"));
                    }
                }
                // Not while the project is being stopped (its stopper may
                // hold the project list and is waiting for this thread).
                if lock(&p.live).is_some() {
                    engine.refresh(&p, &writer);
                }
            })
    }

    /// What an initial build would read (T9b), computed on a thread of its
    /// own; the answer appears in `bootstrap.plan`.
    pub fn plan_bootstrap_for(
        &self,
        project_id: &str,
        options: BootstrapOptions,
    ) -> std::result::Result<(), String> {
        let (project, ..) = self.live_senders(project_id)?;
        if let Some(choice) = &options.choice {
            check_choice(choice)?;
        }
        let n = project.bootstrap.begin_plan();
        self.publish();
        let (engine, p) = (self.clone(), Arc::clone(&project));
        let spawned = std::thread::Builder::new()
            .name(format!("memory-bootstrap-plan-{}", project.id))
            .spawn(move || {
                let settings = lock(&p.settings).clone();
                let plan = crate::bootstrap::plan(
                    engine.inner.host.as_ref(),
                    &p.id,
                    &p.path,
                    &options,
                    &settings,
                );
                p.bootstrap.end_plan(n, plan);
                engine.publish();
            });
        if let Err(e) = spawned {
            project.bootstrap.drop_plan(n);
            self.publish();
            return Err(format!("the plan could not be computed: {e}"));
        }
        Ok(())
    }

    /// Start an initial build (T9b). On a populated memory it reconciles,
    /// never resets. Refused while one runs.
    pub fn bootstrap_for(
        &self,
        project_id: &str,
        options: BootstrapOptions,
    ) -> std::result::Result<(), String> {
        let (project, writer, ..) = self.live_senders(project_id)?;
        if !lock(&project.settings).enabled {
            return Err("memory is off for this project".to_owned());
        }
        if let Some(choice) = &options.choice {
            check_choice(choice)?;
        }
        if let Some(problem) = crate::bootstrap::limit_problem(&lock(&project.settings)) {
            return Err(problem);
        }
        let start = Start::New {
            id: crate::bootstrap::new_id(),
            options: options.clamped(),
        };
        self.launch_bootstrap(&project, writer, start)
    }

    /// Stop the initial build (T9b); it becomes interrupted and can resume.
    pub fn cancel_bootstrap_for(&self, project_id: &str) {
        if let Some(project) = self.project(project_id) {
            project.bootstrap.cancel();
        }
    }

    /// Resume an interrupted, failed or waiting build (T9b) from its last
    /// checkpoint. A build that runs is asked to try again now (a held one).
    pub fn resume_bootstrap_for(&self, project_id: &str) -> std::result::Result<(), String> {
        let (project, writer, ..) = self.live_senders(project_id)?;
        if project.bootstrap.running() {
            project.bootstrap.nudge();
            return Ok(());
        }
        let state = lock(&project.state).data.bootstrap.state.clone();
        if matches!(
            state,
            BootstrapState::NotStarted | BootstrapState::Completed
        ) {
            return Err("there is no initial build to resume".to_owned());
        }
        self.launch_bootstrap(&project, writer, Start::Resume)
    }

    /// Run an initial build on a thread of its own (T9b).
    fn launch_bootstrap(
        &self,
        project: &Arc<Project>,
        writer: WriterHandle,
        start: Start,
    ) -> std::result::Result<(), String> {
        let generation = self.inner.cancel_gen.load(Ordering::SeqCst);
        let (engine, p) = (self.clone(), Arc::clone(project));
        let launched =
            project
                .bootstrap
                .launch(format!("memory-bootstrap-{}", project.id), move |stops| {
                    if engine.inner.cancel_gen.load(Ordering::SeqCst) != generation {
                        stops.cancel();
                    }
                    let settings = lock(&p.settings).clone();
                    let changed = || engine.refresh(&p, &writer);
                    let cx = crate::bootstrap::Context {
                        host: Arc::clone(&engine.inner.host),
                        writer: &writer,
                        project_id: &p.id,
                        project: &p.path,
                        settings,
                        stops,
                        gate: p.dream.gate(),
                        changed: &changed,
                    };
                    match crate::bootstrap::run(&cx, start) {
                        Ok(BootstrapState::Failed { reason }) => {
                            lock(&p.state).last_error = Some(format!("initial build: {reason}"));
                        }
                        Ok(BootstrapState::Completed) => {
                            // What it refused or adjusted, and why, for the
                            // last report (its run rows say "N proposed, 0
                            // kept" on their own).
                            if let Ok(Ok(Some(report))) =
                                writer.call(|s| crate::bootstrap::report(s))
                            {
                                engine.keep_report(&p, &report);
                            }
                        }
                        Ok(_) => {}
                        Err(e) => lock(&p.state).last_error = Some(format!("initial build: {e}")),
                    }
                    engine.note_git(&p);
                    engine.refresh(&p, &writer);
                    engine.check_schedule_now();
                });
        self.publish();
        launched
    }
}

// ---- The dream scheduler and the dream runs (T8, T9).
impl Engine {
    /// The project's publication gate: a dream, a revert and the initial
    /// build each hold it for their whole run, so they never publish at the
    /// same time. `None` when the project is not open.
    #[must_use]
    pub fn publication_gate(&self, project_id: &str) -> Option<Arc<PublicationGate>> {
        self.project(project_id).map(|p| p.dream.gate())
    }

    /// What the project's latest dream or revert did (rejected proposals
    /// with their reasons, claims lowered by a revert).
    #[must_use]
    pub fn dream_report(&self, project_id: &str) -> Option<DreamReport> {
        self.project(project_id).and_then(|p| p.dream.report())
    }

    /// Ask the scheduler to look at every project now (the machine woke up,
    /// the clock changed). It also looks every [`scheduler::TICK`] and when
    /// it sees the wall clock jump.
    pub fn check_schedule_now(&self) {
        let _ = self.inner.nudge.try_send(());
    }

    fn schedule_loop(&self, nudges: &Receiver<()>) {
        let host = self.inner.host.as_ref();
        let mut last_check: Option<Instant> = None;
        let mut last_wall = (Instant::now(), host.now().utc_ms);
        loop {
            if self.inner.stopping.load(Ordering::SeqCst) {
                break;
            }
            let nudged = match nudges.recv_timeout(Duration::from_millis(200)) {
                Ok(()) => true,
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let (mono, wall) = (Instant::now(), host.now().utc_ms);
            let walked = i128::from(wall) - i128::from(last_wall.1);
            let elapsed = i128::try_from(mono.duration_since(last_wall.0).as_millis()).unwrap_or(0);
            let jumped = (walked - elapsed).unsigned_abs() > scheduler::JUMP.as_millis();
            last_wall = (mono, wall);
            let tick = last_check.is_none_or(|t| t.elapsed() >= scheduler::TICK);
            let all = nudged || jumped || tick;
            if all {
                last_check = Some(Instant::now());
            }
            for project in self.projects() {
                if all || !project.dream.looked() {
                    self.schedule_project(&project);
                }
            }
        }
    }

    /// One look at a project's schedule; starts the dream that is due.
    fn schedule_project(&self, project: &Arc<Project>) {
        let settings = lock(&project.settings).clone();
        let Some((writer, ..)) = lock(&project.live).as_ref().map(Live::senders) else {
            return;
        };
        if !settings.enabled {
            return;
        }
        let first = project.dream.first_look();
        let running = project.dream.running();
        let now = self.inner.host.now();
        let program = settings.dream.clone();
        let looked = writer.call(move |s| -> Result<_> {
            if first && !running {
                // Start-up: a dream still `running` was cut short by the
                // app stopping, and its staging is dropped (spec §7).
                for r in s.runs(Some(Cycle::Dream), MAX_RUNS)? {
                    if r.outcome.is_running() {
                        s.finish_run(
                            r.id,
                            &RunOutcome::Interrupted,
                            now.utc_ms,
                            r.chars_in,
                            r.chars_out,
                        )?;
                    }
                }
                scheduler::recover_at_start(s)?;
            }
            let check = scheduler::check(s, &program, &now)?;
            let view = scheduler::view(s, &program, &now)?;
            Ok((check, view))
        });
        let (check, view) = match looked {
            Ok(Ok(found)) => found,
            Ok(Err(e)) | Err(e) => {
                lock(&project.state).last_error = Some(format!("dream schedule: {e}"));
                self.publish();
                return;
            }
        };
        if project.dream.set_schedule_view(view) {
            self.publish();
        }
        if let Check::Run(plan) = check
            && !self.inner.host.paused()
            && !project.dream.running()
            && project.dream.gate().holder().is_none()
        {
            let _ = self.start_dream(project, Trigger::Scheduled(plan));
        }
    }

    /// One ask, start to end, on its own thread: the memory read from a
    /// snapshot, the model run as a utility session (cancelled by Stop
    /// Everything; an answer that arrives after a pause began is dropped),
    /// the reply cleaned. Writes nothing to the store.
    fn answer_ask(
        &self,
        project: &Project,
        question: &str,
        choice: &ModelChoice,
        gate: &crate::gate::RunGate,
    ) -> AskState {
        let host = &self.inner.host;
        if let Some(reason) = host.capability(choice).reason() {
            return AskState::Failed(format!("{}: {reason}", choice.provider.label()));
        }
        let view = self.project_view(project);
        let now = host.now().utc_ms;
        // The pages are redacted as stored: a secret known only since is
        // hidden in the material and the question (as `connecto memory`
        // hides it from what it prints), never in the instructions.
        let secrets = host.known_secrets();
        let built = Reader::open(&project.paths.db(), &project.id).and_then(|reader| {
            let snap = reader.snapshot()?;
            let read = snap.read();
            let (material, _) = crate::ask::material(&read, &view, question, now)?;
            Ok((
                crate::ask::prompt_hiding(question, &material, &secrets),
                read.version()?,
            ))
        });
        let (prompt, version) = match built {
            Ok(x) => x,
            Err(e) => return AskState::Failed(format!("the memory could not be read: {e}")),
        };
        let job = crate::host::ModelJob {
            project_id: project.id.clone(),
            cycle: None,
            choice: choice.clone(),
            prompt,
        };
        let reply = host.run_model(
            &job,
            Instant::now() + crate::ask::ASK_DEADLINE,
            gate.cancel_flag(),
        );
        let reply = match reply {
            Ok(r) => r,
            Err(crate::host::RunError::Cancelled) => {
                return AskState::Failed("cancelled".to_owned());
            }
            Err(crate::host::RunError::Timeout) => {
                return AskState::Failed("no answer in time".to_owned());
            }
            Err(e) => {
                return AskState::Failed(crate::render::clean(&e.to_string()));
            }
        };
        match gate.closed() {
            Some(crate::gate::Closed::Cancelled) => {
                return AskState::Failed("cancelled".to_owned());
            }
            Some(crate::gate::Closed::Paused) => {
                return AskState::Failed(ASK_PAUSED.to_owned());
            }
            None => {}
        }
        let secrets = host.known_secrets();
        let answered = Reader::open(&project.paths.db(), &project.id).and_then(|reader| {
            let snap = reader.snapshot()?;
            let read = snap.read();
            Ok(crate::ask::answer(&reply.text, &secrets, &|t| {
                crate::ask::address_of(&read, t)
            }))
        });
        match answered {
            Ok(Some(a)) => AskState::Answered {
                text: a.text,
                used: a.used,
                version,
            },
            Ok(None) => AskState::Failed("the model gave no answer".to_owned()),
            Err(e) => AskState::Failed(format!("the memory could not be read: {e}")),
        }
    }

    /// Keep `report` for the view, stamped now.
    fn keep_report(&self, project: &Project, report: &DreamReport) {
        let at = self.inner.host.now().utc_ms;
        lock(&project.state).last_report = Some(Arc::new(report.to_view(at)));
    }

    /// Ask the host again why git is not (fully) used for the project. Called
    /// where git was used anyway (start, after a dream or a build), never per
    /// view refresh nor on the UI thread.
    fn note_git(&self, project: &Project) {
        let note = crate::host::git_note(self.inner.host.as_ref(), &project.path);
        lock(&project.state).git_note = note;
    }

    /// Record a statement from the Connecto window on the control thread
    /// ([`crate::user_statement`]): refused as `Disabled` once the project
    /// is stopping, as `Busy` when an Accept or Reject about a memory finds
    /// the publication gate held (taken here, never on the UI thread, and
    /// held until the writer is done). Its one final state goes to the view.
    fn record_statement(
        &self,
        project: &Project,
        writer: &WriterHandle,
        job: crate::user_statement::Job,
    ) {
        let crate::user_statement::Job { id, statement } = job;
        let stopping = self.inner.stopping.load(Ordering::SeqCst) || lock(&project.live).is_none();
        let state = if stopping || !lock(&project.settings).enabled {
            StatementState::Refused(StatementRefusal::Disabled)
        } else {
            let guard = if crate::user_statement::publishes(&statement) {
                project.dream.gate().try_acquire(Cycle::Dream).map(Some)
            } else {
                Some(None)
            };
            match guard {
                None => StatementState::Refused(StatementRefusal::Busy),
                Some(guard) => {
                    let host = self.inner.host.as_ref();
                    let secrets = host.known_secrets();
                    let now = host.now().utc_ms;
                    let state = match crate::user_statement::Stated::redact(&statement, &secrets) {
                        Ok(stated) => crate::user_statement::final_state(
                            &writer
                                .call(move |s| s.user_statement(&stated, now, &secrets))
                                .and_then(|r| r),
                        ),
                        Err(why) => StatementState::Refused(StatementRefusal::Other(why)),
                    };
                    drop(guard);
                    state
                }
            }
        };
        let mut st = lock(&project.state);
        if let Some(r) = st.statements.iter_mut().find(|r| r.id == id) {
            r.state = state;
        }
    }

    /// Start a dream on a thread of its own, holding the project's gate.
    fn start_dream(
        &self,
        project: &Arc<Project>,
        trigger: Trigger,
    ) -> std::result::Result<(), String> {
        let (_, writer, ..) = self.live_senders(&project.id)?;
        if project.dream.running() {
            return Err("a dream is already running".to_owned());
        }
        let guard = project
            .dream
            .gate()
            .try_acquire(Cycle::Dream)
            .ok_or_else(|| "an initial build is running".to_owned())?;
        let generation = self.inner.cancel_gen.load(Ordering::SeqCst);
        let (engine, p) = (self.clone(), Arc::clone(project));
        project.dream.spawn(
            format!("memory-dream-{}", project.id),
            false,
            move |cancel| {
                let _guard = guard;
                if engine.inner.cancel_gen.load(Ordering::SeqCst) != generation {
                    cancel.store(true, Ordering::SeqCst);
                }
                let settings = lock(&p.settings).dream.clone();
                let cx = dream::Context {
                    host: Arc::clone(&engine.inner.host),
                    writer: &writer,
                    project_id: &p.id,
                    project_path: &p.path,
                    settings: &settings,
                    cancel,
                    crash: None,
                    before_publish: None,
                };
                match dream::run(&cx, &trigger) {
                    Ok(report) => {
                        if report.outcome.is_some() {
                            if let Some(RunOutcome::Failed { reason }) = &report.outcome {
                                lock(&p.state).last_error = Some(format!("dream: {reason}"));
                            }
                            engine.keep_report(&p, &report);
                            p.dream.set_report(report);
                            engine.note_git(&p);
                        }
                    }
                    Err(e) => lock(&p.state).last_error = Some(format!("dream: {e}")),
                }
                // Not while the project is being stopped (its stopper may
                // hold the project list and is waiting for this thread).
                if lock(&p.live).is_some() {
                    engine.refresh(&p, &writer);
                    engine.check_schedule_now();
                }
            },
        )
    }
}

/// Answer a UI read on the control thread.
/// `brief`: the project's view and the time, for [`ReadRequest::Brief`].
fn answer(
    reader: Option<&Reader>,
    request: &ReadRequest,
    brief: Option<&(ProjectMemoryView, u64)>,
) -> ReadState {
    let Some(reader) = reader else {
        return ReadState::Failed("the memory could not be read".to_owned());
    };
    let result = reader.snapshot().and_then(|snap| {
        let read = snap.read();
        match request {
            ReadRequest::Search {
                query,
                room,
                scope,
                all,
            } => {
                let mut q = SearchQuery::new(query);
                q.room = *room;
                q.scope.clone_from(scope);
                q.all = *all;
                read.search(&q).map(ReadAnswer::Search)
            }
            ReadRequest::Open(lookup) => read.detail(lookup),
            ReadRequest::Room(room) => read.room(*room).map(ReadAnswer::Room),
            ReadRequest::Brief => match brief {
                Some((view, now_ms)) => {
                    crate::commands::window_brief(&read, view, *now_ms).map(ReadAnswer::Brief)
                }
                None => Ok(ReadAnswer::NotFound),
            },
        }
    });
    match result {
        Ok(answer) => ReadState::Ready(answer),
        Err(e) => ReadState::Failed(e.to_string()),
    }
}

impl MemoryControl for Engine {
    fn view(&self) -> Arc<MemoryView> {
        self.inner.view.load_full()
    }

    fn set_settings(
        &self,
        project_id: &str,
        settings: MemorySettings,
    ) -> std::result::Result<(), String> {
        let project = self
            .project(project_id)
            .ok_or_else(|| "that project is not open".to_owned())?;
        check_choice(&settings.development.choice)?;
        check_choice(&settings.dream.choice)?;
        let settings = settings.clamped();
        self.inner
            .ops
            .try_send(Op::Settings(project_id.to_owned(), settings.clone()))
            .map_err(|_| "memory is busy; try again".to_owned())?;
        *lock(&project.settings) = settings;
        self.publish();
        Ok(())
    }

    fn plan_bootstrap(
        &self,
        project_id: &str,
        options: BootstrapOptions,
    ) -> std::result::Result<(), String> {
        self.plan_bootstrap_for(project_id, options)
    }

    fn bootstrap(
        &self,
        project_id: &str,
        options: BootstrapOptions,
    ) -> std::result::Result<(), String> {
        self.bootstrap_for(project_id, options)
    }

    fn cancel_bootstrap(&self, project_id: &str) {
        self.cancel_bootstrap_for(project_id);
    }

    fn resume_bootstrap(&self, project_id: &str) -> std::result::Result<(), String> {
        self.resume_bootstrap_for(project_id)
    }

    fn run_dream_now(&self, project_id: &str) -> std::result::Result<(), String> {
        if self.inner.host.paused() {
            return Err("paused".to_owned());
        }
        self.dream_now(project_id)
    }

    fn revert(&self, project_id: &str, version: u64) -> std::result::Result<(), String> {
        self.revert_version(project_id, version)
    }

    fn export(&self, project_id: &str, format: ExportFormat) -> std::result::Result<(), String> {
        let (_, _, control, _) = self.live_senders(project_id)?;
        control
            .try_send(Cmd::Export(format))
            .map_err(|_| "memory is busy; try again".to_owned())
    }

    fn request_read(
        &self,
        project_id: &str,
        request: ReadRequest,
    ) -> std::result::Result<u64, String> {
        let (project, _, control, _) = self.live_senders(project_id)?;
        let id = self.inner.next_read.fetch_add(1, Ordering::SeqCst);
        {
            let mut state = lock(&project.state);
            state.reads.insert(
                0,
                ReadResult {
                    id,
                    request: request.clone(),
                    state: ReadState::Pending,
                },
            );
            state.reads.truncate(MAX_READS);
        }
        if control.try_send(Cmd::Read(id, request)).is_err() {
            lock(&project.state).reads.retain(|r| r.id != id);
            return Err("memory is busy; try again".to_owned());
        }
        self.publish();
        Ok(id)
    }

    /// Checked at once; answered on a thread of its own (a model call can
    /// take a minute). The answer appears in the view's `asks` under the
    /// returned id. Nothing is written to the store.
    fn ask(&self, project_id: &str, request: AskRequest) -> std::result::Result<u64, String> {
        use connecto_core::memory::{MAX_ASK_BYTES, MAX_PENDING_ASKS};
        let (project, ..) = self.live_senders(project_id)?;
        if !lock(&project.settings).enabled {
            return Err("memory is off for this project".to_owned());
        }
        if self.inner.host.paused() {
            return Err("Connecto is paused".to_owned());
        }
        let question = request.question.trim().to_owned();
        if question.is_empty() {
            return Err("the question is empty".to_owned());
        }
        if question.len() > MAX_ASK_BYTES {
            return Err(format!(
                "the question is too long (at most {MAX_ASK_BYTES} bytes)"
            ));
        }
        check_choice(&request.choice)?;
        let id = self.inner.next_ask.fetch_add(1, Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut state = lock(&project.state);
            if state.asks.iter().filter(|a| a.state.is_pending()).count() >= MAX_PENDING_ASKS {
                return Err(format!(
                    "{MAX_PENDING_ASKS} questions are already waiting for an answer"
                ));
            }
            state.asks.push(AskEntry {
                id,
                question: question.clone(),
                choice: request.choice.clone(),
                asked_ms: self.inner.host.now().utc_ms,
                state: AskState::Pending,
            });
            trim_asks(&mut state.asks);
            state.ask_cancels.push((id, Arc::clone(&cancel)));
        }
        let (engine, p, choice) = (self.clone(), Arc::clone(&project), request.choice);
        let spawned = std::thread::Builder::new()
            .name(format!("memory-ask-{id}"))
            .spawn(move || {
                let gate = crate::gate::RunGate::new(Arc::clone(&engine.inner.host), cancel);
                let state = engine.answer_ask(&p, &question, &choice, &gate);
                let enabled = lock(&p.settings).enabled;
                // Checked again under the state lock, which cancellation
                // takes to set the flag ([`cancel_asks`]): an answer is
                // shown only if no Stop Everything, memory off, closing or
                // pause came while it was being made, up to this moment.
                let mut st = lock(&p.state);
                let state = match (state, gate.closed()) {
                    (AskState::Answered { .. }, Some(crate::gate::Closed::Cancelled)) => {
                        AskState::Failed("cancelled".to_owned())
                    }
                    (AskState::Answered { .. }, Some(crate::gate::Closed::Paused)) => {
                        AskState::Failed(ASK_PAUSED.to_owned())
                    }
                    (AskState::Answered { .. }, None) if !enabled => {
                        AskState::Failed("memory was turned off".to_owned())
                    }
                    (state, _) => state,
                };
                st.ask_cancels.retain(|(i, _)| *i != id);
                if let Some(a) = st.asks.iter_mut().find(|a| a.id == id) {
                    a.state = state;
                }
                drop(st);
                engine.publish();
            });
        if spawned.is_err() {
            let mut st = lock(&project.state);
            st.ask_cancels.retain(|(i, _)| *i != id);
            st.asks.retain(|a| a.id != id);
            return Err("the question could not be started".to_owned());
        }
        self.publish();
        Ok(id)
    }

    /// Checked at once and queued for the project's control thread; the
    /// result appears in the view's `statements` under the returned id.
    /// Allowed during a pause (the user's own act starts no model run).
    fn user_statement(
        &self,
        project_id: &str,
        statement: UserStatement,
    ) -> std::result::Result<u64, String> {
        crate::user_statement::check(&statement)?;
        let (project, _, control, _) = self.live_senders(project_id)?;
        if !lock(&project.settings).enabled {
            return Err("memory is off for this project".to_owned());
        }
        let id = self.inner.next_statement.fetch_add(1, Ordering::SeqCst);
        {
            let mut state = lock(&project.state);
            state.statements.insert(
                0,
                StatementResult {
                    id,
                    subject: statement.about.clone(),
                    kind: statement.kind,
                    state: StatementState::Pending,
                },
            );
            connecto_core::memory::trim_statements(&mut state.statements);
        }
        let job = crate::user_statement::Job { id, statement };
        if control.try_send(Cmd::Statement(Box::new(job))).is_err() {
            lock(&project.state).statements.retain(|r| r.id != id);
            return Err("memory is busy; try again".to_owned());
        }
        self.publish();
        Ok(id)
    }

    fn delete_memory(&self, project_id: &str) -> std::result::Result<(), String> {
        if self.project(project_id).is_none() {
            return Err("that project is not open".to_owned());
        }
        self.inner
            .ops
            .try_send(Op::Delete(project_id.to_owned()))
            .map_err(|_| "memory is busy; try again".to_owned())?;
        // Where deletion is not supported the files stay (see
        // `delete_memory_files`): memory is turned off and closed, and the
        // user is told which folder to remove.
        if DELETION_SUPPORTED {
            Ok(())
        } else {
            Err(deletion_refusal(&self.inner.data_root, project_id).to_string())
        }
    }
}

impl MemoryBridge for Engine {
    fn command(
        &self,
        project_id: &str,
        caller: &MemoryCaller,
        command: &MemoryCommand,
        text: Option<&str>,
    ) -> String {
        let Ok((project, writer, control, develop)) = self.live_senders(project_id) else {
            return NOT_ENABLED.to_owned();
        };
        if !lock(&project.settings).enabled {
            return NOT_ENABLED.to_owned();
        }
        let reader = match Reader::open(&project.paths.db(), project_id) {
            Ok(r) => r,
            Err(e) => return format!("project memory could not be read: {e}\n"),
        };
        let view = self.project_view(&project);
        let now = self.inner.host.now().utc_ms;
        let secrets = self.inner.host.known_secrets();
        let out = crate::commands::execute(
            &writer, &reader, &view, caller, command, text, &secrets, now,
        );
        if out.proposed {
            let _ = develop.try_send(DevMsg::Proposed);
            let _ = control.try_send(Cmd::Refresh);
        }
        out.text
    }

    /// A project the supervisor has not picked up yet (terminals restored at
    /// start can launch first) is answered from its saved settings.
    fn enabled(&self, project_id: &str) -> bool {
        match self.project(project_id) {
            Some(p) => lock(&p.settings).enabled,
            None => {
                safe_project_id(project_id) && self.inner.host.load_settings(project_id).enabled
            }
        }
    }

    /// The first call for a terminal records the version it starts from (it
    /// will run `brief` anyway); later calls announce each newer version
    /// once, several pending versions as one notice.
    fn take_version_notice(&self, project_id: &str, terminal: &str) -> Option<u64> {
        let project = self.project(project_id)?;
        if !lock(&project.settings).enabled {
            return None;
        }
        let version = lock(&project.state).data.version;
        let mut announced = lock(&project.announced);
        match announced.insert(terminal.to_owned(), version) {
            Some(seen) if version > seen => Some(version),
            Some(seen) => {
                announced.insert(terminal.to_owned(), seen.max(version));
                None
            }
            None => None,
        }
    }
}
