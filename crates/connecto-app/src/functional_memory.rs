//! Functional memory (ADR 0033): each project's `.functionalMemory/` folder,
//! a few Markdown files saying what the project does, kept up to date when
//! the main branch advances.
//!
//! A watcher thread looks at every open project's main branch (`main`, else
//! `master`) every [`POLL`]. When main moved to a commit whose changes since
//! the one `state.json` describes touch more than `.functionalMemory/`, an
//! update is due; it starts once main has been still for [`DEBOUNCE`]. One
//! run per project at a time. The run is a utility session (ADR 0025), read
//! only: it reads the project and replies with the files; this host checks
//! and writes them, then commits only `.functionalMemory/` with a
//! pathspec-limited commit. Main not checked out, a merge or rebase in
//! progress, or Pause: the update waits (deferred) and is tried again.
//!
//! The first build is a full one: on the first move of main seen while the
//! app runs, or by hand. A project with no memory is not built at start up.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use connecto_core::WorkspaceControl;
use connecto_core::functional_memory::{
    EFFORT, FOLDER, FmFile, FmFiles, FmModel, FmState, FmStatus, FunctionalMemoryControl,
    INDEX_FILE, MAX_FILE_BYTES, MAX_FILES, MAX_TOTAL_BYTES, STATE_FILE, default_model,
    valid_doc_name,
};
use connecto_session::Wake;
use connecto_session::launch::Layout;
use connecto_session::utility_run::{UtilityJob, UtilityPurpose, UtilityRunner};
use connecto_workspace::Workspace;
use serde_json::{Value, json};

/// How often main is looked at.
pub const POLL: Duration = Duration::from_secs(20);
/// How long main must stay still before an automatic update starts.
pub const DEBOUNCE: Duration = Duration::from_secs(90);
/// How long a full build may take, and an update.
const FULL_LIMIT: Duration = Duration::from_secs(15 * 60);
const UPDATE_LIMIT: Duration = Duration::from_secs(10 * 60);
/// How long one git command may take.
const GIT_WAIT: Duration = Duration::from_secs(30);
/// Most bytes of one git command's output kept; past it the command counts
/// as failed.
const GIT_MAX: usize = 8 * 1024 * 1024;
/// Most bytes of the diff put in an update's prompt; past it, only the list
/// of changed files, and the session reads what it needs.
const MAX_DIFF: usize = 96 * 1024;
/// Most commits listed in an update's prompt.
const MAX_LOG: usize = 200;
/// Most documents the model may keep (`state.json` is the last file).
const MAX_DOCS: usize = MAX_FILES - 1;
/// Room kept for `state.json` in the total.
const STATE_ROOM: usize = 1024;
/// The commit message, before the short hash.
pub const MESSAGE: &str = "Functional memory: update to";

/// Work in progress that a commit would land in the middle of.
const IN_PROGRESS: [&str; 6] = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "rebase-merge",
    "rebase-apply",
];

/// What the host needs from the rest of the app; faked in tests.
pub trait FmEnv: Send + Sync {
    /// The open projects: id and folder.
    fn projects(&self) -> Vec<(String, PathBuf)>;
    /// Whether Connecto is paused (nothing starts then).
    fn paused(&self) -> bool;
    /// The models of the CLIs that can run utility sessions in the project
    /// now (installed and signed in). Blocking: asks each CLI.
    fn offered(&self, project_id: &str, project: &Path) -> Vec<FmModel>;
    /// One read-only utility session on `model`; its reply, or why none.
    ///
    /// # Errors
    /// Why there is no reply.
    fn ask(
        &self,
        project_id: &str,
        project: &Path,
        model: &FmModel,
        prompt: String,
        limit: Duration,
        cancel: &AtomicBool,
    ) -> Result<String, String>;
}

/// The app's own environment.
struct AppEnv {
    layout: Layout,
    workspace: Arc<Workspace>,
    runner: Arc<UtilityRunner>,
}

/// The Claude models offered: the chooser's, without `opusplan` (a plan
/// mode, not a model of its own).
fn claude_models() -> Vec<String> {
    connecto_core::ToolKind::Claude
        .suggested_models()
        .iter()
        .filter(|m| **m != "opusplan")
        .map(|m| (*m).to_owned())
        .collect()
}

impl FmEnv for AppEnv {
    fn projects(&self) -> Vec<(String, PathBuf)> {
        self.workspace
            .view()
            .projects
            .iter()
            .filter(|p| p.error.is_none())
            .map(|p| (p.id.clone(), PathBuf::from(&p.path)))
            .collect()
    }

    fn paused(&self) -> bool {
        self.workspace.pause_switch().is_paused()
    }

    fn offered(&self, project_id: &str, project: &Path) -> Vec<FmModel> {
        use connecto_core::memory::{Capability, Provider};
        let mut out = Vec::new();
        for provider in [Provider::Claude, Provider::Codex] {
            if connecto_session::utility_run::capability(&self.layout, project, provider)
                != Capability::Available
            {
                continue;
            }
            let models = match provider {
                Provider::Claude => claude_models(),
                // Codex's own list (its model cache), as a new terminal of
                // the project offers it, else the chooser's.
                Provider::Codex => self
                    .workspace
                    .view()
                    .projects
                    .iter()
                    .find(|p| p.id == project_id)
                    .and_then(|p| p.terminals.clone())
                    .and_then(|r| r.offered_models(connecto_core::ToolKind::Codex))
                    .unwrap_or_else(|| {
                        connecto_core::ToolKind::Codex
                            .suggested_models()
                            .iter()
                            .map(|m| (*m).to_owned())
                            .collect()
                    }),
            };
            out.extend(
                models
                    .iter()
                    .map(|m| FmModel::new(provider, m))
                    .filter(FmModel::is_valid),
            );
        }
        out
    }

