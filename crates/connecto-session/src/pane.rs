//! A pane whose session is prepared off the UI thread.
//!
//! A worker thread runs preflight and launches, and reports through a stage the
//! UI reads without waiting. Preflight order: project folder, store hash,
//! version, then auth status.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use connecto_adapter::project_settings_conflicts;
use connecto_core::{
    AuthState, GridSize, InputRefused, PaneControl, PaneStage, PauseSwitch, ResizeOutcome,
    SelectOp, SelectionText, SessionRef, SessionStatus, SnapshotOutcome, TermSnapshot,
    TerminalSession,
};
use connecto_gate::Gate;
use connecto_pty::{SIGKILL_GRACE, Wake};
use connecto_store::{Installed, Store, Tool};
use crossbeam_channel::{Receiver, Sender, bounded};

use crate::LiveSession;
use crate::bridge::render;
use crate::codex_trust;
use crate::draft::{Draft, DraftTracker, Known};
use crate::input_ledger::InputSource;
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
/// The most the user can type while Connecto types into the terminal. More
/// than that is refused, as a full input queue is.
const HELD_INPUT_LIMIT: usize = 1 << 20;

/// The user's way into the terminal. While Connecto types into it, what the
/// user types waits here, in order, and goes in after.
struct InputGate {
    draft: DraftTracker,
    /// `Some` while Connecto types: the user's bytes, kept back.
    held: Option<HeldInput>,
    last_input: Option<Instant>,
}

/// The user's bytes kept back while Connecto types, each run with the source
/// that wrote it, so they reach the PTY with the provenance they came with.
#[derive(Default)]
struct HeldInput {
    runs: Vec<(InputSource, Vec<u8>)>,
    len: usize,
}

impl HeldInput {
    fn push(&mut self, bytes: Vec<u8>, source: InputSource) -> Result<(), InputRefused> {
        if self.len + bytes.len() > HELD_INPUT_LIMIT {
            return Err(InputRefused::QueueFull);
        }
        self.len += bytes.len();
        match self.runs.last_mut() {
            Some((last, run)) if *last == source => run.extend_from_slice(&bytes),
            _ => self.runs.push((source, bytes)),
        }
        Ok(())
    }
}

/// What the draft becomes when Connecto is done typing.
#[derive(Debug)]
pub(crate) enum DraftAfter {
    /// As it was.
    Keep,
    /// Empty: the screen showed it so.
    Reset,
    /// Connecto put this draft back.
    Set(Draft),
    /// Not known any more.
    Forget,
}

