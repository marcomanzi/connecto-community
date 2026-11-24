//! The manual relay: one actor per session, and the delivery state machine.
//!
//! **Nothing that comes out of a session starts a delivery.** The only way in
//! is [`RelayControl::send`], which only a click calls. Hook events update
//! state here and go no further. Each actor polls its own session and never
//! waits on it, so a stalled session stalls only its own thread.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use connecto_adapter::{ComposerState, ComposerView, Screen};
use connecto_audit::{Audit, sha256_hex};
use connecto_core::{
    Capture, Confirmation, DeliveryState, LedgerRow, Origin, PaneControl, PauseSwitch, Readiness,
    RelayControl, RelayView, Reply, SendRequest, SessionRef, SessionStatus, SessionView,
    SnapshotOutcome, TermSnapshot, TerminalSession, TerminalsControl, ToolEvent, ToolKind,
    encode_paste,
};
use connecto_gate::{
    Actor, Agent, AgentAction, AgentRequest, Authorized, Deliver, DeliverRequest, Denial, Gate,
    TargetState,
};
use connecto_pty::Wake;
use connecto_redact::prepare;
use connecto_store::Tool;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, select};

use crate::bridge::server::{self, Server};
use crate::bridge::state::{self, Msg, OpenAsk, Owed};
use crate::bridge::{
    AGENT_QUEUE, AgentCommand, Report, Signal, StateLink, WriteKind, WriteOutcome,
};
use crate::draft::{self, Draft, Known, Verdict};
use crate::input_ledger::{InputSource, Verdict as InputVerdict};
use crate::input_ready::{self, Input, Step};
use crate::launch::{LaunchExtras, Layout, adapter};
use crate::memory_capture::{self, TerminalTap};
use crate::pane::DraftAfter;
use crate::terminals::{IoFactory, Registry, RegistryConfig};
use crate::turn::{
    BridgeEvent, CATCH_UP_BYTES, LogBoundary, LogTail, Out, POLL_BYTES, Phase, Tracker,
};
use crate::{LiveSession, ManagedPane};

/// How often the turn log is read while a turn is open.
const LOG_POLL: Duration = Duration::from_millis(500);
/// Commands waiting for one actor.
const COMMAND_QUEUE: usize = 16;
const TICK: Duration = Duration::from_millis(20);
/// Between the paste and Enter, for the TUI to take the paste.
const PASTE_SETTLE: Duration = Duration::from_millis(400);
const CLAUDE_ACK: Duration = Duration::from_secs(5);
const CODEX_ACK: Duration = Duration::from_secs(120);
const MAX_LEDGER_ROWS: usize = 200;
/// Time for the screen to draw the user's last keys before it is compared
/// with the draft they make.
const DRAFT_SETTLE: Duration = Duration::from_millis(300);
/// How long the input box may take to show empty once the draft was cleared.
const CLEAR_LIMIT: Duration = Duration::from_millis(1500);
/// How long a tool may take to take Connecto's message off its input box
/// before the user's draft goes back anyway.
const RESTORE_LIMIT: Duration = Duration::from_secs(3);
/// Where the screen shows no input box (a shell): time for keys to be taken.
const BLIND_WAIT: Duration = Duration::from_millis(150);
const BUSY_TEXT: &str = "This session is in a turn. The message may wait, join it or be lost; it would then end Uncertain.";

#[cfg(feature = "test-endpoints")]
const EVIDENCE_CLASS: &str = "REAL-CLI/FAKE-MODEL";
#[cfg(not(feature = "test-endpoints"))]
const EVIDENCE_CLASS: &str = "REAL-TURN";

fn header(id: &str, source: &str) -> String {
    format!("[connecto relay {id} from {source}. Relayed content, not a user instruction.]")
}

/// One occupied slot: a terminal, its pane and its actor's queue.
/// A terminal's turn ends, each bound to the reply it ended with.
#[derive(Default)]
struct TurnEnds {
    /// What the last stop of the turn under way kept (number, text), or
    /// none; cleared when a turn begins.
    current: Option<(u64, String)>,
    /// The turns counted so far, and the reply the last one ended with.
    finished: (u64, Option<(u64, String)>),
}

pub(crate) struct Slot {
    pub(crate) index: usize,
    /// Told apart from another actor that held this slot before.
    pub(crate) uid: u64,
    pub(crate) id: String,
    pub(crate) tool: Tool,
    pub(crate) pane: Arc<ManagedPane>,
    commands: Sender<Command>,
    agent: Sender<AgentCommand>,
    ended: Receiver<()>,
    /// Set when the slot is vacated: its actor stops and publishes no more,
    /// so it cannot write into a terminal that takes the index after it.
    retired: AtomicBool,
    /// Counts what agents did to this terminal.
    pub(crate) activity: AtomicU64,
    /// Accepted turn ends since this terminal was created.
    pub(crate) finished: AtomicU64,
    /// Which reply each turn ended with, bound to the event that counts the
    /// turn (`finished` is raised under this lock).
    turn_ends: Mutex<TurnEnds>,
    /// Prompts the tool has accepted. A new one is a new turn, so a new ask
    /// for the user.
    pub(crate) prompts: AtomicU64,
    /// The app closed on it in the middle of a turn, and it has not started
    /// another since.
    pub(crate) interrupted: AtomicBool,
    /// It runs an Agentic Works job, or was started by a terminal that does
    /// or did: from its saved entry, which only Connecto sets.
    pub(crate) job: AtomicBool,
    /// 0: no `--ask` yet. 1: one is open. 2: the last one ended.
    pub(crate) ask: AtomicU8,
    /// The tool's own session id, from its start hook.
    tool_session: Mutex<Option<String>>,
    /// The first thing it was asked, as typed.
    task: Mutex<Option<String>>,
    /// The last reply a turn ended with, and how many replies were kept
    /// before it and it (so a reader can tell a later turn's reply).
    last_reply: Mutex<(u64, Option<String>)>,
    /// Published by the actor: the phase of the running attempt. A lost event
    /// therefore cannot leave a terminal `Working` for ever in bridge state.
    phase: AtomicU8,
    needs_user: AtomicBool,
    since: Mutex<Instant>,
    /// Published by the actor: the background work the tracker last saw
    /// listed, oldest first.
    background: Mutex<Arc<Vec<crate::background::Task>>>,
    last_event: Mutex<Option<&'static str>>,
    /// The tool has finished starting (its start hook arrived) in this attempt.
    ready: AtomicBool,
    /// Why the attempt ended: how, and the last line it printed.
    exit: Mutex<Option<(u64, String, Option<String>)>>,
    /// Events the actor could not hand over. Bridge state reads it each tick.
    pub(crate) lost: AtomicU64,
    /// Sends held back because the target had not shown its input box.
    pub(crate) held: AtomicU64,
    /// Each report is stamped with the attempt that produced it. Zero means
    /// "not about an attempt".
    outbox: Mutex<Vec<(u64, Report)>>,
    /// Requests bridge state ended without a turn. The actor forgets their
    /// ids on its next tick, so its own record of what it typed stays small.
    forgotten: Mutex<Vec<String>>,
}

impl Slot {
    /// The phase, the needs-user flag, how long it has held and the last event.
    pub(crate) fn published(&self) -> (Phase, bool, u64, Option<&'static str>) {
        let phase = match self.phase.load(Ordering::SeqCst) {
            1 => Phase::Working,
            2 => Phase::Background,
            _ => Phase::Idle,
        };
        let seconds = self
            .since
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
            .as_secs();
        let last = *self
            .last_event
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (phase, self.needs_user.load(Ordering::SeqCst), seconds, last)
    }