    fn ask(
        &self,
        project_id: &str,
        project: &Path,
        model: &FmModel,
        prompt: String,
        limit: Duration,
        cancel: &AtomicBool,
    ) -> Result<String, String> {
        let job = UtilityJob {
            project_id: project_id.to_owned(),
            project: project.to_owned(),
            // The project's own choice (Project Memory), not the titles'.
            choice: connecto_core::memory::ModelChoice::new(model.provider, &model.model, EFFORT),
            prompt,
            purpose: UtilityPurpose::FunctionalMemory,
            label: "functional memory".to_owned(),
        };
        self.runner
            .run(&job, Instant::now() + limit, cancel)
            .map(|reply| reply.text)
            .map_err(|err| err.to_string())
    }
}

/// Start the host and its watcher for the app.
pub fn start(layout: &Layout, workspace: &Arc<Workspace>, wake: &Wake) -> Arc<FmHost> {
    let runner = Arc::new(UtilityRunner::new(layout.clone(), Arc::clone(wake)));
    let host = FmHost::new(
        layout.data_root.join("functional-memory"),
        Arc::new(AppEnv {
            layout: layout.clone(),
            workspace: Arc::clone(workspace),
            runner: Arc::clone(&runner),
        }),
        Some(Arc::clone(wake)),
        DEBOUNCE,
    );
    {
        let host = Arc::clone(&host);
        workspace.on_stop_everything(Arc::new(move || {
            host.cancel_runs();
            runner.cancel_all();
        }));
    }
    let weak = Arc::downgrade(&host);
    let _ = thread::Builder::new()
        .name("connecto-functional-memory".into())
        .spawn(move || {
            // A first look soon after start, so the window has a status.
            thread::sleep(Duration::from_secs(2));
            loop {
                let Some(host) = weak.upgrade() else { return };
                host.tick();
                drop(host);
                thread::sleep(POLL);
            }
        });
    host
}

/// How long what a project's CLIs offer is kept before they are asked again.
const OFFERED_FRESH: Duration = Duration::from_secs(10 * 60);

/// A project's choices, kept in `<data>/functional-memory/<id>.json`.
#[derive(Debug, Clone)]
struct Prefs {
    auto: bool,
    /// `None`: the default.
    model: Option<FmModel>,
}

/// One project as the host follows it.
#[derive(Debug, Default)]
struct Proj {
    /// Main's commit when last looked at, and since when it was so.
    seen: Option<(String, Instant)>,
    /// An update is due (main moved past what the memory describes).
    due: bool,
    /// Asked by hand: runs without waiting for main to be still, and is
    /// kept while the update is deferred.
    manual: bool,
    running: bool,
    state: FmState,
    message: Option<String>,
    /// What main's committed `state.json` says, as last read by the
    /// watcher or a run (the window never waits on git).
    commit: Option<String>,
    updated_ms: Option<u64>,
}

