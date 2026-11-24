//! The terminal registry: one thread that owns the slots, the ids, the saved
//! list and the history of one project (ADR 0014).
//!
//! Add, close, rename, start and choosing a project all pass through here, so
//! no check-then-act spans two of them and nothing on the UI thread touches the
//! disk. Every method the UI and the bridge call returns promptly.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use connecto_core::{
    GridSize, HistoryEntry, MAX_TERMINALS, PaneControl, PaneStage, SaveState, TermActivity,
    TerminalEntry, TerminalSession, TerminalsControl, TerminalsView, ToolKind, Transcript,
};
use connecto_gate::Admitted;
use connecto_pty::Wake;
use connecto_store::Tool;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};

use crate::bridge::state::OpenAsk;
use crate::bridge::{AgentCommand, WriteOutcome};
use crate::history::{self, History, Record};
use crate::launch::{LaunchExtras, Layout, project_id, validated};
use crate::pane::{Begin, StartRefusal};
use crate::relay::{Shared, Slot};
use crate::terminal_list::{
    Entry, FILE, FsIo, List, ListIo, Loaded, OwedReply, clean_name, load, valid_effort,
    valid_model, valid_session,
};
use crate::turn::Phase;
use crate::utility::{self, Utility};
use crate::{INITIAL_SIZE, ManagedPane, PaneBridge, PaneSetup};

/// Builds the disk seam for a project folder.
pub(crate) type IoFactory = Box<dyn Fn(&Path) -> Box<dyn ListIo> + Send>;

const TICK: Duration = Duration::from_millis(250);
/// While unsaved, how often the whole list is written again.
pub(crate) const RETRY: Duration = Duration::from_secs(30);
/// How long a stopped pane must look stopped before `desired` follows it, so
/// the brief gap inside a restart is not saved.
const STOPPED_TICKS: u8 = 2;
/// After the registry asks for a start, a pane that still looks stopped is
/// given this long before `desired` follows it.
const START_GRACE: Duration = Duration::from_secs(3);
const QUEUE: usize = 64;
/// How long an update's quit waits for the bridge to list the open asks.
const OPEN_ASKS_WAIT: Duration = Duration::from_millis(500);
/// The most characters an automatic name takes from a first `--ask`.
const AUTO_NAME_CHARS: usize = 40;
/// The first message of a conversation resumed after an update cut its turn
/// (ADR 0030): "carry on", in the words the person uses.
pub const CONTINUE_AFTER_UPDATE: &str = "continua";

/// A terminal changed by an agent, as the reply reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub id: String,
    pub state: &'static str,
    /// False when a change of `desired` could not be written yet.
    pub saved: bool,
    /// What became of the terminal's removal. Only a close asks for one.
    pub removal: Removal,
}

/// A close stops a terminal, saves it to the history and takes it out of the
/// list (project-tree-ui R8). The removal can be done, or refused with a
/// reason, such as a list that cannot be written. A refused removal leaves the
/// terminal stopped and listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    /// Not part of this change.
    NotAsked,
    /// Removed, and the list saved.
    Done,
    /// Not removed, and why. The terminal is stopped and stays listed.
    Kept(String),
}

impl Removal {
    pub(crate) fn word(&self) -> Option<&'static str> {
        match self {
            Self::NotAsked => None,
            Self::Done => Some("done"),
            Self::Kept(_) => Some("kept"),
        }
    }
}

/// Why a change to the list was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TermError {
    #[error(
        "{MAX_TERMINALS} terminals are listed, stopped ones included. Close or start a stopped one instead"
    )]
    CapReached,
    #[error("the terminal list could not be saved, so nothing changed: {0}")]
    SaveFailed(String),
    #[error("no such terminal")]
    Unknown,
    #[error("that terminal is already running")]
    AlreadyRunning,
    #[error("that terminal already has a command waiting")]
    Busy,
    #[error("Connecto is paused by the user. Nothing was started.")]
    Paused,
    #[error("that terminal is running a login")]
    LoggingIn,
    #[error("Connecto is closing")]
    Shutdown,
    #[error("the registry did not answer in time")]
    Timeout,
    #[error("the terminal restarted after this was accepted. Nothing was done")]
    Restarted,
    #[error("{0}")]
    Name(String),
    /// The project's Tools setting leaves the CLI out.
    #[error("{0}")]
    NotAllowed(String),
}

/// What a close was admitted under, checked again by the registry right before it
/// stops anything: a command can wait in the queue while a Pause or a restart
/// comes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Admit {
    /// The pause epoch at admission. `None` is the delayed Stop of a close on
    /// the caller's own terminal, which keeps the documented exception: stopping
    /// is the safe direction, so it is not checked against Pause again.
    pub epoch: Option<u64>,
    /// The target's attempt at admission.
    pub attempt: u64,
}

/// What a pane's launches carry beyond the tool and the session.
#[derive(Debug, Clone, Default)]
struct Opening {
    /// The first message of a new conversation.
    prompt: Option<String>,
    /// The first message of a resumed one.
    note: Option<String>,
    /// The model to launch with, or the tool's default.
    model: Option<String>,
    effort: Option<String>,
}

/// The model and effort an agent's `connecto add` asks for, each the tool's
/// default when not given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AgentLaunch {
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
}

/// A terminal to create.
#[derive(Debug, Clone, Default)]
struct New {
    tool: Option<Tool>,
    name: Option<String>,
    parent: Option<String>,
    asked_by: Option<String>,
    /// The tool session to resume.
    session: Option<String>,
    /// The first message of a new conversation.
    prompt: Option<String>,
    /// The first message of a resumed one.
    note: Option<String>,
    /// The model to launch with, or the tool's default.
    model: Option<String>,
    effort: Option<String>,
    /// An Agentic Works job's terminal, or one a job's terminal started.
    job: bool,
}

enum Cmd {
    /// The app quits for an update: mark the turns it cuts, save, and stop
    /// following the panes, as a closing app does.
    MarkCutByUpdate {
        reply: Sender<bool>,
    },
    UserAdd {
        tool: Tool,
        name: Option<String>,
        /// The model the person chose, already checked.
        model: Option<String>,
    },
    /// A terminal that opens with a prompt, or picks a session up again.
    UserAddWith {
        tool: Tool,
        name: Option<String>,
        prompt: Option<String>,
        session: Option<String>,
        note: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        work_epoch: Option<u64>,
        /// The new terminal's id, or why there is none.
        reply: Sender<Result<String, String>>,
    },
    UserClose(String),
    /// A stop of background shells, done on a thread, refused: why, for the
    /// tab strip.
    BackgroundStopRefused {
        id: String,
        why: String,
    },
    Rename {
        id: String,
        name: Option<String>,
    },
    Restart(String),
    Continue(String),
    Transcript(String),
    /// A transcript read for a peer, answered without changing the view.
    ReadTranscript {
        history_id: String,
        reply: Sender<Result<String, String>>,
    },
    Add {
        tool: Tool,
        name: Option<String>,
        /// The model and effort the agent asked for, already checked.
        launch: AgentLaunch,
        /// The terminal whose agent asked, for the tree.
        caller: Option<String>,
        epoch: u64,
        reply: Sender<Result<Changed, TermError>>,
    },
    Start {
        id: String,
        epoch: u64,
        attempt: u64,
        reply: Sender<Result<Changed, TermError>>,
    },
    Close {
        id: String,
        admit: Admit,
        reply: Sender<Result<Changed, TermError>>,
    },
    /// The body of an `--ask` sent to `id`, for a name.
    AutoName {
        id: String,
        text: String,
    },
    Choose(PathBuf),
    /// A peer's close: every terminal under `id` must be in `allowed`, checked
    /// here where the close runs; otherwise nothing is closed.
    CloseWithin {
        id: String,
        allowed: Vec<String>,
        reply: Sender<Result<(), String>>,
    },
}

pub(crate) struct RegistryConfig {
    pub layout: Layout,
    pub project: Option<PathBuf>,
    pub extras: LaunchExtras,
    pub wake: Wake,
    pub default_tools: Vec<Tool>,
    pub io: Option<IoFactory>,
    /// How often an unsaved list is written again.
    pub retry: Duration,
    /// The bridge socket, already bound. `None` puts every terminal bridge off.
    pub bridge_socket: Option<PathBuf>,
    /// This project's utility runs. `None` turns titling off.
    pub utility: Option<Arc<Utility>>,
    /// Title an unnamed terminal on its first prompt.
    pub auto_title: bool,
    /// The first restore follows an update: see [`crate::relay::RelayConfig::after_update`].
    pub after_update: bool,
}

/// The handle other threads use.
#[derive(Clone)]
pub(crate) struct Registry {
    tx: Sender<Cmd>,
    view: Arc<ArcSwap<TerminalsView>>,
    shutdown: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    shared: Arc<Shared>,
    utility: Option<Arc<Utility>>,
    /// The layout and the project as launches resolve it, for what a tool
    /// keeps in its configuration folder there.
    configs: Option<(Layout, PathBuf)>,
}

/// What the UI holds.
#[derive(Clone)]
pub struct TerminalsHandle {
    tx: Sender<Cmd>,
    view: Arc<ArcSwap<TerminalsView>>,
    utility: Option<(Arc<Utility>, Arc<Shared>)>,
    configs: Option<(Layout, PathBuf)>,
}

pub(crate) fn kind(tool: Tool) -> ToolKind {
    match tool {
        Tool::Claude => ToolKind::Claude,
        Tool::Codex => ToolKind::Codex,
        Tool::Pi => ToolKind::Pi,
        Tool::Antigravity => ToolKind::Antigravity,
        Tool::OpenCode => ToolKind::OpenCode,
        Tool::Shell => ToolKind::Shell,
    }
}

pub(crate) fn tool_of(kind: ToolKind) -> Tool {
    match kind {
        ToolKind::Claude => Tool::Claude,
        ToolKind::Codex => Tool::Codex,
        ToolKind::Pi => Tool::Pi,
        ToolKind::Antigravity => Tool::Antigravity,
        ToolKind::OpenCode => Tool::OpenCode,
        ToolKind::Shell => Tool::Shell,
    }
}

fn busy_error() -> String {
    "busy: try again".to_owned()
}

impl TerminalsHandle {
    fn send(&self, command: Cmd) -> Result<(), String> {
        self.tx.try_send(command).map_err(|_| busy_error())
    }

    fn known(&self, id: &str) -> Result<(), String> {
        let view = self.view.load();
        if view.terminals.iter().any(|entry| entry.id == id) {
            Ok(())
        } else {
            Err(TermError::Unknown.to_string())
        }
    }
}

impl TerminalsControl for TerminalsHandle {
    fn view(&self) -> Arc<TerminalsView> {
        self.view.load_full()
    }

    fn add(&self, tool: ToolKind, name: Option<String>) -> Result<(), String> {
        self.add_on_model(tool, name, None)
    }

    fn add_on_model(
        &self,
        tool: ToolKind,
        name: Option<String>,
        model: Option<String>,
    ) -> Result<(), String> {
        let name = name.map_or(Ok(None), |name| clean_name(&name))?;
        // A model goes on the tool's command line: refused, never guessed at.
        if let Some(model) = &model
            && (tool == ToolKind::Shell || !valid_model(model))
        {
            return Err("that is not a model name".to_owned());
        }
        let view = self.view.load();
        if view.terminals.len() >= MAX_TERMINALS {
            return Err(TermError::CapReached.to_string());
        }
        if view.save == SaveState::ReadOnly {
            return Err("the terminal list is read-only this run".to_owned());
        }
        self.send(Cmd::UserAdd {
            tool: tool_of(tool),
            name,
            model,
        })
    }

    fn add_with(
        &self,
        tool: ToolKind,
        name: Option<String>,
        prompt: Option<String>,
        resume: Option<String>,
        resume_note: Option<String>,
    ) -> Result<String, String> {
        let name = name.map_or(Ok(None), |name| clean_name(&name))?;
        if let Some(id) = &resume
            && !valid_session(id)
        {
            return Err("that is not a session id".to_owned());
        }
        let view = self.view.load();
        if view.terminals.len() >= MAX_TERMINALS {
            return Err(TermError::CapReached.to_string());
        }
        if view.save == SaveState::ReadOnly {
            return Err("the terminal list is read-only this run".to_owned());
        }
        let (reply, answer) = bounded(1);
        self.send(Cmd::UserAddWith {
            tool: tool_of(tool),
            name,
            prompt,
            session: resume,
            note: resume_note,
            work_epoch: None,
            model: None,
            effort: None,
            reply,
        })?;
        answer
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the terminal list did not answer in time".to_owned())?
    }

    fn add_prompted(
        &self,
        tool: ToolKind,
        name: String,
        prompt: String,
        model: Option<String>,
    ) -> Result<String, String> {
        let name = clean_name(&name)?;
        // A model goes on the tool's command line: refused, never guessed at.
        if let Some(model) = &model
            && (!tool.is_agent() || !valid_model(model))
        {
            return Err("that is not a model name".to_owned());
        }
        let view = self.view.load();
        if view.terminals.len() >= MAX_TERMINALS {
            return Err(TermError::CapReached.to_string());
        }
        if view.save == SaveState::ReadOnly {
            return Err("the terminal list is read-only this run".to_owned());
        }
        let (reply, answer) = bounded(1);
        self.send(Cmd::UserAddWith {
            tool: tool_of(tool),
            name,
            prompt: Some(prompt),
            session: None,
            note: None,
            work_epoch: None,
            model,
            effort: None,
            reply,
        })?;
        answer
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the terminal list did not answer in time".to_owned())?
    }

    fn add_work(
        &self,
        tool: ToolKind,
        name: String,
        prompt: String,
        model: Option<String>,
        effort: Option<String>,
        epoch: u64,
    ) -> Result<String, String> {
        let name = clean_name(&name)?;
        if model.as_ref().is_some_and(|s| !valid_model(s)) {
            return Err("Invalid model name".into());
        }
        if !tool.is_agent() {
            return Err("Select an agent CLI.".into());
        }
        if let Some(s) = effort.as_deref()
            && !valid_effort(tool, s)
        {
            return Err(format!(
                "{} does not support explicit effort {s}.",
                tool.label()
            ));
        }
        let (reply, answer) = bounded(1);
        self.send(Cmd::UserAddWith {
            tool: tool_of(tool),
            name,
            prompt: Some(prompt),
            session: None,
            note: None,
            work_epoch: Some(epoch),
            model,
            effort,
            reply,
        })?;
        answer
            .recv()
            .map_err(|_| "Terminal registry closed".to_owned())?
    }

    /// Codex's own list of its models, from the home a new Codex terminal of
    /// this project would use.
    fn offered_models(&self, tool: ToolKind) -> Option<Vec<String>> {
        let (layout, project) = self.configs.as_ref()?;
        match tool {
            ToolKind::Codex => crate::codex_models::listed(&crate::launch::config_dir(
                layout,
                project,
                Tool::Codex,
            )?),
            // The models the user marked or used in OpenCode. A file that
            // names none leaves the default alone on offer.
            ToolKind::OpenCode => {
                let state = crate::launch::opencode_state_dir(layout, project)?;
                let text = std::fs::read_to_string(state.join("model.json")).ok()?;
                let models = connecto_adapter::opencode_models(&text);
                (!models.is_empty()).then_some(models)
            }
            _ => None,
        }
    }

    fn session_of(&self, id: &str) -> Option<String> {
        let (_, shared) = self.utility.as_ref()?;
        shared.slot_by_id(id)?.tool_session()
    }

    fn close(&self, id: &str) -> Result<(), String> {
        self.known(id)?;
        self.send(Cmd::UserClose(id.to_owned()))
    }

    fn close_within(&self, id: &str, allowed: &[String]) -> Result<(), String> {
        self.known(id)?;
        let (reply, answer) = bounded(1);
        self.send(Cmd::CloseWithin {
            id: id.to_owned(),
            allowed: allowed.to_vec(),
            reply,
        })?;
        answer
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the terminal list did not answer in time".to_owned())?
    }

    fn rename(&self, id: &str, name: Option<String>) -> Result<(), String> {
        let name = name.map_or(Ok(None), |name| clean_name(&name))?;
        self.known(id)?;
        self.send(Cmd::Rename {
            id: id.to_owned(),
            name,
        })
    }

    fn restart(&self, id: &str) -> Result<(), String> {
        self.known(id)?;
        self.send(Cmd::Restart(id.to_owned()))
    }

    fn stop_background_now(&self, id: &str, task: Option<&str>) -> Result<String, String> {
        self.known(id)?;
        let slot = self
            .utility
            .as_ref()
            .and_then(|(_, shared)| shared.slot_by_id(id))
            .ok_or_else(|| "That terminal is not running.".to_owned())?;
        crate::background::stop(&slot, task, true, |_| {})
    }

    fn stop_background(&self, id: &str, task: Option<&str>) -> Result<String, String> {
        self.known(id)?;
        let slot = self
            .utility
            .as_ref()
            .and_then(|(_, shared)| shared.slot_by_id(id))
            .ok_or_else(|| "That terminal is not running.".to_owned())?;
        let tx = self.tx.clone();
        let id = id.to_owned();
        crate::background::stop(&slot, task, false, move |why| {
            let _ = tx.try_send(Cmd::BackgroundStopRefused { id, why });
        })
    }

