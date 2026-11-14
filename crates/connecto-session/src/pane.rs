//! A pane whose session is prepared off the UI thread.
//!
//! A worker thread runs preflight and launches, and reports through a stage the
//! UI reads without waiting. Preflight order: project folder, store hash,
//! version, then auth status.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use connecto_adapter::project_settings_conflicts;
use connecto_core::{
    AuthState, GridSize, InputRefused, PaneControl, PaneStage, PauseSwitch, ResizeOutcome,
    SessionRef, SessionStatus, SnapshotOutcome, TermSnapshot, TerminalSession,
};
use connecto_gate::Gate;
use connecto_pty::{SIGKILL_GRACE, Wake};
use connecto_store::{Installed, Store, Tool};
use crossbeam_channel::{Receiver, Sender, bounded};

use crate::LiveSession;
use crate::bridge::render;
use crate::codex_trust;
use crate::launch::{
    BridgeInfo, LaunchError, LaunchExtras, Layout, check_auth, start_login, start_resuming,
    start_shell, validated,
};

const LOGIN_POLL: Duration = Duration::from_millis(200);
/// How long a resumed session must stay up before the resume counts as having
/// worked. A tool that cannot find the session says so and exits at once.
const RESUME_PROBE: Duration = Duration::from_secs(5);
const RESUME_POLL: Duration = Duration::from_millis(50);
/// What the pane says when a resume did not work and a new conversation began.
const RESUME_FAILED: &str = "The earlier conversation could not be resumed, so this is a new one.";

/// What a pane needs to give its terminal the bridge (section 5.15).
#[derive(Debug, Clone)]
pub struct PaneBridge {
    /// The terminal's own id.
    pub terminal: String,
    /// The bridge socket. `None` when it could not be bound, which puts every
    /// terminal bridge off.
    pub socket: Option<PathBuf>,
}

/// What a pane runs.
pub struct PaneSetup {
    pub layout: Layout,
    /// `None` waits for a project to be chosen.
    pub project: Option<PathBuf>,
    pub tool: Tool,
    pub size: GridSize,
    pub wake: Wake,
    pub extras: LaunchExtras,
    /// `None` runs the pane with no bridge at all.
    pub bridge: Option<PaneBridge>,
}

/// Taken when a start is asked for, before it waits in the queue, so a Stop
/// that lands while the command waits also cancels it (R14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartTicket {
    stop_seq: u64,
    /// Only for an agent's start: a Pause after admission cancels it.
    pause_epoch: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
enum Command {
    Start(StartTicket),
    Login,
    Restart(StartTicket),
}

/// How a pane takes a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Begin {
    /// Show it stopped, to be started when it is asked for: a saved
    /// terminal that was not working when the app closed.
    Stopped,
    /// Start it, for the user.
    Start,
    /// Start it, for an agent admitted at this pause epoch.
    Agent(u64),
}

/// Why an agent's start was refused before anything happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRefusal {
    /// A process is running, or preflight is.
    AlreadyRunning,
    /// The pane already has a command waiting.
    Busy,
    /// A login is running in this pane.
    LoggingIn,
}