pub struct FmHost {
    me: Weak<FmHost>,
    /// Where the per-project switches are kept.
    dir: PathBuf,
    env: Arc<dyn FmEnv>,
    wake: Option<Wake>,
    projects: Mutex<HashMap<String, Proj>>,
    /// The per-project switches, read once.
    prefs: Mutex<HashMap<String, Prefs>>,
    /// Held across each change of a project's choices, from reading them to
    /// publishing the file: two changes never lose one another.
    prefs_write: Mutex<()>,
    /// What each project's CLIs offer, and when it was asked.
    offered: Mutex<HashMap<String, (Instant, Vec<FmModel>)>>,
    /// One look at a time: deciding to run and claiming the run are one step.
    ticking: Mutex<()>,
    /// Repositories with a run in flight: two projects of one repository
    /// never write its folder at once.
    busy_roots: Mutex<HashSet<PathBuf>>,
    /// Bumped to cancel the runs in flight.
    epoch: AtomicU64,
    debounce: Duration,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

enum Outcome {
    Deferred(String),
    Failed(String),
}

/// What a run starts from.
struct Start {
    id: String,
    folder: PathBuf,
    root: PathBuf,
    branch: &'static str,
    main: String,
    described: Option<String>,
    manual: bool,
}

impl FmHost {
    pub fn new(
        dir: PathBuf,
        env: Arc<dyn FmEnv>,
        wake: Option<Wake>,
        debounce: Duration,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            dir,
            env,
            wake,
            projects: Mutex::new(HashMap::new()),
            prefs: Mutex::new(HashMap::new()),
            prefs_write: Mutex::new(()),
            offered: Mutex::new(HashMap::new()),
            ticking: Mutex::new(()),
            busy_roots: Mutex::new(HashSet::new()),
            epoch: AtomicU64::new(0),
            debounce,
        })
    }

    fn woke(&self) {
        if let Some(wake) = &self.wake {
            wake();
        }
    }

    /// Stop Everything: the runs in flight end without writing.
    pub fn cancel_runs(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    fn folder_of(&self, project_id: &str) -> Option<PathBuf> {
        self.env
            .projects()
            .into_iter()
            .find(|(id, _)| id == project_id)
            .map(|(_, path)| path)
    }

    fn switch_file(&self, project_id: &str) -> Option<PathBuf> {
        (!project_id.is_empty()
            && project_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .then(|| self.dir.join(format!("{project_id}.json")))
    }

    /// The project's choices, read from its file once.
    fn prefs(&self, project_id: &str) -> Prefs {
        if let Some(prefs) = lock(&self.prefs).get(project_id) {
            return prefs.clone();
        }
        let v = self
            .switch_file(project_id)
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or(Value::Null);
        let model = v.get("model").and_then(|m| {
            let provider =
                connecto_core::memory::Provider::from_word(m.get("provider")?.as_str()?)?;
            Some(FmModel::new(provider, m.get("model")?.as_str()?)).filter(FmModel::is_valid)
        });
        let prefs = Prefs {
            auto: v.get("auto").and_then(Value::as_bool).unwrap_or(true),
            model,
        };
        // A change saved meanwhile wins over what was read before it.
        lock(&self.prefs)
            .entry(project_id.to_owned())
            .or_insert(prefs)
            .clone()
    }

    /// Change the project's choices and write them (both, always).
    fn save_prefs(&self, project_id: &str, f: impl FnOnce(&mut Prefs)) -> Result<Prefs, String> {
        let file = self
            .switch_file(project_id)
            .ok_or_else(|| "not a project id".to_owned())?;
        let _one = lock(&self.prefs_write);
        let mut prefs = self.prefs(project_id);
        f(&mut prefs);
        let model = prefs
            .model
            .as_ref()
            .map(|m| json!({ "provider": m.provider.word(), "model": m.model }));
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        // Whole or not at all: written beside it, then renamed over it.
        let text = json!({ "auto": prefs.auto, "model": model }).to_string();
        let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text)
            .and_then(|()| std::fs::rename(&tmp, &file))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                e.to_string()
            })?;
        lock(&self.prefs).insert(project_id.to_owned(), prefs.clone());
        Ok(prefs)
    }

    /// Whether automatic updates are on (the default).
    fn auto(&self, project_id: &str) -> bool {
        self.prefs(project_id).auto
    }

    /// What the project's CLIs offer, as last asked (`None`: not yet).
    fn offered_now(&self, project_id: &str) -> Option<Vec<FmModel>> {
        lock(&self.offered).get(project_id).map(|(_, m)| m.clone())
    }

    /// Ask the CLIs again when it was long ago. Blocking: the watcher's.
    fn refresh_offered(&self, project_id: &str, folder: &Path, now: Instant) {
        let fresh = lock(&self.offered)
            .get(project_id)
            .is_some_and(|(at, _)| now.duration_since(*at) < OFFERED_FRESH);
        if !fresh {
            let models = self.env.offered(project_id, folder);
            lock(&self.offered).insert(project_id.to_owned(), (now, models));
            self.woke();
        }
    }

    /// The model the project's updates run on: its choice while one of its
    /// CLIs offers it, else the default.
    fn model_for(&self, project_id: &str) -> Option<(FmModel, bool)> {
        let offered = self.offered_now(project_id)?;
        if let Some(chosen) = self.prefs(project_id).model
            && offered.contains(&chosen)
        {
            return Some((chosen, true));
        }
        default_model(&offered).map(|m| (m, false))
    }

    fn set(&self, project_id: &str, f: impl FnOnce(&mut Proj)) {
        f(lock(&self.projects)
            .entry(project_id.to_owned())
            .or_default());
        self.woke();
    }

    /// One look at every open project; starts the updates that are due.
    pub fn tick(&self) {
        let now = Instant::now();
        for (id, folder) in self.env.projects() {
            self.tick_one(&id, &folder, now);
        }
    }

    fn tick_one(&self, id: &str, folder: &Path, now: Instant) {
        let _one = lock(&self.ticking);
        if lock(&self.projects).get(id).is_some_and(|p| p.running) {
            return;
        }
        let place = repo_root(folder).and_then(|root| main_branch(&root).map(|m| (root, m)));
        let Some((root, (branch, main))) = place else {
            return self.set(id, |p| {
                if p.manual {
                    p.manual = false;
                    p.state = FmState::Failed;
                    p.message = Some(
                        "the project is not a git repository with a main or master branch"
                            .to_owned(),
                    );
                }
            });
        };
        // Which CLIs can run here (asked every ten minutes, never by the
        // window).
        self.refresh_offered(id, folder, now);
        let auto = self.auto(id);
        // What main describes, whatever branch is checked out.
        let (described, updated_ms) = read_state_at(&root, branch);
        let (moved, first) = {
            let mut projects = lock(&self.projects);
            let p = projects.entry(id.to_owned()).or_default();
            p.commit.clone_from(&described);
            p.updated_ms = updated_ms;
            let first = p.seen.is_none();
            let moved = p.seen.as_ref().is_none_or(|(sha, _)| *sha != main);
            if moved {
                p.seen = Some((main.clone(), now));
            }
            (moved, first)
        };
        if moved {
            // Due when main has changes the memory does not describe yet. A
            // project with no memory is built on the first move seen, not
            // at start up.
            let due = match &described {
                None => !first,
                Some(commit) if *commit == main => false,
                Some(commit) => relevant_changes(&root, commit, &main).unwrap_or(true),
            };
            self.set(id, |p| {
                p.due = due;
                if !p.manual {
                    p.state = match (due, auto) {
                        (true, true) => FmState::Waiting,
                        (true, false) => FmState::Off,
                        (false, _) if described.is_some() => FmState::Idle,
                        (false, _) => FmState::None,
                    };
                }
            });
        }
        let (due, still, manual) = {
            let projects = lock(&self.projects);
            let p = projects.get(id);
            (
                p.is_some_and(|p| p.due),
                p.and_then(|p| p.seen.as_ref())
                    .is_some_and(|(_, since)| now.duration_since(*since) >= self.debounce),
                p.is_some_and(|p| p.manual),
            )
        };
        if !(manual || (auto && due && still)) {
            return;
        }
        let defer = |why: String| {
            self.set(id, |p| {
                p.state = FmState::Deferred;
                p.message = Some(why);
            });
        };
        if self.env.paused() {
            return defer("Connecto is paused".to_owned());
        }
        if let Err(why) = ready_to_commit(&root) {
            return defer(why);
        }
        // Claimed under the locks: one run per project, one per repository.
        {
            let mut roots = lock(&self.busy_roots);
            if roots.contains(&root) {
                drop(roots);
                return defer("another project of this repository is updating it".to_owned());
            }
            let mut projects = lock(&self.projects);
            let p = projects.entry(id.to_owned()).or_default();
            if p.running {
                return;
            }
            roots.insert(root.clone());
            p.running = true;
            p.manual = false;
            p.state = FmState::Running;
            p.message = Some(if described.is_some() {
                "updating from the latest changes on main".to_owned()
            } else {
                "building it from the code".to_owned()
            });
        }
        self.woke();
        let start = Start {
            id: id.to_owned(),
            folder: folder.to_owned(),
            root,
            branch,
            main,
            described,
            manual,
        };
        let Some(me) = self.me.upgrade() else {
            return self.finish(
                &start,
                Err(Outcome::Failed("Connecto is closing".to_owned())),
            );
        };
        let (id, root) = (start.id.clone(), start.root.clone());
        let spawned = thread::Builder::new()
            .name("connecto-functional-memory-run".into())
            .spawn(move || me.run(&start));
        if let Err(e) = spawned {
            self.set(&id, |p| {
                p.running = false;
                p.state = FmState::Failed;
                p.message = Some(format!("could not start: {e}"));
            });
            lock(&self.busy_roots).remove(&root);
        }
    }

    /// One update, start to commit, and its outcome recorded. Blocking.
    fn run(&self, start: &Start) {
        let epoch = self.epoch.load(Ordering::SeqCst);
        let outcome = self.update(start, epoch);
        self.finish(start, outcome);
    }

    fn finish(&self, start: &Start, outcome: Result<String, Outcome>) {
        let (commit, updated_ms) = read_state_at(&start.root, start.branch);
        self.set(&start.id, |p| {
            p.running = false;
            p.commit = commit;
            p.updated_ms = updated_ms;
            match outcome {
                Ok(text) => {
                    p.state = FmState::Idle;
                    p.message = Some(text);
                    p.due = false;
                    // Looked at again from scratch: our own commit, or main
                    // moved during the run.
                    p.seen = None;
                }
                Err(Outcome::Deferred(why)) => {
                    p.state = FmState::Deferred;
                    p.message = Some(why);
                    // A request by hand waits with it.
                    p.manual |= start.manual;
                }
                Err(Outcome::Failed(why)) => {
                    p.state = FmState::Failed;
                    p.message = Some(why);
                    // Not tried again until main moves again, or by hand.
                    p.due = false;
                }
            }
        });
        lock(&self.busy_roots).remove(&start.root);
    }

    fn update(&self, start: &Start, epoch: u64) -> Result<String, Outcome> {
        let (root, main) = (start.root.as_path(), start.main.as_str());
        let current = read_docs(root);
        let from = start
            .described
            .as_deref()
            .filter(|c| commit_exists(root, c));
        let (prompt, full) = match from {
            Some(from) if !current.is_empty() => (update_prompt(root, from, main, &current), false),
            _ => (full_prompt(), true),
        };
        let limit = if full { FULL_LIMIT } else { UPDATE_LIMIT };
        let cancel = AtomicBool::new(false);
        let (model, _) = self.model_for(&start.id).ok_or_else(|| {
            Outcome::Failed(
                "no CLI can run it here: install and sign in to Claude or Codex".to_owned(),
            )
        })?;
        let reply = self
            .env
            .ask(&start.id, &start.folder, &model, prompt, limit, &cancel)
            .map_err(|e| Outcome::Failed(format!("the model gave no answer: {e}")))?;
        if self.epoch.load(Ordering::SeqCst) != epoch {
            return Err(Outcome::Failed("stopped by Stop Everything".to_owned()));
        }
        let changes = parse_reply(&reply).map_err(Outcome::Failed)?;
        let docs = apply(&current, &changes, full).map_err(Outcome::Failed)?;
        // The tree may have changed during the run.
        ready_to_commit(root).map_err(Outcome::Deferred)?;
        // A commit made while the model worked (a correction to these very
        // files, say) is not undone by a reply written from before it: the
        // reply is dropped, and the next look starts again from main.
        let head = git(root, &["rev-parse", "--verify", "-q", "HEAD"]).unwrap_or_default();
        if head.trim() != main {
            return Err(Outcome::Deferred(
                "main moved while the model worked: starting again from it".to_owned(),
            ));
        }
        if self.env.paused() {
            return Err(Outcome::Deferred("Connecto is paused".to_owned()));
        }
        // `ready_to_commit` just found the folder as `HEAD` has it (nothing
        // staged, changed or untracked there), so `HEAD` is what a failure
        // puts back.
        let short = &main[..main.len().min(7)];
        let done = write_folder(root, &current, &docs, main)
            .and_then(|()| commit(root, &format!("{MESSAGE} {short}")));
        match done {
            Ok(true) => Ok(format!("committed the update to {short}")),
            Ok(false) => Ok(format!("already up to date with {short}")),
            Err(e) => {
                let undone = restore(root, &docs);
                Err(Outcome::Failed(match undone {
                    Ok(()) => format!("not committed, and put back as it was: {e}"),
                    Err(also) => format!("not committed: {e}; putting it back failed too: {also}"),
                }))
            }
        }
    }
}