    fn continue_from(&self, history_id: &str) -> Result<(), String> {
        let view = self.view.load();
        let entry = view
            .history
            .iter()
            .find(|entry| entry.id == history_id)
            .ok_or("no such history entry")?;
        if !entry.resumable {
            return Err("that conversation has no session to resume".to_owned());
        }
        if view.terminals.len() >= MAX_TERMINALS {
            return Err(TermError::CapReached.to_string());
        }
        if view.save == SaveState::ReadOnly {
            return Err("the terminal list is read-only this run".to_owned());
        }
        self.send(Cmd::Continue(history_id.to_owned()))
    }

    fn request_transcript(&self, history_id: &str) -> Result<(), String> {
        let view = self.view.load();
        if !view.history.iter().any(|entry| entry.id == history_id) {
            return Err("no such history entry".to_owned());
        }
        self.send(Cmd::Transcript(history_id.to_owned()))
    }

    fn read_transcript(&self, history_id: &str) -> Result<String, String> {
        let view = self.view.load();
        if !view.history.iter().any(|entry| entry.id == history_id) {
            return Err("no such history entry".to_owned());
        }
        let (reply, answer) = bounded(1);
        self.send(Cmd::ReadTranscript {
            history_id: history_id.to_owned(),
            reply,
        })?;
        answer
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| "the transcript could not be read in time".to_owned())?
    }

    fn start_titling(&self, id: &str) -> Result<(), String> {
        let (utility, shared) = self
            .utility
            .clone()
            .ok_or("utility sessions are not available")?;
        let handle = self.clone();
        let terminal = id.to_owned();
        begin_titling(
            &utility,
            &shared,
            &self.view.load(),
            id,
            Box::new(move |title| handle.rename(&terminal, Some(title))),
            None,
        )
    }

    fn utility_pane(&self, run: u64) -> Option<TerminalEntry> {
        self.utility.as_ref()?.0.pane_entry(run)
    }

    fn utility_runs(&self) -> Arc<Vec<connecto_core::UtilityRun>> {
        self.utility
            .as_ref()
            .map_or_else(|| Arc::new(Vec::new()), |(utility, _)| utility.runs())
    }
}

/// Start a hidden titling run for terminal `id`, the one the title button
/// starts: read what the terminal is about off the caller's thread, ask the
/// model, and give the title to `rename`. `Err` means it was refused at once.
/// When the run fails, `fallback`, if any, is given to `rename` instead.
fn begin_titling(
    utility: &Arc<Utility>,
    shared: &Arc<Shared>,
    view: &TerminalsView,
    id: &str,
    rename: Box<dyn Fn(String) -> Result<(), String> + Send>,
    fallback: Option<String>,
) -> Result<(), String> {
    let label = {
        let entry = view
            .terminals
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| TermError::Unknown.to_string())?;
        entry.name.clone().unwrap_or_else(|| entry.id.clone())
    };
    let run = utility.begin(id, &label)?;
    let terminal = id.to_owned();
    let worker = {
        let utility = Arc::clone(utility);
        let shared = Arc::clone(shared);
        thread::Builder::new()
            .name("utility-run".into())
            .spawn(move || {
                // Reading the screen waits on its lock: off the caller's thread.
                let titled = match utility::context(&shared, utility.project(), &terminal) {
                    None => {
                        utility.finish(
                            run,
                            connecto_core::UtilityStatus::Failed {
                                reason: "no activity in this terminal yet, so there is nothing to title it from".to_owned(),
                            },
                        );
                        false
                    }
                    Some(prompt) => utility.execute(run, prompt, &*rename),
                };
                if !titled && let Some(fallback) = fallback {
                    let _ = rename(fallback);
                }
            })
    };
    if worker.is_err() {
        utility.finish(
            run,
            connecto_core::UtilityStatus::Failed {
                reason: "could not start a thread".to_owned(),
            },
        );
    }
    Ok(())
}

impl Registry {
    pub(crate) fn start(shared: Arc<Shared>, config: RegistryConfig) -> std::io::Result<Self> {
        let (tx, rx) = bounded(QUEUE);
        let shutdown = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let view = Arc::new(ArcSwap::from_pointee(TerminalsView {
            terminals: Vec::new(),
            history: Arc::new(Vec::new()),
            transcript: None,
            save: SaveState::Saved,
            last_error: None,
            warning: None,
        }));
        // As `Core::init` resolves it, so the folder is the one launches use.
        let configs = config.project.as_deref().map(|path| {
            (
                config.layout.clone(),
                fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()),
            )
        });
        let mut core = Core {
            shared: Arc::clone(&shared),
            layout: config.layout,
            extras: config.extras,
            wake: config.wake,
            default_tools: config.default_tools,
            io_factory: config.io,
            bridge_socket: config.bridge_socket,
            project: None,
            io: None,
            list: List::defaults(&[]),
            fresh: true,
            after_update: config.after_update,
            save: SaveState::Saved,
            warning: None,
            last_error: None,
            retry_at: None,
            retry: config.retry,
            watch: BTreeMap::new(),
            history: Vec::new(),
            history_store: None,
            history_view: Arc::new(Vec::new()),
            transcript: None,
            first_ask: BTreeSet::new(),
            utility: config.utility.clone().filter(|_| config.auto_title),
            auto_titled: BTreeSet::new(),
            titles: crossbeam_channel::unbounded(),
            needs_mark: BTreeMap::new(),
            name_sync: BTreeMap::new(),
            view: Arc::clone(&view),
            signature: String::new(),
            shutdown: Arc::clone(&shutdown),
        };
        core.init(config.project.as_deref());
        core.publish();
        let registry = Self {
            tx,
            view,
            shutdown,
            closed: Arc::clone(&closed),
            shared,
            utility: config.utility,
            configs,
        };
        thread::Builder::new()
            .name("terminal-registry".into())
            .spawn(move || core.run(&rx, &closed))?;
        Ok(registry)
    }

    pub(crate) fn view_arc(&self) -> Arc<ArcSwap<TerminalsView>> {
        Arc::clone(&self.view)
    }

    pub(crate) fn control(&self) -> TerminalsHandle {
        TerminalsHandle {
            tx: self.tx.clone(),
            view: Arc::clone(&self.view),
            utility: self
                .utility
                .clone()
                .map(|utility| (utility, Arc::clone(&self.shared))),
            configs: self.configs.clone(),
        }
    }

    pub(crate) fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(utility) = &self.utility {
            utility.cancel_all();
        }
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(utility) = &self.utility {
            utility.cancel_all();
        }
    }

    /// The app quits for an update: mark the terminals in a turn as cut by it,
    /// and save. False when it could not be saved in `wait`.
    pub(crate) fn mark_cut_by_update(&self, wait: Duration) -> bool {
        let (reply, answer) = bounded(1);
        if self
            .tx
            .send_timeout(Cmd::MarkCutByUpdate { reply }, wait)
            .is_err()
        {
            return false;
        }
        answer.recv_timeout(wait).unwrap_or(false)
    }

    /// Refuses at once, with nothing live and nothing in preflight. The
    /// registry checks again before it acts.
    pub(crate) fn choose_project(&self, path: &Path) -> Result<(), String> {
        if busy(&self.shared) {
            return Err("Stop all first".to_owned());
        }
        let project = validated(&self.shared.layout, path).map_err(|err| err.to_string())?;
        self.tx
            .try_send(Cmd::Choose(project))
            .map_err(|_| "busy: try again".to_owned())
    }

    fn ask(
        &self,
        make: impl FnOnce(Sender<Result<Changed, TermError>>) -> Cmd,
        wait: Duration,
    ) -> Result<Changed, TermError> {
        let (reply, answer) = bounded(1);
        self.tx.try_send(make(reply)).map_err(|_| TermError::Busy)?;
        match answer.recv_timeout(wait) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(TermError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(TermError::Shutdown),
        }
    }

    /// An agent's add. `epoch` is the pause epoch read at admission. `caller`
    /// is the terminal whose agent asked, which becomes the new one's parent.
    pub(crate) fn agent_add(
        &self,
        tool: Tool,
        name: Option<String>,
        launch: AgentLaunch,
        caller: Option<String>,
        epoch: u64,
        wait: Duration,
    ) -> Result<Changed, TermError> {
        self.ask(
            |reply| Cmd::Add {
                tool,
                name,
                launch,
                caller,
                epoch,
                reply,
            },
            wait,
        )
    }

    /// The body of an `--ask` to `id`: the first one names a child that has no
    /// name. Never waits, and a full queue just means no name.
    pub(crate) fn auto_name(&self, id: &str, text: &str) {
        let _ = self.tx.try_send(Cmd::AutoName {
            id: id.to_owned(),
            text: text.to_owned(),
        });
    }

    pub(crate) fn agent_start(
        &self,
        id: &str,
        epoch: u64,
        attempt: u64,
        wait: Duration,
    ) -> Result<Changed, TermError> {
        self.ask(
            |reply| Cmd::Start {
                id: id.to_owned(),
                epoch,
                attempt,
                reply,
            },
            wait,
        )
    }

    pub(crate) fn agent_close(
        &self,
        id: &str,
        admit: Admit,
        wait: Duration,
    ) -> Result<Changed, TermError> {
        self.ask(
            |reply| Cmd::Close {
                id: id.to_owned(),
                admit,
                reply,
            },
            wait,
        )
    }
}

/// What a launch does with a saved list: which terminals resume with
/// "continua", and which cut asks are opened again.
#[derive(Debug, Default)]
struct Restore {
    /// The list changed and must be saved before anything starts.
    marked: bool,
    /// Resumed with "continua": an update cut their turn (ADR 0030).
    carry_on: BTreeSet<String>,
    /// Asks an update cut, opened again for their askers.
    asks: Vec<OpenAsk>,
}

/// Which saved terminals start at this launch, and which of them resume with
/// "continua" because an update cut their turn (ADR 0030). Marks a turn cut
/// by the last close as interrupted and drops the update's mark, saying
/// whether the list changed so. A list made this launch (`fresh`) starts
/// whole and carries nothing on.
///
/// A terminal waiting on an `--ask` to one the update cut starts too, and its
/// ask is opened again. It is not told "continua", even when its own turn was
/// cut: the reply wakes it, as it would have without the update.
///
/// The first restore after an update's quit (`after_update`) also starts
/// every terminal saved running, idle ones and the app session included,
/// resumed with no note: an update is not the person quitting. Otherwise an
/// idle terminal stays stopped, as after any quit, and that is saved, so a
/// later update does not bring back one this launch left stopped.
fn plan_restore(entries: &mut [Entry], fresh: bool, after_update: bool) -> Restore {
    let mut restore = Restore::default();
    // Who starts because an update cut it, then, until nothing changes, who
    // waits on one that starts.
    let mut starts: BTreeSet<String> = if fresh {
        BTreeSet::new()
    } else {
        entries
            .iter()
            .filter(|entry| entry.cut_by_update)
            .map(|entry| entry.id.clone())
            .collect()
    };
    let ids: BTreeSet<String> = entries.iter().map(|entry| entry.id.clone()).collect();
    loop {
        let waiting: Vec<String> = entries
            .iter()
            .filter(|entry| starts.contains(&entry.id))
            .flat_map(|entry| entry.owes.iter().map(|owed| owed.to.clone()))
            .filter(|to| ids.contains(to) && !starts.contains(to))
            .collect();
        if waiting.is_empty() {
            break;
        }
        starts.extend(waiting);
    }
    let mut waits: BTreeSet<String> = BTreeSet::new();
    for entry in entries.iter() {
        if !starts.contains(&entry.id) {
            continue;
        }
        for owed in &entry.owes {
            if starts.contains(&owed.to) {
                waits.insert(owed.to.clone());
                restore.asks.push(OpenAsk {
                    request: owed.request.clone(),
                    from: owed.to.clone(),
                    target: entry.id.clone(),
                });
            }
        }
    }
    for entry in entries {
        if fresh {
            entry.interrupted = false;
        } else if (entry.working || entry.cut_by_update) && !entry.interrupted {
            entry.interrupted = true;
            restore.marked = true;
        }
        let cut = !fresh && entry.cut_by_update;
        if entry.cut_by_update {
            entry.cut_by_update = false;
            restore.marked = true;
        }
        if !entry.owes.is_empty() {
            entry.owes.clear();
            restore.marked = true;
        }
        let waiting = waits.contains(&entry.id);
        // Only a conversation that is resumed can be told to carry on, and
        // one waiting on an ask is woken by its reply instead.
        if cut && !waiting && entry.interrupted && entry.session.is_some() {
            restore.carry_on.insert(entry.id.clone());
        }
        let kept = after_update && !fresh && entry.running;
        let running = waiting
            || kept
            || ((fresh || entry.interrupted)
                && (cut || entry.name.as_deref() != Some(connecto_core::session::APP_SESSION)));
        // What is saved is what runs: `desired` is what the next update
        // brings back. A list made this launch is not written for it.
        if running != entry.running && !fresh {
            restore.marked = true;
        }
        entry.running = running;
    }
    restore
}

/// Who an update restarts: the terminals in a turn (`cut`), then, until
/// nothing changes, the askers of an open ask to one that restarts. Gives
/// the asks to save, as each target's `owes` (ADR 0030).
fn owed_replies(
    cut: &BTreeSet<String>,
    asks: &[OpenAsk],
    listed: &BTreeSet<String>,
) -> BTreeMap<String, Vec<OwedReply>> {
    let mut starts = cut.clone();
    loop {
        let waiting: Vec<String> = asks
            .iter()
            .filter(|ask| starts.contains(&ask.target) && !starts.contains(&ask.from))
            .filter(|ask| listed.contains(&ask.from))
            .map(|ask| ask.from.clone())
            .collect();
        if waiting.is_empty() {
            break;
        }
        starts.extend(waiting);
    }
    let mut owes: BTreeMap<String, Vec<OwedReply>> = BTreeMap::new();
    for ask in asks {
        if starts.contains(&ask.target) && starts.contains(&ask.from) && ask.from != ask.target {
            owes.entry(ask.target.clone()).or_default().push(OwedReply {
                request: ask.request.clone(),
                to: ask.from.clone(),
            });
        }
    }
    owes
}

/// Whether any pane runs a process or a preflight.
fn busy(shared: &Shared) -> bool {
    shared
        .slots
        .load()
        .iter()
        .flatten()
        .any(|slot| slot.pane.is_live() || slot.pane.is_preparing())
}

/// How a pane looks to the saved list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Active,
    Stopped,
    /// Refused by the project's Tools setting: not to be brought back.
    Refused,
    Other,
}

fn look(pane: &ManagedPane) -> Look {
    match pane.stage() {
        PaneStage::Preparing { .. } => Look::Active,
        PaneStage::Running if pane.is_live() => Look::Active,
        PaneStage::Running | PaneStage::Stopped => Look::Stopped,
        PaneStage::Failed { .. } if pane.tools_refused() => Look::Refused,
        _ => Look::Other,
    }
}

/// What the registry has seen of one slot.
#[derive(Default)]
struct Watch {
    stopped_ticks: u8,
    grace_until: Option<Instant>,
}

/// A name waiting to be told to a running Claude (its `/rename`).
struct NameSync {
    name: String,
    /// The user's keystrokes seen last, and since when they have not changed.
    quiet: (u64, Instant),
    /// The write on its way and when it was queued; a refusal or a long silence
    /// puts the name back to waiting.
    sent: Option<(Receiver<WriteOutcome>, Instant)>,
    /// Not before this, after a refusal.
    retry_at: Option<Instant>,
}

/// How long the user must have been still before a slash command is typed.
const NAME_QUIET: Duration = Duration::from_secs(2);
/// How long to wait for the outcome of one `/rename`. Longer than the relay's
/// hold for a send (`input_ready::HOLD_LIMIT`): a held `/rename` is refused at
/// that limit, and the sync must still be waiting then. Giving up sooner would
/// re-send the same name while the first is still held, so it is typed twice.
const NAME_WRITE_WAIT: Duration = Duration::from_secs(30);
/// The id prefix of a `/rename` wake. The relay holds only these wakes until the
/// composer shows; other wakes keep their immediate delivery.
pub(crate) const RENAME_WAKE_PREFIX: &str = "rename-";
/// Wait after a name could not be delivered.
const NAME_RETRY: Duration = Duration::from_secs(3);

struct Core {
    shared: Arc<Shared>,
    layout: Layout,
    extras: LaunchExtras,
    wake: Wake,
    default_tools: Vec<Tool>,
    io_factory: Option<IoFactory>,
    bridge_socket: Option<PathBuf>,
    project: Option<PathBuf>,
    io: Option<Box<dyn ListIo>>,
    list: List,
    /// The list was made this launch, not read: all of it starts.
    fresh: bool,
    /// The first restore follows an update's quit: every terminal saved
    /// running starts (ADR 0030). Taken by that restore, so a project chosen
    /// later restores as usual.
    after_update: bool,
    save: SaveState,
    warning: Option<String>,
    last_error: Option<String>,
    retry_at: Option<Instant>,
    retry: Duration,
    watch: BTreeMap<usize, Watch>,
    /// Closed terminals, newest first, and where they are kept.
    history: Vec<Record>,
    history_store: Option<History>,
    history_view: Arc<Vec<HistoryEntry>>,
    transcript: Option<Transcript>,
    /// Terminals that have had an `--ask` this run, so only the first names one.
    first_ask: BTreeSet<String>,
    /// For titling a terminal on its first prompt. `None` turns it off.
    utility: Option<Arc<Utility>>,
    /// Terminals whose first prompt has started a titling run this run, so a
    /// later prompt never starts another.
    auto_titled: BTreeSet<String>,
    /// Titles from those runs, by terminal id, taken up on the next tick. A
    /// channel of its own, so the registry's queue still closes when its
    /// handles are gone.
    titles: (Sender<Titled>, Receiver<Titled>),
    /// For a terminal asking the user (by slot uid): how many keystrokes the
    /// user had typed when it was first seen asking. Typing after that answers it.
    needs_mark: BTreeMap<u64, (u64, u64)>,
    /// Names to say to a running Claude with `/rename`, by terminal id: the last
    /// name given wins, and each waits for a quiet prompt.
    name_sync: BTreeMap<String, NameSync>,
    view: Arc<ArcSwap<TerminalsView>>,
    signature: String,
    shutdown: Arc<AtomicBool>,
}