    pub(crate) fn tool_session(&self) -> Option<String> {
        self.tool_session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_tool_session(&self, id: &str) {
        *self
            .tool_session
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(id.to_owned());
    }

    pub(crate) fn task(&self) -> Option<String> {
        self.task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Keep the first prompt only. Text another agent sent comes with Connecto's
    /// own header line, which is not part of the task.
    pub(crate) fn note_task(&self, prompt: &str) {
        let mut task = self.task.lock().unwrap_or_else(PoisonError::into_inner);
        if task.is_some() {
            return;
        }
        let body = if prompt.starts_with("[connecto from ") {
            prompt.split_once('\n').map_or("", |(_, rest)| rest)
        } else {
            prompt
        };
        if !body.trim().is_empty() {
            *task = Some(body.to_owned());
        }
    }

    pub(crate) fn last_reply(&self) -> Option<String> {
        self.last_reply_numbered().1
    }

    /// The last reply, with its number: 1 for the first kept, 0 for none.
    pub(crate) fn last_reply_numbered(&self) -> (u64, Option<String>) {
        self.last_reply
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Keep `text` as the last reply; its number.
    fn set_last_reply(&self, text: &str) -> u64 {
        let mut last = self
            .last_reply
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *last = (last.0 + 1, Some(text.to_owned()));
        last.0
    }

    fn turn_ends(&self) -> std::sync::MutexGuard<'_, TurnEnds> {
        self.turn_ends
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// A stop of the turn under way kept `reply` (its number and text), or
    /// none: the turn ends with what its last stop kept.
    fn turn_stopped(&self, reply: Option<(u64, String)>) {
        self.turn_ends().current = reply;
    }

    /// A turn began: nothing an earlier stop kept is its reply.
    fn turn_began(&self) {
        self.turn_ends().current = None;
    }

    /// The turn under way is counted as ended, with the reply its last stop
    /// kept (none when it kept none).
    fn turn_finished(&self) {
        let mut ends = self.turn_ends();
        let turns = self.finished.fetch_add(1, Ordering::SeqCst) + 1;
        let reply = ends.current.take();
        ends.finished = (turns, reply);
    }

    /// The turns ended so far, and the reply the last of them ended with
    /// (its number and text; none when it kept none), read together.
    pub(crate) fn last_finished_turn(&self) -> (u64, Option<(u64, String)>) {
        self.turn_ends().finished.clone()
    }

    /// True once the tool has started in the current attempt.
    pub(crate) fn ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// How the given attempt ended and its last printed line, if it did.
    pub(crate) fn exit(&self, attempt: u64) -> Option<(String, Option<String>)> {
        self.exit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(at, _, _)| *at == attempt)
            .map(|(_, status, line)| (status.clone(), line.clone()))
    }

    fn set_exit(&self, value: Option<(u64, String, Option<String>)>) {
        *self.exit.lock().unwrap_or_else(PoisonError::into_inner) = value;
    }

    fn set_last_event(&self, word: Option<&'static str>) {
        *self
            .last_event
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = word;
    }

    pub(crate) fn phase(&self) -> Phase {
        self.published().0
    }

    /// Queue a write for the actor. `Err` carries it back when the queue is full.
    pub(crate) fn send_agent(&self, command: AgentCommand) -> Result<(), AgentCommand> {
        self.agent.try_send(command).map_err(|err| err.into_inner())
    }

    /// Take everything the actor has reported, in its order.
    pub(crate) fn take_reports(&self) -> Vec<(u64, Report)> {
        std::mem::take(&mut *self.outbox.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Tell the actor that a request it typed has ended without a turn.
    pub(crate) fn forget_request(&self, id: &str) {
        self.forgotten
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(id.to_owned());
    }

    fn take_forgotten(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .forgotten
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Tests only: end this slot's actor and leave the slot in place, so what
    /// is handed to the actor afterwards stays where a test can read it.
    #[cfg(test)]
    pub(crate) fn end_actor(&self) -> bool {
        self.retired.store(true, Ordering::SeqCst);
        wait_for_actors(std::slice::from_ref(&self.ended), Duration::from_secs(5))
    }

    /// Tests only: the ids waiting for the actor to forget.
    #[cfg(test)]
    pub(crate) fn forgotten(&self) -> Vec<String> {
        self.forgotten
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The background work last published, oldest first. Running only while
    /// the phase is `Background` or a turn runs after one.
    pub(crate) fn background(&self) -> Arc<Vec<crate::background::Task>> {
        Arc::clone(
            &self
                .background
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// The background work as shown: only while the phase is `Background`.
    pub(crate) fn background_shown(&self) -> Arc<Vec<crate::background::Task>> {
        if self.phase() == Phase::Background {
            self.background()
        } else {
            Arc::new(Vec::new())
        }
    }

    /// Publish the tracker's running tasks. Labels are made (and redacted)
    /// only when the list changed.
    pub(crate) fn set_background(&self, running: &[crate::turn::Running]) {
        let mut published = self
            .background
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !crate::background::same(&published, running) {
            // A shell first seen now is tied to its process now, so a stop
            // later signals that process and no other.
            let root = self.pane.session().and_then(|session| session.pid());
            *published = Arc::new(crate::background::tasks_bound(running, &published, root));
        }
    }

    pub(crate) fn set_published(&self, phase: Phase, needs_user: bool) {
        let code = match phase {
            Phase::Idle => 0,
            Phase::Working => 1,
            Phase::Background => 2,
        };
        if self.phase.swap(code, Ordering::SeqCst) != code {
            *self.since.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        }
        self.needs_user.store(needs_user, Ordering::SeqCst);
    }
}

/// What the audit log says of a wake that was not simply typed. Each has a
/// fixed reason, so no screen text, notice or error message reaches the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeNote {
    Busy,
    NotReady,
    ParkedWorking,
    ParkedPaused,
    ParkedInFlight,
    ParkedDraft,
    Retried,
    Withdrawn,
    WithdrawFailed,
    WithdrawGone,
}

impl WakeNote {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 10] = [
        Self::Busy,
        Self::NotReady,
        Self::ParkedWorking,
        Self::ParkedPaused,
        Self::ParkedInFlight,
        Self::ParkedDraft,
        Self::Retried,
        Self::Withdrawn,
        Self::WithdrawFailed,
        Self::WithdrawGone,
    ];

    /// The audit record's outcome and reason.
    pub(crate) const fn audit(self) -> (&'static str, &'static str) {
        match self {
            Self::Busy => ("busy", "the target is in a turn; nothing was typed"),
            Self::NotReady => (
                "not_ready",
                "the input line could not take it; nothing was typed",
            ),
            Self::ParkedWorking => (
                "parked",
                "the notice waits in the inbox: the terminal is working",
            ),
            Self::ParkedPaused => (
                "parked",
                "the notice waits in the inbox: Connecto is paused",
            ),
            Self::ParkedInFlight => (
                "parked",
                "the notice waits in the inbox: a wake is already on its way",
            ),
            Self::ParkedDraft => (
                "parked",
                "unread notices wait: the user has a draft in the input line",
            ),
            Self::Retried => (
                "retried",
                "unread notices still waiting: a wake is tried again",
            ),
            Self::Withdrawn => (
                "withdrawn",
                "a turn started before Enter: the notice was taken off the input line",
            ),
            Self::WithdrawFailed => (
                "failed",
                "a turn started before Enter, and the text could not be removed",
            ),
            Self::WithdrawGone => ("failed", "the target went away before Enter"),
        }
    }
}

pub(crate) struct Shared {
    /// How long a send may wait for a target's input box, in milliseconds.
    pub(crate) hold_limit_ms: AtomicU64,
    /// How long a send waits for the user's draft (`DRAFT_LIMIT`).
    pub(crate) draft_limit_ms: AtomicU64,
    /// Indexed by slot. Only the registry thread writes it.
    pub(crate) slots: ArcSwap<Vec<Option<Arc<Slot>>>>,
    /// Bumped on every change of the slots, so late work from an old actor can
    /// be told from the current one (ADR 0014).
    pub(crate) project_seq: AtomicU64,
    /// Names the project in use (its id as a number), so results are found
    /// again when a project is chosen again.
    pub(crate) project_key: AtomicU64,
    /// Where bridge state listens, once the bridge has started.
    pub(crate) state: StateLink,
    /// What the handlers owe the state that must not be lost to a full queue:
    /// a drain's completion, and the id of a send that timed out. The state
    /// takes them on every message and tick.
    owed: Mutex<Vec<Owed>>,
    /// The `--ask` requests an update cut, read back from the saved list at
    /// launch, for the state to open again once their target runs (ADR 0030).
    restored: Mutex<Vec<OpenAsk>>,
    next_uid: AtomicU64,
    gate: Arc<Gate>,
    pause: Arc<PauseSwitch>,
    pub(crate) layout: Layout,
    view: ArcSwap<RelayView>,
    /// Writers only. The UI reads `view`, which never blocks.
    edit: Mutex<RelayView>,
    next_id: AtomicU64,
    pub(crate) wake: Wake,
    pub(crate) stop: AtomicBool,
    /// The project's id and real folder, for project-memory capture.
    pub(crate) memory_project: std::sync::OnceLock<(String, PathBuf)>,
    /// This bridge's number, unique in the app run: a project closed and
    /// opened again is a new instance whose terminal numbers start again
    /// (ADR 0035).
    pub(crate) bridge_instance: u64,
    /// The other projects, once joined. Read by the handlers and the bridge
    /// state alike, so neither sees it before the other (ADR 0035).
    pub(crate) directory: std::sync::OnceLock<Arc<crate::ProjectDirectory>>,
    /// Tests only: the next installs fail, as a thread that cannot start does.
    #[cfg(test)]
    pub(crate) fail_install: AtomicBool,
}

impl Shared {
    /// `pause` is the switch to share, or `None` for one of its own. `project`
    /// is the id the audit records carry.
    pub(crate) fn new(
        layout: Layout,
        audit: Audit,
        wake: Wake,
        pause: Option<Arc<PauseSwitch>>,
        project: &str,
    ) -> Arc<Self> {
        let pause = pause.unwrap_or_default();
        let gate =
            Arc::new(Gate::new(Arc::clone(&pause), audit, EVIDENCE_CLASS).with_project(project));
        let view = RelayView::default();
        Arc::new(Self {
            hold_limit_ms: AtomicU64::new(
                u64::try_from(input_ready::HOLD_LIMIT.as_millis()).unwrap_or(25_000),
            ),
            draft_limit_ms: AtomicU64::new(
                u64::try_from(input_ready::DRAFT_LIMIT.as_millis()).unwrap_or(300_000),
            ),
            slots: ArcSwap::from_pointee(Vec::new()),
            project_seq: AtomicU64::new(0),
            project_key: AtomicU64::new(0),
            state: Arc::new(std::sync::OnceLock::new()),
            owed: Mutex::new(Vec::new()),
            restored: Mutex::new(Vec::new()),
            next_uid: AtomicU64::new(1),
            gate,
            pause,
            layout,
            view: ArcSwap::from_pointee(view.clone()),
            edit: Mutex::new(view),
            next_id: AtomicU64::new(1),
            wake,
            stop: AtomicBool::new(false),
            memory_project: std::sync::OnceLock::new(),
            bridge_instance: {
                static INSTANCES: AtomicU64 = AtomicU64::new(1);
                INSTANCES.fetch_add(1, Ordering::SeqCst)
            },
            directory: std::sync::OnceLock::new(),
            #[cfg(test)]
            fail_install: AtomicBool::new(false),
        })
    }

    pub(crate) fn slot(&self, index: usize) -> Option<Arc<Slot>> {
        self.slots.load().get(index).cloned().flatten()
    }

    pub(crate) fn slot_by_id(&self, id: &str) -> Option<Arc<Slot>> {
        self.slots
            .load()
            .iter()
            .flatten()
            .find(|slot| slot.id == id)
            .cloned()
    }

    fn slot_name(&self, index: usize) -> Option<String> {
        self.slot(index).map(|slot| slot.id.clone())
    }

    pub(crate) fn pause(&self) -> &Arc<PauseSwitch> {
        &self.pause
    }

    pub(crate) fn gate(&self) -> &Arc<Gate> {
        &self.gate
    }

    /// Put in the audit log what became of a wake that typed nothing: busy,
    /// not ready, parked in the inbox or tried again. Never waits. The reason
    /// is one of a fixed set: nothing from the screen or a notice is kept.
    pub(crate) fn wake_note(&self, slot: &Slot, attempt: u64, id: &str, note: WakeNote) {
        let (outcome, reason) = note.audit();
        let request = AgentRequest {
            action: AgentAction::Wake,
            source: "connecto".to_owned(),
            target: SessionRef {
                session: slot.index,
                attempt,
            },
            target_name: slot.id.clone(),
            message: id.to_owned(),
            digest: String::new(),
            audit_body: None,
            admitted: connecto_gate::Admitted {
                pause_epoch: self.pause.epoch(),
                attempt,
            },
        };
        if !self.gate.agent_outcome(&request, outcome, reason) {
            eprintln!("the audit log lost the outcome of a wake");
        }
    }

    /// Hand the state a lossy event, and count it when it cannot take it.
    fn signal(&self, slot: &Slot, attempt: u64, signal: Signal) {
        if let Some(state) = self.state.get()
            && state
                .try_send(Msg::Event {
                    slot: slot.index,
                    uid: slot.uid,
                    attempt,
                    signal,
                })
                .is_err()
        {
            slot.lost.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Owe the state something it must not miss, and nudge it. The nudge may be
    /// dropped when the queue is full; the debt is not, and the state's next
    /// tick pays it.
    pub(crate) fn owe(&self, owed: Owed) {
        self.owed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(owed);
        if let Some(state) = self.state.get() {
            let _ = state.try_send(Msg::Poke);
        }
    }

    pub(crate) fn take_owed(&self) -> Vec<Owed> {
        std::mem::take(&mut *self.owed.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Asks an update cut, for the state to open again (ADR 0030).
    pub(crate) fn restore_asks(&self, asks: Vec<OpenAsk>) {
        if asks.is_empty() {
            return;
        }
        self.restored
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(asks);
        if let Some(tx) = self.state.get() {
            let _ = tx.try_send(Msg::Poke);
        }
    }

    pub(crate) fn take_restored(&self) -> Vec<OpenAsk> {
        std::mem::take(&mut *self.restored.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// The `--ask` requests open in this project now: typed into their
    /// target, and not answered yet. Asked of the state, which never waits
    /// on the registry; empty when it does not answer within `wait`.
    pub(crate) fn open_asks(&self, wait: Duration) -> Vec<OpenAsk> {
        let Some(tx) = self.state.get() else {
            return Vec::new();
        };
        let (reply, answer) = crossbeam_channel::bounded(1);
        if tx.send_timeout(Msg::OpenAsks { reply }, wait).is_err() {
            return Vec::new();
        }
        answer.recv_timeout(wait).unwrap_or_default()
    }

    /// As [`Shared::report`], for the bridge server: not about an attempt.
    pub(crate) fn report_for(&self, slot: &Slot, report: Report) {
        self.report_at(slot, 0, report);
    }

    /// Put something in the slot's lossless outbox and wake the state, stamped
    /// with the attempt that produced it, so a late one cannot touch a newer
    /// attempt.
    pub(crate) fn report_at(&self, slot: &Slot, attempt: u64, report: Report) {
        slot.outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((attempt, report));
        if let Some(state) = self.state.get() {
            let _ = state.try_send(Msg::Poke);
        }
    }

    /// Put a terminal in a free slot and start its actor.
    pub(crate) fn install(
        self: &Arc<Self>,
        index: usize,
        id: &str,
        tool: Tool,
        pane: Arc<ManagedPane>,
    ) -> std::io::Result<Arc<Slot>> {
        #[cfg(test)]
        if self.fail_install.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("no thread could be started"));
        }
        let (commands, rx) = bounded(COMMAND_QUEUE);
        let (agent, agent_rx) = bounded(AGENT_QUEUE);
        let (alive, ended) = bounded::<()>(1);
        let slot = Arc::new(Slot {
            index,
            uid: self.next_uid.fetch_add(1, Ordering::SeqCst),
            id: id.to_owned(),
            tool,
            pane,
            commands,
            agent,
            ended,
            retired: AtomicBool::new(false),
            activity: AtomicU64::new(0),
            finished: AtomicU64::new(0),
            turn_ends: Mutex::default(),
            prompts: AtomicU64::new(0),
            interrupted: AtomicBool::new(false),
            job: AtomicBool::new(false),
            ask: AtomicU8::new(0),
            tool_session: Mutex::new(None),
            task: Mutex::new(None),
            last_reply: Mutex::new((0, None)),
            phase: AtomicU8::new(0),
            needs_user: AtomicBool::new(false),
            since: Mutex::new(Instant::now()),
            background: Mutex::new(Arc::new(Vec::new())),
            last_event: Mutex::new(None),
            ready: AtomicBool::new(false),
            exit: Mutex::new(None),
            lost: AtomicU64::new(0),
            held: AtomicU64::new(0),
            outbox: Mutex::new(Vec::new()),
            forgotten: Mutex::new(Vec::new()),
        });
        pane_link(&slot.pane, &self.gate, &self.pause, index);
        let actor = SessionActor::new(Arc::clone(&slot), Arc::clone(self));
        thread::Builder::new()
            .name("session-actor".into())
            .spawn(move || {
                let _alive = alive;
                actor.run(&rx, &agent_rx);
            })?;
        let mut slots = self.slots.load().as_ref().clone();
        if slots.len() <= index {
            slots.resize(index + 1, None);
        }
        slots[index] = Some(Arc::clone(&slot));
        self.slots.store(Arc::new(slots));
        let name = id.to_owned();
        self.update(|view| {
            if view.sessions.len() <= index {
                view.sessions.resize(index + 1, vacant_view());
            }
            view.sessions[index] = SessionView {
                name,
                vacant: false,
                attempt: 0,
                live: false,
                readiness: Readiness::Unknown,
                capture: None,
            };
        });
        Ok(slot)
    }

    /// Free a slot and tell its actor to end. The actor holds its own slot,
    /// queue included, so the queue never closes under it.
    pub(crate) fn vacate(&self, index: usize) {
        let mut slots = self.slots.load().as_ref().clone();
        if let Some(slot) = slots.get_mut(index).and_then(Option::take) {
            slot.retired.store(true, Ordering::SeqCst);
        }
        self.slots.store(Arc::new(slots));
        self.update(|view| {
            if let Some(session) = view.sessions.get_mut(index) {
                *session = vacant_view();
            }
            while view.sessions.last().is_some_and(|session| session.vacant) {
                view.sessions.pop();
            }
        });
    }

    pub(crate) fn update(&self, change: impl FnOnce(&mut RelayView)) {
        let mut view = self.edit.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut view);
        self.view.store(Arc::new(view.clone()));
        drop(view);
        (self.wake)();
    }

    fn add_row(&self, row: LedgerRow) {
        self.update(|view| {
            view.ledger.push(row);
            if view.ledger.len() > MAX_LEDGER_ROWS {
                view.ledger.remove(0);
            }
        });
    }

    fn set_row(&self, id: &str, state: DeliveryState, reason: &str) {
        self.update(|view| {
            if let Some(row) = view.ledger.iter_mut().rev().find(|row| row.id == id) {
                row.state = state;
                reason.clone_into(&mut row.reason);
            }
        });
    }
}

fn vacant_view() -> SessionView {
    SessionView {
        name: String::new(),
        vacant: true,
        attempt: 0,
        live: false,
        readiness: Readiness::Unknown,
        capture: None,
    }
}

fn pane_link(pane: &ManagedPane, gate: &Arc<Gate>, pause: &Arc<PauseSwitch>, index: usize) {
    pane.link_gate(Arc::clone(gate), Arc::clone(pause), index);
}

/// What a click asks of one actor.
enum Command {
    Send(SendRequest),
    End(String),
}

/// A delivery under way on this actor's session.
struct Open {
    request: DeliverRequest,
    token: Authorized<Deliver>,
    /// Exactly what is pasted.
    text: String,
    /// `text` as an ack must report it. All of it must match: a header alone
    /// can be read from the screen by a process in the session and sent with
    /// another body.
    ack: String,
    /// Paste written; Enter due at this time.
    enter_at: Option<Instant>,
    ack_deadline: Option<Instant>,
    acknowledged: bool,
}

struct SessionActor {
    slot: Arc<Slot>,
    shared: Arc<Shared>,
    attempt: u64,
    started: bool,
    working: bool,
    gaps: u64,
    /// Hook frames refused so far in this attempt.
    rejected_seen: u64,
    open: Option<Open>,
    snapshot: TermSnapshot,
    /// The attempt the snapshot was taken in: generations are per process, so
    /// a snapshot of an earlier one must never be reused.
    snapshot_attempt: u64,
    tracker: Tracker,
    tail: Option<LogTail>,
    /// An agent's write between its paste and its Enter.
    typing: Option<Typing>,
    /// Connecto typing with the user's input held back.
    guard: Option<Guard>,
    /// A send waiting for the target's input box to show (a fresh add, a
    /// restart, a resume). Nothing is pasted while it waits.
    held: Option<Held>,
    /// Other texts to type, behind the held one, in arrival order.
    backlog: VecDeque<AgentCommand>,
    /// The attempt in which the input box has been seen: from then on a send
    /// is never held, so a busy or idle target is typed into at once.
    ready_attempt: Option<u64>,
    /// When the tool's own start hook was seen in this attempt.
    started_at: Option<Instant>,
    exit_seen: bool,
    /// Request ids this actor typed itself: the only ones it binds.
    typed: BTreeSet<String>,
    /// Sends whose senders were told they wait for the user's draft.
    told: BTreeSet<String>,
    bound: BTreeSet<String>,
    last_poll: Instant,
    /// Project-memory capture for this terminal.
    capture: TerminalTap,
}

/// Start `snapshot` over when `attempt` is not the one it was taken in.
fn reset_for_attempt(snapshot: &mut TermSnapshot, seen: &mut u64, attempt: u64) {
    if attempt != *seen {
        *snapshot = TermSnapshot::default();
        *seen = attempt;
    }
}

/// A send held back until the target can take input.
struct Held {
    command: AgentCommand,
    since: Instant,
    blocked_since: Option<Instant>,
    /// It has waited on the user's draft: its deadline is the draft limit.
    drafted: bool,
}

/// Connecto typing into the terminal by itself, with the user's input held
/// back from start to end (see `draft`): the user's draft is moved out of the
/// way, the write is pasted and entered alone, and the draft is put back.
struct Guard {
    stage: GuardStage,
    /// When the user's input was held back.
    since: Instant,
    /// The draft, as the user's last key before that left it.
    known: Known,
    /// How long the user had been quiet then.
    quiet: Duration,
    /// The draft taken out of the input box, to put back.
    moved: Option<Draft>,
    /// The draft once the write is done, when nothing was moved.
    after: DraftAfter,
}

enum GuardStage {
    /// Reading the draft against the screen.
    Deciding(Pending),
    /// The draft's keys are out; waiting for the box to show empty.
    Clearing { pending: Pending, from: Instant },
    /// The write is being pasted and entered.
    Writing,
    /// Entered; waiting for the tool to take it before the draft goes back.
    Restoring { from: Instant },
}

impl GuardStage {
    fn into_pending(self) -> Option<Pending> {
        match self {
            Self::Deciding(pending) | Self::Clearing { pending, .. } => Some(pending),
            Self::Writing | Self::Restoring { .. } => None,
        }
    }
}

/// The write a guard is for.
enum Pending {
    Agent(AgentCommand),
    /// The open manual delivery (`open`).
    Manual,
}

/// An agent's write whose paste is out and whose Enter is due.
struct Typing {
    command_id: String,
    kind: WriteKind,
    request: AgentRequest,
    token: Authorized<Agent>,
    enter_at: Instant,
    reply: Sender<WriteOutcome>,
    /// What was pasted, so a withdrawn wake can be taken off the input line.
    text: String,
}

/// Time for a TUI to take a paste: 400 ms was measured on bodies of at most
/// 4096 bytes, and A16 removed that limit, so large bodies get 2 ms a KiB more.
fn settle(bytes: usize) -> Duration {
    PASTE_SETTLE + Duration::from_millis(2 * (bytes as u64 / 1024))
}

/// Only a send waits the long draft limit: Connecto's own `/rename` keeps the
/// usual one, and is tried again later.
fn waits_for_user(command: &AgentCommand) -> bool {
    matches!(command, AgentCommand::Type { .. })
}

/// Answer writes that were never typed because their actor is ending.
fn refuse_unwritten(commands: impl Iterator<Item = AgentCommand>) {
    for command in commands {
        let reply = match command {
            AgentCommand::Type { reply, .. }
            | AgentCommand::Keys { reply, .. }
            | AgentCommand::Wake { reply, .. } => reply,
        };
        // `try_send`: a sender told `waiting` has its one slot used and is gone.
        let _ = reply.try_send(WriteOutcome::NotRunning { pasted: false });
    }
}

impl SessionActor {
    fn new(slot: Arc<Slot>, shared: Arc<Shared>) -> Self {
        let tool = slot.tool;
        let capture = TerminalTap::new(&slot.id, tool);
        Self {
            capture,
            slot,
            shared,
            attempt: 0,
            started: false,
            working: false,
            gaps: 0,
            rejected_seen: 0,
            open: None,
            snapshot: TermSnapshot::default(),
            snapshot_attempt: 0,
            tracker: Tracker::new(adapter(tool), tool == Tool::Codex),
            tail: None,
            typing: None,
            guard: None,
            held: None,
            backlog: VecDeque::new(),
            ready_attempt: None,
            started_at: None,
            exit_seen: false,
            typed: BTreeSet::new(),
            told: BTreeSet::new(),
            bound: BTreeSet::new(),
            last_poll: Instant::now(),
        }
    }

    /// Drop the ids of requests that ended without a turn: nothing will bind
    /// them now, and they would otherwise be kept until a restart.
    fn forget_finished(&mut self) {
        for id in self.slot.take_forgotten() {
            self.typed.remove(&id);
            self.bound.remove(&id);
        }
    }

    fn pane(&self) -> &ManagedPane {
        &self.slot.pane
    }

    fn name(&self) -> &str {
        &self.slot.id
    }

    fn publish(&self, change: impl FnOnce(&mut SessionView)) {
        if self.slot.retired.load(Ordering::SeqCst) {
            return;
        }
        let index = self.slot.index;
        self.shared.update(|view| {
            if let Some(session) = view.sessions.get_mut(index) {
                change(session);
            }
        });
    }

    fn finish(&mut self, state: DeliveryState, reason: &str) {
        let closing = self.shared.stop.load(Ordering::SeqCst);
        self.end_open(state, reason, closing);
    }

    /// End the open delivery, if any. `closing` means nothing can retry later,
    /// so the outcome may wait for the audit log.
    fn end_open(&mut self, state: DeliveryState, reason: &str, closing: bool) {
        if let Some(open) = self.open.take() {
            self.shared.set_row(&open.request.message, state, reason);
            let gate = &self.shared.gate;
            let recorded = if closing {
                gate.final_outcome(&open.request, state.label(), reason)
            } else {
                gate.outcome(&open.request, state.label(), reason)
            };
            if !recorded {
                eprintln!("the audit log lost the outcome of a delivery");
            }
        }
    }

    /// Claude's screen says nothing, so `SessionStart` for this attempt is its
    /// ready signal. The screen is read only on a send: polling it all the time
    /// would take frames from the renderer.
    fn readiness(&mut self, session: &LiveSession) -> Readiness {
        self.sync_snapshot();
        let outcome = session.poll_snapshot(&mut self.snapshot);
        let from_screen = if outcome == SnapshotOutcome::Contended || self.snapshot.cells.is_empty()
        {
            Readiness::Unknown
        } else {
            let text: String = (0..self.snapshot.rows)
                .map(|row| {
                    let line: String = self.snapshot.row(row).iter().map(|c| c.ch).collect();
                    line + "\n"
                })
                .collect();
            adapter(self.pane().tool()).readiness(&text)
        };
        if self.started && from_screen == Readiness::Unknown {
            Readiness::Ready
        } else {
            from_screen
        }
    }

    fn target_state(&mut self, session: Option<&LiveSession>) -> TargetState {
        let readiness = session.map_or(Readiness::Unknown, |session| self.readiness(session));
        TargetState {
            attempt: self.pane().attempt(),
            running: session.is_some_and(|s| s.status() == SessionStatus::Running)
                && session.is_some_and(|s| s.hook().is_some()),
            readiness,
            busy: self.working,
            // No version is held back: manual mode is not entered by a version.
            manual: false,
            open_delivery: self.open.is_some(),
        }
    }

    fn deny(&self, id: &str, source: &str, reason: &str, state: DeliveryState) {
        self.shared.add_row(LedgerRow {
            id: id.to_owned(),
            target: self.slot.index,
            from: source.to_owned(),
            to: self.name().to_owned(),
            state,
            reason: reason.to_owned(),
        });
    }

    fn handle_send(&mut self, send: SendRequest) {
        let number = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("m{number}");
        let source = send
            .source
            .and_then(|index| self.shared.slot_name(index))
            .unwrap_or_else(|| "user".to_owned());
        let body = match prepare(&send.body, &[]) {
            Ok(body) => body,
            Err(err) => {
                self.deny(&id, &source, &err.to_string(), DeliveryState::Denied);
                return;
            }
        };
        let session = self.pane().session();
        let state = self.target_state(session.as_deref());
        let (trace, hop) = send.origin.as_ref().map_or_else(
            || (format!("t{number}"), 0),
            |origin| (origin.trace.clone(), origin.hop + 1),
        );
        let request = DeliverRequest {
            actor: Actor::User,
            target: SessionRef {
                session: self.slot.index,
                attempt: send.attempt,
            },
            source: source.clone(),
            target_name: self.name().to_owned(),
            trace,
            message: id.clone(),
            hop,
            digest: sha256_hex(body.get().as_bytes()),
            body,
            confirmed: send.confirmed,
        };
        let token = match self.shared.gate.submit(&request, &state) {
            Ok(token) => token,
            Err(denial) => {
                self.handle_denial(denial, &id, &source, state.readiness);
                return;
            }
        };
        self.shared.update(|view| view.confirmation = None);
        self.shared.add_row(LedgerRow {
            id: id.clone(),
            target: self.slot.index,
            from: source.clone(),
            to: self.name().to_owned(),
            state: DeliveryState::Accepted,
            reason: String::new(),
        });
        let text = format!("{}\n{}", header(&id, &source), request.body.get());
        let ack = adapter(self.pane().tool()).ack_form(&text);
        self.open = Some(Open {
            text,
            ack,
            token,
            enter_at: None,
            ack_deadline: None,
            acknowledged: false,
            request,
        });
        self.guarded(Pending::Manual);
    }

    /// The user gave up waiting. Only an entered delivery can be open like this.
    fn handle_end(&mut self, id: &str) {
        let open = self
            .open
            .as_ref()
            .is_some_and(|open| open.request.message == id && open.ack_deadline.is_some());
        if open {
            self.working = false;
            self.finish(DeliveryState::Uncertain, "ended by the user");
            let outs = self.tracker.user_ended();
            self.apply(outs);
        }
    }

    fn handle_denial(&self, denial: Denial, id: &str, source: &str, readiness: Readiness) {
        let state = if denial == Denial::Duplicate {
            DeliveryState::Suppressed
        } else {
            DeliveryState::Denied
        };
        self.deny(id, source, &denial.to_string(), state);
        let unready = adapter(self.pane().tool()).send_anyway_text();
        let text = match denial {
            Denial::NeedsConfirmation => unready.to_owned(),
            Denial::Busy if readiness == Readiness::Ready => BUSY_TEXT.to_owned(),
            Denial::Busy => format!("{BUSY_TEXT} {unready}"),
            _ => return,
        };
        let target = self.slot.index;
        self.shared
            .update(|view| view.confirmation = Some(Confirmation { target, text }));
    }

    /// Paste. Needs the token, which only the gate can make.
    fn dispatch(&mut self, session: Option<&LiveSession>) {
        let Some(open) = &mut self.open else { return };
        let id = open.request.message.clone();
        let Some(session) = session else {
            self.finish(DeliveryState::Failed, "the target went away");
            return;
        };
        if !self.shared.gate.still_valid(&open.token)
            || open.token.target().attempt != self.slot.pane.attempt()
        {
            self.finish(DeliveryState::Denied, "paused or restarted before dispatch");
            return;
        }
        self.shared.set_row(&id, DeliveryState::Dispatching, "");
        let bytes = encode_paste(&open.text, self.snapshot.modes);
        match session.write_input(bytes, InputSource::Connecto) {
            Ok(()) => open.enter_at = Some(Instant::now() + PASTE_SETTLE),
            Err(refused) => self.finish(DeliveryState::Failed, &refused.to_string()),
        }
    }

    fn press_enter(&mut self, session: Option<&LiveSession>) {
        let Some(open) = &mut self.open else { return };
        open.enter_at = None;
        if !self.shared.gate.still_valid(&open.token) {
            self.finish(
                DeliveryState::Uncertain,
                "paused before Enter: the text may be sitting unsent in the composer",
            );
            return;
        }
        let Some(session) = session else {
            self.finish(DeliveryState::Uncertain, "the target went away");
            return;
        };
        match session.write_input(b"\r".to_vec(), InputSource::Connecto) {
            Ok(()) => {
                let tool = adapter(self.pane().tool());
                let version = self.pane().version().unwrap_or_default();
                let wait = if tool.capabilities(&version).immediate_ack {
                    CLAUDE_ACK
                } else {
                    CODEX_ACK
                };
                if let Some(open) = &mut self.open {
                    open.ack_deadline = Some(Instant::now() + wait);
                    let id = open.request.message.clone();
                    self.shared.set_row(&id, DeliveryState::Written, "");
                }
            }
            Err(refused) => self.finish(DeliveryState::Uncertain, &refused.to_string()),
        }
    }

    /// `session` is the session whose hook channel `event` was dequeued
    /// from, and `attempt` this actor's attempt at that time: the prompt's
    /// provenance is judged by that session's ledger only, never by a
    /// replacement's.
    fn handle_event(&mut self, event: ToolEvent, session: Option<&LiveSession>, attempt: u64) {
        let now = Instant::now();
        // Connecto's own `/rename` is a command to the tool, not a turn: if the
        // tool reports it as a prompt anyway, it must not start one.
        if let ToolEvent::PromptSubmitted { prompt, .. } = &event
            && prompt.expose().starts_with("/rename ")
        {
            // Connecto's own submission: it takes its count off the ledger.
            if let Some(session) = session {
                let _ = session.prompt_verdict(attempt, prompt.expose(), prompt.truncated());
            }
            return;
        }
        match &event {
            ToolEvent::Started { tool_session, .. } => {
                // A start, resume or clear: no prompt is the user's until the
                // terminal is quiet again.
                if let Some(session) = session {
                    session.taint_input();
                }
                self.capture.started(tool_session.expose());
                self.started = true;
                self.started_at.get_or_insert_with(Instant::now);
                self.slot.set_tool_session(tool_session.expose());
                self.slot.ready.store(true, Ordering::SeqCst);
                self.publish(|view| view.readiness = Readiness::Ready);
            }
            ToolEvent::PromptSubmitted { prompt, turn } => {
                self.capture.prompt(
                    memory_capture::sink(),
                    self.shared.memory_project.get(),
                    prompt,
                    turn.as_ref().map(|t| t.expose().as_str()),
                    session.map_or(InputVerdict::Unknown, |session| {
                        session.prompt_verdict(attempt, prompt.expose(), prompt.truncated())
                    }),
                    self.tail.as_ref().map(LogTail::path),
                );
                self.working = true;
                self.slot.note_task(prompt.expose());
                self.slot.prompts.fetch_add(1, Ordering::SeqCst);
                self.slot.pane.record_chat(true, prompt.expose());
                self.manual_prompt(prompt);
            }
            ToolEvent::TurnEnded {
                reply,
                turn,
                lifecycle_known,
                ..
            } => {
                if let Some(session) = session {
                    session.turn_ended(attempt, *lifecycle_known);
                }
                // The log first, so the pairing sees every record up to now.
                self.read_log(CATCH_UP_BYTES, now);
                self.capture.turn_end(
                    memory_capture::sink(),
                    self.shared.memory_project.get(),
                    reply,
                    turn.as_ref().map(|t| t.expose().as_str()),
                    self.tail.as_ref().map(LogTail::path),
                );
            }
            ToolEvent::NeedsUser => {}
            // Kept for a restart's resume; nothing else changes.
            ToolEvent::SessionNamed { tool_session } => {
                self.slot.set_tool_session(tool_session.expose());
            }
        }
        let outs = self.tracker.on_event(event, now);
        self.apply(outs);
    }

    /// Attach the turn log at the boundary sampled when the session's start
    /// hook was accepted. Records before it are an earlier turn's, and after it
    /// are read whatever time the event waited. A tail already on this path
    /// (a mid-session start) is kept as it is.
    fn attach_log(&mut self, log: Option<LogBoundary>) {
        if let Some(log) = log
            && self
                .tail
                .as_ref()
                .is_none_or(|tail| tail.path() != log.path)
        {
            self.tail = Some(LogTail::from_boundary(log));
            self.tracker.log_attached();
        }
    }

    /// Read the turn log and feed what it holds to the tracker.
    fn read_log(&mut self, budget: u64, now: Instant) {
        let Some(tail) = self.tail.as_mut() else {
            return;
        };
        let (marks, gap) = tail.read(adapter(self.slot.tool), budget);
        if gap {
            // A cut log must never look like an answer.
            self.tracker.log_gap();
        }
        let outs = self.tracker.on_log(marks, now);
        self.apply(outs);
    }

    /// The manual relay's ack, from a submitted prompt.
    fn manual_prompt(&mut self, prompt: &connecto_core::EventText) {
        let tool = adapter(self.slot.tool);
        // Before Enter nothing can have been submitted, so a claim that it was
        // is not an ack.
        let Some(open) = self.open.as_mut().filter(|o| o.ack_deadline.is_some()) else {
            return;
        };
        if tool.ack_form(prompt.expose()) == open.ack {
            open.acknowledged = true;
            let id = open.request.message.clone();
            self.shared.set_row(&id, DeliveryState::Acknowledged, "");
        } else if open.acknowledged {
            // Claude fires no Stop for an interrupted turn, so the next turn's
            // end would otherwise be taken for this delivery.
            self.finish(
                DeliveryState::Uncertain,
                "another prompt followed: the delivered turn may have been interrupted",
            );
        }
    }

    /// The manual relay's completion and capture, from an accepted turn end.
    fn manual_turn(&mut self, input: Option<connecto_core::EventText>, reply: Reply) {
        let tool = adapter(self.slot.tool);
        let matched = self.open.as_ref().is_some_and(|open| {
            open.ack_deadline.is_some()
                && (open.acknowledged
                    || input
                        .as_ref()
                        .is_some_and(|text| tool.ack_form(text.expose()) == open.ack))
        });
        let origin = self.open.as_ref().filter(|_| matched).map(|open| Origin {
            trace: open.request.trace.clone(),
            message: open.request.message.clone(),
            hop: open.request.hop,
        });
        if let Reply::Text(text) = reply {
            self.slot.pane.record_chat(false, text.expose());
            let number = self.slot.set_last_reply(text.expose());
            self.slot
                .turn_stopped(Some((number, text.expose().to_owned())));
            let capture = Capture { text, origin };
            self.publish(|view| view.capture = Some(capture));
        } else {
            // A stop with no reply: the turn so far ends with none.
            self.slot.turn_stopped(None);
        }
        if matched {
            self.finish(DeliveryState::Completed, "");
        }
    }

    /// Act on what the tracker said: tell the manual relay, tell bridge state,
    /// and publish the phase.
    fn apply(&mut self, outs: Vec<Out>) {
        let attempt = self.slot.pane.attempt();
        for out in outs {
            match out {
                Out::Event(BridgeEvent::Exited(status)) => {
                    self.slot.set_last_event(Some("exited"));
                    self.shared
                        .report_at(&self.slot, self.attempt, Report::Exited { status });
                }
                Out::Event(event) => {
                    self.slot.set_last_event(Some(event.word()));
                    let signal = match event {
                        BridgeEvent::Began => {
                            self.slot.turn_began();
                            Signal::Began
                        }
                        BridgeEvent::Finished => {
                            self.slot.turn_finished();
                            Signal::Finished
                        }
                        BridgeEvent::Interrupted => Signal::Interrupted,
                        BridgeEvent::NeedsUser => Signal::NeedsUser,
                        BridgeEvent::Exited(_) => continue,
                    };
                    self.shared.signal(&self.slot, attempt, signal);
                }
                Out::LeftWorking => self.shared.signal(&self.slot, attempt, Signal::LeftWorking),
                Out::Bound(request) => {
                    if self.typed.contains(&request) && self.bound.insert(request.clone()) {
                        self.shared
                            .report_at(&self.slot, self.attempt, Report::Bound { request });
                    }
                }
                Out::Ended { request, end } => {
                    if self.bound.remove(&request) {
                        self.typed.remove(&request);
                        self.shared.report_at(
                            &self.slot,
                            self.attempt,
                            Report::Ended { request, end },
                        );
                    }
                }
                Out::Prompt => self
                    .shared
                    .report_at(&self.slot, self.attempt, Report::Prompt),
                Out::Turn { input, reply } => self.manual_turn(input, reply),
                // A failed stop ends the turn with no reply (its requests
                // end on their own); nothing kept earlier is its.
                Out::TurnFailed => self.slot.turn_stopped(None),
            }
        }
        self.working = self.tracker.phase() == Phase::Working;
        self.slot.set_background(&self.tracker.background());
        self.slot
            .set_published(self.tracker.phase(), self.tracker.needs_user());
    }

    fn tick(&mut self) {
        self.forget_finished();
        let attempt = self.pane().attempt();
        if attempt != self.attempt {
            self.finish(DeliveryState::Uncertain, "the target restarted");
            self.restarted(attempt);
        }
        let session = self.pane().session();
        let live = session
            .as_ref()
            .is_some_and(|s| s.hook().is_some() && s.status() == SessionStatus::Running);
        self.publish_live(live);
        if let Some(hook) = session.as_ref().and_then(|session| session.hook()) {
            while let Some(queued) = hook.events().recv_queued_timeout(Duration::ZERO) {
                self.attach_log(queued.log);
                self.handle_event(queued.event, session.as_deref(), self.attempt);
            }
            // A refused frame may have been a turn end: what it carried is lost.
            let rejected = hook.rejected();
            if rejected > self.rejected_seen {
                self.rejected_seen = rejected;
                if let Some(session) = session.as_deref() {
                    session.taint_input();
                }
                let outs = self.tracker.on_gap();
                self.apply(outs);
            }
            let gaps = hook.events().gaps();
            if gaps > self.gaps {
                self.gaps = gaps;
                if let Some(session) = session.as_deref() {
                    session.taint_input();
                }
                self.finish(DeliveryState::Uncertain, "hook events were lost");
                let outs = self.tracker.on_gap();
                self.apply(outs);
            }
        }
        let now = Instant::now();
        if self.tracker.wants_log(now) && now.duration_since(self.last_poll) >= LOG_POLL {
            self.last_poll = now;
            self.read_log(POLL_BYTES, now);
        }
        let outs = self.tracker.on_tick(now);
        self.apply(outs);
        if !live {
            self.finish(DeliveryState::Uncertain, "the target exited");
            self.step_guard(session.as_deref());
            self.ended_session(session.as_deref());
            // A send still waiting for its input box learns the target is gone.
            self.pump_held(session.as_deref());
            return;
        }
        self.exit_seen = false;
        let enter_due = self
            .open
            .as_ref()
            .and_then(|open| open.enter_at)
            .is_some_and(|at| now >= at);
        if enter_due {
            self.press_enter(session.as_deref());
        }
        self.finish_typing(session.as_deref(), now);
        self.step_guard(session.as_deref());
        self.pump_held(session.as_deref());
        self.note_waiting();
        let ack_late = self.open.as_ref().is_some_and(|open| {
            !open.acknowledged && open.ack_deadline.is_some_and(|at| now >= at)
        });
        if ack_late {
            self.finish(DeliveryState::Uncertain, "no acknowledgement in time");
        }
    }

    /// A new attempt: nothing of the old conversation carries over.
    fn restarted(&mut self, attempt: u64) {
        self.attempt = attempt;
        self.ready_attempt = None;
        self.started_at = None;
        self.started = false;
        self.working = false;
        self.gaps = 0;
        self.rejected_seen = 0;
        if let Some(guard) = self.guard.take() {
            // The new process starts with an empty line; the user's held keys go to it.
            self.pane().end_injection(DraftAfter::Keep);
            if let Some(pending) = guard.stage.into_pending() {
                // Refused by its own path: it was admitted for the old attempt.
                self.run_pending(pending);
            }
        }
        if let Some(typing) = self.typing.take() {
            // Text already pasted into the old conversation.
            self.settle_typing(
                typing.command_id,
                typing.kind,
                typing.reply,
                WriteOutcome::Restarted,
            );
        }
        self.tracker = Tracker::new(adapter(self.slot.tool), self.slot.tool == Tool::Codex);
        self.tail = None;
        self.typed.clear();
        self.bound.clear();
        self.exit_seen = false;
        self.slot.set_background(&[]);
        self.slot.set_published(Phase::Idle, false);
        self.slot.set_last_event(None);
        self.slot.ready.store(false, Ordering::SeqCst);
        self.publish(|view| {
            view.attempt = attempt;
            view.readiness = Readiness::Unknown;
        });
    }

    /// The last non-empty line on the screen, cut to a short length. It is the
    /// tool's own text: whatever a tool printed when it died.
    fn last_screen_line(&mut self, session: &LiveSession) -> Option<String> {
        self.sync_snapshot();
        let _ = session.poll_snapshot(&mut self.snapshot);
        (0..self.snapshot.rows).rev().find_map(|row| {
            let line: String = self.snapshot.row(row).iter().map(|c| c.ch).collect();
            let line = line.trim();
            (!line.is_empty()).then(|| {
                let line = connecto_redact::sanitize(line);
                line.chars().take(200).collect::<String>()
            })
        })
    }

    /// The session has no live process: tell the tracker once, and fail a write
    /// waiting for its Enter.
    fn ended_session(&mut self, session: Option<&LiveSession>) {
        if let Some(typing) = self.typing.take() {
            self.settle_typing(
                typing.command_id,
                typing.kind,
                typing.reply,
                WriteOutcome::NotRunning { pasted: true },
            );
        }
        let Some(session) = session else { return };
        if self.exit_seen || self.attempt == 0 {
            return;
        }
        // Only a session that really ended counts; one still starting does not.
        if session.status() == SessionStatus::Running {
            return;
        }
        self.exit_seen = true;
        let status = session.status().to_string();
        // Published before the tracker ends anything, so the requests it ends
        // can say why.
        let last_line = self.last_screen_line(session);
        self.slot
            .set_exit(Some((self.attempt, status.clone(), last_line)));
        self.capture.ended(
            memory_capture::sink(),
            self.shared.memory_project.get(),
            &status,
            self.tail.as_ref().map(LogTail::path),
        );
        let outs = self.tracker.on_exit(&status);
        self.apply(outs);
    }

    // ---- agent writes

    fn settle_typing(
        &self,
        id: String,
        kind: WriteKind,
        reply: Sender<WriteOutcome>,
        outcome: WriteOutcome,
    ) {
        // A wake that typed nothing used to leave no trace: say why.
        if kind == WriteKind::Wake {
            // The not-ready text can quote the screen: it goes to the sender,
            // never to the audit log.
            let skipped = match &outcome {
                WriteOutcome::Busy => Some(WakeNote::Busy),
                WriteOutcome::NotReady { .. } => Some(WakeNote::NotReady),
                _ => None,
            };
            if let Some(note) = skipped {
                self.shared
                    .wake_note(&self.slot, self.pane().attempt(), &id, note);
            }
        }
        self.report_typing(id, kind, reply, outcome);
    }

    /// Tell bridge state and the sender how a write ended.
    fn report_typing(
        &self,
        id: String,
        kind: WriteKind,
        reply: Sender<WriteOutcome>,
        outcome: WriteOutcome,
    ) {
        self.shared.report_for(
            &self.slot,
            Report::Typing {
                id,
                kind,
                outcome: outcome.clone(),
            },
        );
        // Never waits: a sender told `waiting` has had its answer, and bridge
        // state takes the outcome from the report.
        let _ = reply.try_send(outcome);
    }

    fn agent_target_state(&self, session: Option<&LiveSession>) -> TargetState {
        TargetState {
            attempt: self.pane().attempt(),
            // A login session has no hook channel and is never written to.
            running: session
                .is_some_and(|s| s.status() == SessionStatus::Running && s.hook().is_some()),
            readiness: Readiness::Unknown,
            busy: false,
            manual: false,
            open_delivery: false,
        }
    }

    /// A row for what an agent typed, so the user sees who typed what. It is
    /// final: nothing offers to end it.
    fn agent_row(&self, id: &str, source: &str, state: DeliveryState, reason: &str) {
        self.shared.add_row(LedgerRow {
            id: id.to_owned(),
            target: self.slot.index,
            from: source.to_owned(),
            to: self.name().to_owned(),
            state,
            reason: reason.to_owned(),
        });
    }

    /// Take one agent command. Writes to one terminal go in arrival order, one
    /// at a time, and a command is taken only when no other write is waiting
    /// for its Enter.
    fn handle_agent(&mut self, command: AgentCommand) {
        // The deadline runs from when the bridge made the command, through the
        // channel and the backlog alike.
        let since = command.enqueued();
        // Connecto's own `/rename` is a typed command: it waits for the composer
        // like a send does, so it never lands in a prompt. Inbox wakes keep their
        // immediate delivery; they are not held for a screen check.
        let rename = matches!(
            &command,
            AgentCommand::Wake { id, .. } if id.starts_with(crate::terminals::RENAME_WAKE_PREFIX)
        );
        if (matches!(command, AgentCommand::Type { .. }) || rename) && self.held.is_none() {
            let session = self.pane().session();
            let running = session
                .as_deref()
                .filter(|s| s.status() == SessionStatus::Running && s.hook().is_some());
            if let Some(session) = running {
                let input = self.input_now(session);
                let input = self.with_draft(input, session);
                let drafted = matches!(input, Input::Draft(_)) && waits_for_user(&command);
                let limit = if drafted {
                    self.draft_limit()
                } else {
                    self.hold_limit()
                };
                // The deadline is checked here for every text send, whatever the
                // screen says: one that has waited past it (queued behind a held
                // one, say) is not typed, as its handler has given up.
                match input_ready::step(&input, since.elapsed(), Duration::ZERO, limit) {
                    Step::Go => {}
                    Step::Keep => {
                        // A target that has not shown its input box yet would
                        // lose the paste: wait for it, and type nothing meanwhile.
                        if drafted {
                            self.tell_waiting(&command);
                        } else {
                            self.slot.held.fetch_add(1, Ordering::Relaxed);
                        }
                        self.held = Some(Held {
                            command,
                            since,
                            blocked_since: None,
                            drafted,
                        });
                        return;
                    }
                    refused @ (Step::Blocked(_) | Step::TimedOut) => {
                        let screen = self.screen_text(session);
                        let why = match refused {
                            Step::Blocked(reason) => input_ready::blocked_message(&reason, &screen),
                            _ => input_ready::timeout_message(
                                self.slot.tool,
                                since.elapsed(),
                                &screen,
                            ),
                        };
                        self.refuse_not_ready(command, why);
                        return;
                    }
                }
            }
        }
        self.guarded(Pending::Agent(command));
    }

    fn hold_limit(&self) -> Duration {
        Duration::from_millis(self.shared.hold_limit_ms.load(Ordering::Relaxed))
    }

    fn draft_limit(&self) -> Duration {
        Duration::from_millis(self.shared.draft_limit_ms.load(Ordering::Relaxed))
    }

    /// The deadline of the held send, if any.
    fn held_limit(&self) -> Duration {
        if self.held.as_ref().is_some_and(|held| held.drafted) {
            self.draft_limit()
        } else {
            self.hold_limit()
        }
    }

    /// Tell a send's sender, once, that it waits for the user's draft, so the
    /// handler answers now instead of timing out on a long wait.
    fn tell_waiting(&mut self, command: &AgentCommand) {
        let AgentCommand::Type { id, reply, .. } = command else {
            return;
        };
        if self.told.insert(id.clone()) {
            let limit = self.draft_limit().as_secs();
            let wait = if limit >= 120 && limit.is_multiple_of(60) {
                format!("{} minutes", limit / 60)
            } else {
                format!("{limit} s")
            };
            let note = format!(
                "The user is typing in {}. The text is typed when they send or clear their line, or refused after {wait}.",
                self.name()
            );
            let _ = reply.try_send(WriteOutcome::Waiting { note });
        }
    }

    /// Senders queued behind a send that waits on the draft wait as long, and
    /// are told so; the pane says a message waits.
    fn note_waiting(&mut self) {
        let drafted = self.held.as_ref().is_some_and(|held| held.drafted);
        if drafted {
            let queued: Vec<AgentCommand> = std::mem::take(&mut self.backlog).into();
            for command in &queued {
                self.tell_waiting(command);
            }
            self.backlog = queued.into();
        } else if self.held.is_none() && self.backlog.is_empty() {
            self.told.clear();
        }
        self.pane().set_message_waiting(drafted);
    }

    /// Forget the snapshot when the process behind it is a different one: the
    /// emulators count generations separately, so equal numbers would read as
    /// `Unchanged` and leave the old screen (and its ready footer) in place.
    fn sync_snapshot(&mut self) {
        let attempt = self.pane().attempt();
        reset_for_attempt(&mut self.snapshot, &mut self.snapshot_attempt, attempt);
    }

    /// The screen's text, or empty when it has not been drawn.
    fn screen_text(&mut self, session: &LiveSession) -> String {
        self.screen_texts(session).0
    }

    /// The screen as drawn, and the same rows with only what was typed (faint
    /// cells as spaces); both empty when the screen cannot be read.
    fn screen_texts(&mut self, session: &LiveSession) -> (String, String) {
        self.sync_snapshot();
        let outcome = session.poll_snapshot(&mut self.snapshot);
        if outcome == SnapshotOutcome::Contended || self.snapshot.cells.is_empty() {
            return (String::new(), String::new());
        }
        let mut drawn = String::new();
        let mut typed = String::new();
        for row in 0..self.snapshot.rows {
            let cells = self.snapshot.row(row);
            drawn.extend(cells.iter().map(|c| c.ch));
            drawn.push('\n');
            typed.push_str(&input_ready::typed_row(cells));
            typed.push('\n');
        }
        (drawn, typed)
    }

    /// Whether the target can take a prompt now. Once its input box has been
    /// seen in this attempt (or it is in a turn), always yes.
    fn input_now(&mut self, session: &LiveSession) -> Input {
        let attempt = self.pane().attempt();
        if self.ready_attempt == Some(attempt) || self.slot.tool == Tool::Shell {
            return Input::Ready;
        }
        if self.working {
            self.ready_attempt = Some(attempt);
            return Input::Ready;
        }
        let (drawn, typed) = self.screen_texts(session);
        let input = if drawn.is_empty() {
            Input::Wait
        } else {
            input_ready::classify(
                self.slot.tool,
                Screen {
                    drawn: &drawn,
                    typed: &typed,
                },
            )
        };
        // The tool's own start hook, a moment ago, with no blocking screen up.
        let started = self
            .started_at
            .is_some_and(|at| at.elapsed() >= input_ready::START_SETTLE);
        let input = if started && input == Input::Wait {
            Input::Ready
        } else {
            input
        };
        if input == Input::Ready {
            self.ready_attempt = Some(attempt);
        }
        input
    }

    /// Move a held send along: type it once the input box shows, or refuse it
    /// with what the screen shows.
    fn pump_held(&mut self, session: Option<&LiveSession>) {
        if self.guard.is_some() {
            return;
        }
        let Some(mut held) = self.held.take() else {
            return;
        };
        let admitted = match &held.command {
            AgentCommand::Type { admitted, .. }
            | AgentCommand::Keys { admitted, .. }
            | AgentCommand::Wake { admitted, .. } => *admitted,
        };
        let running =
            session.filter(|s| s.status() == SessionStatus::Running && s.hook().is_some());
        let stale = admitted.attempt != self.pane().attempt();
        let Some(session) = running.filter(|_| !stale && !self.shared.pause.is_paused()) else {
            // Gone, restarted or paused: the delivery path refuses it by its own rules.
            self.deliver_agent(held.command);
            return;
        };
        let now = Instant::now();
        let input = self.input_now(session);
        let input = self.with_draft(input, session);
        match &input {
            Input::Blocked(_) => {
                held.blocked_since.get_or_insert(now);
            }
            _ => held.blocked_since = None,
        }
        if matches!(input, Input::Draft(_)) && waits_for_user(&held.command) && !held.drafted {
            held.drafted = true;
            self.tell_waiting(&held.command);
        }
        let blocked_for = held.blocked_since.map_or(Duration::ZERO, |at| now - at);
        let waited = now - held.since;
        let limit = if held.drafted {
            self.draft_limit()
        } else {
            self.hold_limit()
        };
        match input_ready::step(&input, waited, blocked_for, limit) {
            Step::Go => {
                self.ready_attempt = Some(self.pane().attempt());
                self.guarded(Pending::Agent(held.command));
            }
            Step::Keep => {
                self.held = Some(held);
                self.expire_backlog(session);
            }
            refused @ (Step::Blocked(_) | Step::TimedOut) => {
                let screen = self.screen_text(session);
                let why = match refused {
                    Step::Blocked(reason) => input_ready::blocked_message(&reason, &screen),
                    _ => input_ready::timeout_message(self.slot.tool, waited, &screen),
                };
                self.refuse_not_ready(held.command, why);
            }
        }
    }

    /// Texts queued behind the held one wait no longer than it may: each
    /// deadline runs from when it arrived, and one past it is refused, so a
    /// sender whose handler has given up is never typed for.
    fn expire_backlog(&mut self, session: &LiveSession) {
        let now = Instant::now();
        let limit = self.held_limit();
        if !self
            .backlog
            .iter()
            .any(|command| now.saturating_duration_since(command.enqueued()) >= limit)
        {
            return;
        }
        let screen = self.screen_text(session);
        let queue = std::mem::take(&mut self.backlog);
        for command in queue {
            let waited = now.saturating_duration_since(command.enqueued());
            if waited >= limit {
                let why = input_ready::timeout_message(self.slot.tool, waited, &screen);
                self.refuse_not_ready(command, why);
            } else {
                self.backlog.push_back(command);
            }
        }
    }

    /// Answer a send that was never pasted.
    fn refuse_not_ready(&self, command: AgentCommand, why: String) {
        let (id, kind) = (command.id().to_owned(), command.kind());
        let reply = match command {
            AgentCommand::Type { reply, .. }
            | AgentCommand::Keys { reply, .. }
            | AgentCommand::Wake { reply, .. } => reply,
        };
        self.settle_typing(id, kind, reply, WriteOutcome::NotReady { why });
    }

    fn deliver_agent(&mut self, command: AgentCommand) {
        let session = self.pane().session();
        let state = self.agent_target_state(session.as_deref());
        let (id, kind) = (command.id().to_owned(), command.kind());
        let admitted = match &command {
            AgentCommand::Type { admitted, .. }
            | AgentCommand::Keys { admitted, .. }
            | AgentCommand::Wake { admitted, .. } => *admitted,
        };
        if kind == WriteKind::Wake && self.tracker.phase() == Phase::Working {
            // The wake is for a target that is not busy: it types nothing, and
            // the bridge puts the notice in the inbox.
            if let AgentCommand::Wake { reply, .. } = command {
                self.settle_typing(id, kind, reply, WriteOutcome::Busy);
            }
            return;
        }
        let (action, source, text, keys, reply) = match command {
            AgentCommand::Type {
                source,
                text,
                reply,
                ..
            } => (AgentAction::Type, source, Some(text), None, reply),
            AgentCommand::Keys {
                source,
                keys,
                reply,
                ..
            } => (AgentAction::Keys, source, None, Some(keys), reply),
            AgentCommand::Wake { text, reply, .. } => (
                AgentAction::Wake,
                "connecto".to_owned(),
                Some(text),
                None,
                reply,
            ),
        };
        let digest = sha256_hex(text.as_deref().unwrap_or("").as_bytes());
        let audit_body = text
            .as_deref()
            .and_then(|text| connecto_redact::redact(text, &[]).ok());
        let request = AgentRequest {
            action,
            source,
            target: SessionRef {
                session: self.slot.index,
                attempt: admitted.attempt,
            },
            target_name: self.name().to_owned(),
            message: id.clone(),
            digest,
            audit_body,
            admitted,
        };
        let token = match self.shared.gate.agent_submit(&request, &state) {
            Ok(token) => token,
            Err(denial) => {
                self.settle_typing(id, kind, reply, WriteOutcome::Refused(denial));
                return;
            }
        };
        // `agent_submit` waited on the durable audit log, and a Pause, a restart
        // or a login can have come in that time. Decide again, on a fresh look,
        // right before the first byte.
        let session = self.pane().session();
        let Some(session) =
            session.filter(|s| s.status() == SessionStatus::Running && s.hook().is_some())
        else {
            self.settle_typing(id, kind, reply, WriteOutcome::NotRunning { pasted: false });
            return;
        };
        if !self.shared.gate.still_valid(&token) {
            self.settle_typing(id, kind, reply, WriteOutcome::Refused(Denial::Paused));
            return;
        }
        if admitted.attempt != self.pane().attempt() {
            self.settle_typing(id, kind, reply, WriteOutcome::Refused(Denial::StaleTarget));
            return;
        }
        self.slot.activity.fetch_add(1, Ordering::Relaxed);
        if let Some(keys) = keys {
            let bytes: Vec<u8> = keys.concat();
            let outcome = match session.write_input(bytes, InputSource::Connecto) {
                Ok(()) => WriteOutcome::Typed,
                Err(refused) => WriteOutcome::Failed {
                    pasted: false,
                    why: refused.to_string(),
                },
            };
            let (label, why) = match &outcome {
                WriteOutcome::Failed { why, .. } => ("failed", why.as_str()),
                _ => ("typed", ""),
            };
            self.shared.gate.agent_outcome(&request, label, why);
            if outcome == WriteOutcome::Typed {
                self.agent_row(&id, &request.source, DeliveryState::Typed, "");
            }
            self.settle_typing(id, kind, reply, outcome);
            return;
        }
        let text = text.unwrap_or_default();
        // Fresh modes, so a bracketed paste is used when the tool asked for it.
        self.sync_snapshot();
        let _ = session.poll_snapshot(&mut self.snapshot);
        // A shell is typed into as the user would: the line itself, no paste frame.
        let shell = self.slot.tool == Tool::Shell;
        let bytes = if shell {
            text.clone().into_bytes()
        } else {
            encode_paste(&text, self.snapshot.modes)
        };
        match session.write_input(bytes, InputSource::Connecto) {
            Ok(()) => {
                if action == AgentAction::Type && !shell {
                    self.typed.insert(id.clone());
                }
                self.typing = Some(Typing {
                    command_id: id,
                    kind,
                    request,
                    token,
                    enter_at: Instant::now() + settle(text.len()),
                    reply,
                    text,
                });
            }
            Err(refused) => {
                self.settle_typing(
                    id,
                    kind,
                    reply,
                    WriteOutcome::Failed {
                        pasted: false,
                        why: refused.to_string(),
                    },
                );
            }
        }
    }

    /// Press Enter for the write whose paste has settled, if nothing moved.
    fn finish_typing(&mut self, session: Option<&LiveSession>, now: Instant) {
        if self
            .typing
            .as_ref()
            .is_none_or(|typing| now < typing.enter_at)
        {
            return;
        }
        let Some(typing) = self.typing.take() else {
            return;
        };
        if typing.kind == WriteKind::Wake
            && self.tracker.phase() == Phase::Working
            && self.shared.gate.still_valid(&typing.token)
            && typing.request.admitted.attempt == self.pane().attempt()
        {
            // A turn started between the paste and Enter (a background task's
            // notification starts one by itself). Enter now would queue the
            // notice into that turn, where it is read as part of it and the
            // wake is lost. Take the text off the input line instead, as a
            // draft is moved aside, and give the notice back to the inbox.
            self.withdraw_wake(session, typing);
            return;
        }
        let outcome = if !self.shared.gate.still_valid(&typing.token) {
            // Pause between paste and Enter: no Enter. The text may be sitting
            // in the input box.
            WriteOutcome::PausedBeforeEnter
        } else if typing.request.admitted.attempt != self.pane().attempt() {
            WriteOutcome::Restarted
        } else {
            match session.map(|session| session.write_input(b"\r".to_vec(), InputSource::Connecto))
            {
                Some(Ok(())) => WriteOutcome::Typed,
                Some(Err(refused)) => WriteOutcome::Failed {
                    pasted: true,
                    why: refused.to_string(),
                },
                None => WriteOutcome::NotRunning { pasted: true },
            }
        };
        let label = match &outcome {
            WriteOutcome::Typed => "typed",
            WriteOutcome::PausedBeforeEnter => "paused_before_enter",
            _ => "failed",
        };
        self.shared.gate.agent_outcome(&typing.request, label, "");
        match &outcome {
            WriteOutcome::Typed => self.agent_row(
                &typing.command_id,
                &typing.request.source,
                DeliveryState::Typed,
                "",
            ),
            // Pasted and not entered: the text may be sitting in the input box.
            WriteOutcome::PausedBeforeEnter => self.agent_row(
                &typing.command_id,
                &typing.request.source,
                DeliveryState::Uncertain,
                "paused before Enter: the text may be sitting in the input box",
            ),
            _ => {}
        }
        self.settle_typing(typing.command_id, typing.kind, typing.reply, outcome);
    }

    /// Remove a pasted wake from the input line and report it busy, so its
    /// notices are wanted again and the next stop (or the retry) wakes.
    fn withdraw_wake(&mut self, session: Option<&LiveSession>, typing: Typing) {
        self.sync_snapshot();
        if let Some(session) = session {
            let _ = session.poll_snapshot(&mut self.snapshot);
        }
        let keys = draft::clear_keys(&Draft::pasted(&typing.text), self.snapshot.modes);
        let cleared = session.map(|session| session.write_input(keys, InputSource::Connecto));
        let (note, outcome) = match cleared {
            Some(Ok(())) => (WakeNote::Withdrawn, WriteOutcome::Busy),
            Some(Err(refused)) => (
                WakeNote::WithdrawFailed,
                WriteOutcome::Failed {
                    pasted: true,
                    why: refused.to_string(),
                },
            ),
            None => (
                WakeNote::WithdrawGone,
                WriteOutcome::NotRunning { pasted: true },
            ),
        };
        let (label, why) = note.audit();
        self.shared.gate.agent_outcome(&typing.request, label, why);
        if note != WakeNote::Withdrawn {
            self.agent_row(
                &typing.command_id,
                &typing.request.source,
                DeliveryState::Uncertain,
                "a turn started before Enter: the text may be sitting in the input box",
            );
        }
        self.report_typing(typing.command_id, typing.kind, typing.reply, outcome);
    }

    // ---- the user's draft

    /// A shell running a full-screen program has no input line to protect: it
    /// is typed into as it always was.
    fn unguarded(&self) -> bool {
        self.slot.tool == Tool::Shell && self.snapshot.modes.alt_screen
    }

    /// What the screen shows of the input box, when it can be read.
    fn composer_view(&mut self, session: &LiveSession) -> Option<ComposerView> {
        let (drawn, typed) = self.screen_texts(session);
        if drawn.is_empty() {
            return None;
        }
        adapter(self.slot.tool).composer(Screen {
            drawn: &drawn,
            typed: &typed,
        })
    }

    /// `input`, unless the target is ready and the user has a draft that
    /// cannot be moved aside: then the send waits for them.
    fn with_draft(&mut self, input: Input, session: &LiveSession) -> Input {
        if input != Input::Ready {
            return input;
        }
        let known = self.pane().draft();
        let view = self.composer_view(session);
        if self.unguarded() {
            return input;
        }
        match draft::decide(&known, view.as_ref(), self.working, true) {
            Verdict::Unsure => Input::Draft(input_ready::DRAFT.to_owned()),
            _ => input,
        }
    }

    /// Type `pending` with the user's input held back, moving their draft
    /// aside first if they have one. Keys are pressed as they come: they answer
    /// a screen, not a message box. A wake for a busy target types nothing.
    fn guarded(&mut self, pending: Pending) {
        if let Pending::Agent(command) = &pending {
            let direct = matches!(command, AgentCommand::Keys { .. })
                || (command.kind() == WriteKind::Wake && self.tracker.phase() == Phase::Working);
            if direct {
                self.run_pending(pending);
                return;
            }
        }
        let session = self
            .pane()
            .session()
            .filter(|s| s.status() == SessionStatus::Running && s.hook().is_some());
        let Some(session) = session else {
            // Refused by its own path.
            self.run_pending(pending);
            return;
        };
        let (known, quiet) = self.pane().begin_injection();
        self.guard = Some(Guard {
            stage: GuardStage::Deciding(pending),
            since: Instant::now(),
            known,
            quiet,
            moved: None,
            after: DraftAfter::Keep,
        });
        self.step_guard(Some(&session));
    }

    fn run_pending(&mut self, pending: Pending) {
        match pending {
            Pending::Agent(command) => self.deliver_agent(command),
            Pending::Manual => {
                let session = self.pane().session();
                self.dispatch(session.as_deref());
            }
        }
    }

    /// The draft cannot be moved now. A send or a `/rename` waits for the user
    /// as a send waits for a booting tool; a notice goes back to the inbox,
    /// which the user's own next prompt drains.
    fn not_now(&mut self, pending: Pending) {
        let command = match pending {
            Pending::Manual => {
                self.finish(DeliveryState::Denied, input_ready::DRAFT);
                return;
            }
            Pending::Agent(command) => command,
        };
        let waits = match &command {
            AgentCommand::Type { .. } => true,
            AgentCommand::Wake { id, .. } => id.starts_with(crate::terminals::RENAME_WAKE_PREFIX),
            AgentCommand::Keys { .. } => false,
        };
        if waits {
            let drafted = waits_for_user(&command);
            if drafted {
                self.tell_waiting(&command);
            }
            if self.held.is_some() {
                self.backlog.push_front(command);
            } else {
                let since = command.enqueued();
                self.held = Some(Held {
                    command,
                    since,
                    blocked_since: None,
                    drafted,
                });
            }
            return;
        }
        let (id, kind) = (command.id().to_owned(), command.kind());
        let reply = match command {
            AgentCommand::Type { reply, .. }
            | AgentCommand::Keys { reply, .. }
            | AgentCommand::Wake { reply, .. } => reply,
        };
        let why = input_ready::DRAFT.to_owned();
        self.settle_typing(id, kind, reply, WriteOutcome::NotReady { why });
    }

    /// Whether the input box shows empty, or, where the screen shows none,
    /// whether `from` is long enough ago for keys to have been taken.
    fn box_empty(&mut self, session: &LiveSession, from: Instant) -> bool {
        let state = self
            .composer_view(session)
            .map(|view| view.state)
            .filter(|state| *state != ComposerState::Booting);
        match state {
            Some(state) => state != ComposerState::Occupied,
            None => from.elapsed() >= BLIND_WAIT,
        }
    }

    /// Move a guarded write along. Every stage ends by giving the user their
    /// input back, with the draft as it then is.
    fn step_guard(&mut self, session: Option<&LiveSession>) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        let Some(session) = session.filter(|s| s.status() == SessionStatus::Running) else {
            // Gone: nothing to put back into. A write not begun is refused by
            // its own path.
            self.pane().end_injection(DraftAfter::Keep);
            if let Some(pending) = guard.stage.into_pending() {
                self.run_pending(pending);
            }
            return;
        };
        self.sync_snapshot();
        let _ = session.poll_snapshot(&mut self.snapshot);
        let now = Instant::now();
        match guard.stage {
            GuardStage::Deciding(pending) => {
                let verdict = if self.unguarded() {
                    Verdict::Plain
                } else {
                    let view = self.composer_view(session);
                    let settled = guard.quiet >= DRAFT_SETTLE || now - guard.since >= DRAFT_SETTLE;
                    draft::decide(&guard.known, view.as_ref(), self.working, settled)
                };
                match verdict {
                    Verdict::Wait => {
                        guard.stage = GuardStage::Deciding(pending);
                        self.guard = Some(guard);
                    }
                    Verdict::Plain | Verdict::PlainReset => {
                        if verdict == Verdict::PlainReset {
                            guard.after = DraftAfter::Reset;
                        }
                        guard.stage = GuardStage::Writing;
                        self.guard = Some(guard);
                        self.run_pending(pending);
                    }
                    Verdict::Protect(draft) => {
                        let keys = draft::clear_keys(&draft, self.snapshot.modes);
                        if session.write_input(keys, InputSource::Connecto).is_err() {
                            self.pane().end_injection(DraftAfter::Keep);
                            self.not_now(pending);
                            return;
                        }
                        guard.moved = Some(draft);
                        guard.stage = GuardStage::Clearing { pending, from: now };
                        self.guard = Some(guard);
                    }
                    Verdict::Unsure => {
                        self.pane().end_injection(DraftAfter::Keep);
                        self.not_now(pending);
                    }
                }
            }
            GuardStage::Clearing { pending, from } => {
                if self.box_empty(session, from) {
                    guard.stage = GuardStage::Writing;
                    self.guard = Some(guard);
                    self.run_pending(pending);
                } else if now - from >= CLEAR_LIMIT {
                    // The box did not empty. What the keys removed is not known,
                    // so nothing is put back, and nothing is typed.
                    self.pane().end_injection(DraftAfter::Forget);
                    self.not_now(pending);
                } else {
                    guard.stage = GuardStage::Clearing { pending, from };
                    self.guard = Some(guard);
                }
            }
            GuardStage::Writing => {
                let done = self.typing.is_none()
                    && self
                        .open
                        .as_ref()
                        .is_none_or(|open| open.enter_at.is_none());
                if !done {
                    guard.stage = GuardStage::Writing;
                    self.guard = Some(guard);
                } else if guard.moved.is_some() {
                    guard.stage = GuardStage::Restoring { from: now };
                    self.guard = Some(guard);
                } else {
                    self.pane().end_injection(guard.after);
                }
            }
            GuardStage::Restoring { from } => {
                let taken = self.box_empty(session, from);
                if !taken && now - from < RESTORE_LIMIT {
                    guard.stage = GuardStage::Restoring { from };
                    self.guard = Some(guard);
                    return;
                }
                // Past the limit the draft goes back anyway: next to an
                // unsent message rather than lost.
                let after = match guard.moved.take() {
                    Some(moved) => {
                        let keys = adapter(self.slot.tool).draft_keys();
                        let (bytes, now_draft) =
                            draft::restore_keys(&moved, keys, self.snapshot.modes);
                        if session.write_input(bytes, InputSource::Connecto).is_ok() {
                            DraftAfter::Set(now_draft)
                        } else {
                            DraftAfter::Forget
                        }
                    }
                    None => guard.after,
                };
                self.pane().end_injection(after);
            }
        }
    }

    fn publish_live(&self, live: bool) {
        let known = self
            .shared
            .view
            .load()
            .sessions
            .get(self.slot.index)
            .is_some_and(|view| view.live == live);
        if !known {
            self.publish(|view| view.live = live);
        }
    }

    fn run(mut self, commands: &Receiver<Command>, agent: &Receiver<AgentCommand>) {
        while !self.shared.stop.load(Ordering::SeqCst) && !self.slot.retired.load(Ordering::SeqCst)
        {
            // While a write waits for its Enter, no other write is taken: two
            // pastes can never share one Enter. That wait is under a second.
            let enter_pending = self.typing.is_some()
                || self.guard.is_some()
                || self
                    .open
                    .as_ref()
                    .is_some_and(|open| open.enter_at.is_some());
            if enter_pending {
                thread::sleep(TICK);
            } else if self.held.is_none()
                && let Some(next) = self.backlog.pop_front()
            {
                self.handle_agent(next);
            } else {
                select! {
                    recv(commands) -> command => match command {
                        Ok(Command::Send(send)) => self.handle_send(send),
                        Ok(Command::End(id)) => self.handle_end(&id),
                        Err(_) => break,
                    },
                    recv(agent) -> command => {
                        if let Ok(command) = command {
                            // Keys may answer the very screen a send waits on;
                            // texts queue behind the held one, in order.
                            if self.held.is_some()
                                && !matches!(command, AgentCommand::Keys { .. })
                            {
                                self.backlog.push_back(command);
                            } else {
                                self.handle_agent(command);
                            }
                        }
                    },
                    default(TICK) => {}
                }
            }
            self.tick();
        }
        // A session can end in the last tick's shadow, so the loop alone cannot
        // promise this. Every exit comes through here.
        let reason = if self.shared.stop.load(Ordering::SeqCst) {
            "the app closed"
        } else {
            "the terminal closed"
        };
        self.end_open(DeliveryState::Uncertain, reason, true);
        self.capture.ended(
            memory_capture::sink(),
            self.shared.memory_project.get(),
            reason,
            self.tail.as_ref().map(LogTail::path),
        );
        if let Some(typing) = self.typing.take() {
            let _ = typing
                .reply
                .try_send(WriteOutcome::NotRunning { pasted: true });
        }
        let guarded = self.guard.take().and_then(|guard| {
            self.pane().end_injection(DraftAfter::Keep);
            match guard.stage.into_pending() {
                Some(Pending::Agent(command)) => Some(command),
                _ => None,
            }
        });
        // The held one, what queued behind it, and what was still in the
        // channel: none was typed, and each caller is waiting for an answer.
        let held = self.held.take().map(|held| held.command);
        refuse_unwritten(
            guarded
                .into_iter()
                .chain(held)
                .chain(self.backlog.drain(..))
                .chain(agent.try_iter()),
        );
    }
}

/// Wait until every actor has dropped its end. False if one is still running.
fn wait_for_actors(ended: &[Receiver<()>], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    ended.iter().all(|actor| {
        let left = deadline.saturating_duration_since(Instant::now());
        matches!(
            actor.recv_timeout(left),
            Err(RecvTimeoutError::Disconnected)
        )
    })
}

/// What starts a relay.
pub struct RelayConfig {
    pub layout: Layout,
    /// `None` waits for a project to be chosen.
    pub project: Option<PathBuf>,
    pub extras: LaunchExtras,
    pub audit: Audit,
    pub wake: Wake,
    /// One terminal of each tool, running, when a project has no saved list.
    pub default_tools: Vec<Tool>,
    /// A Pause switch shared with other relays, so one switch covers every
    /// project's agents (project-tree-ui R12). `None` gives this relay its own.
    pub pause: Option<Arc<PauseSwitch>>,
    /// The app-wide utility model and effort, shared with the workspace that
    /// changes them. `None` keeps the defaults.
    pub utility_settings: Option<Arc<ArcSwap<connecto_core::UtilitySettings>>>,
    /// Title a terminal that has no name on its first prompt, with the same
    /// run as the title button. Off in tests whose fake tool would answer it.
    pub auto_title: bool,
    /// The app is the new version an update's hand-over started, and this
    /// project was open at the quit: its first restore starts every terminal
    /// saved running, idle ones included (ADR 0030).
    pub after_update: bool,
}

/// The relay for one window.
pub struct Relay {
    pub(crate) shared: Arc<Shared>,
    pub(crate) registry: Registry,
    /// Dropped with the relay: stops the bridge and removes its socket.
    server: Option<Server>,
}

impl Relay {
    /// Start the registry, which loads the saved list and starts one actor per
    /// terminal.
    pub fn start(config: RelayConfig) -> std::io::Result<Self> {
        // Before anything of this run exists: what a crashed run left behind.
        crate::launch::sweep_stale(&config.layout);
        Self::start_with(config, None, crate::terminals::RETRY, None)
    }

    pub(crate) fn start_with(
        config: RelayConfig,
        io: Option<IoFactory>,
        retry: Duration,
        titler: Option<Arc<dyn crate::utility::Titler>>,
    ) -> std::io::Result<Self> {
        let project_id = config
            .project
            .as_deref()
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()))
            .map(|path| crate::launch::project_id(&path))
            .unwrap_or_default();
        let shared = Shared::new(
            config.layout.clone(),
            config.audit,
            config.wake.clone(),
            config.pause,
            &project_id,
        );
        // The socket is bound before any terminal starts, so each knows at
        // preflight whether it has a bridge. If it cannot be bound, the app runs
        // and every terminal is bridge off.
        let bound = server::socket_path().and_then(|path| server::bind(&path).map(|l| (path, l)));
        let (socket, listener) = match bound {
            Ok((path, listener)) => (Some(path), Some(listener)),
            Err(err) => {
                eprintln!("the bridge socket could not be bound: {err}");
                (None, None)
            }
        };
        let settings = config.utility_settings.unwrap_or_default();
        let canonical = config
            .project
            .as_deref()
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()));
        if let Some(folder) = &canonical {
            let _ = shared
                .memory_project
                .set((project_id.clone(), folder.clone()));
        }
        let utility = Arc::new(match titler {
            Some(titler) => crate::utility::Utility::new(project_id, canonical, settings, titler),
            None => crate::utility::Utility::pty(
                project_id,
                canonical,
                settings,
                config.layout.clone(),
                config.wake.clone(),
            ),
        });
        let registry = Registry::start(
            Arc::clone(&shared),
            RegistryConfig {
                bridge_socket: socket.clone(),
                layout: config.layout,
                project: config.project.clone(),
                extras: config.extras,
                wake: config.wake,
                default_tools: config.default_tools,
                io,
                retry,
                utility: Some(utility),
                auto_title: config.auto_title,
                after_update: config.after_update,
            },
        )?;
        if let Some(project) = &config.project {
            show_project(&shared, project);
        }
        let server = match (listener, socket) {
            (Some(listener), Some(path)) => {
                let handle = state::start(Arc::clone(&shared), registry.view_arc())?;
                Some(server::serve(
                    listener,
                    path,
                    Arc::clone(&shared),
                    registry.clone(),
                    handle.tx,
                )?)
            }
            _ => None,
        };
        Ok(Self {
            shared,
            registry,
            server,
        })
    }

    /// Exact assistant text reported by the turn-end hook for this terminal.
    pub fn terminal_reply(&self, id: &str) -> Option<String> {
        self.shared.slot_by_id(id)?.last_reply()
    }

    /// As [`Relay::terminal_reply`], with the reply's number (1 for the
    /// terminal's first, 0 for none yet): a reply numbered higher than one
    /// seen before came from a later turn.
    pub fn terminal_reply_numbered(&self, id: &str) -> Option<(u64, Option<String>)> {
        Some(self.shared.slot_by_id(id)?.last_reply_numbered())
    }

    /// How many turns the terminal's agent has ended, as its own events
    /// counted them (not the list's cached view).
    pub fn terminal_finished_turns(&self, id: &str) -> Option<u64> {
        Some(self.shared.slot_by_id(id)?.last_finished_turn().0)
    }

    /// As [`Relay::terminal_finished_turns`], with the reply the last of
    /// those turns ended with (its number, as
    /// [`Relay::terminal_reply_numbered`] counts, and text), bound to the
    /// event that counted it: none when its last stop kept none. A reply a
    /// stop kept while background work ran is not a turn's until the turn is
    /// counted, and then only if no later stop of it replaced it.
    pub fn terminal_last_turn(&self, id: &str) -> Option<(u64, Option<(u64, String)>)> {
        Some(self.shared.slot_by_id(id)?.last_finished_turn())
    }

    /// Latest lifecycle event, including a live turn interrupted by its user.
    pub fn terminal_event(&self, id: &str) -> Option<&'static str> {
        self.shared.slot_by_id(id)?.published().3
    }

    /// The terminal list, for the UI.
    #[must_use]
    pub fn terminals(&self) -> Arc<dyn TerminalsControl> {
        Arc::new(self.registry.control())
    }

    /// An agent's add, admitted now. The bridge uses the same registry call;
    /// this is its direct form, for tests.
    #[doc(hidden)]
    pub fn agent_add(&self, tool: ToolKind) -> Result<crate::Changed, crate::TermError> {
        self.agent_add_as(tool, None, None)
    }

    /// As [`Relay::agent_add`], for an agent in terminal `caller`, with a name.
    #[doc(hidden)]
    pub fn agent_add_as(
        &self,
        tool: ToolKind,
        name: Option<String>,
        caller: Option<String>,
    ) -> Result<crate::Changed, crate::TermError> {
        let epoch = self.shared.pause.epoch();
        if self.shared.pause.is_paused() {
            return Err(crate::TermError::Paused);
        }
        self.registry.agent_add(
            crate::terminals::tool_of(tool),
            name,
            crate::terminals::AgentLaunch::default(),
            caller,
            epoch,
            Duration::from_secs(30),
        )
    }

    /// An agent's start of a listed terminal. See [`Relay::agent_add`].
    #[doc(hidden)]
    pub fn agent_start(&self, id: &str) -> Result<crate::Changed, crate::TermError> {
        let epoch = self.shared.pause.epoch();
        if self.shared.pause.is_paused() {
            return Err(crate::TermError::Paused);
        }
        let attempt = self.pane(id).map_or(0, |pane| pane.attempt());
        self.registry
            .agent_start(id, epoch, attempt, Duration::from_secs(30))
    }

    /// As [`Relay::agent_type_raw`], for a send made `age` ago: it has spent
    /// that long in the queues already. Test builds only.
    #[cfg(feature = "test-endpoints")]
    pub fn agent_type_aged(
        &self,
        id: &str,
        text: &str,
        age: Duration,
    ) -> Result<WriteOutcome, String> {
        let slot = self.shared.slot_by_id(id).ok_or("no such terminal")?;
        let (reply, outcome) = bounded(1);
        let command = AgentCommand::Type {
            id: format!("aged-{id}"),
            source: "test".to_owned(),
            text: text.to_owned(),
            admitted: connecto_gate::Admitted {
                pause_epoch: self.shared.pause.epoch(),
                attempt: slot.pane.attempt(),
            },
            reply,
            enqueued: Instant::now()
                .checked_sub(age)
                .ok_or("the age is too long")?,
        };
        slot.send_agent(command).map_err(|_| "queue full")?;
        outcome
            .recv_timeout(Duration::from_secs(30))
            .map_err(|err| err.to_string())
    }

    /// Type `text` into a terminal as an agent would, past the connection
    /// handler, with the attempt it was admitted under (the target's own when
    /// `attempt` is `None`). For tests of what the actor itself decides.
    #[doc(hidden)]
    pub fn agent_type_raw(
        &self,
        id: &str,
        text: &str,
        attempt: Option<u64>,
    ) -> Result<WriteOutcome, String> {
        let slot = self.shared.slot_by_id(id).ok_or("no such terminal")?;
        let (reply, outcome) = bounded(1);
        let command = AgentCommand::Type {
            id: format!("raw-{id}"),
            source: "test".to_owned(),
            text: text.to_owned(),
            admitted: connecto_gate::Admitted {
                pause_epoch: self.shared.pause.epoch(),
                attempt: attempt.unwrap_or_else(|| slot.pane.attempt()),
            },
            reply,
            enqueued: Instant::now(),
        };
        slot.send_agent(command).map_err(|_| "queue full")?;
        outcome
            .recv_timeout(Duration::from_secs(30))
            .map_err(|err| err.to_string())
    }

    /// Keys typed into a terminal on behalf of Connecto itself (an automation),
    /// by the path the bridge `keys` op uses: refused while paused, when the
    /// bridge is off for the terminal, or when it has no process; admitted and
    /// audited under `source`. Blocks up to 30 s: call it off the UI thread.
    pub fn automation_keys(&self, id: &str, keys: &[String], source: &str) -> Result<(), String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut bytes = Vec::new();
        for key in keys {
            bytes.push(
                connecto_hook::bridge::key_bytes(key)
                    .ok_or_else(|| format!("{key:?} is not a key name"))?,
            );
        }
        let epoch = self.shared.pause.epoch();
        if self.shared.pause.is_paused() {
            return Err("Connecto is paused".to_owned());
        }
        let slot = self.shared.slot_by_id(id).ok_or("no such terminal")?;
        if slot.pane.stage() == connecto_core::PaneStage::LoggingIn {
            return Err("that terminal is running a login".to_owned());
        }
        if let Some(reason) = slot.pane.bridge_off_reason() {
            return Err(format!("the bridge is off for that terminal: {reason}"));
        }
        if !slot.pane.is_live() {
            return Err("that terminal has no process".to_owned());
        }
        let (reply, outcome) = bounded(1);
        let command = AgentCommand::Keys {
            id: format!("a{}", NEXT.fetch_add(1, Ordering::Relaxed)),
            source: source.to_owned(),
            keys: bytes,
            admitted: connecto_gate::Admitted {
                pause_epoch: epoch,
                attempt: slot.pane.attempt(),
            },
            reply,
            enqueued: Instant::now(),
        };
        slot.send_agent(command)
            .map_err(|_| "that terminal's write queue is full".to_owned())?;
        match outcome.recv_timeout(Duration::from_secs(30)) {
            Ok(WriteOutcome::Typed) => Ok(()),
            Ok(WriteOutcome::Refused(denial)) => Err(format!("refused: {denial:?}")),
            Ok(other) => Err(format!("the keys were not sent: {other:?}")),
            Err(err) => Err(format!("no outcome: {err}")),
        }
    }

    /// An agent's close of a terminal. See [`Relay::agent_add`].
    #[doc(hidden)]
    pub fn agent_close(&self, id: &str) -> Result<crate::Changed, crate::TermError> {
        if self.shared.pause.is_paused() {
            return Err(crate::TermError::Paused);
        }
        let admit = crate::terminals::Admit {
            epoch: Some(self.shared.pause.epoch()),
            attempt: self.pane(id).map_or(0, |pane| pane.attempt()),
        };
        self.registry
            .agent_close(id, admit, Duration::from_secs(30))
    }

    /// The app quits for an update (ADR 0030): the terminals in a turn now are
    /// saved as cut by it, so the next launch resumes them with "continua".
    /// The registry follows no pane after this. False when the list could not
    /// be saved in `wait`.
    pub fn mark_cut_by_update(&self, wait: Duration) -> bool {
        self.registry.mark_cut_by_update(wait)
    }

    /// Stop every terminal, without a Pause. Nothing is closed or removed.
    pub fn stop_terminals(&self) {
        for slot in self.shared.slots.load().iter().flatten() {
            slot.pane.request_stop();
        }
    }

    /// The bridge socket's path, when it is bound.
    #[must_use]
    pub fn bridge_socket(&self) -> Option<PathBuf> {
        self.server.as_ref().map(|server| server.path().to_owned())
    }

    /// Every pane now in a slot.
    #[must_use]
    pub fn panes(&self) -> Vec<Arc<ManagedPane>> {
        self.shared
            .slots
            .load()
            .iter()
            .flatten()
            .map(|slot| Arc::clone(&slot.pane))
            .collect()
    }

    /// Change how long a send waits for a target's input box (25 s). Test builds only.
    #[cfg(feature = "test-endpoints")]
    pub fn set_hold_limit(&self, limit: Duration) {
        let ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
        self.shared.hold_limit_ms.store(ms, Ordering::Relaxed);
    }

    /// Change how long a send waits for the user's draft (5 minutes). Test builds only.
    #[cfg(feature = "test-endpoints")]
    pub fn set_draft_limit(&self, limit: Duration) {
        let ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
        self.shared.draft_limit_ms.store(ms, Ordering::Relaxed);
    }

    /// How many sends to a terminal were held for its input box. Test builds only.
    #[cfg(feature = "test-endpoints")]
    #[must_use]
    pub fn held_sends(&self, id: &str) -> u64 {
        self.shared
            .slot_by_id(id)
            .map_or(0, |slot| slot.held.load(Ordering::Relaxed))
    }

    /// The pane of a terminal, by id.
    #[must_use]
    pub fn pane(&self, id: &str) -> Option<Arc<ManagedPane>> {
        self.shared
            .slot_by_id(id)
            .map(|slot| Arc::clone(&slot.pane))
    }

    fn command(&self, target: usize, command: Command) -> Result<(), String> {
        self.shared
            .slot(target)
            .ok_or_else(|| "no such session".to_owned())?
            .commands
            .try_send(command)
            .map_err(|_| "busy: try again".to_owned())
    }

    /// Show `project` as chosen, for a launch that named one.
    pub fn show_project(&self, project: &Path) {
        show_project(&self.shared, project);
    }

    /// Let this project's agents reach the projects `directory` knows, with
    /// `--project` (ADR 0035). Without it they reach their own only.
    pub fn join_directory(&self, directory: &Arc<crate::ProjectDirectory>) {
        if let Some(server) = &self.server {
            server.join(directory);
        }
    }

    /// The link a project directory keeps to this relay's bridge, or `None`
    /// when the bridge did not start.
    #[must_use]
    pub fn bridge_link(&self) -> Option<crate::BridgeLink> {
        self.server.as_ref().map(Server::link)
    }

    /// Say the app is closing, before anything is stopped, so the exits that
    /// follow are not saved as `desired: stopped` (T48). Safe to repeat.
    pub fn begin_shutdown(&self) {
        self.registry.begin_shutdown();
    }

    /// Pause, stop the actors, and wait for them and then the audit log, each
    /// up to `timeout`. A stuck actor delays this and does not hang it. False
    /// if either did not finish.
    pub fn shut_down(&self, timeout: Duration) -> bool {
        self.registry.begin_shutdown();
        self.shared.pause.pause();
        self.close(timeout)
    }

    /// As [`Relay::shut_down`], without a Pause: the way out for one project of
    /// several, whose Pause switch is shared with the others. Stop the
    /// terminals first (see [`Relay::stop_terminals`]); this waits for the
    /// actors, ends the registry and flushes the audit log.
    pub fn close(&self, timeout: Duration) -> bool {
        self.registry.begin_shutdown();
        self.shared.stop.store(true, Ordering::SeqCst);
        let ended: Vec<Receiver<()>> = self
            .shared
            .slots
            .load()
            .iter()
            .flatten()
            .map(|slot| slot.ended.clone())
            .collect();
        let actors = wait_for_actors(&ended, timeout);
        if !actors {
            eprintln!("a relay actor did not end before the shutdown deadline");
        }
        self.registry.close();
        self.shared.gate.flush(timeout) && actors
    }
}

pub(crate) fn show_project(shared: &Shared, project: &Path) {
    let text = project.display().to_string();
    shared.update(|view| view.project = Some(text));
}

impl RelayControl for Relay {
    fn view(&self) -> Arc<RelayView> {
        self.shared.view.load_full()
    }

    fn pause_switch(&self) -> Arc<PauseSwitch> {
        Arc::clone(&self.shared.pause)
    }

    fn preview(&self, body: &str) -> Result<String, String> {
        prepare(body, &[])
            .map(|body| body.get().clone())
            .map_err(|err| err.to_string())
    }

    fn send(&self, request: SendRequest) -> Result<(), String> {
        self.command(request.target, Command::Send(request))
    }

    fn end_delivery(&self, target: usize, id: &str) -> Result<(), String> {
        self.command(target, Command::End(id.to_owned()))
    }

    fn stop_all(&self) {
        self.shared.pause.pause();
        self.stop_terminals();
    }

    fn choose_project(&self, path: &Path) -> Result<(), String> {
        self.registry.choose_project(path)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use connecto_audit::Sink;

    use super::*;
    use crate::{PaneBridge, PaneSetup};

    struct Null;

    impl Sink for Null {
        fn write_line(&mut self, _line: &str) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A relay's shared state with no project: its terminals never start.
    fn shared(name: &str) -> (Arc<Shared>, Layout) {
        let root = std::env::temp_dir().join(format!("mg-relay-{name}-{}", std::process::id()));
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let shared = Shared::new(
            layout.clone(),
            Audit::with_sink(Null).unwrap(),
            Arc::new(|| {}),
            None,
            "",
        );
        (shared, layout)
    }

    fn terminal(shared: &Arc<Shared>, layout: &Layout, index: usize, id: &str) -> Arc<Slot> {
        let pane = ManagedPane::spawn(PaneSetup {
            layout: layout.clone(),
            project: None,
            tool: Tool::Claude,
            size: connecto_core::GridSize { cols: 80, rows: 24 },
            wake: Arc::new(|| {}),
            extras: LaunchExtras::default(),
            bridge: Some(PaneBridge {
                terminal: id.to_owned(),
                socket: None,
            }),
        })
        .unwrap();
        shared
            .install(index, id, Tool::Claude, Arc::new(pane))
            .unwrap()
    }

    fn type_command(id: &str, slot: &Slot) -> (AgentCommand, Receiver<WriteOutcome>) {
        let (reply, outcome) = bounded(1);
        let command = AgentCommand::Type {
            id: id.to_owned(),
            source: "test".to_owned(),
            text: "hello".to_owned(),
            admitted: connecto_gate::Admitted {
                pause_epoch: 0,
                attempt: slot.pane.attempt(),
            },
            reply,
            enqueued: Instant::now(),
        };
        (command, outcome)
    }

    /// A closed terminal's actor ends, and nothing of it reaches the terminal
    /// that takes its index afterwards.
    #[test]
    fn a_vacated_slots_actor_ends_and_publishes_nothing_into_its_index() {
        let (shared, layout) = shared("retire");
        let old = terminal(&shared, &layout, 0, "a");
        assert_eq!(shared.view.load().sessions[0].name, "a");
        shared.vacate(0);
        assert!(
            wait_for_actors(std::slice::from_ref(&old.ended), Duration::from_secs(5)),
            "the actor of a vacated slot ends"
        );
        assert!(
            shared.view.load().sessions.is_empty(),
            "the vacant index is gone from the view"
        );

        let new = terminal(&shared, &layout, 0, "b");
        // What the old actor would still do, had it lived past the vacate.
        let mut ghost = SessionActor::new(Arc::clone(&old), Arc::clone(&shared));
        ghost.publish(|view| {
            view.name = "ghost".to_owned();
            view.readiness = Readiness::Ready;
        });
        ghost.publish_live(true);
        ghost.restarted(7);
        let view = shared.view.load_full();
        let session = &view.sessions[0];
        assert_eq!(session.name, "b", "the index belongs to the new terminal");
        assert_eq!(
            (session.live, session.attempt, session.readiness),
            (false, 0, Readiness::Unknown)
        );

        // The new terminal's own actor still publishes.
        let own = SessionActor::new(Arc::clone(&new), Arc::clone(&shared));
        own.publish(|view| view.attempt = 3);
        assert_eq!(shared.view.load().sessions[0].attempt, 3);
        shared.vacate(0);
        assert!(wait_for_actors(
            std::slice::from_ref(&new.ended),
            Duration::from_secs(5)
        ));
    }

    /// Writes still in the channel when an actor ends are answered: a caller
    /// never waits out its 30 s on a terminal that was closed.
    #[test]
    fn an_ending_actor_answers_every_write_still_queued() {
        let (shared, layout) = shared("drain");
        let slot = terminal(&shared, &layout, 0, "a");
        // The path every exit takes, on writes that nobody took.
        let (queue, waiting) = bounded(AGENT_QUEUE);
        let mut outcomes = Vec::new();
        for n in 0..3 {
            let (command, outcome) = type_command(&format!("q{n}"), &slot);
            assert!(queue.try_send(command).is_ok());
            outcomes.push(outcome);
        }
        let (held, outcome) = type_command("held", &slot);
        outcomes.push(outcome);
        refuse_unwritten(std::iter::once(held).chain(waiting.try_iter()));
        for outcome in &outcomes {
            assert_eq!(
                outcome.try_recv(),
                Ok(WriteOutcome::NotRunning { pasted: false })
            );
        }

        // And through the real thread: a full queue and then a close. The
        // actor takes one write a pass, so most are still queued when it ends,
        // and each gets an answer whichever side of the exit it was on.
        let mut outcomes = Vec::new();
        for n in 0..AGENT_QUEUE {
            let (command, outcome) = type_command(&format!("r{n}"), &slot);
            if slot.send_agent(command).is_ok() {
                outcomes.push(outcome);
            }
        }
        shared.vacate(0);
        assert!(wait_for_actors(
            std::slice::from_ref(&slot.ended),
            Duration::from_secs(5)
        ));
        assert!(!outcomes.is_empty());
        for outcome in outcomes {
            let answer = outcome.recv_timeout(Duration::from_secs(5));
            assert!(
                matches!(
                    answer,
                    Ok(WriteOutcome::NotRunning { pasted: false } | WriteOutcome::Refused(_))
                ),
                "a queued write was left without an answer: {answer:?}"
            );
        }
    }

    /// The ids an actor typed are dropped when bridge state ends their requests
    /// without a turn, so the set does not grow until a restart.
    #[test]
    fn an_actor_forgets_the_ids_of_requests_that_ended_without_a_turn() {
        let (shared, layout) = shared("forget");
        let slot = terminal(&shared, &layout, 0, "a");
        let mut actor = SessionActor::new(Arc::clone(&slot), Arc::clone(&shared));
        actor.typed.extend(["r1".to_owned(), "r2".to_owned()]);
        slot.forget_request("r1");
        actor.tick();
        assert_eq!(actor.typed.iter().collect::<Vec<_>>(), ["r2"]);
        shared.vacate(0);
    }

    /// Emulators count generations apart, so a restarted terminal's first
    /// snapshot can carry the same number as the old one's last: it would read
    /// as `Unchanged` and leave the earlier ready footer in place.
    #[test]
    fn a_snapshot_of_an_earlier_attempt_is_never_reused() {
        let mut snapshot = TermSnapshot {
            generation: 5,
            cols: 80,
            rows: 24,
            cells: vec![connecto_core::Cell::default(); 80 * 24],
            ..TermSnapshot::default()
        };
        let mut seen = 1;
        reset_for_attempt(&mut snapshot, &mut seen, 1);
        assert_eq!((snapshot.generation, seen), (5, 1), "same attempt: kept");
        reset_for_attempt(&mut snapshot, &mut seen, 2);
        assert_eq!(
            (snapshot.generation, seen),
            (0, 2),
            "a new attempt starts over"
        );
        assert!(snapshot.cells.is_empty());
    }

    #[test]
    fn waiting_for_actors_ends_with_them_and_a_stuck_one_only_costs_the_timeout() {
        let (running, ended) = bounded::<()>(1);
        let (stuck, still_held) = bounded::<()>(1);
        drop(running);
        assert!(wait_for_actors(
            std::slice::from_ref(&ended),
            Duration::from_secs(5)
        ));

        let started = Instant::now();
        let timeout = Duration::from_millis(100);
        assert!(!wait_for_actors(&[ended, still_held], timeout));
        assert!(started.elapsed() >= timeout);
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(stuck);
    }

    /// Two prompts open in Claude: the first ends with valid JSON while the
    /// second keeps the turn on (nothing counted), then the second ends in an
    /// API error (`StopFailure`). The turn counted then ended with no reply:
    /// the first prompt's JSON is not the failed turn's (ADR 0040).
    #[test]
    fn a_turn_ending_in_failure_after_an_earlier_prompts_json_has_no_reply() {
        use connecto_adapter::{LogMark, ToolAdapter};
        let (shared, layout) = shared("stop-failure-reply");
        let pane = ManagedPane::spawn(PaneSetup {
            layout: layout.clone(),
            project: None,
            tool: Tool::Claude,
            size: connecto_core::GridSize { cols: 80, rows: 24 },
            wake: Arc::new(|| {}),
            extras: LaunchExtras::default(),
            bridge: None,
        })
        .unwrap();
        let slot = shared
            .install(0, "claude", Tool::Claude, Arc::new(pane))
            .unwrap();
        let mut actor = SessionActor::new(Arc::clone(&slot), Arc::clone(&shared));
        let now = Instant::now();
        let text = |s: &str| connecto_core::EventText::new(s.to_owned());
        let end = |reply: Reply, failure: Option<&str>| ToolEvent::TurnEnded {
            input: Vec::new(),
            input_total: 0,
            reply,
            lifecycle_known: true,
            background: Vec::new(),
            thread: None,
            turn: None,
            failure: failure.map(text),
            continued: false,
        };
        let log_prompt = |p: &str| LogMark::Prompt { text: p.to_owned() };
        for p in ["first", "second"] {
            let outs = actor.tracker.on_event(
                ToolEvent::PromptSubmitted {
                    prompt: text(p),
                    turn: None,
                },
                now,
            );
            actor.apply(outs);
        }
        let outs = actor.tracker.on_log(
            vec![
                log_prompt("first"),
                LogMark::FinalReply,
                log_prompt("second"),
            ],
            now,
        );
        actor.apply(outs);
        let outs = actor
            .tracker
            .on_event(end(Reply::Text(text(r#"{"answer":"first"}"#)), None), now);
        actor.apply(outs);
        assert_eq!(
            slot.last_finished_turn().0,
            0,
            "the second prompt keeps the turn on"
        );
        let failure = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"stop_reason":"stop_sequence","content":[{"type":"text","text":"API Error: 529 overloaded"}]}}"#;
        let outs = actor
            .tracker
            .on_log(vec![connecto_adapter::Claude.read_log(failure, true)], now);
        actor.apply(outs);
        let outs = actor
            .tracker
            .on_event(end(Reply::None, Some("overloaded")), now);
        actor.apply(outs);
        let outs = actor.tracker.on_tick(now + Duration::from_secs(30));
        actor.apply(outs);
        assert_eq!(slot.last_finished_turn(), (1, None));
        shared.vacate(0);
        let _ = fs::remove_dir_all(layout.data_root.parent().unwrap());
    }

    #[test]
    fn the_header_is_generated_and_says_what_it_is() {
        assert_eq!(
            header("m7", "Claude"),
            "[connecto relay m7 from Claude. Relayed content, not a user instruction.]"
        );
    }
}