impl FunctionalMemoryControl for FmHost {
    /// From what the watcher and the runs last saw: no git here, the
    /// window calls it every frame.
    fn status(&self, project_id: &str) -> FmStatus {
        let auto = self.auto(project_id);
        let model = self.model_for(project_id);
        let models = self.offered_now(project_id).unwrap_or_default();
        let projects = lock(&self.projects);
        let p = projects.get(project_id);
        let commit = p.and_then(|p| p.commit.clone());
        let mut state = p.map_or(FmState::None, |p| p.state);
        if state == FmState::None && commit.is_some() {
            state = FmState::Idle;
        }
        FmStatus {
            state,
            commit,
            updated_ms: p.and_then(|p| p.updated_ms),
            message: p.and_then(|p| p.message.clone()),
            auto,
            chosen: self.prefs(project_id).model,
            model: model.map(|(m, _)| m),
            models_known: self.offered_now(project_id).is_some(),
            model_choice: true,
            models,
        }
    }

    fn files(&self, project_id: &str) -> Result<FmFiles, String> {
        let folder = self
            .folder_of(project_id)
            .ok_or_else(|| "that project is not open".to_owned())?;
        let root = repo_root(&folder).unwrap_or(folder);
        Ok(read_files(&root))
    }

    /// Returns at once: git is asked on a thread of its own.
    fn update_now(&self, project_id: &str) -> Result<(), String> {
        let folder = self
            .folder_of(project_id)
            .ok_or_else(|| "that project is not open".to_owned())?;
        if lock(&self.projects)
            .get(project_id)
            .is_some_and(|p| p.running)
        {
            return Err("an update is already running".to_owned());
        }
        self.set(project_id, |p| {
            p.manual = true;
            p.state = FmState::Waiting;
            p.message = Some("starting".to_owned());
        });
        let me = self.me.upgrade().ok_or("Connecto is closing")?;
        let id = project_id.to_owned();
        thread::Builder::new()
            .name("connecto-functional-memory-now".into())
            .spawn(move || me.tick_one(&id, &folder, Instant::now()))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn set_model(&self, project_id: &str, model: Option<FmModel>) -> Result<(), String> {
        if let Some(model) = &model {
            // Only what the project's CLIs offer: never a word of a peer's
            // own making passed to a CLI.
            if !self
                .offered_now(project_id)
                .is_some_and(|offered| offered.contains(model))
            {
                return Err(format!("{} is not offered here", model.label()));
            }
        }
        self.save_prefs(project_id, |p| p.model = model)?;
        self.woke();
        Ok(())
    }

    fn set_auto(&self, project_id: &str, on: bool) -> Result<(), String> {
        self.save_prefs(project_id, |p| p.auto = on)?;
        self.set(project_id, |p| {
            if p.due && !p.running && !p.manual {
                p.state = if on { FmState::Waiting } else { FmState::Off };
            }
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Git.

/// What one git command printed, when it ended well. The repository's own
/// hooks and fsmonitor never run, external diff and textconv drivers are off
/// wherever a diff is asked, and git never prompts.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    // Read while it runs: a long diff would fill the pipe and stall it.
    let mut stdout = child.stdout.take();
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        let mut over = false;
        if let Some(pipe) = stdout.as_mut() {
            let mut buf = [0u8; 64 * 1024];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if out.len() + n > GIT_MAX {
                    over = true;
                } else {
                    out.extend_from_slice(&buf[..n]);
                }
            }
        }
        (out, over)
    });
    let deadline = Instant::now() + GIT_WAIT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("git {} took too long", args.join(" ")));
            }
            Err(e) => return Err(format!("git: {e}")),
        }
    };
    let (out, over) = reader.join().unwrap_or_default();
    if status.success() {
        if over {
            return Err(format!("git {}: output too large", args.join(" ")));
        }
        return String::from_utf8(out).map_err(|_| "git output is not text".to_owned());
    }
    let mut err = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut err);
    }
    Err(err
        .lines()
        .next()
        .map_or_else(|| format!("git {} failed", args.join(" ")), str::to_owned))
}