/// A title from a first-prompt run: terminal id, title.
type Titled = (String, String);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The first words of an `--ask`, as a name: one line, at most
/// [`AUTO_NAME_CHARS`] characters.
fn name_from(text: &str) -> Option<String> {
    let line: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(AUTO_NAME_CHARS)
        .collect();
    clean_name(&line).ok().flatten()
}

impl Core {
    fn io_for(&self, project: &Path) -> Box<dyn ListIo> {
        if let Some(factory) = &self.io_factory {
            return factory(project);
        }
        Box::new(FsIo::new(
            self.layout
                .data_root
                .join("projects")
                .join(project_id(project))
                .join(FILE),
        ))
    }

    /// The size a new terminal starts at: what the others are showing.
    fn grid(&self) -> GridSize {
        self.shared
            .slots
            .load()
            .iter()
            .flatten()
            .next()
            .map_or(INITIAL_SIZE, |slot| slot.pane.size())
    }

    fn make_pane(
        &self,
        tool: Tool,
        id: &str,
        how: Begin,
        resume: Option<&str>,
        opening: Opening,
    ) -> Option<Arc<ManagedPane>> {
        // With the pane's setup, so it is there before `begin_project` queues
        // the startup that uses it.
        let mut extras = self.extras.clone();
        extras.initial_prompt = opening.prompt;
        extras.resume_note = opening.note;
        extras.model = opening.model;
        extras.effort = opening.effort;
        let pane = ManagedPane::spawn(PaneSetup {
            layout: self.layout.clone(),
            project: None,
            tool,
            size: self.grid(),
            wake: Arc::clone(&self.wake),
            extras,
            bridge: Some(PaneBridge {
                terminal: id.to_owned(),
                socket: self.bridge_socket.clone(),
            }),
        })
        .ok()?;
        let pane = Arc::new(pane);
        pane.set_resume(resume.filter(|id| valid_session(id)).map(str::to_owned));
        pane.set_session_name(
            self.list
                .terminals
                .iter()
                .find(|entry| entry.id == id)
                .and_then(|entry| entry.name.clone()),
        );
        if let Some(project) = &self.project {
            pane.begin_project(project.clone(), how);
        }
        Some(pane)
    }

    fn free_slot(&self) -> Option<usize> {
        let slots = self.shared.slots.load();
        (0..MAX_TERMINALS).find(|index| slots.get(*index).is_none_or(Option::is_none))
    }