/// The project id memory is kept under: the one `launch::project_id` gives
/// the project's real path.
fn memory_project_id(project: &Path) -> String {
    let real = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_owned());
    crate::launch::project_id(&real)
}

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
    /// The last start or login was refused because the project's Tools
    /// setting leaves the CLI out. Cleared by the next start or login.
    tools_refused: AtomicBool,
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
    /// The model and effort of the next launch (`connecto add --model` and
    /// `--effort`), or the tool's defaults. Set again when the pane is
    /// reused for another project's terminal of the same id.
    model: ArcSwapOption<String>,
    effort: ArcSwapOption<String>,
    /// Keystrokes and pastes from the user (the UI's input path).
    user_input: AtomicU64,
    input: Mutex<InputGate>,
    /// Connecto is typing into the terminal; read by the UI.
    injecting: AtomicBool,
    /// A message waits for the user to send or clear their line; read by the UI.
    message_waiting: AtomicBool,
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
        // A new process starts with an empty input line.
        self.input().draft.reset();
        self.live.store(session.map(Arc::new));
        self.epoch.fetch_add(1, Ordering::SeqCst);
        // Closed while this was starting: nobody else would stop it.
        if self.closing.load(Ordering::SeqCst)
            && let Some(session) = self.live.load_full()
        {
            session.terminate();
        }
    }

    fn input(&self) -> MutexGuard<'_, InputGate> {
        self.input.lock().unwrap_or_else(PoisonError::into_inner)
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

    /// Whether the project's saved Tools setting leaves this pane's CLI out;
    /// if so the pane ends failed with the reason, marked as refused so the
    /// list stops bringing it back. Hidden utility sessions are internal and
    /// are never refused.
    fn refuse_by_tools(&self, project: &Path) -> bool {
        self.tools_refused.store(false, Ordering::SeqCst);
        if self.setup.extras.utility.is_some() {
            return false;
        }
        match crate::project_settings::tool_refusal_in(
            &self.setup.layout.data_root,
            project,
            crate::terminals::kind(self.setup.tool),
        ) {
            Some(why) => {
                self.refused_by_tools(why);
                true
            }
            None => false,
        }
    }

    /// Undo what a start took that never launched: the session it was to
    /// resume, its opening prompt and the note, so the next start by hand
    /// opens as this one would have.
    fn put_back(
        &self,
        allow_resume: bool,
        resume: Option<String>,
        prompt: Option<Arc<String>>,
        before: PromptSlots,
        note: Option<Arc<String>>,
    ) {
        if allow_resume {
            self.resume.store(resume.map(Arc::new));
            self.first_note.store(note);
        }
        put_back_prompt(
            &self.first_prompt,
            &self.retry_prompt,
            allow_resume,
            prompt,
            before,
        );
    }

    fn refused_by_tools(&self, why: String) {
        self.tools_refused.store(true, Ordering::SeqCst);
        self.failed(why);
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
        // A tool that runs where it is installed (Pi) is looked for again at
        // every start: its record is only where the install was, and an update
        // or a reinstall moves or renumbers it.
        let missing = matches!(checked, Err(connecto_store::StoreError::NoVersion(_)))
            || setup.tool.runs_in_place();
        if missing && Store::find_installed(setup.tool, &setup.layout.home).is_some() {
            self.preparing(&format!("Getting {} ready", setup.tool.name()));
            // An install whose signature is refused is passed over for the next one.
            let found = store
                .provision_found(setup.tool, &setup.layout.home, &connecto_store::Codesign)
                .map(|provisioned| provisioned.installed);
            // A record that still stands is not given up for a look that
            // failed this time.
            if found.is_ok() || checked.is_err() {
                checked = found;
            }
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
        if setup.tool == Tool::Claude
            && crate::launch::claude_login_not_needed(&setup.layout, project)
        {
            return Login::Ready;
        }
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
        // A CLI the project's Tools setting leaves out does not start, however
        // it was asked for (a restart, a saved terminal brought back).
        if self.refuse_by_tools(&project) {
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
        // As they were, for a start refused before it launches.
        let before = (self.first_prompt.load_full(), self.retry_prompt.load_full());
        let prompt = take_prompt(
            &self.first_prompt,
            &self.retry_prompt,
            allow_resume,
            resume.is_some(),
        );
        let mut extras = setup.extras.clone();
        extras.initial_prompt = prompt.as_ref().map(|text| text.as_ref().clone());
        extras.session_name = self
            .session_name
            .load_full()
            .map(|name| name.as_ref().clone());
        extras.model = self.model.load_full().map(|m| m.as_ref().clone());
        extras.effort = self.effort.load_full().map(|e| e.as_ref().clone());
        // The note goes only with a resume, and only with the first launch.
        let note = if allow_resume {
            self.first_note.swap(None)
        } else {
            None
        };
        extras.resume_note = note
            .as_ref()
            .filter(|_| resume.is_some())
            .map(|text| text.as_ref().clone());
        // A conversation resumed after project memory was turned on is told
        // once (spec §6.1); only when the bridge, and so the command, is on.
        if extras.resume_note.is_none()
            && bridge.hooks
            && let Some(session) = resume.as_deref()
        {
            extras.resume_note = crate::bridge::memory::resume_note(
                &*crate::bridge::memory::memory_bridge(),
                &setup.layout.data_root,
                &memory_project_id(&project),
                session,
            );
        }
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
                // Project memory: a prompt on the command line fires a hook no
                // key caused (the adapters pass one in exactly these cases).
                if (resume.is_some() && extras.resume_note.is_some())
                    || (resume.is_none() && extras.initial_prompt.is_some())
                {
                    session.expect_startup_prompt();
                }
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
            // Left out while this start waited: no fallback start, and what
            // this start took is put back for the next one.
            Err(LaunchError::NotAllowed(why)) => {
                self.put_back(allow_resume, resume, prompt, before, note);
                self.refused_by_tools(why);
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
                } else if let Some(line) = self.first_prompt.swap(None)
                    && let Some(typed) = shell_first_line(&line)
                    && let Some(live) = self.live.load_full()
                {
                    // A shell started with a command (a package's install,
                    // ADR 0037) runs it once, typed as the user would; the
                    // shell reads it when its startup files are done.
                    let _ = live.write_input(typed, InputSource::Connecto);
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
                    .map(|value| (helper, value))
                }) {
                // Codex gets its instructions only from the hook (ADR 0046):
                // a helper that would not hand them over, or hooks the
                // terminal's configuration turns off, turn the bridge off
                // with a reason instead of silently leaving them out.
                Ok((helper, value)) => {
                    let env =
                        crate::launch::codex_config_env(&setup.layout, project, &installed.entry);
                    match codex_trust::hook_handover(
                        &helper,
                        &installed.entry,
                        env.as_deref(),
                        project,
                    ) {
                        Ok(()) => trust = Some(value),
                        Err(why) => reason = Some(why),
                    }
                }
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
            text: on.then(|| {
                let project_id = memory_project_id(project);
                let memory = crate::bridge::memory::memory_bridge().enabled(&project_id);
                let ops = crate::bridge::ops::ops_bridge().has_ops(&project_id, project);
                render::launch_text_with(terminal, memory, ops)
            }),
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
        // A login runs the CLI too: not one the project leaves out.
        if self.refuse_by_tools(&project) {
            return;
        }
        // The pane's own ended session stays until the login is accepted: a
        // refused login keeps that screen, and its claim no longer counts.
        let Some(installed) = self.verified_copy() else {
            return;
        };
        if auto.is_some_and(|ticket| self.cancel_to_stopped(ticket)) {
            return;
        }
        // `start_login` checks the Tools setting again at its last moment.
        self.probe("login_verified");
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
            // Left out while the login was prepared.
            Err(LaunchError::NotAllowed(why)) => {
                self.refused_by_tools(why);
                return;
            }
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
    chat: Mutex<std::sync::Arc<connecto_core::session::ChatHistory>>,
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
        let model = ArcSwapOption::new(setup.extras.model.clone().map(Arc::new));
        let effort = ArcSwapOption::new(setup.extras.effort.clone().map(Arc::new));
        let draft_keys = crate::launch::adapter(setup.tool).draft_keys();
        let shared = Arc::new(Shared {
            size: AtomicU32::new(pack(setup.size)),
            setup,
            project: ArcSwapOption::new(project),
            version: ArcSwapOption::empty(),
            attempt: AtomicU64::new(0),
            stop_requested: AtomicBool::new(false),
            tools_refused: AtomicBool::new(false),
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
            model,
            effort,
            user_input: AtomicU64::new(0),
            input: Mutex::new(InputGate {
                draft: DraftTracker::new(draft_keys),
                held: None,
                last_input: None,
            }),
            injecting: AtomicBool::new(false),
            message_waiting: AtomicBool::new(false),
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
        let pane = Self {
            shared,
            commands,
            chat: Mutex::default(),
        };
        if pane.shared.project.load().is_some() {
            let ticket = pane.shared.ticket(false);
            let _ = pane.commands.try_send(Command::Start(ticket));
        }
        Ok(pane)
    }

    pub(crate) fn record_chat(&self, user: bool, text: &str) {
        if self
            .shared
            .session_name
            .load()
            .as_deref()
            .map(String::as_str)
            != Some(connecto_core::session::APP_SESSION)
            || text.is_empty()
        {
            return;
        }
        let mut guard = self
            .chat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let chat = Arc::make_mut(&mut guard);
        let text = &text[..text.floor_char_boundary(text.len().min(96 * 1024))];
        chat.messages.push((user, text.to_owned()));
        while chat.messages.len() > 256
            || chat
                .messages
                .iter()
                .map(|(_, text)| text.len())
                .sum::<usize>()
                > 192 * 1024
        {
            chat.messages.remove(0);
        }
        chat.revision = chat.revision.saturating_add(1);
    }

    /// The project folder the pane runs in, once one is chosen.
    pub(crate) fn project_path(&self) -> Option<PathBuf> {
        self.shared.project()
    }

    /// Set the project and begin preflight. The caller checked that nothing runs.
    pub(crate) fn begin_project(&self, project: PathBuf, how: Begin) {
        if self.shared.project.load().as_deref() != Some(&project) {
            *self
                .chat
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::default();
        }
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

    /// The model and effort of the next launch, or `None` for the tool's own.
    pub(crate) fn set_launch(&self, model: Option<String>, effort: Option<String>) {
        self.shared.model.store(model.map(Arc::new));
        self.shared.effort.store(effort.map(Arc::new));
    }

    /// The effort of the next launch, as [`Self::set_launch`] or the setup
    /// left it.
    #[cfg(test)]
    pub(crate) fn launch_effort(&self) -> Option<String> {
        self.shared.effort.load_full().map(|e| e.as_ref().clone())
    }

    /// Keystrokes and pastes the user has typed into this pane so far.
    pub(crate) fn user_inputs(&self) -> u64 {
        self.shared.user_input.load(Ordering::SeqCst)
    }

    /// How long since the person last typed into this pane; `None` when they
    /// have not this run.
    #[must_use]
    pub fn quiet_for(&self) -> Option<Duration> {
        self.shared.input().last_input.map(|at| at.elapsed())
    }

    /// What is known of the user's unsent draft now.
    pub(crate) fn draft(&self) -> Known {
        self.shared.input().draft.known().clone()
    }

    /// Hold the user's input back while Connecto types into the terminal, so
    /// the two never mix. Returns the draft as the held input leaves it, and
    /// how long the user had been quiet.
    pub(crate) fn begin_injection(&self) -> (Known, Duration) {
        let known = {
            let mut gate = self.shared.input();
            gate.held.get_or_insert_with(HeldInput::default);
            let quiet = gate.last_input.map_or(Duration::MAX, |at| at.elapsed());
            (gate.draft.known().clone(), quiet)
        };
        self.shared.injecting.store(true, Ordering::SeqCst);
        (self.shared.setup.wake)();
        known
    }

    /// Whether a message waits for the user's draft. Wakes the UI on a change.
    pub(crate) fn set_message_waiting(&self, waiting: bool) {
        if self.shared.message_waiting.swap(waiting, Ordering::SeqCst) != waiting {
            (self.shared.setup.wake)();
        }
    }

    /// Connecto is done typing: the draft is now `after`, and what the user
    /// typed meanwhile goes in, in order, where the cursor now is.
    pub(crate) fn end_injection(&self, after: DraftAfter) {
        {
            let mut gate = self.shared.input();
            match after {
                DraftAfter::Keep => {}
                DraftAfter::Reset => gate.draft.reset(),
                DraftAfter::Set(draft) => gate.draft.set(draft),
                DraftAfter::Forget => gate.draft.forget(),
            }
            let held = gate.held.take().unwrap_or_default();
            let live = self.shared.live.load_full();
            for (source, bytes) in held.runs {
                let sent = live
                    .as_ref()
                    .is_some_and(|session| session.write_input(bytes.clone(), source).is_ok());
                if sent {
                    gate.draft.feed(&bytes);
                } else {
                    // What did not go in is dropped, as a refused write is.
                    gate.draft.forget();
                    break;
                }
            }
        }
        self.shared.injecting.store(false, Ordering::SeqCst);
        (self.shared.setup.wake)();
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

    /// Whether the last start or login was refused because the project's
    /// Tools setting leaves this CLI out.
    #[must_use]
    pub fn tools_refused(&self) -> bool {
        self.shared.tools_refused.load(Ordering::SeqCst)
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

    fn injecting(&self) -> bool {
        self.shared.injecting.load(Ordering::SeqCst)
    }

    fn message_waiting(&self) -> bool {
        self.shared.message_waiting.load(Ordering::SeqCst)
    }
}

impl TerminalSession for ManagedPane {
    fn chat_history(&self) -> Option<Arc<connecto_core::session::ChatHistory>> {
        Some(
            self.chat
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
    }

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

    /// The window's input, or a remote peer's (the remote guard marks the
    /// peer's thread): written with that source, for prompt provenance.
    /// Followed for the draft, and held back while Connecto types into the
    /// terminal (see `begin_injection`).
    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        let source = crate::input_ledger::pane_source();
        self.shared.user_input.fetch_add(1, Ordering::SeqCst);
        let live = self.shared.live.load_full().ok_or(InputRefused::Closed)?;
        let mut gate = self.shared.input();
        gate.last_input = Some(Instant::now());
        if let Some(held) = &mut gate.held {
            return held.push(bytes, source);
        }
        gate.draft.feed(&bytes);
        let sent = live.write_input(bytes, source);
        if sent.is_err() {
            gate.draft.forget();
        }
        sent
    }

    fn input_generation(&self) -> Option<connecto_core::InputGeneration> {
        self.shared
            .live
            .load()
            .as_ref()
            .map(|session| session.input_generation())
    }

    /// Checked under the input lock, with the write: not while Connecto
    /// types (what it holds back would be replayed after the screen this was
    /// meant for), no other write since the generation read, and the input
    /// line holding exactly what is expected, both as every key typed into
    /// it says and as the screen draws it (a prompt the agent put back after
    /// an interrupt is drawn, though nobody typed it here).
    fn send_input_if(
        &self,
        condition: &connecto_core::InputCondition,
        bytes: Vec<u8>,
    ) -> Result<(), InputRefused> {
        let source = crate::input_ledger::pane_source();
        let mut gate = self.shared.input();
        if gate.held.is_some() {
            return Err(InputRefused::Held);
        }
        let live = self.shared.live.load_full().ok_or(InputRefused::Closed)?;
        // An early look; the generation is checked again where it counts,
        // in the input gate, with the enqueue.
        if condition
            .generation
            .is_some_and(|expected| live.input_generation() != expected)
        {
            return Err(InputRefused::Stale);
        }
        if let Some(expected) = &condition.composer {
            let crate::draft::Known::Exact(draft) = gate.draft.known() else {
                return Err(InputRefused::Stale);
            };
            if draft.text() != *expected {
                return Err(InputRefused::Stale);
            }
            let view = composer_view(self.shared.setup.tool, &live);
            let drawn = match &view {
                Some(view) if expected.is_empty() => {
                    shows_empty(view)
                        || (condition.unread_box_ok
                            && view.rest == connecto_adapter::BoxRows::NotRead
                            // The tool drawing a draft, or at work, vetoes
                            // it, whatever the host's activity says.
                            && !matches!(
                                view.state,
                                connecto_adapter::ComposerState::Occupied
                                    | connecto_adapter::ComposerState::Busy
                            ))
                }
                // A tool whose input box is not read at all.
                None => {
                    expected.is_empty()
                        && condition.unread_box_ok
                        && !matches!(self.shared.setup.tool, Tool::Claude | Tool::Codex)
                }
                Some(view) => {
                    view.state == connecto_adapter::ComposerState::Occupied
                        && shows_exactly(view, expected)
                }
            };
            if !drawn {
                return Err(InputRefused::Stale);
            }
        }
        if let Some(expected) = &condition.typed
            && !matches!(gate.draft.known(), crate::draft::Known::Exact(d) if d.text() == *expected)
        {
            return Err(InputRefused::Stale);
        }
        // Followed only once it went in, still under the input lock: a
        // refused write (another write came first, or a redraw) leaves the
        // draft as it was, so a retry finds it whole.
        let copy = bytes.clone();
        live.write_input_if(condition.generation, condition.output, bytes, source)?;
        self.shared.user_input.fetch_add(1, Ordering::SeqCst);
        gate.last_input = Some(Instant::now());
        gate.draft.feed(&copy);
        Ok(())
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

    fn claim_local_size(&self) {
        if let Some(session) = self.shared.live.load().as_ref() {
            session.claim_local_size();
        }
    }

    fn applied_size(&self) -> Option<GridSize> {
        self.shared
            .live
            .load()
            .as_ref()
            .and_then(|session| session.applied_size())
    }

    fn local_size_claims(&self) -> u64 {
        self.shared
            .live
            .load()
            .as_ref()
            .map_or(0, |session| session.local_size_claims())
    }

    fn scroll(&self, lines: i32) -> bool {
        self.shared
            .live
            .load()
            .as_ref()
            .is_some_and(|session| session.scroll(lines))
    }

    fn poll_capture(
        &self,
        max_cells: usize,
        flush: bool,
        screen: &mut TermSnapshot,
        history: &mut TermSnapshot,
    ) -> SnapshotOutcome {
        match self.shared.live.load().as_ref() {
            Some(session) => session.poll_capture(max_cells, flush, screen, history),
            None => {
                *screen = TermSnapshot::default();
                *history = TermSnapshot::default();
                SnapshotOutcome::Updated
            }
        }
    }

    fn select(&self, op: SelectOp) -> bool {
        self.shared
            .live
            .load()
            .as_ref()
            .is_some_and(|session| session.select(op))
    }

    fn selection_text(&self) -> SelectionText {
        self.shared
            .live
            .load()
            .as_ref()
            .map_or(SelectionText::Nothing, |session| session.selection_text())
    }

    fn clean_selection_text(&self) -> SelectionText {
        self.shared
            .live
            .load()
            .as_ref()
            .map_or(SelectionText::Nothing, |session| {
                session.clean_selection_text()
            })
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
/// What a shell is typed for its first command: one line, then Enter. `None`
/// for text that is not one plain command line (several lines, control
/// characters), which is never typed.
fn shell_first_line(text: &str) -> Option<Vec<u8>> {
    let line = text.trim_end_matches(['\n', '\r']).trim();
    if line.is_empty() || line.chars().any(|c| c.is_control() && c != '\t') {
        return None;
    }
    Some(format!("{line}\r").into_bytes())
}

/// The first and the retry prompt as a start found them.
type PromptSlots = (Option<Arc<String>>, Option<Arc<String>>);

/// Put the prompt a refused start took ([`take_prompt`]) where the next start
/// by hand reads it. A first start (`allow_resume`) leaves both slots as it
/// found them: the next one takes them again the same way, so a stale retry
/// prompt stays a retry prompt and never becomes the opening one. A fresh
/// fallback start had taken the retry prompt; its session is gone, so the
/// next start is a new conversation and reads the first prompt.
fn put_back_prompt(
    first: &ArcSwapOption<String>,
    retry: &ArcSwapOption<String>,
    allow_resume: bool,
    taken: Option<Arc<String>>,
    before: PromptSlots,
) {
    if allow_resume {
        first.store(before.0);
        retry.store(before.1);
    } else {
        first.store(taken);
        retry.store(None);
    }
}

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

/// Whether the input box as drawn (`view`) proves it holds exactly
/// `expected` and nothing else: one line, the whole of it on the box's first
/// row, and nothing typed on any row after it. A box whose rows past the
/// first the tool does not read, text over several rows or lines, or a long
/// paste drawn as a placeholder (which names a count, not the text) proves
/// nothing: what Enter would submit is not known, so it is not pressed.
/// Whether the input box as drawn (`view`) proves it is empty: the tool says
/// so, nothing typed on its first row (a hint is drawn faint; a line shaped
/// like the hint but not faint is someone's text), and every other row of
/// the box read and empty.
fn shows_empty(view: &connecto_adapter::ComposerView) -> bool {
    view.state == connecto_adapter::ComposerState::Empty
        && view.first_line.trim().is_empty()
        && view.rest.all_empty()
}

fn shows_exactly(view: &connecto_adapter::ComposerView, expected: &str) -> bool {
    !expected.contains('\n') && view.first_line.trim() == expected && view.rest.all_empty()
}

/// What `session`'s live bottom shows of `tool`'s input box, read whole (a
/// redraw still held back is not read); None when it cannot be read.
fn composer_view(tool: Tool, session: &LiveSession) -> Option<connecto_adapter::ComposerView> {
    let mut screen = TermSnapshot::default();
    let mut history = TermSnapshot::default();
    if session.poll_capture(0, false, &mut screen, &mut history) != SnapshotOutcome::Updated
        || screen.cells.is_empty()
    {
        return None;
    }
    let mut drawn = String::new();
    let mut typed = String::new();
    for row in 0..screen.rows {
        let cells = screen.row(row);
        drawn.extend(cells.iter().map(|c| c.ch));
        drawn.push('\n');
        typed.push_str(&crate::input_ready::typed_row(cells));
        typed.push('\n');
    }
    crate::launch::adapter(tool).composer(connecto_adapter::Screen {
        drawn: &drawn,
        typed: &typed,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod prompt_tests {
    use super::*;

    #[test]
    fn a_shell_is_typed_one_plain_command_line_only() {
        assert_eq!(
            shell_first_line("brew install --cask godot\n"),
            Some(b"brew install --cask godot\r".to_vec())
        );
        for refused in ["", "  ", "a\nb", "a\u{1b}[2J", "rm x\rrm y"] {
            assert_eq!(shell_first_line(refused), None, "{refused:?}");
        }
    }

    /// A write that is only right for the screen just read is refused
    /// while Connecto types, never held back and replayed after it.
    fn spawn_pane(name: &str) -> (ManagedPane, PathBuf) {
        spawn_pane_for(name, Tool::Claude)
    }

    fn spawn_pane_for(name: &str, tool: Tool) -> (ManagedPane, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mg-{name}-{}", std::process::id()));
        let pane = ManagedPane::spawn(PaneSetup {
            layout: Layout {
                data_root: dir.join("data"),
                home: dir.join("home"),
            },
            project: None,
            tool,
            size: GridSize {
                cols: 130,
                rows: 40,
            },
            wake: Arc::new(|| {}),
            extras: LaunchExtras::default(),
            bridge: None,
        })
        .unwrap();
        (pane, dir)
    }

    /// The pane runs a program that draws `screen` and then waits.
    fn draw(pane: &ManagedPane, dir: &Path, screen: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let file = dir.join("screen.txt");
        std::fs::write(&file, screen).unwrap();
        let spec = connecto_pty::PtySpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                // No echo, and input read and dropped: what tests write
                // never changes the screen, and never fills the terminal.
                format!("stty -echo; cat '{}'; exec cat > /dev/null", file.display()).into(),
            ],
            cwd: dir.to_owned(),
            env: Vec::new(),
            size: GridSize {
                cols: 130,
                rows: 40,
            },
        };
        let session = LiveSession::start_on(
            connecto_pty::OpenPty::open(spec.size).unwrap(),
            spec,
            Arc::new(|| {}),
        )
        .unwrap();
        pane.shared.set_live(Some(session));
        // Until the screen shows it.
        let first = screen
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap()
            .trim();
        for _ in 0..200 {
            let mut snap = TermSnapshot::default();
            let mut history = TermSnapshot::default();
            if let Some(live) = pane.shared.live.load_full()
                && live.poll_capture(0, false, &mut snap, &mut history) == SnapshotOutcome::Updated
                && (0..snap.rows).any(|r| {
                    snap.row(r)
                        .iter()
                        .map(|c| c.ch)
                        .collect::<String>()
                        .contains(first)
                })
            {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        unreachable!("the screen was never drawn");
    }

    const CLAUDE_EMPTY: &str =
        include_str!("../../connecto-adapter/tests/fixtures/claude/fresh_bypass_ready.txt");

    /// Claude's empty input box as Claude draws it: its hint faint.
    fn claude_empty() -> String {
        CLAUDE_EMPTY.replace(
            "Try \"how does <filepath> work?\"",
            "\x1b[2mTry \"how does <filepath> work?\"\x1b[22m",
        )
    }
    const CLAUDE_DRAFT: &str =
        include_str!("../../connecto-adapter/tests/fixtures/claude/repo_typed_draft.txt");

    /// A write that is only right for the screen just read is refused
    /// while Connecto types, never held back and replayed after it.
    #[test]
    fn a_conditional_write_is_refused_while_connecto_types() {
        let (pane, dir) = spawn_pane("held");
        let _ = pane.begin_injection();
        assert_eq!(
            pane.send_input_if(&connecto_core::InputCondition::default(), b"\r".to_vec()),
            Err(InputRefused::Held)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Any other write since the generation read refuses the write.
    #[test]
    fn a_conditional_write_is_refused_after_any_other_write() {
        let (pane, dir) = spawn_pane("generation");
        draw(&pane, &dir, &claude_empty());
        let read = pane.input_generation().unwrap();
        let on = |generation| connecto_core::InputCondition {
            generation: Some(generation),
            ..Default::default()
        };
        assert_eq!(pane.send_input_if(&on(read), b"\x1b[B".to_vec()), Ok(()));
        // The first write moved it: the same condition no longer holds.
        assert_eq!(
            pane.send_input_if(&on(read), b"\x1b[B".to_vec()),
            Err(InputRefused::Stale)
        );
        assert_eq!(pane.send_input_if(&on(read.next()), b"\r".to_vec()), Ok(()));
        // A key from the window in between: refused.
        let now = pane.input_generation().unwrap();
        pane.send_input(b"x".to_vec()).unwrap();
        assert_eq!(
            pane.send_input_if(&on(now), b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // Regression: a prompt the agent put back after an interrupt is drawn
    // in its input line though nobody typed it here; the tracker says the
    // line is empty, and the answer was pasted into it and submitted.
    #[test]
    fn a_composer_drawn_occupied_is_not_empty_whatever_was_typed() {
        let (pane, dir) = spawn_pane("composer");
        draw(&pane, &dir, CLAUDE_DRAFT);
        assert!(matches!(pane.shared.input().draft.known(), Known::Exact(d) if d.is_empty()));
        let empty = connecto_core::InputCondition {
            composer: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(
            pane.send_input_if(&empty, b"yes".to_vec()),
            Err(InputRefused::Stale)
        );
        let (pane, dir2) = spawn_pane("composer-empty");
        draw(&pane, &dir2, &claude_empty());
        assert_eq!(pane.send_input_if(&empty, b"yes".to_vec()), Ok(()));
        // Now it holds "yes", as typed: only that is expected before Enter.
        assert_eq!(
            pane.send_input_if(&empty, b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    // Regression: a draft shaped like Claude's hint but not drawn faint
    // ("Try \"existing draft\"", someone's text) read as an empty input
    // line; the paste went after it and Enter submitted both.
    #[test]
    fn a_draft_shaped_like_the_hint_is_not_an_empty_line() {
        let (pane, dir) = spawn_pane("hint-shaped");
        draw(
            &pane,
            &dir,
            &CLAUDE_EMPTY.replace("how does <filepath> work?", "existing draft"),
        );
        let empty = connecto_core::InputCondition {
            composer: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(
            pane.send_input_if(&empty, b"yes".to_vec()),
            Err(InputRefused::Stale)
        );
        let live = pane.shared.live.load_full().unwrap();
        assert!(live.input_log().is_empty(), "nothing written");
        // Drawn faint, it is the hint: the line is empty.
        let (pane, dir2) = spawn_pane("hint-faint");
        draw(&pane, &dir2, &claude_empty());
        assert_eq!(pane.send_input_if(&empty, b"yes".to_vec()), Ok(()));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    // Regression: a refused Enter (the paste's redraw still coming) made the
    // input line's draft unknown, so every retry failed after it.
    #[test]
    fn a_refused_conditional_write_leaves_the_draft_for_a_retry() {
        let (pane, dir) = spawn_pane("retry");
        draw(&pane, &dir, &claude_empty());
        let live = pane.shared.live.load_full().unwrap();
        let g = pane.input_generation().unwrap();
        let s = settled(&live);
        let paste = connecto_core::InputCondition {
            generation: Some(g),
            composer: Some(String::new()),
            output: Some(s),
            ..Default::default()
        };
        assert_eq!(pane.send_input_if(&paste, b"yes".to_vec()), Ok(()));
        let enter = |output| connecto_core::InputCondition {
            generation: Some(g.next()),
            typed: Some("yes".to_owned()),
            output: Some(output),
            ..Default::default()
        };
        // Lost to a redraw: refused, nothing written...
        assert_eq!(
            pane.send_input_if(&enter(s.wrapping_add(1)), b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        // ...and the retry, on the screen as it is, finds the draft whole.
        assert_eq!(
            pane.send_input_if(&enter(settled(&live)), b"\r".to_vec()),
            Ok(())
        );
        let log = live.input_log();
        assert_eq!(log.iter().filter(|b| **b == b'\r').count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    // Regression: Pi's and OpenCode's adapters do not read their input box
    // beyond its first row, so an idle, empty app session refused every
    // call prompt (ask_project, start_task), which worked there before. A
    // call prompt there rests on the draft tracker and on the tool not
    // drawing a draft (Enter then only on G+1); a dialog answer or typed
    // answer stays strict.
    #[test]
    fn a_call_prompt_goes_to_a_tool_whose_box_is_not_read_on_the_tracker() {
        let rule = "────────────────────";
        let pi = |editor: &str| {
            format!(
                " REPLY to something\n\n{rule}\n{editor}\n{rule}\n~/project\n↑5 ↓5 0.0%/128k (auto)                    gemma\n"
            )
        };
        let opencode = |prompt: &str| {
            format!(
                "┃{prompt}\n┃  Build  gemma\n╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n  tab agents  ctrl+p commands\n"
            )
        };
        let prompt_paste = connecto_core::InputCondition {
            composer: Some(String::new()),
            unread_box_ok: true,
            ..Default::default()
        };
        let strict = connecto_core::InputCondition {
            composer: Some(String::new()),
            ..Default::default()
        };
        let pi_busy = format!(
            " REPLY to something\n\n── ⠧ Working ───────\n\n{rule}\n~/project\n↑5 ↓5 0.0%/128k (auto)                    gemma\n"
        );
        let opencode_busy =
            "┃\n┃  Build  gemma\n╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n   ⬝⬝⬝⬝⬝⬝⬝⬝  esc interrupt\n".to_owned();
        for (i, (tool, empty, occupied)) in [
            (Tool::Pi, pi(""), Some(pi("half typed"))),
            (Tool::OpenCode, opencode(""), Some(opencode(" half typed"))),
            // At work, as the tool draws it (the host's activity may lag).
            (Tool::Pi, pi(""), Some(pi_busy)),
            (Tool::OpenCode, opencode(""), Some(opencode_busy)),
            (Tool::Antigravity, "> \n".to_owned(), None),
        ]
        .into_iter()
        .enumerate()
        {
            // Empty, as the tracker and the tool say: the prompt's paste
            // goes in; a strict write (an answer) still does not.
            let (pane, dir) = spawn_pane_for(&format!("unread-{i}"), tool);
            draw(&pane, &dir, &empty);
            assert_eq!(
                pane.send_input_if(&strict, b"yes".to_vec()),
                Err(InputRefused::Stale),
                "{tool:?}"
            );
            assert_eq!(
                pane.send_input_if(&prompt_paste, b"yes".to_vec()),
                Ok(()),
                "{tool:?}"
            );
            // Something typed, as the tracker knows: refused.
            let (pane, dir2) = spawn_pane_for(&format!("unread-typed-{i}"), tool);
            draw(&pane, &dir2, &empty);
            pane.shared.input().draft.set(Draft::pasted("half typed"));
            assert_eq!(
                pane.send_input_if(&prompt_paste, b"yes".to_vec()),
                Err(InputRefused::Stale),
                "{tool:?}"
            );
            // A draft the tool draws: refused, whatever the tracker says.
            if let Some(occupied) = occupied {
                let (pane, dir3) = spawn_pane_for(&format!("unread-drawn-{i}"), tool);
                draw(&pane, &dir3, &occupied);
                assert_eq!(
                    pane.send_input_if(&prompt_paste, b"yes".to_vec()),
                    Err(InputRefused::Stale),
                    "{tool:?}"
                );
                let _ = std::fs::remove_dir_all(dir3);
            }
            let _ = std::fs::remove_dir_all(dir);
            let _ = std::fs::remove_dir_all(dir2);
        }
    }

    // Regression: Claude's box was read from the last row starting with its
    // marker, so a draft above an indented `❯` row read as empty.
    #[test]
    fn a_codex_footer_without_a_dot_takes_a_strict_paste() {
        // Regression: this captured footer read as Booting, so a call's
        // prompt or a chat answer was refused before the paste.
        let strict = connecto_core::InputCondition {
            composer: Some(String::new()),
            ..Default::default()
        };
        let fixture =
            include_str!("../../connecto-adapter/tests/fixtures/codex/footer_without_dot.txt");
        // A plain first line for the drawing to be waited on.
        let screen = format!("Earlier output\n\n{fixture}").replace(
            "› Ask Codex to do anything",
            "› \u{1b}[2mAsk Codex to do anything\u{1b}[0m",
        );
        let (pane, dir) = spawn_pane_for("footer-without-dot", Tool::Codex);
        draw(&pane, &dir, &screen);
        assert_eq!(pane.send_input_if(&strict, b"yes".to_vec()), Ok(()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_draft_above_a_typed_marker_is_not_an_empty_line() {
        let empty = connecto_core::InputCondition {
            composer: Some(String::new()),
            unread_box_ok: true,
            ..Default::default()
        };
        let exact = connecto_core::InputCondition {
            composer: Some("yes".to_owned()),
            ..Default::default()
        };
        let codex = |rows: &str| {
            format!("{rows}\n\n  GPT-6-Astra medium · ~/workspace\n  ? for shortcuts\n")
        };
        for (tool, screen_empty, screen_yes) in [
            (
                Tool::Claude,
                CLAUDE_DRAFT.replace("\u{a0}Try a draft", "\u{a0}saved draft\n  ❯"),
                CLAUDE_DRAFT.replace("\u{a0}Try a draft", "\u{a0}saved draft\n  ❯ yes"),
            ),
            (
                Tool::Codex,
                codex("› saved draft\n  ›"),
                codex("› saved draft\n  › yes"),
            ),
        ] {
            // A draft, then an empty marker typed on the next row: not an
            // empty line.
            let (pane, dir) = spawn_pane_for(&format!("marker-empty-{tool:?}"), tool);
            draw(&pane, &dir, &screen_empty);
            assert_eq!(
                pane.send_input_if(&empty, b"yes".to_vec()),
                Err(InputRefused::Stale),
                "{tool:?}"
            );
            // A draft, then "› yes": not "yes".
            let (pane, dir2) = spawn_pane_for(&format!("marker-yes-{tool:?}"), tool);
            draw(&pane, &dir2, &screen_yes);
            pane.shared.input().draft.set(Draft::pasted("yes"));
            assert_eq!(
                pane.send_input_if(&exact, b"\r".to_vec()),
                Err(InputRefused::Stale),
                "{tool:?}"
            );
            let _ = std::fs::remove_dir_all(dir);
            let _ = std::fs::remove_dir_all(dir2);
        }
        // Codex: a typed `extra · note` right above its footer, no blank row
        // between: where the box ends is not known, nothing is proven.
        let footer_shaped = |first: &str| {
            format!(
                "{first}\n  extra · note\n  GPT-6-Astra medium · ~/workspace\n  ? for shortcuts\n"
            )
        };
        let (pane, dir4) = spawn_pane_for("footer-shaped-empty", Tool::Codex);
        draw(&pane, &dir4, &footer_shaped("› "));
        assert_eq!(
            pane.send_input_if(&empty, b"yes".to_vec()),
            Err(InputRefused::Stale)
        );
        let (pane, dir5) = spawn_pane_for("footer-shaped-yes", Tool::Codex);
        draw(&pane, &dir5, &footer_shaped("› yes"));
        pane.shared.input().draft.set(Draft::pasted("yes"));
        assert_eq!(
            pane.send_input_if(&exact, b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        let _ = std::fs::remove_dir_all(dir4);
        let _ = std::fs::remove_dir_all(dir5);
        // Nor does "yes" under a typed rule pass for "yes" alone.
        let (pane, dir2) = spawn_pane("typed-rule");
        draw(
            &pane,
            &dir2,
            &CLAUDE_DRAFT.replace("\u{a0}Try a draft", "\u{a0}yes\n  ────\n  extra text"),
        );
        pane.shared.input().draft.set(Draft::pasted("yes"));
        assert_eq!(
            pane.send_input_if(&exact, b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        let _ = std::fs::remove_dir_all(dir2);
    }

    // Regression: a long multi-line paste was taken as drawn when the
    // input line showed unrelated text (the draft comparison skipped a
    // paste that may be collapsed, and matched any occupied line).
    #[test]
    fn enter_needs_the_input_line_to_show_exactly_the_paste() {
        let long: String = (0..40)
            .map(|i| format!("line {i} of the task prompt"))
            .collect::<Vec<_>>()
            .join("\n");
        let on = |text: &str| connecto_core::InputCondition {
            composer: Some(text.to_owned()),
            ..Default::default()
        };
        // An unrelated draft is drawn: the paste is not shown.
        let (pane, dir) = spawn_pane("exact-unrelated");
        draw(&pane, &dir, CLAUDE_DRAFT);
        pane.shared.input().draft.set(Draft::pasted(&long));
        assert_eq!(
            pane.send_input_if(&on(&long), b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        // The line as drawn, exactly: yes; one character off: no.
        pane.shared.input().draft.set(Draft::pasted("Try a draft"));
        assert_eq!(
            pane.send_input_if(&on("Try a draft"), b"\r".to_vec()),
            Ok(())
        );
        pane.shared.input().draft.set(Draft::pasted("Try a draft."));
        assert_eq!(
            pane.send_input_if(&on("Try a draft."), b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        // A collapsed paste names a count, not the text: never proven.
        let (pane, dir2) = spawn_pane("exact-collapsed");
        draw(
            &pane,
            &dir2,
            &CLAUDE_DRAFT.replace("Try a draft", "[Pasted text #1 +39 lines]"),
        );
        pane.shared.input().draft.set(Draft::pasted(&long));
        assert_eq!(
            pane.send_input_if(&on(&long), b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        // "yes" with a second row under it is not "yes".
        let (pane, dir3) = spawn_pane("exact-extra-row");
        draw(
            &pane,
            &dir3,
            &CLAUDE_DRAFT.replace(
                "\u{a0}Try a draft",
                "\u{a0}yes\n  and also delete the tests",
            ),
        );
        pane.shared.input().draft.set(Draft::pasted("yes"));
        assert_eq!(
            pane.send_input_if(&on("yes"), b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        let _ = std::fs::remove_dir_all(dir3);
        // Nor with a blank row between, nor a row that starts with `─`
        // without being the box's rule: the box is read to its rule.
        for (i, rows) in ["\n\n  extra text", "\n─ extra text"].iter().enumerate() {
            let (pane, dir) = spawn_pane(&format!("exact-layout-{i}"));
            draw(
                &pane,
                &dir,
                &CLAUDE_DRAFT.replace("\u{a0}Try a draft", &format!("\u{a0}yes{rows}")),
            );
            pane.shared.input().draft.set(Draft::pasted("yes"));
            assert_eq!(
                pane.send_input_if(&on("yes"), b"\r".to_vec()),
                Err(InputRefused::Stale),
                "{rows:?}"
            );
            let _ = std::fs::remove_dir_all(dir);
        }
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    /// The pane runs a program that draws `before`, and `after` once the
    /// first input comes (a tool redrawing its input line on a paste).
    fn draw_then(pane: &ManagedPane, dir: &Path, before: &str, after: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("before.txt"), before).unwrap();
        std::fs::write(dir.join("after.txt"), after).unwrap();
        let script = format!(
            "stty -echo -icanon min 1; cat '{0}/before.txt'; head -c 1 > /dev/null; \
             printf '\\033[H\\033[2J'; cat '{0}/after.txt'; exec cat > /dev/null",
            dir.display()
        );
        let spec = connecto_pty::PtySpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: dir.to_owned(),
            env: Vec::new(),
            size: GridSize {
                cols: 130,
                rows: 40,
            },
        };
        let session = LiveSession::start_on(
            connecto_pty::OpenPty::open(spec.size).unwrap(),
            spec,
            Arc::new(|| {}),
        )
        .unwrap();
        pane.shared.set_live(Some(session));
    }

    /// The screen's generation once nothing more is drawn for a while.
    fn settled(live: &LiveSession) -> u64 {
        let mut last = live.output_generation();
        for _ in 0..100 {
            thread::sleep(Duration::from_millis(20));
            let now = live.output_generation();
            if now == last {
                return now;
            }
            last = now;
        }
        last
    }

    // Decision (c): a call's prompt over several lines is drawn collapsed
    // and cannot be read back; what Enter submits is known by construction:
    // an empty line, our paste at G and S, Enter at exactly G+1.
    #[test]
    fn a_collapsed_prompt_is_submitted_on_the_generation_after_its_own_paste() {
        let prompt = "Start task t-1:\nGoal: dark mode\nScope: the theme\nDone: it switches";
        let collapsed = CLAUDE_DRAFT.replace("Try a draft", "[Pasted text #1 +3 lines]");
        let condition = |generation, composer: Option<&str>, typed: Option<&str>, output| {
            connecto_core::InputCondition {
                generation: Some(generation),
                composer: composer.map(String::from),
                typed: typed.map(String::from),
                output: Some(output),
                unread_box_ok: false,
            }
        };
        let paste = |pane: &ManagedPane, live: &LiveSession| {
            let g = pane.input_generation().unwrap();
            let s = settled(live);
            let bytes = connecto_core::encode_paste(
                prompt,
                connecto_core::InputModes {
                    bracketed_paste: true,
                    ..Default::default()
                },
            );
            assert_eq!(
                pane.send_input_if(&condition(g, Some(""), None, s), bytes),
                Ok(())
            );
            g.next()
        };
        // Submitted: the line was empty, only our paste went in.
        let (pane, dir) = spawn_pane("collapsed");
        draw_then(&pane, &dir, &claude_empty(), &collapsed);
        let live = pane.shared.live.load_full().unwrap();
        let _ = settled(&live);
        let after = paste(&pane, &live);
        let s = settled(&live);
        // The Claude adapter sees the collapsed paste, not the text.
        let view = composer_view(Tool::Claude, &live).unwrap();
        assert_eq!(view.state, connecto_adapter::ComposerState::Occupied);
        assert_eq!(view.first_line, "[Pasted text #1 +3 lines]");
        // Read back from the screen, it proves nothing (a placeholder)...
        assert_eq!(
            pane.send_input_if(&condition(after, Some(prompt), None, s), b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        // Not holding exactly the prompt as typed: refused.
        assert_eq!(
            pane.send_input_if(
                &condition(after, None, Some("another prompt"), s),
                b"\r".to_vec()
            ),
            Err(InputRefused::Stale)
        );
        // ...but at G+1, holding our paste as typed, Enter goes in.
        assert_eq!(
            pane.send_input_if(&condition(after, None, Some(prompt), s), b"\r".to_vec()),
            Ok(())
        );
        // A key typed on the Mac between the paste and Enter: no Enter.
        let (pane, dir2) = spawn_pane("collapsed-key");
        draw_then(&pane, &dir2, &claude_empty(), &collapsed);
        let live = pane.shared.live.load_full().unwrap();
        let _ = settled(&live);
        let after = paste(&pane, &live);
        pane.send_input(b"x".to_vec()).unwrap();
        let s = settled(&live);
        assert_eq!(
            pane.send_input_if(&condition(after, None, Some(prompt), s), b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    // Regression: a redraw between the host's last look and the enqueue
    // (the selection moved to another choice) did not stop the Enter.
    #[test]
    fn a_write_on_a_screen_generation_is_refused_once_anything_is_drawn() {
        let (pane, dir) = spawn_pane("output");
        // This program draws back what it is sent: each write redraws.
        std::fs::create_dir_all(&dir).unwrap();
        let spec = connecto_pty::PtySpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "stty -echo; echo ready; exec cat".into()],
            cwd: dir.clone(),
            env: Vec::new(),
            size: GridSize { cols: 80, rows: 24 },
        };
        let session = LiveSession::start_on(
            connecto_pty::OpenPty::open(spec.size).unwrap(),
            spec,
            Arc::new(|| {}),
        )
        .unwrap();
        pane.shared.set_live(Some(session));
        let live = pane.shared.live.load_full().unwrap();
        let settled = || {
            let mut last = live.output_generation();
            for _ in 0..50 {
                thread::sleep(Duration::from_millis(20));
                let now = live.output_generation();
                if now == last {
                    return now;
                }
                last = now;
            }
            last
        };
        let read = settled();
        let on = |output| connecto_core::InputCondition {
            output: Some(output),
            ..Default::default()
        };
        assert_eq!(pane.send_input_if(&on(read), b"a\r".to_vec()), Ok(()));
        // It drew "a": what was read is no longer the screen.
        for _ in 0..100 {
            if live.output_generation() != read {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(live.output_generation(), read);
        assert_eq!(
            pane.send_input_if(&on(read), b"\r".to_vec()),
            Err(InputRefused::Stale)
        );
        assert_eq!(pane.send_input_if(&on(settled()), b"b".to_vec()), Ok(()));
        let _ = std::fs::remove_dir_all(dir);
    }

    // Regression: the generation was checked under the pane's lock but
    // counted after the PTY's gate let go, so a direct write (the relay's)
    // could land between a conditional write's check and its enqueue.
    #[test]
    fn a_conditional_write_lands_on_its_generation_whatever_writes_race_it() {
        let (pane, dir) = spawn_pane("race");
        draw(&pane, &dir, "ready\n");
        let live = pane.shared.live.load_full().unwrap();
        let racer = Arc::clone(&live);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let direct = thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let _ = racer.write_input(b"x".to_vec(), InputSource::Connecto);
            }
        });
        let mut landed = Vec::new();
        for _ in 0..20_000 {
            let read = live.input_generation();
            if live
                .write_input_if(Some(read), None, b"y".to_vec(), InputSource::Connecto)
                .is_ok()
            {
                landed.push(read.writes);
            }
        }
        stop.store(true, Ordering::SeqCst);
        direct.join().unwrap();
        // Each write is one byte: a conditional one sits exactly where its
        // generation said, nothing having come between.
        let log = live.input_log();
        let at: Vec<u64> = log
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'y')
            .map(|(i, _)| i as u64)
            .collect();
        assert!(!landed.is_empty());
        assert_eq!(at, landed);
        let _ = std::fs::remove_dir_all(dir);
    }

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

    /// A terminal's model and effort are installed with the pane, and a pane
    /// reused for another project's terminal of the same id takes that
    /// terminal's own, so its next launch doesn't keep the previous ones.
    #[test]
    fn the_launch_model_and_effort_come_with_the_pane_and_change_on_reuse() {
        let dir = std::env::temp_dir().join(format!("mg-launch-pane-{}", std::process::id()));
        let extras = LaunchExtras {
            model: Some("gpt-6.2-astra".to_owned()),
            effort: Some("medium".to_owned()),
            ..LaunchExtras::default()
        };
        let pane = ManagedPane::spawn(PaneSetup {
            layout: Layout {
                data_root: dir.join("data"),
                home: dir.join("home"),
            },
            project: None,
            tool: Tool::Codex,
            size: crate::INITIAL_SIZE,
            wake: Arc::new(|| {}),
            extras,
            bridge: None,
        })
        .unwrap();
        let now = |cell: &ArcSwapOption<String>| cell.load_full().map(|v| v.as_ref().clone());
        assert_eq!(now(&pane.shared.model).as_deref(), Some("gpt-6.2-astra"));
        assert_eq!(now(&pane.shared.effort).as_deref(), Some("medium"));
        pane.set_launch(None, Some("high".to_owned()));
        assert_eq!(now(&pane.shared.model), None);
        assert_eq!(now(&pane.shared.effort).as_deref(), Some("high"));
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

    /// A start refused before it launched (the Tools setting) puts its
    /// prompt back where the next start by hand reads it.
    #[test]
    fn a_refused_start_puts_its_prompt_back_for_the_next_start() {
        let text = |p: Option<Arc<String>>| p.as_deref().cloned();
        let slots = |first: &ArcSwapOption<String>, retry: &ArcSwapOption<String>| {
            (first.load_full(), retry.load_full())
        };
        // A resume refused: the next start resumes, and its fallback still
        // opens with the prompt.
        let (first, retry) = cells("hello");
        let before = slots(&first, &retry);
        let taken = take_prompt(&first, &retry, true, true);
        put_back_prompt(&first, &retry, true, taken, before);
        assert!(take_prompt(&first, &retry, true, true).is_none());
        assert_eq!(
            text(take_prompt(&first, &retry, false, false)).as_deref(),
            Some("hello")
        );
        // A stale retry prompt is not picked up as the opening one.
        let first = ArcSwapOption::empty();
        let retry = ArcSwapOption::from_pointee("stale".to_owned());
        let before = slots(&first, &retry);
        let taken = take_prompt(&first, &retry, true, false);
        put_back_prompt(&first, &retry, true, taken, before);
        assert!(
            take_prompt(&first, &retry, true, false).is_none(),
            "not the stale one"
        );
        // A fresh fallback refused: its session is gone, so the next start
        // by hand is new and opens with the prompt.
        let (first, retry) = cells("hello");
        let _ = take_prompt(&first, &retry, true, true);
        let before = slots(&first, &retry);
        let taken = take_prompt(&first, &retry, false, false);
        put_back_prompt(&first, &retry, false, taken, before);
        assert_eq!(
            text(take_prompt(&first, &retry, true, false)).as_deref(),
            Some("hello")
        );
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

    /// Input held while Connecto types keeps who wrote each run, so it reaches
    /// the PTY with its own provenance, and the limit counts every run.
    #[test]
    fn held_input_keeps_each_runs_source() {
        let mut held = HeldInput::default();
        assert!(held.push(b"ab".to_vec(), InputSource::Window).is_ok());
        assert!(held.push(b"c".to_vec(), InputSource::Window).is_ok());
        assert!(held.push(b"d".to_vec(), InputSource::Remote).is_ok());
        assert!(held.push(b"e".to_vec(), InputSource::Window).is_ok());
        assert_eq!(
            held.runs,
            vec![
                (InputSource::Window, b"abc".to_vec()),
                (InputSource::Remote, b"d".to_vec()),
                (InputSource::Window, b"e".to_vec()),
            ]
        );
        let rest = HELD_INPUT_LIMIT - held.len;
        assert!(held.push(vec![b'x'; rest], InputSource::Window).is_ok());
        assert_eq!(
            held.push(b"y".to_vec(), InputSource::Remote),
            Err(InputRefused::QueueFull)
        );
    }
}