/// Points in a start where a test can hold the worker.
type Probe = Box<dyn Fn(&'static str) + Send + Sync>;

/// What the relay needs to reach a stop through the gate.
struct GateLink {
    gate: Arc<Gate>,
    session: usize,
}

struct Shared {
    setup: PaneSetup,
    project: ArcSwapOption<PathBuf>,
    version: ArcSwapOption<String>,
    /// The current contained session's attempt. Zero before the first.
    attempt: AtomicU64,
    stop_requested: AtomicBool,
    /// Why the bridge is off for this terminal, or `None` when it is on.
    bridge_off: ArcSwapOption<String>,
    /// Set when the project's settings stop this terminal running with full power.
    power_warning: ArcSwapOption<String>,
    /// Bumped first by every stop, so any start that began earlier is cancelled.
    stop_seq: AtomicU64,
    pause: OnceLock<Arc<PauseSwitch>>,
    probe: OnceLock<Probe>,
    warnings: ArcSwap<Vec<String>>,
    /// The tool session the next launch picks up again, when there is one.
    resume: ArcSwapOption<String>,
    /// The first message of the next new conversation, used up by the launch.
    first_prompt: ArcSwapOption<String>,
    /// The note a resumed conversation opens with; used up by the first launch.
    first_note: ArcSwapOption<String>,
    /// Kept from a launch that resumed, for the fresh start if the resume fails.
    retry_prompt: ArcSwapOption<String>,
    /// Set when a resume failed and a new conversation began, until the next
    /// start that is not that fallback.
    resume_note: ArcSwapOption<String>,
    /// The name the user gave the terminal, for the tool's next launch.
    session_name: ArcSwapOption<String>,
    /// Keystrokes and pastes from the user (the UI's input path).
    user_input: AtomicU64,
    gate: OnceLock<GateLink>,
    live: ArcSwapOption<LiveSession>,
    stage: ArcSwap<PaneStage>,
    size: AtomicU32,
    /// Bumped whenever `live` changes, so the UI drops the previous screen.
    epoch: AtomicU64,
    seen_epoch: AtomicU64,
    closing: AtomicBool,
}

/// What the status check came to.
enum Login {
    Ready,
    /// The stage says why the tool is not starting.
    Blocked,
    LoggedOut,
}

fn pack(size: GridSize) -> u32 {
    (u32::from(size.cols) << 16) | u32::from(size.rows)
}

fn unpack(value: u32) -> GridSize {
    GridSize {
        cols: (value >> 16) as u16,
        rows: (value & 0xffff) as u16,
    }
}

impl Shared {
    fn ticket(&self, agent: bool) -> StartTicket {
        // Read the epoch, then the stop sequence, as admission does elsewhere.
        let pause_epoch = agent
            .then(|| self.pause.get().map(|pause| pause.epoch()))
            .flatten();
        StartTicket {
            stop_seq: self.stop_seq.load(Ordering::SeqCst),
            pause_epoch,
        }
    }

    /// Whether a Stop, a close or (for an agent's start) a Pause came after the
    /// ticket was taken.
    fn cancelled(&self, ticket: &StartTicket) -> bool {
        self.stop_seq.load(Ordering::SeqCst) != ticket.stop_seq
            || self.closing.load(Ordering::SeqCst)
            || ticket.pause_epoch.is_some_and(|epoch| {
                self.pause
                    .get()
                    .is_some_and(|pause| pause.epoch() != epoch || pause.is_paused())
            })
    }

    fn probe(&self, point: &'static str) {
        if let Some(probe) = self.probe.get() {
            probe(point);
        }
    }

    fn set_stage(&self, stage: PaneStage) {
        self.stage.store(Arc::new(stage));
        (self.setup.wake)();
    }

    fn preparing(&self, step: &str) {
        self.set_stage(PaneStage::Preparing {
            step: step.to_owned(),
        });
    }

    fn failed(&self, reason: impl ToString) {
        self.set_stage(PaneStage::Failed {
            reason: reason.to_string(),
        });
    }

    fn set_live(&self, session: Option<LiveSession>) {
        self.live.store(session.map(Arc::new));
        self.epoch.fetch_add(1, Ordering::SeqCst);
        // Closed while this was starting: nobody else would stop it.
        if self.closing.load(Ordering::SeqCst)
            && let Some(session) = self.live.load_full()
        {
            session.terminate();
        }
    }

    fn current_size(&self) -> GridSize {
        unpack(self.size.load(Ordering::Relaxed))
    }

    fn project(&self) -> Option<PathBuf> {
        self.project.load_full().map(|path| path.as_ref().clone())
    }

    /// Whether the project may hold a session. `start_verified` checks again;
    /// this one saves a login that could only end in the same refusal.
    fn project_allowed(&self, project: &Path) -> bool {
        self.preparing("Checking the project folder");
        match validated(&self.setup.layout, project).map_err(LaunchError::from) {
            Ok(_) => true,
            Err(err) => {
                self.failed(err);
                false
            }
        }
    }

    fn verified_copy(&self) -> Option<Installed> {
        let setup = &self.setup;
        let store = Store::new(&setup.layout.data_root);
        self.preparing("Checking the private copy");
        let mut checked = store
            .newest(setup.tool)
            .and_then(|version| store.verify(setup.tool, &version));
        // Nothing set up yet for this tool (it was left out at the first launch,
        // or installed since): opening a terminal for it is the ask. Set it up
        // from the user's own install, with every check provisioning makes.
        if matches!(checked, Err(connecto_store::StoreError::NoVersion(_)))
            && Store::find_installed(setup.tool, &setup.layout.home).is_some()
        {
            self.preparing(&format!("Getting {} ready", setup.tool.name()));
            // An install whose signature is refused is passed over for the next one.
            checked = store
                .provision_found(setup.tool, &setup.layout.home, &connecto_store::Codesign)
                .map(|provisioned| provisioned.installed);
        }
        if matches!(
            checked,
            Err(connecto_store::StoreError::Tampered(_)
                | connecto_store::StoreError::CopyMismatch(_))
        ) {
            // The next start sets the stored copy up again.
            store.mark_repair();
        }
        match checked {
            Ok(installed) => {
                self.version
                    .store(Some(Arc::new(installed.version.clone())));
                Some(installed)
            }
            Err(err) => {
                self.failed(err);
                None
            }
        }
    }

    #[cfg(feature = "test-endpoints")]
    fn skips_auth(&self) -> bool {
        self.setup.extras.fake_model.is_some()
    }

    #[cfg(not(feature = "test-endpoints"))]
    #[allow(clippy::unused_self)]
    const fn skips_auth(&self) -> bool {
        false
    }

    /// What the status command says. Anything but `Ready` has set the stage,
    /// except `LoggedOut`, which the caller answers by logging in.
    fn logged_in(&self, project: &Path, installed: &Installed) -> Login {
        if self.skips_auth() {
            return Login::Ready;
        }
        let setup = &self.setup;
        self.preparing("Checking login");
        match check_auth(&setup.layout, project, setup.tool, installed) {
            Ok(AuthState::LoggedIn { .. }) => Login::Ready,
            Ok(AuthState::LoggedOut) => Login::LoggedOut,
            Ok(auth) => {
                self.set_stage(PaneStage::NeedsLogin { auth });
                Login::Blocked
            }
            Err(err) => {
                self.failed(err);
                Login::Blocked
            }
        }
    }

    fn start(&self, ticket: &StartTicket) {
        self.start_with(ticket, true, true);
    }

    /// `auto_login`: a tool that is logged out goes straight into its login.
    /// `allow_resume`: a saved session is picked up again; with it off, this
    /// is the fresh start after a resume failed.
    fn start_with(&self, ticket: &StartTicket, auto_login: bool, allow_resume: bool) {
        if allow_resume {
            self.resume_note.store(None);
        }
        let Some(project) = self.project() else {
            self.set_stage(PaneStage::Idle);
            return;
        };
        if self.cancel_to_stopped(ticket) {
            return;
        }
        self.stop_requested.store(false, Ordering::SeqCst);
        // A session that ended still holds its folders until it is dropped.
        self.set_live(None);
        if !self.project_allowed(&project) || self.cancel_to_stopped(ticket) {
            return;
        }
        if self.setup.tool == Tool::Shell {
            self.start_plain(ticket, &project);
            return;
        }
        let Some(installed) = self.verified_copy() else {
            return;
        };
        if self.cancel_to_stopped(ticket) {
            return;
        }
        match self.logged_in(&project, &installed) {
            Login::Ready => {}
            Login::Blocked => return,
            Login::LoggedOut => {
                // Nothing to confirm: there is no login to lose (R4).
                if auto_login {
                    if !self.cancel_to_stopped(ticket) {
                        self.login_flow(Some(ticket));
                    }
                } else {
                    self.set_stage(PaneStage::NeedsLogin {
                        auth: AuthState::LoggedOut,
                    });
                }
                return;
            }
        }
        let bridge = self.bridge_for(&installed, &project);
        self.probe("before_spawn");
        if self.cancel_to_stopped(ticket) {
            return;
        }
        let setup = &self.setup;
        self.preparing("Starting");
        // Only the first launch picks the saved session up again: a start
        // or restart later on is a new conversation, as it always was.
        let resume = allow_resume
            .then(|| self.resume.swap(None))
            .flatten()
            .map(|id| id.as_ref().clone());
        let prompt = take_prompt(
            &self.first_prompt,
            &self.retry_prompt,
            allow_resume,
            resume.is_some(),
        );
        let mut extras = setup.extras.clone();
        extras.initial_prompt = prompt.map(|text| text.as_ref().clone());
        extras.session_name = self
            .session_name
            .load_full()
            .map(|name| name.as_ref().clone());
        // The note goes only with a resume, and only with the first launch.
        let note = if allow_resume {
            self.first_note.swap(None)
        } else {
            None
        };
        extras.resume_note = note
            .filter(|_| resume.is_some())
            .map(|text| text.as_ref().clone());
        let started = start_resuming(
            &setup.layout,
            &project,
            setup.tool,
            &installed,
            self.current_size(),
            Arc::clone(&setup.wake),
            &extras,
            &bridge,
            resume.as_deref(),
        );
        match started {
            Ok(session) => {
                self.probe("spawned");
                // Publish in a fixed order: `live`, then the stage, then check
                // the ticket again. A Stop whose bump came before this check is
                // seen here. One whose bump came after finds `live` stored and
                // the stage `Running`, and terminates it itself.
                let attempt = self.attempt.fetch_add(1, Ordering::SeqCst) + 1;
                session.set_attempt(attempt);
                self.set_live(Some(session));
                self.set_stage(PaneStage::Running);
                if self.cancelled(ticket) {
                    if let Some(live) = self.live.load_full() {
                        live.terminate();
                    }
                    self.set_stage(PaneStage::Stopped);
                }
                self.probe("published");
                if resume.is_some() && self.resume_failed(ticket) {
                    // Once: the fresh start does not resume, so it cannot loop.
                    self.resume.store(None);
                    self.resume_note
                        .store(Some(Arc::new(RESUME_FAILED.to_owned())));
                    self.start_with(ticket, auto_login, false);
                }
            }
            Err(err) => {
                if resume.is_some() {
                    self.resume.store(None);
                    self.resume_note
                        .store(Some(Arc::new(RESUME_FAILED.to_owned())));
                    self.start_with(ticket, auto_login, false);
                } else {
                    self.failed(err);
                }
            }
        }
    }

    /// A plain shell: nothing to copy, verify or log in to, and nothing to
    /// resume. The bridge is on (other terminals and remote peers can type into
    /// it through the gate), but it gets no hooks and no instructions.
    fn start_plain(&self, ticket: &StartTicket, project: &Path) {
        let setup = &self.setup;
        self.power_warning.store(None);
        self.warnings.store(Arc::new(Vec::new()));
        self.bridge_off.store(None);
        (setup.wake)();
        let bridge = match &setup.bridge {
            Some(PaneBridge {
                terminal,
                socket: Some(socket),
            }) => BridgeInfo {
                socket: socket.clone(),
                terminal: terminal.clone(),
                text: None,
                hooks: false,
                trust: None,
            },
            _ => BridgeInfo::absent(),
        };
        self.probe("before_spawn");
        if self.cancel_to_stopped(ticket) {
            return;
        }
        self.preparing("Starting");
        match start_shell(
            &setup.layout,
            project,
            self.current_size(),
            Arc::clone(&setup.wake),
            &bridge,
        ) {
            Ok(session) => {
                self.probe("spawned");
                let attempt = self.attempt.fetch_add(1, Ordering::SeqCst) + 1;
                session.set_attempt(attempt);
                self.set_live(Some(session));
                self.set_stage(PaneStage::Running);
                if self.cancelled(ticket) {
                    if let Some(live) = self.live.load_full() {
                        live.terminate();
                    }
                    self.set_stage(PaneStage::Stopped);
                }
                self.probe("published");
            }
            Err(err) => self.failed(err),
        }
    }

    /// Whether a resumed session ended at once, before the probe time was up,
    /// without a Stop or a close having asked for it.
    fn resume_failed(&self, ticket: &StartTicket) -> bool {
        let deadline = std::time::Instant::now() + RESUME_PROBE;
        while std::time::Instant::now() < deadline {
            if self.cancelled(ticket) {
                return false;
            }
            let Some(session) = self.live.load_full() else {
                return false;
            };
            if session.wait_exit(RESUME_POLL) {
                return !self.cancelled(ticket);
            }
        }
        false
    }

    /// If the ticket was cancelled, end in `Stopped` and say so.
    fn cancel_to_stopped(&self, ticket: &StartTicket) -> bool {
        if self.cancelled(ticket) {
            self.set_stage(PaneStage::Stopped);
            true
        } else {
            false
        }
    }

    /// What the terminal gets at launch, and whether the bridge is on for it.
    /// Off means no launch text, no hooks and no trust, but the socket and
    /// `PATH` still, so the command can say why (section 5.15).
    fn bridge_for(&self, installed: &Installed, project: &Path) -> BridgeInfo {
        let setup = &self.setup;
        let Some(PaneBridge { terminal, socket }) = &setup.bridge else {
            return BridgeInfo::absent();
        };
        let mut reason = if socket.is_none() {
            Some("the bridge socket could not be bound".to_owned())
        } else {
            None
        };
        // Project settings Claude cannot be protected against: say so, rather than
        // run a terminal that silently has no bridge, or less than full power.
        let mut power = None;
        if setup.tool == Tool::Claude {
            let found = project_settings_conflicts(project);
            if reason.is_none()
                && let Some(why) = found.hooks_disabled
            {
                reason = Some(format!(
                    "the project's settings turn Connecto's hooks off ({why}), so Connecto cannot see this terminal"
                ));
            }
            power = found.bypass_disabled.map(|why| {
                format!("Not running with full power: the project's settings turn the bypass off ({why}).")
            });
        }
        self.power_warning.store(power.clone().map(Arc::new));
        let note = self
            .resume_note
            .load_full()
            .map(|note| note.as_ref().clone());
        self.warnings
            .store(Arc::new(power.into_iter().chain(note).collect()));
        (setup.wake)();
        let mut trust = None;
        if reason.is_none() && setup.tool == Tool::Codex {
            self.preparing("Reading Codex hook trust");
            let store = Store::new(&setup.layout.data_root);
            match store
                .verify_hook()
                .map_err(|err| err.to_string())
                .and_then(|helper| {
                    codex_trust::trust_state(
                        &installed.entry,
                        &installed.version,
                        &helper,
                        &setup.layout.data_root.join("run"),
                    )
                }) {
                Ok(value) => trust = Some(value),
                Err(why) => {
                    reason = Some(format!("Codex's hooks could not be trusted: {why}"));
                }
            }
        }
        self.bridge_off.store(reason.clone().map(Arc::new));
        let on = reason.is_none();
        BridgeInfo {
            socket: socket
                .clone()
                .unwrap_or_else(|| PathBuf::from("/nonexistent/bridge.sock")),
            terminal: terminal.clone(),
            text: on.then(|| render::launch_text(terminal)),
            hooks: on,
            trust,
        }
    }

    fn restart(&self, ticket: &StartTicket) {
        if let Some(session) = self.live.load_full() {
            session.terminate();
            let _ = session.wait_exit(SIGKILL_GRACE + Duration::from_secs(1));
        }
        self.start(ticket);
    }

    /// Run the tool's login in this pane. `auto` carries the ticket of the
    /// start it belongs to, for a tool found logged out: a Stop that came after
    /// the ticket was taken cancels it, at each step. `None` is a click.
    fn login_flow(&self, auto: Option<&StartTicket>) {
        let Some(project) = self.project() else {
            return;
        };
        if auto.is_none() {
            self.stop_requested.store(false, Ordering::SeqCst);
        }
        // The pane's own ended session stays until the login is accepted: a
        // refused login keeps that screen, and its claim no longer counts.
        let Some(installed) = self.verified_copy() else {
            return;
        };
        if auto.is_some_and(|ticket| self.cancel_to_stopped(ticket)) {
            return;
        }
        let setup = &self.setup;
        let started = start_login(
            &setup.layout,
            &project,
            setup.tool,
            &installed,
            self.current_size(),
            Arc::clone(&setup.wake),
        );
        match started {
            Ok(session) => {
                self.set_live(Some(session));
                self.set_stage(PaneStage::LoggingIn);
                // A Stop that landed while it launched: stopped, not logging in.
                if let Some(ticket) = auto
                    && self.cancelled(ticket)
                {
                    if let Some(live) = self.live.load_full() {
                        live.terminate();
                    }
                    self.set_stage(PaneStage::Stopped);
                }
            }
            // Another terminal of this tool is logging in: that login is the
            // one that counts, and this one waits for a click.
            Err(LaunchError::LoginRunning) if auto.is_some() => {
                self.set_stage(PaneStage::NeedsLogin {
                    auth: AuthState::LoggedOut,
                });
                return;
            }
            Err(err) => {
                self.failed(err);
                return;
            }
        }
        while let Some(session) = self.live.load_full() {
            if session.wait_exit(LOGIN_POLL) {
                break;
            }
        }
        // Nothing of the login screen stays behind.
        self.set_live(None);
        if self.stop_requested.load(Ordering::SeqCst) {
            self.set_stage(PaneStage::Stopped);
        } else if !self.closing.load(Ordering::SeqCst) {
            // A login that did not take leaves the tool logged out: it waits
            // for a click and does not start another by itself.
            self.start_with(&self.ticket(false), false, true);
        }
    }
}

fn worker(shared: &Shared, commands: &Receiver<Command>) {
    for command in commands {
        if shared.closing.load(Ordering::SeqCst) {
            return;
        }
        match command {
            Command::Start(ticket) => shared.start(&ticket),
            Command::Login => shared.login_flow(None),
            Command::Restart(ticket) => shared.restart(&ticket),
        }
    }
}

/// A pane with a managed session. It starts preparing at once.
pub struct ManagedPane {
    shared: Arc<Shared>,
    commands: Sender<Command>,
}

impl ManagedPane {
    /// Begin preflight on a new thread. Returns at once.
    pub fn spawn(setup: PaneSetup) -> std::io::Result<Self> {
        let (commands, receiver) = bounded(1);
        let stage = if setup.project.is_some() {
            PaneStage::Preparing {
                step: "Waiting".to_owned(),
            }
        } else {
            PaneStage::Idle
        };
        let project = setup.project.clone().map(Arc::new);
        let initial_off = match &setup.bridge {
            None => Some("this terminal is not part of a relay".to_owned()),
            Some(PaneBridge { socket: None, .. }) => {
                Some("the bridge socket could not be bound".to_owned())
            }
            Some(_) => None,
        };
        // Installed with the pane, before any startup can be queued.
        let first_prompt = ArcSwapOption::new(setup.extras.initial_prompt.clone().map(Arc::new));
        let first_note = ArcSwapOption::new(setup.extras.resume_note.clone().map(Arc::new));
        let session_name = ArcSwapOption::new(setup.extras.session_name.clone().map(Arc::new));
        let shared = Arc::new(Shared {
            size: AtomicU32::new(pack(setup.size)),
            setup,
            project: ArcSwapOption::new(project),
            version: ArcSwapOption::empty(),
            attempt: AtomicU64::new(0),
            stop_requested: AtomicBool::new(false),
            bridge_off: ArcSwapOption::new(initial_off.map(Arc::new)),
            power_warning: ArcSwapOption::empty(),
            stop_seq: AtomicU64::new(0),
            pause: OnceLock::new(),
            probe: OnceLock::new(),
            warnings: ArcSwap::from_pointee(Vec::new()),
            resume: ArcSwapOption::empty(),
            first_prompt,
            first_note,
            retry_prompt: ArcSwapOption::empty(),
            resume_note: ArcSwapOption::empty(),
            session_name,
            user_input: AtomicU64::new(0),
            gate: OnceLock::new(),
            live: ArcSwapOption::empty(),
            stage: ArcSwap::from_pointee(stage),
            epoch: AtomicU64::new(0),
            seen_epoch: AtomicU64::new(0),
            closing: AtomicBool::new(false),
        });
        let worker_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("pane-worker".into())
            .spawn(move || worker(&worker_shared, &receiver))?;
        let pane = Self { shared, commands };
        if pane.shared.project.load().is_some() {
            let ticket = pane.shared.ticket(false);
            let _ = pane.commands.try_send(Command::Start(ticket));
        }
        Ok(pane)
    }

    /// Set the project and begin preflight. The caller checked that nothing runs.
    pub(crate) fn begin_project(&self, project: PathBuf, how: Begin) {
        self.shared.project.store(Some(Arc::new(project)));
        match how {
            Begin::Stopped => self.shared.set_stage(PaneStage::Stopped),
            Begin::Start => {
                let ticket = self.shared.ticket(false);
                let _ = self.commands.try_send(Command::Start(ticket));
            }
            Begin::Agent(epoch) => {
                let mut ticket = self.shared.ticket(false);
                ticket.pause_epoch = Some(epoch);
                let _ = self.commands.try_send(Command::Start(ticket));
            }
        }
    }

    /// The tool session the next launch picks up again, or `None` for a new
    /// conversation. One launch uses it up, and a failed resume clears it.
    pub(crate) fn set_resume(&self, session: Option<String>) {
        self.shared.resume.store(session.map(Arc::new));
    }

    /// The name the terminal is given (or `None`) at its next launch, for a tool
    /// that can show it.
    pub(crate) fn set_session_name(&self, name: Option<String>) {
        self.shared.session_name.store(name.map(Arc::new));
    }

    /// Keystrokes and pastes the user has typed into this pane so far.
    pub(crate) fn user_inputs(&self) -> u64 {
        self.shared.user_input.load(Ordering::SeqCst)
    }

    /// Why this terminal is not running with full power, if it is not.
    #[must_use]
    pub fn power_warning(&self) -> Option<String> {
        self.shared
            .power_warning
            .load_full()
            .map(|warning| warning.as_ref().clone())
    }

    /// Why the bridge is off for this terminal, if it is.
    #[must_use]
    pub fn bridge_off_reason(&self) -> Option<String> {
        self.shared
            .bridge_off
            .load_full()
            .map(|reason| reason.as_ref().clone())
    }

    /// The size the last resize set.
    #[must_use]
    pub(crate) fn size(&self) -> GridSize {
        self.shared.current_size()
    }

    pub(crate) fn link_gate(&self, gate: Arc<Gate>, pause: Arc<PauseSwitch>, session: usize) {
        let _ = self.shared.gate.set(GateLink { gate, session });
        let _ = self.shared.pause.set(pause);
    }

    /// Move the attempt counter, as a restart that completed would. For tests
    /// of what happens when an attempt goes stale while a write is in flight.
    #[doc(hidden)]
    pub fn force_attempt(&self, attempt: u64) {
        self.shared.attempt.store(attempt, Ordering::SeqCst);
    }

    /// Hold the worker at a named point of a start. For tests.
    #[doc(hidden)]
    pub fn set_probe(&self, probe: impl Fn(&'static str) + Send + Sync + 'static) {
        let _ = self.shared.probe.set(Box::new(probe));
    }

    /// An agent's start. The ticket is taken now, before the queue, so a Pause
    /// or Stop that lands while the command waits cancels it. Refused at once
    /// if a process runs, a preflight runs, a login runs or a command waits.
    pub fn request_start_agent(
        &self,
        _pause: &PauseSwitch,
        epoch: u64,
    ) -> Result<(), StartRefusal> {
        match self.stage() {
            PaneStage::LoggingIn => return Err(StartRefusal::LoggingIn),
            PaneStage::Preparing { .. } => return Err(StartRefusal::AlreadyRunning),
            PaneStage::Running if self.is_live() => return Err(StartRefusal::AlreadyRunning),
            _ => {}
        }
        let mut ticket = self.shared.ticket(false);
        ticket.pause_epoch = Some(epoch);
        self.commands
            .try_send(Command::Start(ticket))
            .map_err(|_| StartRefusal::Busy)
    }

    /// Whether preflight or a launch is running.
    #[must_use]
    pub fn is_preparing(&self) -> bool {
        matches!(self.stage(), PaneStage::Preparing { .. })
    }

    pub(crate) fn tool(&self) -> Tool {
        self.shared.setup.tool
    }

    /// The current contained session's attempt.
    #[must_use]
    pub fn attempt(&self) -> u64 {
        self.shared.attempt.load(Ordering::SeqCst)
    }

    /// The version running, once preflight found it.
    #[must_use]
    pub fn version(&self) -> Option<String> {
        self.shared.version.load_full().map(|v| v.as_ref().clone())
    }

    fn audit_control(&self, action: &str) {
        if let Some(link) = self.shared.gate.get() {
            let target = SessionRef {
                session: link.session,
                attempt: self.attempt(),
            };
            let _ = link
                .gate
                .control(action, target, self.shared.setup.tool.name());
        }
    }

    /// Refuse further starts and stop what is running. Never blocks.
    pub fn close(&self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        if let Some(session) = self.shared.live.load_full() {
            session.terminate();
        }
    }

    /// Drop the ended session, which removes its folders and socket.
    pub fn release(&self) {
        self.shared.set_live(None);
    }

    /// Wait up to `timeout` for the session to end. True if nothing is running.
    #[must_use]
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        self.shared
            .live
            .load_full()
            .is_none_or(|session| session.wait_exit(timeout))
    }

    /// The session, once it exists.
    #[must_use]
    pub fn session(&self) -> Option<Arc<LiveSession>> {
        self.shared.live.load_full()
    }

    fn command_allowed(&self) -> bool {
        let stage = self.stage();
        let ended = stage == PaneStage::Running
            && self
                .shared
                .live
                .load()
                .as_ref()
                .is_none_or(|session| session.status() != SessionStatus::Running);
        stage.accepts_commands() || ended
    }
}

impl PaneControl for ManagedPane {
    fn stage(&self) -> PaneStage {
        self.shared.stage.load().as_ref().clone()
    }

    fn request_login(&self) {
        if self.command_allowed() {
            let _ = self.commands.try_send(Command::Login);
        }
    }

    fn request_start(&self) {
        if self.command_allowed() {
            let ticket = self.shared.ticket(false);
            let _ = self.commands.try_send(Command::Start(ticket));
        }
    }

    fn request_stop(&self) {
        self.audit_control("stop");
        // First, so a start in flight sees it whatever it has published.
        self.shared.stop_seq.fetch_add(1, Ordering::SeqCst);
        self.shared.stop_requested.store(true, Ordering::SeqCst);
        if let Some(session) = self.shared.live.load_full() {
            session.terminate();
        }
        // A login reports its own end; a session's pane stays to show its exit.
        // A start still in preflight ends `Stopped` at its next checkpoint, and
        // shows it now.
        if matches!(
            self.stage(),
            PaneStage::Running | PaneStage::Preparing { .. }
        ) {
            self.shared.set_stage(PaneStage::Stopped);
        }
    }

    fn request_restart(&self) {
        self.audit_control("restart");
        if matches!(self.stage(), PaneStage::Running | PaneStage::Stopped) {
            let ticket = self.shared.ticket(false);
            let _ = self.commands.try_send(Command::Restart(ticket));
        }
    }

    fn is_live(&self) -> bool {
        self.shared
            .live
            .load()
            .as_ref()
            .is_some_and(|session| session.status() == SessionStatus::Running)
    }

    fn warnings(&self) -> Arc<Vec<String>> {
        self.shared.warnings.load_full()
    }

    fn rejected_hook_frames(&self) -> u64 {
        self.shared
            .live
            .load()
            .as_ref()
            .and_then(|session| session.hook().map(crate::HookHandle::rejected))
            .unwrap_or(0)
    }
}

impl TerminalSession for ManagedPane {
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        let epoch = self.shared.epoch.load(Ordering::SeqCst);
        let changed = self.shared.seen_epoch.swap(epoch, Ordering::SeqCst) != epoch;
        if changed {
            *out = TermSnapshot::default();
        }
        match self.shared.live.load().as_ref() {
            Some(session) => session.poll_snapshot(out),
            None if changed => SnapshotOutcome::Updated,
            None => SnapshotOutcome::Unchanged,
        }
    }

    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        self.shared.user_input.fetch_add(1, Ordering::SeqCst);
        self.shared
            .live
            .load()
            .as_ref()
            .ok_or(InputRefused::Closed)?
            .send_input(bytes)
    }

    fn resize(&self, size: GridSize) -> ResizeOutcome {
        self.shared.size.store(pack(size), Ordering::Relaxed);
        self.shared
            .live
            .load()
            .as_ref()
            .map_or(ResizeOutcome::Applied, |session| session.resize(size))
    }

    fn resize_remote(&self, size: GridSize) -> ResizeOutcome {
        self.shared
            .live
            .load()
            .as_ref()
            .map_or(ResizeOutcome::Applied, |session| {
                session.resize_remote(size)
            })
    }

    fn set_remote_size_control(&self, on: bool) {
        if let Some(session) = self.shared.live.load().as_ref() {
            session.set_remote_size_control(on);
        }
    }

    fn scroll(&self, lines: i32) -> bool {
        self.shared
            .live
            .load()
            .as_ref()
            .is_some_and(|session| session.scroll(lines))
    }

    fn status(&self) -> SessionStatus {
        self.shared.live.load().as_ref().map_or_else(
            || SessionStatus::Unknown {
                reason: "not started".to_owned(),
            },
            |session| session.status(),
        )
    }
}