    /// Load the project's list and history and start its terminals. Runs once,
    /// before the thread starts.
    fn init(&mut self, project: Option<&Path>) {
        self.project =
            project.map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()));
        self.load_list();
        self.fill_slots();
    }

    fn load_list(&mut self) {
        if let Some(project) = &self.project {
            let key = u64::from_str_radix(&project_id(project), 16).unwrap_or(0);
            self.shared.project_key.store(key, Ordering::SeqCst);
        }
        let defaults = self.default_tools.clone();
        self.fresh = true;
        let (list, save, warning) = match self.project.clone() {
            None => (List::defaults(&defaults), SaveState::Saved, None),
            Some(project) => {
                let io = self.io_for(&project);
                let loaded = load(&*io, &defaults);
                self.io = Some(io);
                self.fresh = !matches!(loaded, Loaded::Read(_));
                match loaded {
                    Loaded::Read(list) | Loaded::Fresh(list) => (list, SaveState::Saved, None),
                    Loaded::Bad { list, warning } => (list, SaveState::Saved, Some(warning)),
                    Loaded::ReadOnly { list, warning } => {
                        (list, SaveState::ReadOnly, Some(warning))
                    }
                }
            }
        };
        self.list = list;
        self.save = save;
        self.warning = warning;
        self.retry_at = None;
        self.load_history();
    }

    fn load_history(&mut self) {
        self.history_store = self.project.as_ref().map(|project| {
            History::new(History::dir_of(
                &self.layout.data_root,
                &project_id(project),
            ))
        });
        let (records, warning) = self
            .history_store
            .as_ref()
            .map_or((Vec::new(), None), History::load);
        self.history = records;
        self.refresh_history_view();
        self.transcript = None;
        self.first_ask.clear();
        self.auto_titled.clear();
        if let Some(warning) = warning {
            self.warning = Some(match self.warning.take() {
                Some(existing) => format!("{existing} {warning}"),
                None => warning,
            });
        }
    }

    fn refresh_history_view(&mut self) {
        self.history_view = Arc::new(
            self.history
                .iter()
                .map(|record| record.entry.clone())
                .collect(),
        );
    }

    /// Give every listed terminal a slot, reusing the pane of one that is
    /// already there with the same id and tool. A list made this launch starts
    /// whole (R3). Of a saved one, only the terminals that were working when
    /// the app closed start; the rest are shown stopped until they are asked
    /// for. A started one is not told to carry on: its conversation comes
    /// back at its prompt, marked interrupted until its next turn. Either way
    /// a saved tool session is picked up again by the first start (R11).
    ///
    /// A terminal whose turn an update cut (ADR 0030) starts too, the app
    /// session included, and its resumed conversation is told "continua" as
    /// its first message: once, as the mark is dropped and saved here. A turn
    /// cut by a crash or a quit is not carried on.
    fn fill_slots(&mut self) {
        let Restore {
            marked,
            mut carry_on,
            mut asks,
        } = plan_restore(
            &mut self.list.terminals,
            self.fresh,
            std::mem::take(&mut self.after_update),
        );
        // Kept at once, before anything starts: a second quit before its next
        // turn must not lose it, and an update's mark is gone from the disk
        // before "continua" is said, so it is said at most once. Unsaved,
        // nothing is carried on.
        if marked && !self.save_now() {
            carry_on.clear();
            asks.clear();
        }
        self.shared.restore_asks(asks);
        let entries = self.list.terminals.clone();
        // Free slots that no longer match.
        let current: Vec<Arc<Slot>> = self.shared.slots.load().iter().flatten().cloned().collect();
        for slot in current {
            let keep = entries
                .iter()
                .any(|entry| entry.id == slot.id && entry.tool == slot.tool);
            if !keep {
                slot.pane.close();
                slot.pane.release();
                self.shared.vacate(slot.index);
                self.watch.remove(&slot.index);
            }
        }
        for entry in entries {
            if let Some(slot) = self.shared.slot_by_id(&entry.id) {
                slot.pane.release();
                slot.pane.set_session_name(entry.name.clone());
                slot.pane
                    .set_resume(entry.session.clone().filter(|id| valid_session(id)));
                slot.pane
                    .set_launch(entry.model.clone(), entry.launch_effort());
                slot.interrupted.store(entry.interrupted, Ordering::SeqCst);
                slot.job.store(entry.job, Ordering::SeqCst);
                if let Some(project) = &self.project {
                    let how = if entry.running {
                        Begin::Start
                    } else {
                        Begin::Stopped
                    };
                    slot.pane.begin_project(project.clone(), how);
                }
                self.begin_watch(&slot, true);
                continue;
            }
            let Some(index) = self.free_slot() else {
                continue;
            };
            let Some(pane) = self.make_pane(
                entry.tool,
                &entry.id,
                if entry.running {
                    Begin::Start
                } else {
                    Begin::Stopped
                },
                entry.session.as_deref(),
                Opening {
                    model: entry.model.clone(),
                    effort: entry.launch_effort(),
                    note: carry_on
                        .contains(&entry.id)
                        .then(|| CONTINUE_AFTER_UPDATE.to_owned()),
                    ..Opening::default()
                },
            ) else {
                continue;
            };
            if let Ok(slot) = self.shared.install(index, &entry.id, entry.tool, pane) {
                slot.interrupted.store(entry.interrupted, Ordering::SeqCst);
                slot.job.store(entry.job, Ordering::SeqCst);
                self.begin_watch(&slot, true);
            }
        }
    }

    /// Mark each terminal in a turn now as cut by an update, and save. Then
    /// nothing the panes do is followed any more: the app is quitting, and
    /// what is saved is what the update cut. True when it is saved.
    fn mark_cut_by_update(&mut self) -> bool {
        if self.shutdown.load(Ordering::SeqCst) {
            // Already closing (a second shutdown path, say): what it saw last
            // is saved, and no longer looked at.
            return true;
        }
        // What the panes say right now, not at the last tick.
        self.observe();
        // Only a turn a live agent is in now: a saved `working` that a pane
        // still starting has not confirmed is an older cut, not this one.
        // With the conversation it is in, read after the turn: a pane that
        // restarted since `observe` is in a new one.
        let in_turn: BTreeMap<String, Option<String>> = self
            .shared
            .slots
            .load()
            .iter()
            .flatten()
            .filter(|slot| {
                slot.tool != Tool::Shell
                    && slot.pane.stage() == PaneStage::Running
                    && slot.pane.is_live()
                    // A turn, not background work after one: that one is done.
                    && slot.published().0 == Phase::Working
            })
            .map(|slot| {
                let session = slot.tool_session().filter(|id| valid_session(id));
                (slot.id.clone(), session)
            })
            .collect();
        // The asks open now, so an asker waiting on a cut turn restarts too
        // and gets its reply (ADR 0030). The state never waits on the
        // registry, so this cannot stall the quit for long.
        let cut: BTreeSet<String> = in_turn.keys().cloned().collect();
        let listed: BTreeSet<String> = self.list.terminals.iter().map(|e| e.id.clone()).collect();
        let mut owes = owed_replies(&cut, &self.shared.open_asks(OPEN_ASKS_WAIT), &listed);
        let mut any = false;
        for entry in &mut self.list.terminals {
            entry.owes = owes.remove(&entry.id).unwrap_or_default();
            any |= !entry.owes.is_empty();
            entry.cut_by_update = in_turn.contains_key(&entry.id);
            // One look for both: a turn that began after `observe` is a turn.
            entry.working |= entry.cut_by_update;
            if let Some(Some(session)) = in_turn.get(&entry.id) {
                entry.session = Some(session.clone());
            }
            any |= entry.cut_by_update;
        }
        self.shutdown.store(true, Ordering::SeqCst);
        // The cut is now, right after the look and before the disk write: a
        // turn must not end after it was seen, so the panes stop here rather
        // than when the rest of the app has shut down.
        for slot in self.shared.slots.load().iter().flatten() {
            slot.pane.request_stop();
        }
        // Written even when nothing was cut: the next launch brings back what
        // the list says was running, so it must be what ran now (ADR 0030).
        // A list that is read-only this run is never written, and fails only
        // what it had to keep.
        if self.save == SaveState::ReadOnly {
            return !any;
        }
        self.save_now()
    }

    fn begin_watch(&mut self, slot: &Slot, starting: bool) {
        self.watch.insert(
            slot.index,
            Watch {
                stopped_ticks: 0,
                grace_until: starting.then(|| Instant::now() + START_GRACE),
            },
        );
    }

    fn write(&mut self, list: &List) -> Result<(), String> {
        if self.save == SaveState::ReadOnly {
            return Err("the terminal list is read-only this run".to_owned());
        }
        match &self.io {
            None => Ok(()),
            Some(io) => io.write(&list.to_json()).map_err(|err| err.to_string()),
        }
    }

    /// Write the list after a change of `desired`. Never blocks a stop on the
    /// disk: the change is already made, and a failure marks the list unsaved.
    fn save_now(&mut self) -> bool {
        if self.save == SaveState::ReadOnly {
            return false;
        }
        let list = self.list.clone();
        match self.write(&list) {
            Ok(()) => {
                self.save = SaveState::Saved;
                self.retry_at = None;
                true
            }
            Err(why) => {
                self.save = SaveState::Unsaved;
                self.retry_at = Some(Instant::now() + self.retry);
                self.last_error = Some(format!(
                    "The terminal list could not be saved ({why}). Terminals may come back \
                     running or stopped differently after a relaunch."
                ));
                false
            }
        }
    }

    fn set_desired(&mut self, id: &str, running: bool) -> bool {
        if let Some(entry) = self.list.terminals.iter_mut().find(|entry| entry.id == id) {
            if entry.running == running {
                return self.save != SaveState::Unsaved;
            }
            entry.running = running;
            return self.save_now();
        }
        true
    }

    fn state_word(slot: &Slot) -> &'static str {
        match slot.pane.stage() {
            PaneStage::Stopped => "stopped",
            _ => "starting",
        }
    }

    /// Where an agent's new terminal sits in the tree: under the caller, or
    /// under the caller's own parent when the caller is a child, with the real
    /// caller kept as `asked_by`. Never deeper than one level.
    /// `None` when the caller is no longer listed: an add queued just before
    /// its caller's close must not appear as a stray top-level terminal.
    fn lineage(&self, caller: &str) -> Option<(Option<String>, Option<String>)> {
        let entry = self
            .list
            .terminals
            .iter()
            .find(|entry| entry.id == caller)?;
        Some(match &entry.parent {
            Some(parent) => (Some(parent.clone()), Some(caller.to_owned())),
            None => (Some(caller.to_owned()), None),
        })
    }

    /// Why a terminal of `tool` may not be added or started in this
    /// project: its saved Tools setting leaves the CLI out.
    fn tool_refusal(&self, tool: Tool) -> Option<TermError> {
        let project = self.project.as_deref()?;
        crate::project_settings::tool_refusal_in(&self.layout.data_root, project, kind(tool))
            .map(TermError::NotAllowed)
    }

    fn add(&mut self, new: New, how: Begin) -> Result<Changed, TermError> {
        let tool = new.tool.ok_or(TermError::Unknown)?;
        if let Some(refused) = self.tool_refusal(tool) {
            return Err(refused);
        }
        if new.name.as_deref() == Some(connecto_core::session::APP_SESSION) {
            if tool == Tool::Shell || new.parent.is_some() {
                return Err(TermError::Name(
                    "app-session requires a top-level agent".into(),
                ));
            }
            if let Some(entry) = self.list.terminals.iter().find(|e| e.name == new.name) {
                return Ok(Changed {
                    id: entry.id.clone(),
                    state: "existing",
                    saved: true,
                    removal: Removal::NotAsked,
                });
            }
        }
        if self.save == SaveState::ReadOnly {
            return Err(TermError::SaveFailed(
                "the terminal list is read-only this run".to_owned(),
            ));
        }
        if self.list.terminals.len() >= MAX_TERMINALS {
            return Err(TermError::CapReached);
        }
        let Some(index) = self.free_slot() else {
            return Err(TermError::CapReached);
        };
        // Write the list first. If that fails nothing changes: no slot, no
        // start, and `next` stays, so the unused id was never given out.
        let mut next = self.list.clone();
        let id = next.mint(tool).ok_or_else(|| {
            TermError::SaveFailed(format!(
                "the ids for {} are used up, so no new terminal can be named",
                tool.name()
            ))
        })?;
        let mut entry = Entry::new(id.clone(), tool, true);
        entry.name = new.name;
        entry.parent = new.parent;
        entry.asked_by = new.asked_by;
        entry.session = new.session.filter(|session| valid_session(session));
        entry.model = new.model.filter(|model| valid_model(model));
        entry.effort = new.effort;
        entry.job = new.job;
        let model = entry.model.clone();
        let resume = entry.session.clone();
        let opening = Opening {
            prompt: new.prompt,
            note: new.note,
            effort: entry.launch_effort(),
            model,
        };
        next.terminals.push(entry);
        self.write(&next).map_err(TermError::SaveFailed)?;
        let previous = std::mem::replace(&mut self.list, next);
        self.save = SaveState::Saved;
        self.retry_at = None;
        // The slot is filled before the start is queued, so Stop all finds it.
        let made = self
            .make_pane(tool, &id, how, resume.as_deref(), opening)
            .ok_or_else(|| "the terminal could not be created".to_owned())
            .and_then(|pane| {
                self.shared
                    .install(index, &id, tool, Arc::clone(&pane))
                    .map_err(|err| {
                        // No slot holds the pane, so nothing else would end it.
                        pane.close();
                        pane.release();
                        err.to_string()
                    })
            });
        let slot = match made {
            Ok(slot) => slot,
            Err(why) => {
                self.take_back(previous);
                return Err(TermError::SaveFailed(why));
            }
        };
        slot.job.store(new.job, Ordering::SeqCst);
        self.begin_watch(&slot, true);
        Ok(Changed {
            id,
            state: "starting",
            saved: true,
            removal: Removal::NotAsked,
        })
    }

    /// Undo an add whose terminal could not be made: the list is the one from
    /// before, in memory and on disk, so no entry is left without a slot. The
    /// id is not given out, as `next` goes back too. If the disk refuses, the
    /// list is marked unsaved and the usual retry writes it.
    fn take_back(&mut self, previous: List) {
        self.list = previous;
        self.save_now();
    }

    fn start(&mut self, id: &str, epoch: u64, attempt: u64) -> Result<Changed, TermError> {
        let slot = self.shared.slot_by_id(id).ok_or(TermError::Unknown)?;
        let pause = self.shared.pause();
        if pause.is_paused() || pause.epoch() != epoch {
            return Err(TermError::Paused);
        }
        if slot.pane.attempt() != attempt {
            return Err(TermError::Restarted);
        }
        if let Some(refused) = self.tool_refusal(slot.tool) {
            return Err(refused);
        }
        slot.pane
            .request_start_agent(pause, epoch)
            .map_err(|refusal| match refusal {
                StartRefusal::AlreadyRunning => TermError::AlreadyRunning,
                StartRefusal::Busy => TermError::Busy,
                StartRefusal::LoggingIn => TermError::LoggingIn,
            })?;
        let saved = self.set_desired(id, true);
        self.begin_watch(&slot, true);
        Ok(Changed {
            id: id.to_owned(),
            state: "starting",
            saved,
            removal: Removal::NotAsked,
        })
    }

    /// Close a terminal and, once it is removed, every terminal under it
    /// (R8): a parent never leaves its children behind in the tree. The
    /// children are found before the close, which would otherwise move them
    /// up. A child that cannot be removed stays, stopped, at top level, and
    /// says why in `last_error`; the parent's result is not changed by it.
    /// The close a peer asked for. The set the close would affect is worked out
    /// now, in the worker, which also runs every add: a child created after the
    /// peer looked is in the set, and is refused unless it is allowed.
    fn close_within(&mut self, id: &str, allowed: &[String]) -> Result<(), String> {
        let mut affected = self.descendants(id);
        affected.push(id.to_owned());
        if affected.iter().any(|entry| !allowed.contains(entry)) {
            return Err(connecto_core::CLOSE_NOT_ALL_LISTED.to_owned());
        }
        self.close(id, None)
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    fn close(&mut self, id: &str, admit: Option<Admit>) -> Result<Changed, TermError> {
        let descendants = self.descendants(id);
        let changed = self.close_one(id, admit)?;
        if changed.removal == Removal::Done {
            for child in descendants {
                // The children of a listed parent were admitted with it.
                if let Err(err) = self.close_one(&child, None) {
                    self.last_error = Some(format!("{child} was not closed: {err}"));
                }
            }
        }
        Ok(changed)
    }

    /// Every terminal under `id`, at any depth, in list order. The list is
    /// kept one level deep, but older or hand-edited lists are not trusted to be.
    fn descendants(&self, id: &str) -> Vec<String> {
        let mut found: Vec<String> = vec![id.to_owned()];
        let mut at = 0;
        while at < found.len() {
            let parent = found[at].clone();
            for entry in &self.list.terminals {
                if entry.parent.as_deref() == Some(parent.as_str()) && !found.contains(&entry.id) {
                    found.push(entry.id.clone());
                }
            }
            at += 1;
        }
        found.remove(0);
        found
    }

    /// Close one terminal: stop it, keep it in the history, take it out of the
    /// list. The stop always happens. The removal follows only if the new
    /// list can be written first, and otherwise the terminal stays, stopped.
    fn close_one(&mut self, id: &str, admit: Option<Admit>) -> Result<Changed, TermError> {
        let slot = self.shared.slot_by_id(id).ok_or(TermError::Unknown)?;
        // Decided again, right before the Stop: this command may have waited.
        if let Some(admit) = admit {
            if let Some(epoch) = admit.epoch {
                let pause = self.shared.pause();
                if pause.is_paused() || pause.epoch() != epoch {
                    return Err(TermError::Paused);
                }
            }
            if slot.pane.attempt() != admit.attempt {
                return Err(TermError::Restarted);
            }
        }
        // What the history keeps is read before the stop changes anything.
        let transcript = slot
            .pane
            .session()
            .map(|session| session.text_all().0)
            .unwrap_or_default();
        slot.pane.request_stop();
        let removal = self.remove_closed(&slot, &transcript);
        let saved = removal == Removal::Done;
        Ok(Changed {
            id: id.to_owned(),
            state: Self::state_word(&slot),
            saved,
            removal,
        })
    }

    fn remove_closed(&mut self, slot: &Arc<Slot>, transcript: &str) -> Removal {
        let id = slot.id.as_str();
        if self.save == SaveState::ReadOnly {
            let why = "the terminal list is read-only this run".to_owned();
            self.last_error = Some(format!("{id} was stopped but not removed: {why}"));
            return Removal::Kept(why);
        }
        let Some(entry) = self
            .list
            .terminals
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
        else {
            return Removal::Kept("it is not in the list".to_owned());
        };
        let mut next = self.list.clone();
        next.terminals.retain(|other| other.id != id);
        // Children still listed (a cascade closes them right after) and
        // anything it asked for lose the link to it.
        for other in &mut next.terminals {
            if other.parent.as_deref() == Some(id) {
                other.parent = None;
                other.asked_by = None;
            } else if other.asked_by.as_deref() == Some(id) {
                other.asked_by = None;
            }
        }
        // The history first: the terminal and its screen are not let go of
        // until the transcript is safe.
        let archived = match self.keep_in_history(&entry, slot, transcript) {
            Ok(archived) => archived,
            Err(why) => {
                self.last_error = Some(format!(
                    "{id} was stopped but not removed: the history could not be saved ({why})"
                ));
                return Removal::Kept(why);
            }
        };
        if let Err(why) = self.write(&next) {
            // Nothing was closed, so the entry must not stay in the history.
            if let (Some(store), Some(history_id)) = (&self.history_store, archived) {
                store.remove(&mut self.history, &history_id);
                self.refresh_history_view();
            }
            self.last_error = Some(format!("{id} was stopped but not removed: {why}"));
            return Removal::Kept(why);
        }
        self.list = next;
        self.save = SaveState::Saved;
        self.retry_at = None;
        slot.pane.close();
        slot.pane.release();
        self.shared.vacate(slot.index);
        self.watch.remove(&slot.index);
        self.needs_mark.remove(&slot.uid);
        self.first_ask.remove(id);
        self.auto_titled.remove(id);
        Removal::Done
    }

    /// Save the closed terminal to the history. `Ok(None)` when the project has
    /// no history folder. On `Err` nothing was written.
    fn keep_in_history(
        &mut self,
        entry: &Entry,
        slot: &Slot,
        transcript: &str,
    ) -> Result<Option<String>, String> {
        let Some(store) = &self.history_store else {
            return Ok(None);
        };
        let closed_at_ms = now_ms();
        let session = slot
            .tool_session()
            .filter(|session| valid_session(session))
            .or_else(|| entry.session.clone());
        let record = Record {
            entry: HistoryEntry {
                id: History::mint(&self.history, &entry.id, closed_at_ms),
                terminal_id: entry.id.clone(),
                name: entry.name.clone(),
                tool: kind(entry.tool),
                parent: entry.parent.clone(),
                closed_at_ms,
                task: slot.task().and_then(|task| history::field(&task)),
                last_reply: slot.last_reply().and_then(|reply| history::field(&reply)),
                resumable: session.is_some(),
            },
            session,
        };
        let history_id = record.entry.id.clone();
        store
            .add(&mut self.history, record, transcript)
            .map_err(|err| err.to_string())?;
        self.refresh_history_view();
        Ok(Some(history_id))
    }

    fn rename(&mut self, id: &str, name: Option<String>) {
        let Some(entry) = self.list.terminals.iter_mut().find(|entry| entry.id == id) else {
            self.last_error = Some(TermError::Unknown.to_string());
            return;
        };
        if entry.name == name {
            return;
        }
        if entry.name.as_deref() == Some(connecto_core::session::APP_SESSION)
            || name.as_deref() == Some(connecto_core::session::APP_SESSION)
        {
            self.last_error = Some("The app-session name is reserved for project chat.".into());
            return;
        }
        entry.name = name.clone();
        let tool = entry.tool;
        self.save_now();
        // The tool's own name for the session follows: at Claude's next launch
        // (`--name`; Codex has no such flag), and now, if it is running and can be told.
        if let Some(slot) = self.shared.slot_by_id(id) {
            slot.pane.set_session_name(name.clone());
        }
        match name {
            Some(name) if matches!(tool, Tool::Claude | Tool::Codex) => {
                self.name_sync.insert(
                    id.to_owned(),
                    NameSync {
                        name,
                        quiet: (0, Instant::now()),
                        sent: None,
                        retry_at: None,
                    },
                );
            }
            _ => {
                self.name_sync.remove(id);
            }
        }
    }

    /// Tell running Claudes and Codexes their new names with `/rename` (both
    /// have the slash command; only Claude also has a launch flag), one at a time and
    /// only at a quiet prompt: not in a turn, not at a permission prompt, not
    /// while the user is typing. The write is the relay's own wake, so it goes
    /// through the Gate and the audit log like any other, and a Pause stops it.
    fn sync_names(&mut self) {
        let ids: Vec<String> = self.name_sync.keys().cloned().collect();
        for id in ids {
            let Some(slot) = self.shared.slot_by_id(&id) else {
                self.name_sync.remove(&id);
                continue;
            };
            let Some(entry) = self.list.terminals.iter().find(|e| e.id == id).cloned() else {
                self.name_sync.remove(&id);
                continue;
            };
            // Not running: its next launch carries the name.
            if !slot.pane.is_live() || !matches!(entry.tool, Tool::Claude | Tool::Codex) {
                self.name_sync.remove(&id);
                continue;
            }
            let state = self.activity(&entry, &slot);
            let Some(sync) = self.name_sync.get_mut(&id) else {
                continue;
            };
            if let Some((outcome, since)) = &sync.sent {
                match outcome.try_recv() {
                    Ok(WriteOutcome::Typed) => {
                        self.name_sync.remove(&id);
                    }
                    Ok(_) | Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        sync.sent = None;
                        sync.retry_at = Some(Instant::now() + NAME_RETRY);
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        if since.elapsed() > NAME_WRITE_WAIT {
                            sync.sent = None;
                            sync.retry_at = Some(Instant::now() + NAME_RETRY);
                        }
                    }
                }
                continue;
            }
            if sync.retry_at.is_some_and(|at| Instant::now() < at) {
                continue;
            }
            let typed = slot.pane.user_inputs();
            if sync.quiet.0 != typed {
                sync.quiet = (typed, Instant::now());
            }
            let at_prompt = matches!(
                state,
                TermActivity::Idle | TermActivity::Done | TermActivity::Interrupted
            );
            if !at_prompt || sync.quiet.1.elapsed() < NAME_QUIET {
                continue;
            }
            let pause = self.shared.pause();
            if pause.is_paused() {
                continue;
            }
            let (reply, outcome) = bounded(1);
            let command = AgentCommand::Wake {
                id: format!("{RENAME_WAKE_PREFIX}{}", slot.uid),
                text: format!("/rename {}", sync.name),
                admitted: Admitted {
                    pause_epoch: pause.epoch(),
                    attempt: slot.pane.attempt(),
                },
                reply,
                enqueued: Instant::now(),
            };
            if slot.send_agent(command).is_ok() {
                sync.sent = Some((outcome, Instant::now()));
            }
        }
    }

    /// A terminal with no name is titled once, on its first prompt, by the
    /// same run as the title button. A title is used only if the terminal
    /// still has no name: the user's own name always wins.
    fn auto_title(&mut self) {
        while let Ok((id, title)) = self.titles.1.try_recv() {
            let unnamed = self
                .list
                .terminals
                .iter()
                .any(|entry| entry.id == id && entry.name.is_none());
            if unnamed {
                self.rename(&id, Some(title));
            }
        }
        let Some(utility) = self.utility.clone() else {
            return;
        };
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let due: Vec<String> = self
            .list
            .terminals
            .iter()
            .filter(|entry| entry.name.is_none() && !self.auto_titled.contains(&entry.id))
            .filter(|entry| {
                self.shared
                    .slot_by_id(&entry.id)
                    .is_some_and(|slot| slot.prompts.load(Ordering::SeqCst) > 0)
            })
            .map(|entry| entry.id.clone())
            .collect();
        if due.is_empty() {
            return;
        }
        // The view the run reads its label from must hold these terminals.
        self.publish();
        for id in due {
            self.auto_titled.insert(id.clone());
            let titles = self.titles.0.clone();
            let terminal = id.clone();
            let rename = Box::new(move |title: String| {
                let title = clean_name(&title)?.ok_or("the title is empty")?;
                titles
                    .send((terminal.clone(), title))
                    .map_err(|_| "the terminal list is closed".to_owned())
            });
            // A run that fails (the model did the task instead of titling
            // it, or timed out) leaves the start of the first prompt as the
            // name, as a child terminal gets: it is titled only once.
            let fallback = self
                .shared
                .slot_by_id(&id)
                .and_then(|slot| slot.task())
                .and_then(|task| name_from(&task));
            // Refused (already titling, too many runs): the fallback now, and
            // the title button is still there.
            let refused = begin_titling(
                &utility,
                &self.shared,
                &self.view.load(),
                &id,
                rename,
                fallback.clone(),
            )
            .is_err();
            if refused && let Some(name) = fallback {
                self.rename(&id, Some(name));
            }
        }
    }

    fn auto_name(&mut self, id: &str, text: &str) {
        if !self.first_ask.insert(id.to_owned()) {
            return;
        }
        let Some(name) = name_from(text) else { return };
        let child = self
            .list
            .terminals
            .iter()
            .any(|entry| entry.id == id && entry.parent.is_some() && entry.name.is_none());
        if child {
            self.rename(id, Some(name));
        }
    }

    /// Restart an exited, stopped or failed terminal in place (same id, same
    /// name). A saved session is picked up again.
    fn restart(&mut self, id: &str) {
        let Some(slot) = self.shared.slot_by_id(id) else {
            self.last_error = Some(TermError::Unknown.to_string());
            return;
        };
        let pane = &slot.pane;
        if pane.is_live() || pane.is_preparing() || pane.stage() == PaneStage::LoggingIn {
            self.last_error = Some(TermError::AlreadyRunning.to_string());
            return;
        }
        match pane.stage() {
            PaneStage::Running | PaneStage::Stopped => pane.request_restart(),
            _ => pane.request_start(),
        }
        // Started by hand: nothing is owed to what was cut short before.
        slot.interrupted.store(false, Ordering::SeqCst);
        if let Some(entry) = self.list.terminals.iter_mut().find(|entry| entry.id == id) {
            entry.interrupted = false;
        }
        self.set_desired(id, true);
        self.begin_watch(&slot, true);
    }

    fn continue_from(&mut self, history_id: &str) {
        let Some(record) = self
            .history
            .iter()
            .find(|record| record.entry.id == history_id)
            .cloned()
        else {
            self.last_error = Some("no such history entry".to_owned());
            return;
        };
        let Some(session) = record.session else {
            self.last_error = Some("that conversation has no session to resume".to_owned());
            return;
        };
        let new = New {
            tool: Some(tool_of(record.entry.tool)),
            name: record.entry.name,
            session: Some(session),
            ..New::default()
        };
        if let Err(err) = self.add(new, Begin::Start) {
            self.last_error = Some(err.to_string());
        }
    }

    fn load_transcript(&mut self, history_id: &str) {
        let Some(store) = &self.history_store else {
            return;
        };
        match store.transcript(history_id) {
            Ok(text) => {
                self.transcript = Some(Transcript {
                    history_id: history_id.to_owned(),
                    text: Arc::new(text),
                });
            }
            Err(err) => {
                self.last_error = Some(format!("The transcript could not be read ({err})."));
            }
        }
    }

    fn choose(&mut self, project: PathBuf) {
        // Re-checked here: a start may have begun since the caller looked.
        if busy(&self.shared) {
            self.last_error = Some("Stop all first".to_owned());
            return;
        }
        self.project = Some(project.clone());
        self.load_list();
        self.fill_slots();
        self.shared.project_seq.fetch_add(1, Ordering::SeqCst);
        crate::relay::show_project(&self.shared, &project);
    }

    fn handle(&mut self, command: Cmd) {
        let closing = self.shutdown.load(Ordering::SeqCst);
        match command {
            Cmd::MarkCutByUpdate { reply } => {
                let saved = self.mark_cut_by_update();
                let _ = reply.send(saved);
            }
            Cmd::UserAdd { tool, name, model } => {
                self.last_error = None;
                let new = New {
                    tool: Some(tool),
                    name,
                    model,
                    ..New::default()
                };
                if let Err(err) = self.add(new, Begin::Start) {
                    self.last_error = Some(err.to_string());
                }
            }
            Cmd::UserAddWith {
                tool,
                name,
                prompt,
                session,
                note,
                model,
                effort,
                work_epoch,
                reply,
            } => {
                self.last_error = None;
                let new = New {
                    tool: Some(tool),
                    name,
                    prompt,
                    session,
                    note,
                    model,
                    effort,
                    // Only Agentic Works dispatches work with an epoch.
                    job: work_epoch.is_some(),
                    ..New::default()
                };
                let pause = self.shared.pause();
                let result = if work_epoch
                    .is_some_and(|epoch| closing || pause.is_paused() || pause.epoch() != epoch)
                {
                    Err("Work dispatch cancelled by Stop, Pause or shutdown.".into())
                } else {
                    match self.add(new, Begin::Start) {
                        Ok(changed) => Ok(changed.id),
                        Err(err) => {
                            self.last_error = Some(err.to_string());
                            Err(err.to_string())
                        }
                    }
                };
                // The view first, so the id is already in it when it is known.
                self.publish();
                let _ = reply.send(result);
            }
            Cmd::BackgroundStopRefused { id, why } => {
                self.last_error = Some(format!("{id}: background shell not stopped. {why}"));
            }
            Cmd::UserClose(id) => {
                self.last_error = None;
                if let Err(err) = self.close(&id, None) {
                    self.last_error = Some(err.to_string());
                }
            }
            Cmd::Rename { id, name } => {
                self.last_error = None;
                self.rename(&id, name);
            }
            Cmd::Restart(id) => {
                self.last_error = None;
                self.restart(&id);
            }
            Cmd::Continue(history_id) => {
                self.last_error = None;
                self.continue_from(&history_id);
            }
            Cmd::Transcript(history_id) => {
                self.last_error = None;
                self.load_transcript(&history_id);
            }
            Cmd::ReadTranscript { history_id, reply } => {
                let text = self
                    .history_store
                    .as_ref()
                    .ok_or_else(|| "this project keeps no history".to_owned())
                    .and_then(|store| {
                        store
                            .transcript(&history_id)
                            .map_err(|err| format!("The transcript could not be read ({err})."))
                    });
                let _ = reply.send(text);
            }
            Cmd::AutoName { id, text } => self.auto_name(&id, &text),
            Cmd::Add {
                tool,
                name,
                launch: AgentLaunch { model, effort },
                caller,
                epoch,
                reply,
            } => {
                let pause = self.shared.pause();
                let result = if closing {
                    Err(TermError::Shutdown)
                } else if pause.is_paused() || pause.epoch() != epoch {
                    Err(TermError::Paused)
                } else {
                    let lineage = match caller.as_deref() {
                        None => Some((None, None)),
                        Some(caller) => self.lineage(caller),
                    };
                    match lineage {
                        // The caller was closed while this waited.
                        None => Err(TermError::Unknown),
                        Some((parent, asked_by)) => {
                            // What a job's terminal starts is a job's too,
                            // from the caller's own entry, whatever its parent.
                            let job = caller.as_deref().is_some_and(|caller| {
                                self.list
                                    .terminals
                                    .iter()
                                    .any(|entry| entry.id == caller && entry.job)
                            });
                            let new = New {
                                job,
                                effort,
                                tool: Some(tool),
                                name,
                                parent,
                                asked_by,
                                session: None,
                                prompt: None,
                                note: None,
                                model,
                            };
                            self.add(new, Begin::Agent(epoch))
                        }
                    }
                };
                // The view first, so a caller that reads it after the reply
                // sees the change.
                self.publish();
                let _ = reply.send(result);
            }
            Cmd::Start {
                id,
                epoch,
                attempt,
                reply,
            } => {
                let result = if closing {
                    Err(TermError::Shutdown)
                } else {
                    self.start(&id, epoch, attempt)
                };
                self.publish();
                let _ = reply.send(result);
            }
            Cmd::Close { id, admit, reply } => {
                let result = self.close(&id, Some(admit));
                self.publish();
                let _ = reply.send(result);
            }
            Cmd::Choose(project) => self.choose(project),
            Cmd::CloseWithin { id, allowed, reply } => {
                self.last_error = None;
                let result = self.close_within(&id, &allowed);
                let _ = reply.send(result);
            }
        }
        self.publish();
    }

    /// Follow what the panes do by themselves: a stop from the tab strip, an
    /// exit, a start from a button, and the session id a tool reports. Ignored
    /// once the app is closing, so a quit never saves "all stopped".
    fn observe(&mut self) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let slots: Vec<Arc<Slot>> = self.shared.slots.load().iter().flatten().cloned().collect();
        let mut session_changed = false;
        for slot in slots {
            // In a turn or not, as far as a running terminal says. One that is
            // still starting says nothing yet, so what was saved stands.
            let working = match slot.pane.stage() {
                PaneStage::Running if slot.pane.is_live() => {
                    Some(slot.published().0 != Phase::Idle)
                }
                // Not while a start is under way: the pane looks stopped
                // for a moment before its start is taken up.
                PaneStage::Running | PaneStage::Stopped | PaneStage::Failed { .. }
                    if !self.watch.get(&slot.index).is_some_and(|watch| {
                        watch
                            .grace_until
                            .is_some_and(|until| Instant::now() < until)
                    }) =>
                {
                    Some(false)
                }
                _ => None,
            };
            if let Some(working) = working
                && let Some(entry) = self
                    .list
                    .terminals
                    .iter_mut()
                    .find(|entry| entry.id == slot.id)
                && entry.working != working
            {
                entry.working = working;
                session_changed = true;
            }
            // A turn of its own, however short: whatever was cut short is
            // behind it. Counted, not watched, so a quick one is not missed.
            if slot.interrupted.load(Ordering::SeqCst) && slot.prompts.load(Ordering::SeqCst) > 0 {
                slot.interrupted.store(false, Ordering::SeqCst);
                if let Some(entry) = self
                    .list
                    .terminals
                    .iter_mut()
                    .find(|entry| entry.id == slot.id)
                {
                    entry.interrupted = false;
                }
                session_changed = true;
            }
            if let Some(session) = slot.tool_session().filter(|id| valid_session(id))
                && let Some(entry) = self
                    .list
                    .terminals
                    .iter_mut()
                    .find(|entry| entry.id == slot.id)
                && entry.session.as_deref() != Some(session.as_str())
            {
                entry.session = Some(session);
                session_changed = true;
            }
            let seen = look(&slot.pane);
            let watch = self.watch.entry(slot.index).or_default();
            let in_grace = watch
                .grace_until
                .is_some_and(|until| Instant::now() < until);
            let running = self
                .list
                .terminals
                .iter()
                .find(|entry| entry.id == slot.id)
                .is_some_and(|entry| entry.running);
            match seen {
                Look::Active => {
                    watch.stopped_ticks = 0;
                    watch.grace_until = None;
                    if !running {
                        self.set_desired(&slot.id, true);
                    }
                }
                Look::Stopped => {
                    watch.stopped_ticks = watch.stopped_ticks.saturating_add(1);
                    if running && !in_grace && watch.stopped_ticks >= STOPPED_TICKS {
                        self.set_desired(&slot.id, false);
                    }
                }
                // A CLI the project leaves out is not started again by itself
                // (a relaunch, an update): its conversation and the refusal
                // shown stay, a start by hand is checked again.
                Look::Refused => {
                    watch.stopped_ticks = 0;
                    watch.grace_until = None;
                    if let Some(entry) = self
                        .list
                        .terminals
                        .iter_mut()
                        .find(|entry| entry.id == slot.id)
                        && (entry.running || entry.interrupted || entry.working)
                    {
                        entry.running = false;
                        entry.interrupted = false;
                        entry.working = false;
                        session_changed = true;
                    }
                }
                Look::Other => {
                    watch.stopped_ticks = 0;
                    watch.grace_until = None;
                }
            }
        }
        if session_changed {
            self.save_now();
        }
    }

    fn tick(&mut self) {
        self.observe();
        self.auto_title();
        self.sync_names();
        if self.save == SaveState::Unsaved
            && self.retry_at.is_some_and(|at| Instant::now() >= at)
            && !self.shutdown.load(Ordering::SeqCst)
        {
            self.save_now();
            if self.save == SaveState::Saved {
                self.last_error = None;
            }
        }
        self.publish();
    }

    fn run(mut self, rx: &Receiver<Cmd>, closed: &AtomicBool) {
        while !closed.load(Ordering::SeqCst) {
            match rx.recv_timeout(TICK) {
                Ok(command) => self.handle(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
    }

    /// What the status dot shows (R7), from the signals the registry already
    /// has: the pane's stage, the turn phase, `needs-user` and the asks.
    fn activity(&mut self, entry: &Entry, slot: &Slot) -> TermActivity {
        let pane = &slot.pane;
        match pane.stage() {
            PaneStage::Idle => TermActivity::NotStarted,
            PaneStage::Stopped if pane.attempt() == 0 => TermActivity::NotStarted,
            PaneStage::Stopped => TermActivity::Exited,
            PaneStage::Preparing { .. } => TermActivity::Starting,
            PaneStage::NeedsLogin { .. } | PaneStage::LoggingIn => TermActivity::NeedsLogin,
            PaneStage::Failed { .. } => TermActivity::Failed,
            PaneStage::Running if !pane.is_live() => TermActivity::Exited,
            // A shell has no turns and sends no events: nothing is known about
            // what it is doing, and the dot says only that it runs.
            PaneStage::Running if entry.tool == Tool::Shell => TermActivity::Plain,
            PaneStage::Running => {
                let (phase, needs_user, _, _) = slot.published();
                // The latch first: a permission prompt arrives inside a turn,
                // and the user is wanted then, not when the turn is over.
                if needs_user {
                    // Typing after it was seen asking answers it.
                    let typed = pane.user_inputs();
                    let prompts = slot.prompts.load(Ordering::SeqCst);
                    let mark = self.needs_mark.entry(slot.uid).or_insert((typed, prompts));
                    // A prompt since then is a new turn, and a new ask.
                    if mark.1 != prompts {
                        *mark = (typed, prompts);
                    }
                    if typed == mark.0 {
                        return TermActivity::NeedsUser;
                    }
                } else {
                    self.needs_mark.remove(&slot.uid);
                }
                if phase != Phase::Idle {
                    return TermActivity::Working;
                }
                if slot.interrupted.load(Ordering::SeqCst)
                    && (slot.ready() || pane.bridge_off_reason().is_some())
                {
                    TermActivity::Interrupted
                } else if entry.parent.is_some() && slot.ask.load(Ordering::SeqCst) == 2 {
                    TermActivity::Done
                } else if !slot.ready() && pane.bridge_off_reason().is_none() {
                    TermActivity::Starting
                } else {
                    TermActivity::Idle
                }
            }
        }
    }

    fn publish(&mut self) {
        let entries = self.list.terminals.clone();
        let terminals: Vec<TerminalEntry> = entries
            .iter()
            .filter_map(|entry| {
                let slot = self.shared.slot_by_id(&entry.id)?;
                let session: Arc<dyn TerminalSession> = Arc::clone(&slot.pane) as _;
                let control: Arc<dyn PaneControl> = Arc::clone(&slot.pane) as _;
                let state = self.activity(entry, &slot);
                Some(TerminalEntry {
                    id: entry.id.clone(),
                    name: entry.name.clone(),
                    parent: entry.parent.clone(),
                    asked_by: entry.asked_by.clone(),
                    tool: kind(entry.tool),
                    state,
                    in_turn: state == TermActivity::Working && slot.published().0 == Phase::Working,
                    background: if state == TermActivity::Working {
                        slot.background_shown()
                            .iter()
                            .map(|task| task.work.clone())
                            .collect()
                    } else {
                        Vec::new()
                    },
                    finished_turns: slot.finished.load(Ordering::Relaxed),
                    session,
                    control: Some(control),
                    desired_running: entry.running,
                    shown_id: None,
                    activity: slot.activity.load(Ordering::Relaxed),
                })
            })
            .collect();
        let signature = format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            terminals
                .iter()
                .map(|entry| (
                    &entry.id,
                    &entry.name,
                    &entry.parent,
                    &entry.asked_by,
                    entry.state,
                    entry.finished_turns,
                    entry.desired_running,
                    entry.activity,
                    entry
                        .background
                        .iter()
                        .map(|work| (&work.id, &work.label))
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            self.save,
            self.last_error,
            self.warning,
            self.history_view
                .iter()
                .map(|entry| &entry.id)
                .collect::<Vec<_>>(),
            self.transcript.as_ref().map(|t| &t.history_id),
        );
        // The same panes behind a changed project are told apart by the ids
        // alone, which is what the UI reconciles on.
        if signature == self.signature {
            return;
        }
        self.signature = signature;
        self.view.store(Arc::new(TerminalsView {
            terminals,
            history: Arc::clone(&self.history_view),
            transcript: self.transcript.clone(),
            save: self.save,
            last_error: self.last_error.clone(),
            warning: self.warning.clone(),
        }));
        (self.wake)();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    /// A held `/rename` is refused at the relay's hold limit, so the sync must
    /// still be waiting for that refusal, and a second `/rename` is not sent
    /// meanwhile.
    #[test]
    fn the_rename_wait_outlives_the_relay_hold() {
        assert!(NAME_WRITE_WAIT > crate::input_ready::HOLD_LIMIT);
    }

    use std::io;
    use std::sync::Mutex;

    use connecto_audit::Audit;

    use super::*;
    use crate::Relay;
    use crate::relay::RelayConfig;

    /// The disk, with a failure switch for each step.
    #[derive(Default)]
    struct Disk {
        content: Option<String>,
        fail_read: bool,
        fail_write: bool,
        /// Writes that still succeed before every later one fails.
        writes_left: Option<usize>,
        fail_quarantine: bool,
        quarantined: bool,
        writes: usize,
    }

    #[derive(Clone, Default)]
    struct FakeIo(Arc<Mutex<Disk>>);

    impl ListIo for FakeIo {
        fn read(&self) -> io::Result<Option<String>> {
            let disk = self.0.lock().unwrap();
            if disk.fail_read {
                return Err(io::Error::other("unreadable"));
            }
            Ok(disk.content.clone())
        }

        fn write(&self, text: &str) -> io::Result<()> {
            let mut disk = self.0.lock().unwrap();
            if disk.fail_write || disk.writes_left == Some(0) {
                return Err(io::Error::other("disk full"));
            }
            if let Some(left) = &mut disk.writes_left {
                *left -= 1;
            }
            disk.content = Some(text.to_owned());
            disk.writes += 1;
            Ok(())
        }

        fn quarantine(&self) -> io::Result<()> {
            let mut disk = self.0.lock().unwrap();
            if disk.fail_quarantine {
                return Err(io::Error::other("cannot rename"));
            }
            disk.quarantined = true;
            disk.content = None;
            Ok(())
        }
    }

    struct Scene {
        root: PathBuf,
        relay: Relay,
    }

    impl Drop for Scene {
        fn drop(&mut self) {
            let _ = self.relay.shut_down(Duration::from_secs(2));
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn scene_with(name: &str, disk: &FakeIo, retry: Duration) -> Scene {
        scene_full(name, disk, retry, None)
    }

    fn scene_full(
        name: &str,
        disk: &FakeIo,
        retry: Duration,
        titler: Option<Arc<dyn utility::Titler>>,
    ) -> Scene {
        scene_launch(name, disk, retry, titler, false)
    }

    fn scene_launch(
        name: &str,
        disk: &FakeIo,
        retry: Duration,
        titler: Option<Arc<dyn utility::Titler>>,
        after_update: bool,
    ) -> Scene {
        let root = std::env::temp_dir().join(format!("mg-reg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home/project", "data"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let factory_disk = disk.clone();
        let relay = Relay::start_with(
            RelayConfig {
                layout,
                project: Some(root.join("home/project")),
                extras: LaunchExtras::default(),
                audit: Audit::open(&root.join("audit.jsonl")).unwrap(),
                wake: Arc::new(|| {}),
                default_tools: vec![Tool::Claude, Tool::Codex],
                pause: None,
                utility_settings: None,
                auto_title: true,
                after_update,
            },
            Some(Box::new(move |_| Box::new(factory_disk.clone()))),
            retry,
            titler,
        )
        .unwrap();
        Scene { root, relay }
    }

    fn scene(name: &str) -> Scene {
        scene_with(name, &FakeIo::default(), RETRY)
    }

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Every pane has finished its preflight. There is no store here, so each
    /// ends refused and nothing runs.
    fn settled(scene: &Scene) -> bool {
        scene
            .relay
            .panes()
            .iter()
            .all(|pane| matches!(pane.stage(), PaneStage::Failed { .. }))
    }

    fn ids(scene: &Scene) -> Vec<String> {
        scene
            .relay
            .terminals()
            .view()
            .terminals
            .iter()
            .map(|entry| entry.id.clone())
            .collect()
    }

    fn agent_add(scene: &Scene, tool: Tool) -> Result<Changed, TermError> {
        scene.relay.registry.agent_add(
            tool,
            None,
            AgentLaunch::default(),
            None,
            scene.relay.shared.pause().epoch(),
            Duration::from_secs(5),
        )
    }

    fn now_admit(scene: &Scene, id: &str) -> Admit {
        Admit {
            epoch: Some(scene.relay.shared.pause().epoch()),
            attempt: scene.relay.pane(id).unwrap().attempt(),
        }
    }

    /// A close that waited in the queue while a Pause came in, or while the
    /// terminal restarted, does nothing; the delayed self-close keeps its
    /// documented Pause exception and still checks the attempt.
    #[test]
    fn a_queued_close_is_decided_again_when_it_runs() {
        let disk = FakeIo::default();
        let scene = scene_with("closeadmit", &disk, RETRY);
        wait_for("preflight to end", || settled(&scene));
        let admitted = now_admit(&scene, "codex");
        scene.relay.shared.pause().pause();
        scene.relay.shared.pause().resume();
        let wait = Duration::from_secs(5);
        let stale = scene.relay.registry.agent_close("codex", admitted, wait);
        assert_eq!(
            stale,
            Err(TermError::Paused),
            "a Pause came after admission"
        );
        assert!(
            !disk
                .0
                .lock()
                .unwrap()
                .content
                .as_deref()
                .unwrap_or("")
                .contains("stopped")
        );
        // Another attempt than the one admitted is left alone.
        let other = Admit {
            attempt: admitted.attempt + 1,
            ..now_admit(&scene, "codex")
        };
        assert_eq!(
            scene.relay.registry.agent_close("codex", other, wait),
            Err(TermError::Restarted)
        );
        // The self-close exception: no epoch, so a Pause does not stop it.
        scene.relay.shared.pause().pause();
        let own = Admit {
            epoch: None,
            attempt: admitted.attempt,
        };
        let closed = scene
            .relay
            .registry
            .agent_close("codex", own, wait)
            .unwrap();
        assert_eq!(closed.removal, Removal::Done, "and a close removes it");
        let wrong = Admit {
            epoch: None,
            attempt: now_admit(&scene, "claude").attempt + 1,
        };
        assert_eq!(
            scene.relay.registry.agent_close("claude", wrong, wait),
            Err(TermError::Restarted),
            "a delayed self-close never kills a restarted attempt"
        );
        scene.relay.shared.pause().resume();
        // The same two checks for a start.
        let epoch = scene.relay.shared.pause().epoch();
        assert_eq!(
            scene
                .relay
                .registry
                .agent_start("claude", epoch, admitted.attempt + 1, wait),
            Err(TermError::Restarted)
        );
    }

    /// An exhausted counter refuses the add with a reason and changes nothing; an
    /// id at the top of the range makes the saved list unusable, with the
    /// warning and the defaults, not a panic.
    #[test]
    fn exhausted_id_counters_refuse_an_add_and_a_top_of_range_id_is_a_bad_list() {
        let disk = FakeIo::default();
        disk.0.lock().unwrap().content = Some(
            r#"{"version":1,"next":{"claude":4294967295},"terminals":
               [{"id":"claude","tool":"claude","desired":"running"}]}"#
                .to_owned(),
        );
        let scene = scene_with("exhausted", &disk, RETRY);
        let refused = agent_add(&scene, Tool::Claude);
        assert!(
            matches!(&refused, Err(TermError::SaveFailed(why)) if why.contains("used up")),
            "{refused:?}"
        );
        assert_eq!(ids(&scene), ["claude"]);
        drop(scene);

        let bad = FakeIo::default();
        bad.0.lock().unwrap().content = Some(
            r#"{"version":1,"terminals":
               [{"id":"claude-4294967295","tool":"claude","desired":"running"}]}"#
                .to_owned(),
        );
        let scene = scene_with("topid", &bad, RETRY);
        assert_eq!(ids(&scene), ["claude", "codex"], "the defaults");
        let view = scene.relay.terminals().view();
        assert!(
            view.warning
                .as_deref()
                .unwrap()
                .contains("terminals.json.bad")
        );
        assert!(bad.0.lock().unwrap().quarantined);
    }

    /// `close` takes the terminal out of the view and the saved list and into
    /// the history. The last one goes too.
    #[test]
    fn a_close_removes_the_terminal_and_keeps_it_in_the_history() {
        let disk = FakeIo::default();
        let scene = scene_with("closeremoves", &disk, RETRY);
        wait_for("preflight to end", || settled(&scene));
        let wait = Duration::from_secs(5);
        let closed = scene
            .relay
            .registry
            .agent_close("codex", now_admit(&scene, "codex"), wait)
            .unwrap();
        assert_eq!(closed.removal, Removal::Done);
        assert_eq!(
            ids(&scene),
            ["claude"],
            "the tab is gone from the live view"
        );
        assert_eq!(scene.relay.terminals().view().history.len(), 1);
        assert_eq!(saved_ids(&disk), ["claude"], "and from the saved list");
        assert!(scene.relay.pane("codex").is_none());
        assert_eq!(
            scene
                .relay
                .registry
                .agent_close("codex", now_admit_for(1), wait),
            Err(TermError::Unknown)
        );
        // The last one goes too (R8).
        let last = scene
            .relay
            .registry
            .agent_close("claude", now_admit(&scene, "claude"), wait)
            .unwrap();
        assert_eq!(last.removal, Removal::Done);
        assert!(ids(&scene).is_empty());
    }

    fn child_add(scene: &Scene, caller: &str) -> Result<Changed, TermError> {
        scene.relay.registry.agent_add(
            Tool::Claude,
            None,
            AgentLaunch::default(),
            Some(caller.to_owned()),
            scene.relay.shared.pause().epoch(),
            Duration::from_secs(5),
        )
    }

    /// Closing a parent closes its children and the grandchildren they asked
    /// for, each saved to the history; an unrelated terminal stays. An add
    /// from a caller that is gone is refused, not made a top-level terminal.
    #[test]
    fn a_close_takes_the_children_with_it() {
        let disk = FakeIo::default();
        let scene = scene_with("closecascade", &disk, RETRY);
        wait_for("preflight to end", || settled(&scene));
        let child = child_add(&scene, "claude").unwrap().id;
        let grandchild = child_add(&scene, &child).unwrap().id;
        let other = child_add(&scene, "codex").unwrap().id;
        assert_eq!(ids(&scene).len(), 5);
        let closed = scene
            .relay
            .registry
            .agent_close(
                "claude",
                now_admit(&scene, "claude"),
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(closed.removal, Removal::Done);
        assert_eq!(ids(&scene), ["codex", other.as_str()]);
        assert_eq!(saved_ids(&disk), ["codex", other.as_str()]);
        assert_eq!(scene.relay.terminals().view().history.len(), 3);
        assert!(scene.relay.pane(&child).is_none());
        assert!(scene.relay.pane(&grandchild).is_none());
        assert_eq!(child_add(&scene, "claude"), Err(TermError::Unknown));
        assert_eq!(ids(&scene), ["codex", other.as_str()]);
    }

    /// A close whose list cannot be saved still stops the terminal, says it was
    /// not removed, and leaves it listed.
    #[test]
    fn a_close_whose_list_cannot_be_saved_stops_and_keeps_the_terminal() {
        let disk = FakeIo::default();
        let scene = scene_with("closekeeps", &disk, RETRY);
        wait_for("preflight to end", || settled(&scene));
        disk.0.lock().unwrap().fail_write = true;
        let closed = scene
            .relay
            .registry
            .agent_close("codex", now_admit(&scene, "codex"), Duration::from_secs(5))
            .unwrap();
        assert!(matches!(closed.removal, Removal::Kept(_)), "{closed:?}");
        assert!(!closed.saved);
        assert_eq!(ids(&scene), ["claude", "codex"]);
    }

    fn now_admit_for(attempt: u64) -> Admit {
        Admit {
            epoch: None,
            attempt,
        }
    }

    fn saved_ids(disk: &FakeIo) -> Vec<String> {
        let text = disk.0.lock().unwrap().content.clone().unwrap_or_default();
        List::parse(&text)
            .map(|list| list.terminals.into_iter().map(|entry| entry.id).collect())
            .unwrap_or_default()
    }

    /// AC3: up to 16, a 17th is refused with a reason, and the list survives a
    /// relaunch from the same data.
    #[test]
    fn terminals_are_added_up_to_sixteen_and_the_list_survives_a_relaunch() {
        let disk = FakeIo::default();
        let first = scene_with("cap", &disk, RETRY);
        assert_eq!(ids(&first), ["claude", "codex"]);
        for _ in 0..14 {
            agent_add(&first, Tool::Claude).unwrap();
        }
        assert_eq!(ids(&first).len(), MAX_TERMINALS);
        assert_eq!(agent_add(&first, Tool::Codex), Err(TermError::CapReached));
        let user = first.relay.terminals().add(ToolKind::Codex, None);
        assert!(
            user.unwrap_err().contains("stopped"),
            "the reason suggests start"
        );
        let listed = ids(&first);
        assert_eq!(listed[2], "claude-2");
        assert_eq!(listed[15], "claude-15");
        assert_eq!(saved_ids(&disk), listed);
        drop(first);

        let again = scene_with("cap-again", &disk, RETRY);
        assert_eq!(ids(&again), listed);
    }

    /// ADR 0030: a terminal waiting on an `--ask` to one an update cut starts
    /// too, its ask is opened again, and it is not told "continua", even when
    /// its own turn was cut: the reply wakes it. Through a lead whose child
    /// waits on a grandchild, too.
    #[test]
    fn askers_of_a_cut_turn_start_without_continua_and_get_their_asks_back() {
        let entry = |id: &str, cut: bool, owes: &[(&str, &str)]| {
            let mut entry = Entry::new(id.to_owned(), Tool::Claude, true);
            entry.working = cut;
            entry.cut_by_update = cut;
            entry.session = Some(format!("{id}-session"));
            entry.owes = owes
                .iter()
                .map(|(request, to)| OwedReply {
                    request: (*request).to_owned(),
                    to: (*to).to_owned(),
                })
                .collect();
            entry
        };
        let mut list = vec![
            entry("lead", false, &[]),
            entry("mid", false, &[("r1", "lead")]),
            entry("coder", true, &[("r2", "mid")]),
            entry("cut-lead", true, &[]),
            entry("child", true, &[("r3", "cut-lead")]),
            entry("idle", false, &[("r4", "other")]),
            entry("other", false, &[]),
        ];
        let restore = plan_restore(&mut list, false, false);
        assert!(restore.marked);
        assert_eq!(
            restore.carry_on,
            BTreeSet::from(["coder".to_owned(), "child".to_owned()]),
            "the cut turns carry on, their askers do not"
        );
        let by = |id: &str| list.iter().find(|e| e.id == id).unwrap();
        for id in ["lead", "mid", "coder", "cut-lead", "child"] {
            assert!(by(id).running, "{id} starts");
        }
        assert!(!by("idle").running && !by("other").running);
        assert!(!by("lead").interrupted && !by("mid").interrupted);
        assert!(by("cut-lead").interrupted, "its turn was cut all the same");
        let mut asks: Vec<(String, String, String)> = restore
            .asks
            .into_iter()
            .map(|a| (a.request, a.from, a.target))
            .collect();
        asks.sort();
        assert_eq!(
            asks,
            vec![
                ("r1".into(), "lead".into(), "mid".into()),
                ("r2".into(), "mid".into(), "coder".into()),
                ("r3".into(), "cut-lead".into(), "child".into()),
            ],
            "an ask to a terminal that does not start is not opened again"
        );
        assert!(list.iter().all(|e| e.owes.is_empty()), "taken once");
        let again = plan_restore(&mut list, false, false);
        assert!(again.asks.is_empty() && again.carry_on.is_empty());
    }

    /// ADR 0030: at an update, the asks to a cut turn are saved on its
    /// target, and so on up the chain of askers; an ask to an idle terminal
    /// or from one that is not listed is not.
    #[test]
    fn an_update_saves_the_asks_to_the_turns_it_cuts_up_the_chain() {
        let ask = |request: &str, from: &str, target: &str| OpenAsk {
            request: request.to_owned(),
            from: from.to_owned(),
            target: target.to_owned(),
        };
        let set = |ids: &[&str]| {
            ids.iter()
                .map(|id| (*id).to_owned())
                .collect::<BTreeSet<_>>()
        };
        let owes = owed_replies(
            &set(&["coder"]),
            &[
                ask("r1", "lead", "mid"),
                ask("r2", "mid", "coder"),
                ask("r3", "x", "idle"),
                ask("r4", "gone", "coder"),
            ],
            &set(&["lead", "mid", "coder", "x", "idle"]),
        );
        let to = |id: &str| {
            owes.get(id)
                .map(|o| {
                    o.iter()
                        .map(|o| (o.request.clone(), o.to.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        assert_eq!(to("coder"), vec![("r2".to_owned(), "mid".to_owned())]);
        assert_eq!(to("mid"), vec![("r1".to_owned(), "lead".to_owned())]);
        assert!(to("idle").is_empty());
    }

    #[test]
    fn only_turns_cut_by_an_update_are_carried_on_once_the_app_session_too() {
        let entry = |id: &str, working: bool, cut: bool| {
            let mut entry = Entry::new(id.to_owned(), Tool::Claude, true);
            entry.working = working;
            entry.cut_by_update = cut;
            entry.session = Some(format!("{id}-session"));
            entry
        };
        let mut app = entry("claude-1", true, true);
        app.name = Some(connecto_core::session::APP_SESSION.to_owned());
        let mut no_session = entry("claude-4", true, true);
        no_session.session = None;
        let mut list = vec![
            app,
            entry("claude-2", true, false),  // a crash or a quit
            entry("claude-3", false, false), // idle
            no_session,
        ];
        // Marked by an update whose turn began after `working` was last saved.
        list.push(entry("claude-6", false, true));
        let Restore {
            marked, carry_on, ..
        } = plan_restore(&mut list, false, false);
        assert!(marked);
        assert_eq!(
            carry_on,
            BTreeSet::from(["claude-1".to_owned(), "claude-6".to_owned()])
        );
        assert!(list[4].running && list[4].interrupted);
        assert!(
            list[0].running,
            "the app session starts when an update cut it"
        );
        assert!(
            list[1].running && list[1].interrupted,
            "started, not carried on"
        );
        assert!(!list[2].running);
        assert!(
            list[3].running,
            "starts, but has no conversation to carry on"
        );
        assert!(list.iter().all(|e| !e.cut_by_update), "the mark is taken");
        // The next launch, before any new turn: nothing is carried on again.
        let again = plan_restore(&mut list, false, false).carry_on;
        assert!(again.is_empty());
        assert!(!list[0].running, "back to waiting for an explicit start");
        // A list made this launch carries nothing on.
        let mut fresh = vec![entry("claude-5", true, true)];
        assert!(plan_restore(&mut fresh, true, false).carry_on.is_empty());
    }

    #[test]
    fn an_update_quit_saves_the_turns_it_cuts() {
        let disk = FakeIo::default();
        let mut list = List::defaults(&[Tool::Claude, Tool::Codex]);
        list.terminals[0].working = true;
        disk.0.lock().unwrap().content = Some(list.to_json());
        let scene = scene_with("update-cut", &disk, RETRY);
        // The saved turn is an older one, cut by a crash: no agent is live in
        // a turn now, so the update cuts nothing, and the quit is saved.
        assert!(scene.relay.mark_cut_by_update(Duration::from_secs(5)));
        let saved = List::parse(disk.0.lock().unwrap().content.as_deref().unwrap()).unwrap();
        assert!(saved.terminals.iter().all(|e| !e.cut_by_update));
        assert!(
            saved.terminals[0].interrupted,
            "still interrupted, as before"
        );
        // Closing now: a second mark changes nothing.
        let writes = disk.0.lock().unwrap().writes;
        assert!(scene.relay.mark_cut_by_update(Duration::from_secs(5)));
        assert_eq!(disk.0.lock().unwrap().writes, writes);
    }

    /// A saved entry as a restore sees it: `running` is the saved `desired`.
    fn saved(id: &str, running: bool, working: bool, cut: bool) -> Entry {
        let mut entry = Entry::new(id.to_owned(), Tool::Claude, running);
        entry.working = working;
        entry.cut_by_update = cut;
        entry.session = Some(format!("{id}-session"));
        entry
    }

    fn by<'a>(list: &'a [Entry], id: &str) -> &'a Entry {
        list.iter().find(|e| e.id == id).unwrap()
    }

    /// ADR 0030 (t1): after an update every terminal saved running starts,
    /// idle ones and the app session included, with no note; one saved
    /// stopped stays stopped.
    #[test]
    fn after_an_update_every_running_terminal_starts_idle_ones_with_no_note() {
        let mut app = saved("claude-1", true, false, false);
        app.name = Some(connecto_core::session::APP_SESSION.to_owned());
        let mut list = vec![
            app,
            saved("claude-2", true, false, false),
            saved("claude-3", false, false, false),
            saved("claude-4", true, true, true),
        ];
        let restore = plan_restore(&mut list, false, true);
        assert!(by(&list, "claude-1").running, "the app session comes back");
        assert!(by(&list, "claude-2").running, "an idle one comes back");
        assert!(
            !by(&list, "claude-3").running,
            "a stopped one stays stopped"
        );
        assert!(by(&list, "claude-4").running);
        assert_eq!(
            restore.carry_on,
            BTreeSet::from(["claude-4".to_owned()]),
            "only the cut turn is told to carry on"
        );
        assert!(!by(&list, "claude-2").interrupted && !by(&list, "claude-1").interrupted);
    }

    /// ADR 0030 (t2): a plain launch leaves an idle terminal stopped, as
    /// before, and saves that, so `desired` is what runs.
    #[test]
    fn a_plain_launch_leaves_idle_terminals_stopped_and_saves_it() {
        let mut list = vec![saved("claude-1", true, false, false)];
        let restore = plan_restore(&mut list, false, false);
        assert!(!list[0].running);
        assert!(restore.marked, "the change of desired is saved");
        assert!(restore.carry_on.is_empty());
        // Nothing changes the next time: nothing to save.
        assert!(!plan_restore(&mut list, false, false).marked);
    }

    /// ADR 0030 (t3): the list an older version (0.2.18) saves at an update's
    /// quit, with no `owes` and nothing cut, brings back every terminal it
    /// says was running.
    #[test]
    fn a_list_saved_by_an_older_version_at_an_update_brings_back_what_ran() {
        let text = r#"{"version":1,"next":{"claude":5,"codex":3},"terminals":[
            {"id":"claude-1","tool":"claude","desired":"running","session":"s-1"},
            {"id":"claude-2","tool":"claude","desired":"running","session":"s-2","working":true},
            {"id":"codex-1","tool":"codex","desired":"running","parent":"claude-1"},
            {"id":"claude-3","tool":"claude","desired":"stopped","session":"s-3"},
            {"id":"claude-4","tool":"claude","desired":"running","name":"app-session"}]}"#;
        let mut list = List::parse(text).unwrap();
        let restore = plan_restore(&mut list.terminals, false, true);
        let running: Vec<&str> = list
            .terminals
            .iter()
            .filter(|e| e.running)
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(running, ["claude-1", "claude-2", "codex-1", "claude-4"]);
        assert!(restore.carry_on.is_empty() && restore.asks.is_empty());
        assert!(by(&list.terminals, "claude-2").interrupted);
    }

    /// ADR 0030 (t4): an idle parent whose child was cut, with no `--ask`
    /// between them, comes back too; only the child carries on.
    #[test]
    fn after_an_update_an_idle_parent_of_a_cut_child_starts_too() {
        let mut child = saved("codex-1", true, true, true);
        child.parent = Some("claude-1".to_owned());
        let mut list = vec![saved("claude-1", true, false, false), child];
        let restore = plan_restore(&mut list, false, true);
        assert!(list.iter().all(|e| e.running));
        assert_eq!(restore.carry_on, BTreeSet::from(["codex-1".to_owned()]));
    }

    /// ADR 0030 (t5): a parent cut mid-turn and its idle child both come
    /// back; only the parent carries on.
    #[test]
    fn after_an_update_the_idle_child_of_a_cut_parent_starts_too() {
        let mut child = saved("codex-1", true, false, false);
        child.parent = Some("claude-1".to_owned());
        let mut list = vec![saved("claude-1", true, true, true), child];
        let restore = plan_restore(&mut list, false, true);
        assert!(list.iter().all(|e| e.running));
        assert_eq!(restore.carry_on, BTreeSet::from(["claude-1".to_owned()]));
        assert!(!by(&list, "codex-1").interrupted);
    }

    /// ADR 0030 (t6): one at its prompt with background work (saved working,
    /// not cut) starts, interrupted, and is not told to carry on.
    #[test]
    fn after_an_update_background_work_starts_with_no_note() {
        let mut list = vec![saved("claude-1", true, true, false)];
        let restore = plan_restore(&mut list, false, true);
        assert!(list[0].running && list[0].interrupted);
        assert!(restore.carry_on.is_empty());
    }

    /// ADR 0030 (t7): a plain launch that leaves an idle terminal stopped
    /// writes that, and an update's quit writes its list even with nothing
    /// cut, so the update after does not bring that terminal back.
    #[test]
    fn a_terminal_a_plain_launch_left_stopped_stays_stopped_after_an_update() {
        let disk = FakeIo::default();
        let mut list = List::defaults(&[Tool::Claude]);
        list.terminals[0].session = Some("0b1c2d3e-0000-4000-8000-000000000001".to_owned());
        disk.0.lock().unwrap().content = Some(list.to_json());
        {
            let scene = scene_with("plain-then-update", &disk, RETRY);
            let view = scene.relay.terminals().view();
            assert!(!view.terminals[0].desired_running);
            let saved = List::parse(disk.0.lock().unwrap().content.as_deref().unwrap()).unwrap();
            assert!(!saved.terminals[0].running, "written at launch");
            let writes = disk.0.lock().unwrap().writes;
            assert!(scene.relay.mark_cut_by_update(Duration::from_secs(5)));
            assert!(
                disk.0.lock().unwrap().writes > writes,
                "an update's quit writes even with nothing cut"
            );
        }
        let saved = List::parse(disk.0.lock().unwrap().content.as_deref().unwrap()).unwrap();
        assert!(!saved.terminals[0].running);
        let scene = scene_launch("plain-then-update-2", &disk, RETRY, None, true);
        assert!(!scene.relay.terminals().view().terminals[0].desired_running);
    }

    /// ADR 0030: an update's quit that cuts nothing still says when its list
    /// could not be written, since the next launch restores from it.
    #[test]
    fn an_update_quit_with_nothing_cut_reports_a_failed_write() {
        let disk = FakeIo::default();
        disk.0.lock().unwrap().content = Some(List::defaults(&[Tool::Claude]).to_json());
        let scene = scene_with("update-write-fails", &disk, RETRY);
        disk.0.lock().unwrap().fail_write = true;
        assert!(!scene.relay.mark_cut_by_update(Duration::from_secs(5)));
    }

    /// ADR 0030 (t8): the update's restore is the first one only: a project
    /// chosen later restores as after a quit.
    #[test]
    fn only_the_first_restore_after_an_update_brings_back_idle_terminals() {
        let disk = FakeIo::default();
        let list = List::defaults(&[Tool::Claude]);
        let idle = list.to_json();
        disk.0.lock().unwrap().content = Some(idle.clone());
        let scene = scene_launch("first-restore", &disk, RETRY, None, true);
        assert!(scene.relay.terminals().view().terminals[0].desired_running);
        wait_for("settled", || settled(&scene));
        disk.0.lock().unwrap().content = Some(idle);
        let other = scene.root.join("home/other");
        fs::create_dir_all(&other).unwrap();
        scene.relay.registry.choose_project(&other).unwrap();
        // Waited for, not looked at once: the view is published after the
        // choice. Restored as after the update, it would never show stopped.
        wait_for("the chosen project restored as after a quit", || {
            scene
                .relay
                .terminals()
                .view()
                .terminals
                .first()
                .is_some_and(|e| !e.desired_running)
        });
    }

    #[test]
    fn restoring_an_interrupted_app_session_waits_for_an_explicit_start() {
        let disk = FakeIo::default();
        let mut list = List::defaults(&[Tool::Claude]);
        let app = &mut list.terminals[0];
        app.name = Some(connecto_core::session::APP_SESSION.to_owned());
        app.working = true;
        disk.0.lock().unwrap().content = Some(list.to_json());
        let scene = scene_with("app-session-restore", &disk, RETRY);
        let view = scene.relay.terminals().view();
        assert_eq!(view.terminals[0].state, TermActivity::NotStarted);
        assert!(!view.terminals[0].desired_running);
    }

    /// The phone starts the app session with no effort: it launches on
    /// high, its analyses being the point of it, while any other terminal
    /// added without one keeps the tool's own.
    #[test]
    fn the_app_session_launches_on_high_effort() {
        let scene = scene("app-session-effort");
        let control = scene.relay.terminals();
        let app = control
            .add_with(
                ToolKind::Claude,
                Some(connecto_core::session::APP_SESSION.into()),
                None,
                None,
                None,
            )
            .unwrap();
        let other = control
            .add_with(ToolKind::Claude, Some("plain".into()), None, None, None)
            .unwrap();
        let effort = |id: &str| {
            scene
                .relay
                .shared
                .slot_by_id(id)
                .unwrap()
                .pane
                .launch_effort()
        };
        assert_eq!(effort(&app).as_deref(), Some("high"));
        assert_eq!(effort(&other), None);
    }

    #[test]
    fn repeated_app_creations_reuse_the_first_provider_and_entry() {
        let scene = scene("app-session");
        let control = scene.relay.terminals();
        let add = |tool| {
            control
                .add_with(
                    tool,
                    Some(connecto_core::session::APP_SESSION.into()),
                    None,
                    None,
                    None,
                )
                .unwrap()
        };
        assert_eq!(add(ToolKind::Pi), add(ToolKind::Claude));
        let view = control.view();
        let chats: Vec<_> = view
            .terminals
            .iter()
            .filter(|e| e.name.as_deref() == Some(connecto_core::session::APP_SESSION))
            .collect();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].tool, ToolKind::Pi);
    }

    /// `add_with` answers with the id of the terminal it made, and that
    /// terminal is already in the view when the id is known. A refused add
    /// answers with the reason instead.
    #[test]
    fn add_with_returns_the_id_of_the_terminal_it_made() {
        let disk = FakeIo::default();
        let scene = scene_with("addwith", &disk, RETRY);
        let control = scene.relay.terminals();
        let first = control
            .add_with(
                ToolKind::Claude,
                Some("A".into()),
                Some("hello".into()),
                None,
                None,
            )
            .unwrap();
        let second = control
            .add_with(
                ToolKind::Claude,
                Some("B".into()),
                None,
                Some("sess-1".into()),
                Some("note".into()),
            )
            .unwrap();
        assert_ne!(first, second);
        let view = control.view();
        for (id, name) in [(&first, "A"), (&second, "B")] {
            let entry = view.terminals.iter().find(|e| &e.id == id).unwrap();
            assert_eq!(entry.name.as_deref(), Some(name));
        }
        // Not a session id: refused before anything is made.
        let bad = control.add_with(ToolKind::Claude, None, None, Some("--last".into()), None);
        assert!(bad.is_err());
        assert_eq!(view.terminals.len(), control.view().terminals.len());
    }

    /// A terminal that opens with a prompt on a model (Settings > CLI
    /// improvements): named, on that model from its first start, and its id
    /// given back. A model that is not one, or a shell with one, is refused.
    #[test]
    fn add_prompted_starts_a_named_terminal_on_its_model() {
        use connecto_core::cli_improvements::{INSIGHTS_MODEL, INSIGHTS_TERMINAL, insights_prompt};
        let disk = FakeIo::default();
        let scene = scene_with("addprompted", &disk, RETRY);
        let control = scene.relay.terminals();
        let id = control
            .add_prompted(
                ToolKind::Claude,
                INSIGHTS_TERMINAL.into(),
                insights_prompt(),
                Some(INSIGHTS_MODEL.into()),
            )
            .unwrap();
        let view = control.view();
        let entry = view.terminals.iter().find(|e| e.id == id).unwrap();
        assert_eq!(entry.name.as_deref(), Some(INSIGHTS_TERMINAL));
        assert_eq!(entry.tool, ToolKind::Claude);
        let saved: serde_json::Value =
            serde_json::from_str(disk.0.lock().unwrap().content.as_deref().unwrap()).unwrap();
        let text = saved.to_string();
        assert!(
            text.contains(&format!("\"{INSIGHTS_MODEL}\"")),
            "kept on its model: {text}"
        );
        let count = view.terminals.len();
        for (tool, model) in [
            (ToolKind::Claude, "--dangerously-skip-permissions"),
            (ToolKind::Shell, "opus"),
        ] {
            assert!(
                control
                    .add_prompted(tool, "x".into(), "hi".into(), Some(model.into()))
                    .is_err(),
                "{tool:?} {model}"
            );
        }
        assert_eq!(control.view().terminals.len(), count, "nothing made");
    }

    /// T54: an add whose write fails leaves no slot, no process, and `next`.
    #[test]
    fn an_add_that_cannot_be_saved_changes_nothing_and_gives_out_no_id() {
        let disk = FakeIo::default();
        let scene = scene_with("addfail", &disk, RETRY);
        disk.0.lock().unwrap().fail_write = true;
        let refused = agent_add(&scene, Tool::Claude);
        assert!(
            matches!(refused, Err(TermError::SaveFailed(_))),
            "{refused:?}"
        );
        assert_eq!(ids(&scene), ["claude", "codex"]);
        assert_eq!(scene.relay.panes().len(), 2);
        disk.0.lock().unwrap().fail_write = false;
        // The unused id was never given out.
        assert_eq!(agent_add(&scene, Tool::Claude).unwrap().id, "claude-2");
    }

    /// An add whose terminal cannot be made after the list was written takes
    /// the entry back out: nothing is listed without a slot, in memory or on
    /// disk, the cap does not count it, and its id was never given out.
    #[test]
    fn an_add_whose_terminal_cannot_be_made_leaves_no_entry_behind() {
        let disk = FakeIo::default();
        let scene = scene_with("addnoslot", &disk, Duration::from_millis(100));
        wait_for("preflight to end", || settled(&scene));
        let control = scene.relay.terminals();
        let fail = &scene.relay.shared.fail_install;
        fail.store(true, Ordering::SeqCst);
        for _ in 0..MAX_TERMINALS {
            let refused = agent_add(&scene, Tool::Claude);
            assert!(
                matches!(refused, Err(TermError::SaveFailed(_))),
                "{refused:?}"
            );
        }
        assert_eq!(ids(&scene), ["claude", "codex"]);
        assert_eq!(saved_ids(&disk), ["claude", "codex"]);
        assert_eq!(scene.relay.panes().len(), 2);
        assert_eq!(control.view().save, SaveState::Saved);
        fail.store(false, Ordering::SeqCst);
        // Not at the cap, and the unused id was never given out.
        assert_eq!(agent_add(&scene, Tool::Claude).unwrap().id, "claude-2");
        wait_for("claude-2", || ids(&scene).len() == 3);

        // When the disk also refuses the way back, the entry is gone from
        // memory at once and the retry takes it off the disk.
        fail.store(true, Ordering::SeqCst);
        disk.0.lock().unwrap().writes_left = Some(1);
        let refused = agent_add(&scene, Tool::Codex);
        assert!(
            matches!(refused, Err(TermError::SaveFailed(_))),
            "{refused:?}"
        );
        assert_eq!(saved_ids(&disk), ["claude", "codex", "claude-2", "codex-2"]);
        wait_for("the unsaved warning", || {
            control.view().save == SaveState::Unsaved
        });
        assert_eq!(ids(&scene), ["claude", "codex", "claude-2"]);
        assert!(
            control.close("codex-2").is_err(),
            "nothing of it is left to close"
        );
        disk.0.lock().unwrap().writes_left = None;
        wait_for("the retry", || control.view().save == SaveState::Saved);
        assert_eq!(saved_ids(&disk), ["claude", "codex", "claude-2"]);
        fail.store(false, Ordering::SeqCst);
        assert_eq!(agent_add(&scene, Tool::Codex).unwrap().id, "codex-2");
    }

    /// T54: a close whose write fails keeps the terminal, and a later close
    /// works.
    #[test]
    fn a_close_that_cannot_be_saved_keeps_the_terminal() {
        let disk = FakeIo::default();
        let scene = scene_with("rmfail", &disk, RETRY);
        let control = scene.relay.terminals();
        agent_add(&scene, Tool::Claude).unwrap();
        wait_for("claude-2", || ids(&scene).len() == 3);
        // No store here, so every pane ends Failed: nothing runs.
        wait_for("stopped panes", || settled(&scene));
        disk.0.lock().unwrap().fail_write = true;
        control.close("claude-2").unwrap();
        wait_for("the failure to show", || {
            control.view().last_error.is_some()
        });
        assert_eq!(ids(&scene).len(), 3, "the terminal stays listed");
        assert!(scene.relay.pane("claude-2").is_some());
        disk.0.lock().unwrap().fail_write = false;
        control.close("claude-2").unwrap();
        wait_for("the removal", || ids(&scene).len() == 2);
        assert!(scene.relay.pane("claude-2").is_none());
        assert_eq!(saved_ids(&disk), ["claude", "codex"]);
    }

    /// R8: the "last terminal is only stopped" rule is gone. A project may end
    /// with none, and the empty list is a list.
    #[test]
    fn a_project_may_end_with_no_terminals() {
        let disk = FakeIo::default();
        let scene = scene_with("rmlast", &disk, RETRY);
        let control = scene.relay.terminals();
        wait_for("stopped panes", || settled(&scene));
        control.close("codex").unwrap();
        wait_for("codex gone", || ids(&scene) == ["claude"]);
        control.close("claude").unwrap();
        wait_for("claude gone", || ids(&scene).is_empty());
        assert!(control.view().last_error.is_none());
        assert!(saved_ids(&disk).is_empty());
        let text = disk.0.lock().unwrap().content.clone().unwrap();
        assert!(List::parse(&text).unwrap().terminals.is_empty());
        assert_eq!(control.view().history.len(), 2);
    }

    /// T9 and T48: a quit keeps `desired: running`, whatever order exits arrive
    /// in. The same stop, with no quit under way, is saved.
    #[test]
    fn shutdown_keeps_running_and_a_stop_without_it_is_saved() {
        let disk = FakeIo::default();
        let scene = scene_with("quit", &disk, RETRY);
        agent_add(&scene, Tool::Claude).unwrap();
        let project = scene.root.join("home/project");
        let panes = scene.relay.panes();
        // A user's Stop: the pane looks stopped, the list follows.
        let codex = scene.relay.pane("codex").unwrap();
        wait_for("preflight to end", || settled(&scene));
        codex.begin_project(project.clone(), crate::pane::Begin::Stopped);
        wait_for("desired stopped saved", || {
            disk.0
                .lock()
                .unwrap()
                .content
                .as_deref()
                .is_some_and(|text| text.contains("stopped"))
        });
        // Now a quit: every pane stops, and nothing more is saved.
        let before = disk.0.lock().unwrap().writes;
        scene.relay.begin_shutdown();
        for pane in &panes {
            pane.begin_project(project.clone(), crate::pane::Begin::Stopped);
        }
        thread::sleep(Duration::from_millis(1500));
        assert_eq!(
            disk.0.lock().unwrap().writes,
            before,
            "a quit saved a change"
        );
        let text = disk.0.lock().unwrap().content.clone().unwrap();
        assert!(
            text.contains(r#""id": "claude""#) && text.contains("running"),
            "{text}"
        );
    }

    /// T9: a bad file falls back to the default two with a warning, and keeps
    /// the bad file aside.
    #[test]
    fn a_bad_list_falls_back_with_a_warning_and_is_kept_aside() {
        let disk = FakeIo::default();
        disk.0.lock().unwrap().content = Some("{ not json".to_owned());
        let scene = scene_with("bad", &disk, RETRY);
        assert_eq!(ids(&scene), ["claude", "codex"]);
        let view = scene.relay.terminals().view();
        assert!(
            view.warning
                .as_deref()
                .unwrap()
                .contains("terminals.json.bad")
        );
        assert!(disk.0.lock().unwrap().quarantined);
        assert_eq!(view.save, SaveState::Saved);
    }

    /// T56: a file that cannot be read is never overwritten in that run.
    #[test]
    fn an_unreadable_list_is_never_written_this_run() {
        for (read, quarantine) in [(true, false), (false, true)] {
            let disk = FakeIo::default();
            {
                let mut state = disk.0.lock().unwrap();
                state.fail_read = read;
                state.fail_quarantine = quarantine;
                state.content = quarantine.then(|| "garbage".to_owned());
            }
            let scene = scene_with("readonly", &disk, RETRY);
            let control = scene.relay.terminals();
            assert_eq!(control.view().save, SaveState::ReadOnly);
            assert_eq!(ids(&scene), ["claude", "codex"]);
            assert!(matches!(
                agent_add(&scene, Tool::Claude),
                Err(TermError::SaveFailed(_))
            ));
            // A stop works and is held in memory only.
            let closed = scene
                .relay
                .registry
                .agent_close(
                    "claude",
                    now_admit(&scene, "claude"),
                    Duration::from_secs(5),
                )
                .unwrap();
            assert!(!closed.saved);
            assert_eq!(disk.0.lock().unwrap().writes, 0);
            assert_ne!(disk.0.lock().unwrap().content.as_deref(), Some("x"));
        }
    }

    /// T55: a change whose save fails is reported and kept in memory, and a
    /// later retry heals the file.
    #[test]
    fn a_change_whose_save_fails_is_reported_and_a_retry_heals_it() {
        let disk = FakeIo::default();
        let scene = scene_with("heal", &disk, Duration::from_millis(100));
        wait_for("preflight to end", || settled(&scene));
        let control = scene.relay.terminals();
        disk.0.lock().unwrap().fail_write = true;
        control.rename("codex", Some("review".to_owned())).unwrap();
        wait_for("the unsaved warning", || {
            control.view().save == SaveState::Unsaved
        });
        assert!(control.view().last_error.is_some());
        assert_eq!(control.view().terminals[1].name.as_deref(), Some("review"));
        disk.0.lock().unwrap().fail_write = false;
        wait_for("the retry", || control.view().save == SaveState::Saved);
        let text = disk.0.lock().unwrap().content.clone().unwrap();
        assert!(text.contains("review"), "the healed file holds it: {text}");
    }

    /// A close whose transcript cannot be archived keeps the stopped terminal
    /// listed, says why, and works once the folder can be written.
    #[test]
    fn a_close_that_cannot_be_archived_keeps_the_terminal() {
        let disk = FakeIo::default();
        let scene = scene_with("archivefail", &disk, RETRY);
        wait_for("preflight to end", || settled(&scene));
        let project = fs::canonicalize(scene.root.join("home/project")).unwrap();
        let history = History::dir_of(&scene.root.join("data"), &project_id(&project));
        // A file where the folder should be.
        fs::create_dir_all(history.parent().unwrap()).unwrap();
        fs::write(&history, "in the way").unwrap();
        let control = scene.relay.terminals();
        control.close("codex").unwrap();
        wait_for("the failure", || {
            control
                .view()
                .last_error
                .as_deref()
                .is_some_and(|why| why.contains("history could not be saved"))
        });
        assert_eq!(ids(&scene), ["claude", "codex"], "it stays listed");
        assert!(scene.relay.pane("codex").is_some());
        assert!(control.view().history.is_empty());
        fs::remove_file(&history).unwrap();
        control.close("codex").unwrap();
        wait_for("the close", || ids(&scene) == ["claude"]);
        assert_eq!(control.view().history.len(), 1);
    }

    #[test]
    fn an_agent_add_under_pause_is_refused_and_a_stale_epoch_too() {
        let scene = scene("paused");
        let epoch = scene.relay.shared.pause().epoch();
        scene.relay.shared.pause().pause();
        let refused = scene.relay.registry.agent_add(
            Tool::Claude,
            None,
            AgentLaunch::default(),
            None,
            epoch,
            Duration::from_secs(5),
        );
        assert_eq!(refused, Err(TermError::Paused));
        scene.relay.shared.pause().resume();
        // The admission was taken before the Pause: still refused after Resume.
        let stale = scene.relay.registry.agent_add(
            Tool::Claude,
            None,
            AgentLaunch::default(),
            None,
            epoch,
            Duration::from_secs(5),
        );
        assert_eq!(stale, Err(TermError::Paused));
        assert_eq!(ids(&scene).len(), 2);
    }

    struct FixedTitler(Result<String, String>);

    impl utility::Titler for FixedTitler {
        fn reply(
            &self,
            _: &utility::TitleJob,
            _: Instant,
            _: &AtomicBool,
            _: &dyn Fn(Arc<ManagedPane>),
        ) -> Result<String, String> {
            self.0.clone()
        }
    }

    fn name_of(scene: &Scene, id: &str) -> Option<String> {
        let view = scene.relay.terminals().view();
        view.terminals.iter().find(|t| t.id == id)?.name.clone()
    }

    fn titled(name: &str, reply: Result<&str, &str>, task: Option<&str>) -> (Scene, String) {
        let titler = FixedTitler(reply.map(str::to_owned).map_err(str::to_owned));
        let scene = scene_full(name, &FakeIo::default(), RETRY, Some(Arc::new(titler)));
        wait_for("the terminals", || !ids(&scene).is_empty());
        let id = ids(&scene)[0].clone();
        if let Some(task) = task {
            scene.relay.shared.slot_by_id(&id).unwrap().note_task(task);
        }
        (scene, id)
    }

    fn finished(scene: &Scene) -> connecto_core::UtilityRun {
        let control = scene.relay.terminals();
        wait_for("the run to end", || {
            control
                .utility_runs()
                .first()
                .is_some_and(|run| run.status != connecto_core::UtilityStatus::Running)
        });
        control.utility_runs()[0].clone()
    }

    #[test]
    fn a_titling_run_renames_the_terminal_without_a_new_terminal() {
        let (scene, id) = titled("t-ok", Ok("Fix login redirect"), Some("fix the login bug"));
        let control = scene.relay.terminals();
        let before = ids(&scene);
        control.start_titling(&id).unwrap();
        let run = finished(&scene);
        assert_eq!(
            run.status,
            connecto_core::UtilityStatus::Done {
                title: "Fix login redirect".into()
            }
        );
        wait_for("the name", || {
            name_of(&scene, &id).as_deref() == Some("Fix login redirect")
        });
        assert_eq!(ids(&scene), before, "no terminal was added");
        assert_eq!(control.view().terminals.len(), before.len());
    }

    #[test]
    fn a_failed_titling_run_keeps_the_name() {
        let (scene, id) = titled("t-bad", Err("timed out after 60 s"), Some("task"));
        let control = scene.relay.terminals();
        control.rename(&id, Some("Mine".into())).unwrap();
        wait_for("the rename", || {
            name_of(&scene, &id).as_deref() == Some("Mine")
        });
        control.start_titling(&id).unwrap();
        let run = finished(&scene);
        assert!(matches!(
            run.status,
            connecto_core::UtilityStatus::Failed { .. }
        ));
        assert_eq!(run.terminal_label, "Mine");
        assert_eq!(name_of(&scene, &id).as_deref(), Some("Mine"));
    }

    #[test]
    fn titling_refuses_an_unknown_terminal_and_one_with_nothing_to_describe() {
        let (scene, id) = titled("t-none", Ok("Fix login redirect"), None);
        let control = scene.relay.terminals();
        assert!(control.start_titling("nope").is_err());
        control.start_titling(&id).unwrap();
        let run = finished(&scene);
        assert!(matches!(
            run.status,
            connecto_core::UtilityStatus::Failed { .. }
        ));
        assert_eq!(name_of(&scene, &id), None);
    }

    /// What the relay does on a submitted prompt, as far as titling looks.
    fn prompt(scene: &Scene, id: &str, text: &str) {
        let slot = scene.relay.shared.slot_by_id(id).unwrap();
        slot.note_task(text);
        slot.prompts.fetch_add(1, Ordering::SeqCst);
    }

    /// An unnamed terminal is titled on its first prompt, by the same run as
    /// the title button, and never again on a later one.
    #[test]
    fn the_first_prompt_titles_an_unnamed_terminal_once() {
        let (scene, id) = titled("t-auto", Ok("Fix login redirect"), None);
        let control = scene.relay.terminals();
        thread::sleep(TICK * 3);
        assert!(control.utility_runs().is_empty(), "nothing before a prompt");
        prompt(&scene, &id, "fix the login bug");
        let run = finished(&scene);
        assert_eq!(run.terminal_id, id);
        wait_for("the name", || {
            name_of(&scene, &id).as_deref() == Some("Fix login redirect")
        });
        // Cleared by the user, then prompted again: no second run.
        control.rename(&id, None).unwrap();
        wait_for("the name to clear", || name_of(&scene, &id).is_none());
        prompt(&scene, &id, "and the logout one");
        thread::sleep(TICK * 3);
        assert_eq!(control.utility_runs().len(), 1);
        assert_eq!(name_of(&scene, &id), None);
    }

    /// A first prompt the model does instead of titling (a spec full of
    /// orders) fails the run: the start of the prompt is the name, as for a
    /// child terminal, since there is no second run.
    #[test]
    fn a_failed_first_title_names_the_terminal_from_its_first_prompt() {
        let (scene, id) = titled(
            "t-auto-fail",
            Ok("Here is the plan for the feature. First, the database."),
            None,
        );
        prompt(&scene, &id, "add the jobs table\nthen check it");
        let run = finished(&scene);
        assert!(matches!(
            run.status,
            connecto_core::UtilityStatus::Failed { .. }
        ));
        wait_for("the fallback name", || {
            name_of(&scene, &id).as_deref() == Some("add the jobs table then check it")
        });
    }

    /// A name the user gave before the first prompt is never titled over.
    #[test]
    fn a_named_terminal_is_not_titled_on_its_first_prompt() {
        let (scene, id) = titled("t-named", Ok("Fix login redirect"), None);
        let control = scene.relay.terminals();
        control.rename(&id, Some("Mine".into())).unwrap();
        wait_for("the rename", || {
            name_of(&scene, &id).as_deref() == Some("Mine")
        });
        prompt(&scene, &id, "fix the login bug");
        thread::sleep(TICK * 3);
        assert!(control.utility_runs().is_empty());
        assert_eq!(name_of(&scene, &id).as_deref(), Some("Mine"));
    }

    /// A name the user gives while the run is going wins over its title.
    #[test]
    fn a_name_given_during_the_run_is_kept() {
        let titler = Arc::new(GatedTitler {
            calls: std::sync::atomic::AtomicUsize::new(0),
            gate: Arc::new(AtomicBool::new(false)),
        });
        let scene = scene_full(
            "t-autorace",
            &FakeIo::default(),
            RETRY,
            Some(Arc::clone(&titler) as Arc<dyn utility::Titler>),
        );
        wait_for("the terminals", || !ids(&scene).is_empty());
        let id = ids(&scene)[0].clone();
        prompt(&scene, &id, "fix the login bug");
        wait_for("the titler", || titler.calls.load(Ordering::SeqCst) == 1);
        let control = scene.relay.terminals();
        control.rename(&id, Some("Mine".into())).unwrap();
        wait_for("the rename", || {
            name_of(&scene, &id).as_deref() == Some("Mine")
        });
        titler.gate.store(true, Ordering::SeqCst);
        let run = finished(&scene);
        assert!(matches!(
            run.status,
            connecto_core::UtilityStatus::Done { .. }
        ));
        thread::sleep(TICK * 3);
        assert_eq!(name_of(&scene, &id).as_deref(), Some("Mine"));
    }

    struct GatedTitler {
        calls: std::sync::atomic::AtomicUsize,
        gate: Arc<AtomicBool>,
    }

    impl utility::Titler for GatedTitler {
        fn reply(
            &self,
            _: &utility::TitleJob,
            _: Instant,
            _: &AtomicBool,
            _: &dyn Fn(Arc<ManagedPane>),
        ) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            while !self.gate.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            Ok("Fix login redirect".to_owned())
        }
    }

    #[test]
    fn many_quick_clicks_start_exactly_one_run() {
        let titler = Arc::new(GatedTitler {
            calls: std::sync::atomic::AtomicUsize::new(0),
            gate: Arc::new(AtomicBool::new(false)),
        });
        let scene = scene_full(
            "t-race",
            &FakeIo::default(),
            RETRY,
            Some(Arc::clone(&titler) as Arc<dyn utility::Titler>),
        );
        wait_for("the terminals", || !ids(&scene).is_empty());
        let id = ids(&scene)[0].clone();
        scene
            .relay
            .shared
            .slot_by_id(&id)
            .unwrap()
            .note_task("fix the login bug");
        let control = scene.relay.terminals();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let results: Vec<Result<(), String>> = (0..8)
            .map(|_| {
                let (control, id, barrier) =
                    (Arc::clone(&control), id.clone(), Arc::clone(&barrier));
                thread::spawn(move || {
                    barrier.wait();
                    control.start_titling(&id)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        assert!(
            results
                .iter()
                .filter_map(|r| r.as_ref().err())
                .all(|e| e == "already titling")
        );
        // Still refused while it runs, and only one run exists.
        assert_eq!(
            control.start_titling(&id),
            Err("already titling".to_owned())
        );
        wait_for("the titler", || titler.calls.load(Ordering::SeqCst) == 1);
        assert_eq!(control.utility_runs().len(), 1);
        titler.gate.store(true, Ordering::SeqCst);
        let run = finished(&scene);
        assert!(matches!(
            run.status,
            connecto_core::UtilityStatus::Done { .. }
        ));
        assert_eq!(titler.calls.load(Ordering::SeqCst), 1);
        assert_eq!(control.utility_runs().len(), 1);
    }

    /// A scene with one terminal this test added (the scene starts with its
    /// own default terminals, which are not the test's).
    fn one_terminal(name: &str) -> (Scene, String) {
        let scene = scene(name);
        let before = ids(&scene);
        agent_add(&scene, Tool::Claude).unwrap();
        let id = ids(&scene)
            .into_iter()
            .find(|id| !before.contains(id))
            .unwrap();
        (scene, id)
    }

    fn child_of(scene: &Scene, parent: &str) -> String {
        let epoch = scene.relay.shared.pause().epoch();
        let before = ids(scene);
        scene
            .relay
            .registry
            .agent_add(
                Tool::Claude,
                None,
                AgentLaunch::default(),
                Some(parent.to_owned()),
                epoch,
                Duration::from_secs(5),
            )
            .unwrap();
        ids(scene)
            .into_iter()
            .find(|id| !before.contains(id))
            .unwrap()
    }

    /// A job's terminal marks what it starts, and what those start, at
    /// creation, whatever their names; other terminals are never marked.
    #[test]
    fn a_jobs_terminal_marks_what_it_starts_for_good() {
        let (scene, other) = one_terminal("job-lineage");
        let control = scene.relay.terminals();
        let epoch = scene.relay.shared.pause().epoch();
        let job = control
            .add_work(
                connecto_core::ToolKind::Claude,
                "renamed later".into(),
                "work".into(),
                None,
                None,
                epoch,
            )
            .unwrap();
        let child = child_of(&scene, &job);
        let grandchild = child_of(&scene, &child);
        let plain = child_of(&scene, &other);
        let marked = |id: &str| {
            scene
                .relay
                .shared
                .slot_by_id(id)
                .is_some_and(|slot| slot.job.load(Ordering::SeqCst))
        };
        for id in [&job, &child, &grandchild] {
            assert!(marked(id), "{id}");
        }
        assert!(!marked(&other) && !marked(&plain));
        // A name is no authority: renamed, they are still a job's.
        control.rename(&job, Some("plain name".into())).unwrap();
        control.rename(&child, Some("plain name 2".into())).unwrap();
        assert!(marked(&job) && marked(&child));
        // Closing the job's terminal, however it is closed, closes what it
        // started with it: nothing it started is left running unmarked.
        control.close(&job).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while [&job, &child, &grandchild]
            .iter()
            .any(|id| ids(&scene).contains(*id))
        {
            assert!(Instant::now() < until, "the job's terminals did not close");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(ids(&scene).contains(&other) && ids(&scene).contains(&plain));
    }

    #[test]
    fn a_child_added_after_the_listing_makes_the_peer_close_refused_and_nothing_closed() {
        let (scene, parent) = one_terminal("within-refused");
        let listed = vec![parent.clone()];
        // A child arrives after the peer looked, and before its close runs.
        let child = child_of(&scene, &parent);
        let terminals = scene.relay.terminals();
        let refused = terminals.close_within(&parent, &listed);
        assert_eq!(refused, Err(connecto_core::CLOSE_NOT_ALL_LISTED.to_owned()));
        std::thread::sleep(Duration::from_millis(100));
        assert!(ids(&scene).contains(&parent), "the parent was closed");
        assert!(ids(&scene).contains(&child), "the child was closed");
    }

    #[test]
    fn a_close_within_everything_listed_closes_the_whole_tree() {
        let (scene, parent) = one_terminal("within-tree");
        let child = child_of(&scene, &parent);
        let terminals = scene.relay.terminals();
        terminals
            .close_within(&parent, &[parent.clone(), child.clone()])
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        wait_for("the tree to close", || {
            let now = ids(&scene);
            !now.contains(&parent) && !now.contains(&child)
        });
    }
}