/// The repository's top folder, found without running git first (on a Mac
/// without the developer tools, running it opens an install prompt).
fn repo_root(folder: &Path) -> Option<PathBuf> {
    if !folder.ancestors().any(|dir| dir.join(".git").exists()) {
        return None;
    }
    git(folder, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|out| PathBuf::from(out.trim()))
        .filter(|p| !p.as_os_str().is_empty())
}

/// The main branch (`main`, else `master`) and its commit.
fn main_branch(root: &Path) -> Option<(&'static str, String)> {
    ["main", "master"].into_iter().find_map(|name| {
        git(
            root,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{name}^{{commit}}"),
            ],
        )
        .ok()
        .map(|sha| (name, sha.trim().to_owned()))
        .filter(|(_, sha)| !sha.is_empty())
    })
}

fn commit_exists(root: &Path, sha: &str) -> bool {
    sha.chars().all(|c| c.is_ascii_hexdigit())
        && git(root, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok()
}

/// Whether a commit can be made on main now; why not otherwise.
fn ready_to_commit(root: &Path) -> Result<(), String> {
    let (name, _) = main_branch(root).ok_or("the project has no main or master branch")?;
    let head = git(root, &["symbolic-ref", "-q", "HEAD"]).unwrap_or_default();
    if head.trim() != format!("refs/heads/{name}") {
        return Err(format!("waiting until {name} is checked out"));
    }
    let git_dir = git(root, &["rev-parse", "--absolute-git-dir"])?;
    let git_dir = Path::new(git_dir.trim());
    if IN_PROGRESS.iter().any(|f| git_dir.join(f).exists()) {
        return Err("waiting until the merge, rebase or cherry-pick in progress ends".to_owned());
    }
    if std::fs::symlink_metadata(root.join(FOLDER)).is_ok_and(|meta| !meta.is_dir()) {
        return Err(format!("{FOLDER} is not a plain folder"));
    }
    // The person's own changes there (staged, changed, new or ignored
    // files) are never written over, staged or committed: the update waits
    // for them.
    let changes = git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            // Ignored files too: git has no copy of them to put back.
            "--ignored=matching",
            "--",
            FOLDER,
        ],
    )?;
    if !changes.is_empty() {
        return Err(format!(
            "waiting until the changes in {FOLDER} are committed or dropped"
        ));
    }
    Ok(())
}

/// Whether `from..to` changes anything outside the folder.
fn relevant_changes(root: &Path, from: &str, to: &str) -> Result<bool, String> {
    if !commit_exists(root, from) {
        return Ok(true);
    }
    let out = git(
        root,
        &["diff", "--name-only", "--no-renames", "-z", from, to],
    )?;
    let prefix = format!("{FOLDER}/");
    Ok(out
        .split('\0')
        .any(|path| !path.is_empty() && !path.starts_with(&prefix)))
}