/// The first message for a launch. A new conversation opens with it. A resume
/// keeps it only for the fresh start that follows if the resume fails
/// (`allow_resume` off), and a later start or restart has none.
fn take_prompt(
    first: &ArcSwapOption<String>,
    retry: &ArcSwapOption<String>,
    allow_resume: bool,
    resuming: bool,
) -> Option<Arc<String>> {
    if !allow_resume {
        return retry.swap(None);
    }
    let prompt = first.swap(None);
    if resuming {
        retry.store(prompt);
        None
    } else {
        prompt
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod prompt_tests {
    use super::*;

    /// The prompt is part of the pane from the moment it exists, so the
    /// startup that `begin_project` queues can never run without it.
    #[test]
    fn the_first_prompt_is_installed_with_the_pane_before_any_startup() {
        let dir = std::env::temp_dir().join(format!("mg-prompt-pane-{}", std::process::id()));
        let extras = LaunchExtras {
            initial_prompt: Some("you are authoring".to_owned()),
            ..LaunchExtras::default()
        };
        let pane = ManagedPane::spawn(PaneSetup {
            layout: Layout {
                data_root: dir.join("data"),
                home: dir.join("home"),
            },
            project: None,
            tool: Tool::Claude,
            size: crate::INITIAL_SIZE,
            wake: Arc::new(|| {}),
            extras,
            bridge: None,
        })
        .unwrap();
        // Nothing has been started or queued, and the prompt is already there.
        assert_eq!(
            pane.shared
                .first_prompt
                .load()
                .as_deref()
                .map(String::as_str),
            Some("you are authoring")
        );
        assert_eq!(
            take_prompt(
                &pane.shared.first_prompt,
                &pane.shared.retry_prompt,
                true,
                false
            )
            .as_deref()
            .map(String::as_str),
            Some("you are authoring")
        );
    }

    fn cells(prompt: &str) -> (ArcSwapOption<String>, ArcSwapOption<String>) {
        (
            ArcSwapOption::new(Some(Arc::new(prompt.to_owned()))),
            ArcSwapOption::empty(),
        )
    }

    #[test]
    fn a_new_conversation_gets_its_prompt_once() {
        let (first, retry) = cells("hello");
        assert_eq!(
            take_prompt(&first, &retry, true, false)
                .as_deref()
                .map(String::as_str),
            Some("hello")
        );
        assert!(
            take_prompt(&first, &retry, true, false).is_none(),
            "a restart has none"
        );
        assert!(take_prompt(&first, &retry, false, false).is_none());
    }

    #[test]
    fn a_resume_has_no_prompt_unless_it_fails_and_a_new_conversation_begins() {
        let (first, retry) = cells("hello");
        assert!(take_prompt(&first, &retry, true, true).is_none());
        // The resume failed: the fresh start opens with the prompt, once.
        assert_eq!(
            take_prompt(&first, &retry, false, false)
                .as_deref()
                .map(String::as_str),
            Some("hello")
        );
        assert!(take_prompt(&first, &retry, false, false).is_none());
        // A resume that worked leaves nothing for a later fallback to find
        // after the next normal start.
        let (first, retry) = cells("hello");
        let _ = take_prompt(&first, &retry, true, true);
        assert!(take_prompt(&first, &retry, true, false).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_survives_packing() {
        let size = GridSize {
            cols: 321,
            rows: 65,
        };
        assert_eq!(unpack(pack(size)), size);
    }
}