/// Commit the folder and nothing else. `Ok(false)`: nothing changed.
fn commit(root: &Path, message: &str) -> Result<bool, String> {
    let paths = ["--", FOLDER];
    git(root, &[&["add", "-A"][..], &paths].concat())?;
    if git(
        root,
        &[&["diff", "--cached", "--quiet"][..], &paths].concat(),
    )
    .is_ok()
    {
        return Ok(false);
    }
    git(
        root,
        &[
            &["commit", "-q", "--no-verify", "-m", message, "--only"][..],
            &paths,
        ]
        .concat(),
    )?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// The folder.

/// At most `max` of `bytes` as text, and whether there were more. A
/// character cut at the end is dropped; bytes that are not UTF-8 are none.
fn decode(mut bytes: Vec<u8>, max: usize) -> Option<(String, bool)> {
    let cut = bytes.len() > max;
    bytes.truncate(max);
    match String::from_utf8(bytes) {
        Ok(text) => Some((text, cut)),
        Err(err) if cut && err.utf8_error().error_len().is_none() => {
            let valid = err.utf8_error().valid_up_to();
            let mut bytes = err.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).ok().map(|text| (text, true))
        }
        Err(_) => None,
    }
}

/// The commit `state.json` names in `branch`'s committed tree, and when it
/// was written: what main describes, whatever is checked out and whatever
/// was written but not committed.
fn read_state_at(root: &Path, branch: &str) -> (Option<String>, Option<u64>) {
    let Ok(text) = git(
        root,
        &[
            "cat-file",
            "blob",
            &format!("refs/heads/{branch}:{FOLDER}/{STATE_FILE}"),
        ],
    ) else {
        return (None, None);
    };
    let Some(v) = (text.len() <= 4096)
        .then(|| serde_json::from_str::<Value>(&text).ok())
        .flatten()
    else {
        return (None, None);
    };
    let commit = v
        .get("commit")
        .and_then(Value::as_str)
        .filter(|c| c.len() >= 7 && c.len() <= 64 && c.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_owned);
    (commit, v.get("updatedMs").and_then(Value::as_u64))
}

/// Reads at most `max + 1` bytes: enough to tell a longer file.
type Reader<'a> = dyn FnMut(usize) -> Option<Vec<u8>> + 'a;

/// Most folder entries looked at.
const MAX_ENTRIES: usize = 1000;

/// Index first, then by name, `state.json` last.
fn order(name: &str) -> (bool, bool, String) {
    (name != INDEX_FILE, name == STATE_FILE, name.to_owned())
}

fn kept(name: &str) -> bool {
    valid_doc_name(name) || name == STATE_FILE
}

/// Reading the folder relative to open folder handles, never following a
/// link: `.functionalMemory` must be a real folder in the repository's top
/// folder, and each file a regular file in it, whatever is swapped in at
/// any moment.
#[cfg(unix)]
mod at {
    use std::os::fd::{AsRawFd, RawFd};
    use std::path::Path;

    use nix::dir::Dir;
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::{Mode, SFlag, fstat};
    use nix::unistd::{close, read};

    use super::{FOLDER, MAX_ENTRIES, Reader, kept, order};

    struct Fd(RawFd);

    impl Drop for Fd {
        fn drop(&mut self) {
            let _ = close(self.0);
        }
    }

    /// See [`super::walk`].
    pub(super) fn walk(root: &Path, f: &mut dyn FnMut(&str, &mut Reader<'_>) -> bool) {
        let Ok(top) = Dir::open(
            root,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) else {
            return;
        };
        let Ok(mut dir) = Dir::openat(
            Some(top.as_raw_fd()),
            FOLDER,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) else {
            return;
        };
        let fd = dir.as_raw_fd();
        let mut names: Vec<String> = dir
            .iter()
            .flatten()
            .take(MAX_ENTRIES)
            .filter_map(|e| e.file_name().to_str().ok().map(str::to_owned))
            .filter(|n| kept(n))
            .collect();
        names.sort_by_key(|n| order(n));
        for name in names {
            if !f(&name, &mut |max| read_one(fd, &name, max)) {
                return;
            }
        }
    }

    fn read_one(dir: RawFd, name: &str, max: usize) -> Option<Vec<u8>> {
        let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
        let fd = Fd(openat(Some(dir), name, flags, Mode::empty()).ok()?);
        let stat = fstat(fd.0).ok()?;
        if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
            return None;
        }
        let want = max.saturating_add(1);
        let mut bytes = Vec::new();
        let mut buf = [0_u8; 16 * 1024];
        while bytes.len() < want {
            let room = (want - bytes.len()).min(buf.len());
            let n = read(fd.0, &mut buf[..room]).ok()?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        Some(bytes)
    }
}

/// The folder's files (documents and `state.json`) in [`order`], one at a
/// time: `f` gets the name and a reader of at most a given number of bytes
/// (plus one), never through a link, and says whether to go on. Only the
/// names are held, never more than one file's bytes.
fn walk(root: &Path, mut f: impl FnMut(&str, &mut Reader<'_>) -> bool) {
    #[cfg(unix)]
    at::walk(root, &mut f);
    #[cfg(not(unix))]
    {
        // No folder handles here: the path checks only.
        use std::io::Read as _;
        let dir = root.join(FOLDER);
        if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            return;
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .map(|entries| entries.flatten().take(MAX_ENTRIES).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| kept(n))
            .collect();
        names.sort_by_key(|n| order(n));
        for name in names {
            let path = dir.join(&name);
            let mut reader = |max: usize| -> Option<Vec<u8>> {
                if !std::fs::symlink_metadata(&path).ok()?.is_file() {
                    return None;
                }
                let mut bytes = Vec::new();
                std::fs::File::open(&path)
                    .ok()?
                    .take(max as u64 + 1)
                    .read_to_end(&mut bytes)
                    .ok()?;
                Some(bytes)
            };
            if !f(&name, &mut reader) {
                return;
            }
        }
    }
}

/// The folder's documents for the prompt, by name: whole files only, within
/// the per-file and total limits.
fn read_docs(root: &Path) -> BTreeMap<String, String> {
    let mut docs = BTreeMap::new();
    let mut total = 0;
    walk(root, |name, read| {
        if name == STATE_FILE {
            return true;
        }
        if docs.len() >= MAX_DOCS {
            return false;
        }
        let cap = MAX_FILE_BYTES.min(MAX_TOTAL_BYTES - total);
        if let Some((text, false)) = read(cap).and_then(|bytes| decode(bytes, cap)) {
            total += text.len();
            docs.insert(name.to_owned(), text);
        }
        true
    });
    docs
}

/// The files handed to a peer: the index first, then by name, then
/// `state.json`; within the caps, never more than what is left read.
pub(crate) fn read_files(root: &Path) -> FmFiles {
    let mut out = FmFiles::default();
    let mut total = 0;
    walk(root, |name, read| {
        let left = MAX_TOTAL_BYTES - total;
        if out.files.len() >= MAX_FILES || left == 0 {
            out.truncated = true;
            return false;
        }
        let cap = MAX_FILE_BYTES.min(left);
        if let Some((text, cut)) = read(cap).and_then(|bytes| decode(bytes, cap)) {
            out.truncated |= cut;
            total += text.len();
            out.files.push(FmFile {
                path: name.to_owned(),
                text,
            });
        }
        true
    });
    out
}

/// Put `text` at `name` in `dir`: written to a new temporary file there
/// (created exclusively, so nothing already at that name, a link included,
/// is opened), then renamed over the name, which replaces a link rather
/// than following it.
fn put_file(dir: &Path, name: &str, text: &str) -> Result<(), String> {
    use std::io::Write as _;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("{name}: {e}"))?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let moved = written.and_then(|()| std::fs::rename(&tmp, dir.join(name)));
    if let Err(e) = moved {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{name}: {e}"));
    }
    Ok(())
}

/// Remove `name` from `dir` when it is a regular file.
fn remove_file(dir: &Path, name: &str) -> Result<(), String> {
    let path = dir.join(name);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        std::fs::remove_file(&path).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

fn plain_folder(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join(FOLDER);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if !meta.is_dir() => Err(format!("{FOLDER} is not a plain folder")),
        Ok(_) => Ok(dir),
        Err(_) => std::fs::create_dir(&dir)
            .map(|()| dir)
            .map_err(|e| format!("{FOLDER}: {e}")),
    }
}

/// Write the documents (removing those dropped first) and `state.json`.
fn write_folder(
    root: &Path,
    current: &BTreeMap<String, String>,
    docs: &BTreeMap<String, String>,
    main: &str,
) -> Result<(), String> {
    let dir = plain_folder(root)?;
    for name in current.keys().filter(|n| !docs.contains_key(*n)) {
        remove_file(&dir, name)?;
    }
    for (name, text) in docs {
        if current.get(name) != Some(text) {
            put_file(&dir, name, text)?;
        }
    }
    let state = json!({
        "commit": main,
        "updatedMs": now_ms(),
        "about": "Written by Connecto: the commit of the main branch these files describe (ADR 0033).",
    });
    let state = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())? + "\n";
    put_file(&dir, STATE_FILE, &state)
}

/// After a write or commit that failed: the folder and its index entries
/// as `HEAD` has them, which is how they were just before the write (the
/// write starts only from a folder with no change of the person's).
fn restore(root: &Path, written: &BTreeMap<String, String>) -> Result<(), String> {
    git(root, &["reset", "-q", "--", FOLDER])?;
    let prefix = format!("{FOLDER}/");
    let in_head: HashSet<String> = git(
        root,
        &["ls-tree", "-z", "--name-only", "HEAD", "--", &prefix],
    )?
    .split('\0')
    .filter_map(|path| path.strip_prefix(&prefix))
    .map(str::to_owned)
    .collect();
    let dir = root.join(FOLDER);
    if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
        for name in written
            .keys()
            .map(String::as_str)
            .chain([STATE_FILE])
            .filter(|n| !in_head.contains(*n))
        {
            remove_file(&dir, name)?;
        }
    }
    if in_head.is_empty() {
        let _ = std::fs::remove_dir(&dir);
    } else {
        git(root, &["checkout", "-q", "HEAD", "--", FOLDER])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The model's work.

const ABOUT: &str = "\
You keep the functional memory of this software project: a few short Markdown files in `.functionalMemory/` that say what the project does. They are read by a voice assistant that talks with the project's owner about new features: it asks informed questions about the related existing features before a coding agent is started. So write for that reader: functional and user-facing (what it does, for whom, how it is used, how the parts fit), concise, no code dumps. Name a code place (a crate, a folder, a module) only where it helps find a feature. Never include secrets, keys, tokens or personal data.

Rules: do not create, edit or delete any file, and do not run builds, tests or other commands that change anything; read what you need. Connecto writes the files from your reply and commits them. All files together should stay under about 30 KB. File names are plain lowercase Markdown names at the top level (`overview.md`), except `README.md`. `state.json` is Connecto's own: never write it.";

const FORMAT: &str = "\
Reply with the files only, in exactly this format, with nothing outside the blocks:

<<<FILE README.md>>>
(the whole content of the file)
<<<END FILE>>>

One block per file, the whole file each time.";

fn full_prompt() -> String {
    format!(
        "{ABOUT}

Task: build the functional memory from the code. Read the project (its README, docs, main source folders, configuration) as much as you need. Write these files:
- README.md: the index. One paragraph on what the project is, then each other file with one line on what it holds.
- overview.md: purpose, who uses it, the problems it solves, platforms and how it is delivered.
- architecture.md: the main components and how they work together, in functional terms.
- features.md: the main user-facing features and functions: for each, what it does, how it is used, notable limits, and related features.
- flows.md: the key flows, end to end, step by step.
- glossary.md: the project's own terms, one line each.
Merge or add files when the project calls for it.

{FORMAT}"
    )
}

fn update_prompt(root: &Path, from: &str, to: &str, current: &BTreeMap<String, String>) -> String {
    let short = |s: &str| s[..s.len().min(7)].to_owned();
    let exclude = format!(":(exclude){FOLDER}");
    let range = format!("{from}..{to}");
    let log = git(
        root,
        &[
            "log",
            "--no-color",
            &format!("-n{MAX_LOG}"),
            "--format=%h %s",
            &range,
        ],
    )
    .unwrap_or_default();
    let diff_args = |what: &str| {
        git(
            root,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                what,
                from,
                to,
                "--",
                ".",
                &exclude,
            ],
        )
    };
    let stat = diff_args("--stat=200").unwrap_or_default();
    let diff = diff_args("--patch")
        .ok()
        .filter(|d| d.len() <= MAX_DIFF)
        .map_or_else(
            || "(too large to include: read the changed files listed above as you need)".to_owned(),
            |d| format!("```diff\n{d}\n```"),
        );
    let mut files = String::new();
    for (name, text) in current {
        files.push_str(&format!("<<<FILE {name}>>>\n{text}"));
        if !text.ends_with('\n') {
            files.push('\n');
        }
        files.push_str("<<<END FILE>>>\n\n");
    }
    format!(
        "{ABOUT}

Task: update the functional memory. It describes commit {old}; the main branch is now at {new}. Change only what these changes make out of date or missing: new or changed features, flows, terms; removed ones. Keep the rest as it is. Read the project's files where the diff is not enough.

Commits since then:
{log}
Changed files:
{stat}
Diff:
{diff}

The current files:

{files}{FORMAT}
Give only the files that change, each whole. To remove a file, give `<<<DELETE name.md>>>` on a line of its own. When nothing needs to change, reply only `<<<NO CHANGES>>>`.",
        old = short(from),
        new = short(to),
    )
}

/// What a reply asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Changes {
    pub write: BTreeMap<String, String>,
    pub delete: Vec<String>,
    pub none: bool,
}

/// The files of a reply, checked. Text outside the blocks (a fence, a
/// sentence) is ignored; a block that does not end, a name that is not a
/// plain Markdown file name, or a reply with no block is refused.
pub(crate) fn parse_reply(reply: &str) -> Result<Changes, String> {
    let mut changes = Changes::default();
    let mut open: Option<(String, String)> = None;
    for line in reply.lines() {
        let marker = line.trim();
        if let Some((name, text)) = &mut open {
            if marker == "<<<END FILE>>>" {
                let (name, text) = open.take().unwrap_or_default();
                if changes.write.insert(name.clone(), text).is_some() {
                    return Err(format!("the reply gives {name} twice"));
                }
            } else if marker.starts_with("<<<FILE ") {
                return Err(format!("{name} has no end in the reply"));
            } else {
                text.push_str(line);
                text.push('\n');
            }
            continue;
        }
        let named = |prefix: &str| {
            marker
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(">>>"))
                .map(str::trim)
        };
        if let Some(name) = named("<<<FILE ") {
            if !valid_doc_name(name) {
                return Err(format!(
                    "the reply names a file that is not allowed: {name:.80}"
                ));
            }
            open = Some((name.to_owned(), String::new()));
        } else if let Some(name) = named("<<<DELETE ") {
            if !valid_doc_name(name) {
                return Err(format!(
                    "the reply names a file that is not allowed: {name:.80}"
                ));
            }
            changes.delete.push(name.to_owned());
        } else if marker == "<<<NO CHANGES>>>" {
            changes.none = true;
        }
    }
    if let Some((name, _)) = open {
        return Err(format!("{name} has no end in the reply"));
    }
    if changes.write.is_empty() && changes.delete.is_empty() && !changes.none {
        return Err("the reply holds no files".to_owned());
    }
    Ok(changes)
}

/// The documents after the reply, within the limits. A full build replaces
/// the folder; an update changes what it names.
pub(crate) fn apply(
    current: &BTreeMap<String, String>,
    changes: &Changes,
    full: bool,
) -> Result<BTreeMap<String, String>, String> {
    let mut docs = if full {
        BTreeMap::new()
    } else {
        current.clone()
    };
    for name in &changes.delete {
        docs.remove(name);
    }
    for (name, text) in &changes.write {
        if text.len() > MAX_FILE_BYTES {
            return Err(format!("{name} is over {} KiB", MAX_FILE_BYTES / 1024));
        }
        docs.insert(name.clone(), text.clone());
    }
    // One file per name, whatever the case: on a case-insensitive disk
    // `readme.md` would land on `README.md`.
    let folded: HashSet<String> = docs.keys().map(|n| n.to_ascii_lowercase()).collect();
    if folded.len() != docs.len() {
        return Err("the reply names two files that differ only in case".to_owned());
    }
    if docs.is_empty() || !docs.contains_key(INDEX_FILE) {
        return Err(format!("the memory would have no {INDEX_FILE}"));
    }
    if docs.len() > MAX_DOCS {
        return Err(format!("the memory would have over {MAX_DOCS} files"));
    }
    let total: usize = docs.values().map(String::len).sum();
    if total + STATE_ROOM > MAX_TOTAL_BYTES {
        return Err(format!(
            "the memory would be over {} KiB",
            (MAX_TOTAL_BYTES - STATE_ROOM) / 1024
        ));
    }
    Ok(docs)
}

#[cfg(test)]
#[path = "functional_memory_tests.rs"]
mod tests;
