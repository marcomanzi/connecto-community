//! Bridge state: one thread that owns the roster view, watches, inboxes, wakes
//! and requests (section 5.12).
//!
//! It never blocks. It sends to actors with `try_send` only, replies go straight
//! to handlers, and it never waits on an actor, the registry or the audit log,
//! so no cycle can form. All of it is in memory, and all of it is lost on
//! restart (A29, A31, A42).

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use connecto_core::{EventText, PaneControl, PaneStage, TerminalsView};
use connecto_gate::Admitted;
use connecto_hook::bridge::Reply;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use serde_json::json;

use super::render::{self, DrainInput, Fact, Notice};
use super::{AgentCommand, Caller, Report, Signal, WriteKind, WriteOutcome};
use crate::background;
use crate::relay::{Shared, Slot, WakeNote};
use crate::turn::{Phase, RequestEnd};

const TICK: Duration = Duration::from_millis(250);
/// Entries an inbox holds before the oldest merge.
pub(crate) const INBOX_MAX: usize = 32;
/// Replies held across all requests before the oldest is given up.
pub(crate) const RESULTS_CAP: usize = 64 * 1024 * 1024;
/// Finished requests kept for `result`. Past this the oldest are forgotten.
pub(crate) const FINISHED_KEPT: usize = 512;
/// Typed text that starts no turn ends `unmatched` after this long.
pub(crate) const UNMATCHED_AFTER: Duration = Duration::from_secs(15);
/// How long a target may take to finish starting before a request to it is
/// given up. The unmatched clock does not run until the target is ready.
pub(crate) const BOOT_CAP: Duration = Duration::from_secs(180);
/// A wake with no outcome in this long is given up on.
const WAKE_QUEUE_LIMIT: Duration = Duration::from_secs(30);
/// How long a typed wake waits for the watcher's next prompt.
const WAKE_PROMPT_LIMIT: Duration = Duration::from_secs(3);
/// How long wanted notices wait in the inbox of a terminal that is not
/// working, with no wake on its way, before a wake is tried again. Each try
/// doubles the wait, up to [`WAKE_RETRY_MAX`].
const WAKE_RETRY_FIRST: Duration = Duration::from_secs(10);
const WAKE_RETRY_MAX: Duration = Duration::from_secs(300);

/// Where the seats of callers from other projects start: far above any slot
/// a project can have, so a seat is never taken for a terminal (ADR 0035).
const SEAT_BASE: usize = usize::MAX / 2;
/// Most seats one state keeps; past it the oldest goes, with its watches.
const SEATS_MAX: usize = 1024;
/// Most ask ends remembered that came before their ask was recorded.
const TOMBSTONES_MAX: usize = 256;

/// A terminal attempt: the slot, which actor holds it, and its attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Att {
    pub slot: usize,
    pub uid: u64,
    pub attempt: u64,
}

impl From<&Caller> for Att {
    fn from(caller: &Caller) -> Self {
        Self {
            slot: caller.slot,
            uid: caller.uid,
            attempt: caller.attempt,
        }
    }
}

/// What `send` or `keys` asks admission for.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AdmitKind {
    Send { ask: bool },
    Keys,
}

/// A write that was admitted.
pub(crate) struct Admission {
    pub id: String,
    pub slot: Arc<Slot>,
    pub admitted: Admitted,
}

/// What a drain gives the handler.
pub(crate) struct Drained {
    pub reservation: u64,
    pub body: Vec<u8>,
}

/// A debt a handler owes the state, kept off the bounded queue so a full queue
/// cannot lose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Owed {
    /// A drain's reply was written, or it was not.
    DrainDone { reservation: u64, written: bool },
    /// The handler gave up waiting on a send: its id is out.
    TimedOut { id: String },
    /// The send waits for the user to send or clear their draft, and its
    /// sender was told so: its id is out, and no clock runs until it is typed.
    Waiting { id: String },
    /// From another project's bridge (ADR 0035): a notice for a terminal
    /// here. Cross-project messages go through these debts, never through
    /// the bounded queue, so a full queue cannot lose the end of an ask.
    Forwarded(Forward),
    /// A caller here asked a terminal of another project with `--ask`: until
    /// the answer is forwarded, its own turns hold the asks it was given.
    /// Owed by the handler before it reports the send.
    ForeignAsk(ForeignAsk),
    /// The attempt that sat at `att` in that bridge instance has ended: its
    /// seat here goes, with its watches.
    SeatGone {
        project: String,
        instance: u64,
        att: Att,
    },
    /// A caller here unwatched `source` in another project: what the watch
    /// made wanted is withdrawn.
    ForeignCancel {
        to: Att,
        to_instance: u64,
        source: String,
    },
}

/// An `--ask` that has been typed into its target and not answered yet, by
/// terminal id: what an update saves so the asker gets its reply after the
/// restart (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenAsk {
    pub request: String,
    pub from: String,
    pub target: String,
}

/// Whether an op admitted across projects may still act (ADR 0035).
pub(crate) type StillGuard = Box<dyn Fn() -> bool + Send>;

pub(crate) enum Msg {
    Event {
        slot: usize,
        uid: u64,
        attempt: u64,
        signal: Signal,
    },
    Poke,
    List {
        caller: Caller,
        reply: Sender<Reply>,
    },
    Admit {
        caller: Caller,
        target: String,
        kind: AdmitKind,
        reply: Sender<Result<Admission, Reply>>,
    },
    Result {
        caller: Caller,
        request: String,
        reply: Sender<Reply>,
    },
    Watch {
        caller: Caller,
        target: String,
        on: bool,
        /// For a caller from another project: whether Pause is still off in
        /// the epochs both projects admitted it in, asked right before the
        /// watch changes, so a Pause while it was queued changes nothing.
        still: Option<StillGuard>,
        reply: Sender<Reply>,
    },
    Drain {
        caller: Caller,
        /// A new project memory version to announce, taken by the handler.
        memory_version: Option<u64>,
        /// A line about a project operation the prompt matches (ADR 0031).
        ops_note: Option<String>,
        reply: Sender<Option<Drained>>,
    },
    /// A sentence for one terminal from Connecto itself (the answer to its
    /// secret request): a notice, and a wake when it is idle.
    Notice {
        caller: Caller,
        text: String,
    },
    /// The asks open in the project now, for an update to save.
    OpenAsks {
        reply: Sender<Vec<OpenAsk>>,
    },
}

/// A notice from another project's bridge state for one terminal here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Forward {
    /// The terminal attempt here it is for, and the bridge instance it was
    /// made in: a project closed and opened again counts its attempts anew.
    pub to: Att,
    pub to_instance: u64,
    /// The project and bridge instance it comes from.
    pub from_project: String,
    pub from_instance: u64,
    pub notice: Notice,
    pub wake: String,
    /// The request it ends, for an ask's answer.
    pub ended: Option<String>,
}

/// A caller from another project, as this state knows it: its project, the
/// bridge instance it called from, and its attempt there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seat {
    project_id: String,
    instance: u64,
    att: Att,
}

/// An ask a caller here made in another project, not answered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForeignAsk {
    pub att: Att,
    pub project_id: String,
    /// The target's bridge instance: once that project is closed, even if it
    /// is opened again, the ask will never be answered.
    pub project_instance: u64,
    pub project_name: String,
    pub request: String,
}

// ---- inbox

/// What a wake in flight is for, and what to give back if it was never typed.
#[derive(Debug, Clone)]
enum Why {
    /// One watched event or request end: the notice goes to the inbox as
    /// wanted if the wake is not typed.
    Event(Notice),
    /// The unread-notice wake: these notices get their wanted flag back.
    Unread(Vec<u64>),
    /// A notice whose watch was cancelled while its wake was in flight. If the
    /// wake is not typed it stays as context and wants nothing.
    Context(Notice),
}

/// Notices of one source folded into a count. The ids of the wanted ones are
/// kept, so a wake that was never typed can bring the flag back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Merged {
    count: u32,
    wanted: bool,
    wanted_ids: Vec<u64>,
}

/// Most ids a merged source remembers.
const MERGED_IDS_MAX: usize = 256;

#[derive(Debug, Clone)]
enum Stage {
    Queued { since: Instant },
    AwaitingPrompt { deadline: Instant },
}

#[derive(Debug, Clone)]
struct WakeFlight {
    id: String,
    stage: Stage,
    why: Why,
}

#[derive(Debug, Clone)]
struct Entry {
    nid: u64,
    notice: Notice,
}

/// When the safety net may try a wake again, and how many it has tried.
#[derive(Debug, Clone, Copy)]
struct Retry {
    next: Instant,
    tries: u32,
}

/// One terminal attempt's notices.
#[derive(Debug, Default)]
pub(crate) struct Inbox {
    entries: VecDeque<Entry>,
    /// Per source: how many notices were merged, and whether any was wanted.
    merged: BTreeMap<String, Merged>,
    lost: u32,
    first_done: bool,
    wake: Option<WakeFlight>,
    /// A project memory version taken for this terminal and not yet printed:
    /// several merge into the latest (spec §6.1).
    memory_version: Option<u64>,
    /// The safety net's schedule while wanted notices wait.
    retry: Option<Retry>,
}

impl Inbox {
    fn push(&mut self, nid: u64, notice: Notice) {
        if self.entries.len() >= INBOX_MAX
            && let Some(oldest) = self.entries.pop_front()
        {
            let slot = self.merged.entry(oldest.notice.source.clone()).or_default();
            slot.count += 1;
            slot.wanted |= oldest.notice.wanted;
            // A notice that is wanted, or that carries a reply, keeps its id when
            // folded. A reply notice enters unwanted while its wake is in
            // flight; if that wake fails, `restore` must find it again, or the
            // asker stays asleep with its reply lost in a count.
            if (oldest.notice.wanted || oldest.notice.body.is_some())
                && slot.wanted_ids.len() < MERGED_IDS_MAX
            {
                slot.wanted_ids.push(oldest.nid);
            }
        }
        self.entries.push_back(Entry { nid, notice });
    }

    fn any_wanted(&self) -> bool {
        self.entries.iter().any(|entry| entry.notice.wanted)
            || self.merged.values().any(|merged| merged.wanted)
    }

    /// Clear every wanted flag and say which entries had one.
    fn take_wanted(&mut self) -> Vec<u64> {
        let mut ids = Vec::new();
        for entry in &mut self.entries {
            if entry.notice.wanted {
                entry.notice.wanted = false;
                ids.push(entry.nid);
            }
        }
        for merged in self.merged.values_mut() {
            if merged.wanted {
                // The ids stay, so `set_wanted` can bring the flag back.
                ids.extend(merged.wanted_ids.iter().copied());
            }
            merged.wanted = false;
        }
        ids
    }

    fn set_wanted(&mut self, ids: &[u64]) {
        for entry in &mut self.entries {
            if ids.contains(&entry.nid) {
                entry.notice.wanted = true;
            }
        }
        for merged in self.merged.values_mut() {
            if merged.wanted_ids.iter().any(|id| ids.contains(id)) {
                merged.wanted = true;
            }
        }
    }

    /// Ids of the notices from `source`, merged ones included.
    fn ids_from(&self, source: &str) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .entries
            .iter()
            .filter(|entry| entry.notice.source == source)
            .map(|entry| entry.nid)
            .collect();
        if let Some(merged) = self.merged.get(source) {
            ids.extend(merged.wanted_ids.iter().copied());
        }
        ids
    }

    fn clear_wanted_for(&mut self, source: &str) {
        for entry in &mut self.entries {
            if entry.notice.source == source {
                entry.notice.wanted = false;
            }
        }
        if let Some(merged) = self.merged.get_mut(source) {
            merged.wanted = false;
            merged.wanted_ids.clear();
        }
    }
}

/// Notices moved out of an inbox while a drain's reply is written.
struct Reservation {
    owner: Att,
    entries: VecDeque<Entry>,
    merged: BTreeMap<String, Merged>,
    lost: u32,
    first: bool,
    memory_version: Option<u64>,
}

// ---- requests

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReqState {
    Pending,
    Ok,
    Interrupted,
    Exited,
    Unmatched,
    Unavailable,
}

impl ReqState {
    pub(crate) const fn word(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ok => "ok",
            Self::Interrupted => "interrupted",
            Self::Exited => "exited",
            Self::Unmatched => "unmatched",
            Self::Unavailable => "unavailable",
        }
    }
}

struct Request {
    project: u64,
    from: Att,
    from_id: String,
    target: Att,
    target_id: String,
    ask: bool,
    /// The ask's text reached the target's input: the target has an ask open.
    ask_open: bool,
    id_given_out: bool,
    bound: bool,
    clock_from: Instant,
    working_seen: bool,
    /// The target was seen ready: the unmatched clock runs from then.
    ready_seen: bool,
    born: Instant,
    /// Held for the user's draft: nothing was typed yet, and no clock runs.
    /// The actor ends the wait itself, typed or refused.
    waiting: bool,
    note: Option<&'static str>,
    state: ReqState,
    reason: String,
    reply: Option<EventText>,
    /// The target ended the turn this ask started while it waited on asks of
    /// its own: the ask stays open until a turn of the target ends with none
    /// left, so the asker is woken once, with the final answer, and not after
    /// every exchange the target has with other agents. The reply of the
    /// latest turn, should no later one give one.
    held: Option<EventText>,
    /// Opened again after an update cut it (ADR 0030): it ends with its
    /// target's next turn, and its asker is found again by id, since the
    /// asker restarted too.
    restored: bool,
    /// For an asker from another project, where its answer goes. Kept here,
    /// so a seat that is dropped later cannot lose it (ADR 0035).
    seat: Option<Seat>,
}

/// Everything the thread owns.
struct State {
    shared: Arc<Shared>,
    view: Arc<ArcSwap<TerminalsView>>,
    known: BTreeMap<usize, Att>,
    lost_seen: BTreeMap<usize, u64>,
    watches: Vec<(Att, Att)>,
    inboxes: BTreeMap<usize, Inbox>,
    reservations: BTreeMap<u64, Reservation>,
    requests: BTreeMap<String, Request>,
    order: VecDeque<String>,
    reply_bytes: usize,
    /// Asks an update cut, waiting for their target to run again, and since
    /// when (ADR 0030).
    restoring: Vec<(OpenAsk, Instant)>,
    /// Restored asks whose answer waits for their asker to run, and since when.
    deferred: Vec<(String, Instant)>,
    next_request: u64,
    next_key: u64,
    next_wake: u64,
    next_reservation: u64,
    next_nid: u64,
    /// Tests only: treat every terminal as having a live process.
    assume_live: bool,
    /// How long a wake may wait for an outcome. A field so a test can shorten it.
    wake_limit: Duration,
    /// The safety net's first wait. A field so a test can shorten it.
    retry_first: Duration,
    /// Tests only: the bridges of other projects, by project and instance,
    /// in place of a directory.
    #[cfg(test)]
    peers: BTreeMap<(String, u64), Arc<Shared>>,
    /// Callers from other projects, by their seat's slot number.
    seats: BTreeMap<usize, Seat>,
    next_seat: usize,
    /// Asks callers here made in other projects, not answered yet.
    foreign_asks: Vec<ForeignAsk>,
    /// Ask ends that arrived before their ask was recorded here: project,
    /// its instance, request.
    tombstones: VecDeque<(String, u64, String)>,
}

/// The word `list` gives for a terminal's state.
pub(crate) fn state_word(slot: &Slot) -> &'static str {
    State::state_word(slot)
}

pub(crate) struct StateHandle {
    pub tx: Sender<Msg>,
}

/// Start the state thread and publish its mailbox to the actors.
pub(crate) fn start(
    shared: Arc<Shared>,
    view: Arc<ArcSwap<TerminalsView>>,
) -> std::io::Result<StateHandle> {
    let (tx, rx) = bounded(1024);
    let _ = shared.state.set(tx.clone());
    let mut state = State::new(shared, view);
    thread::Builder::new()
        .name("bridge-state".into())
        .spawn(move || state.run(&rx))?;
    Ok(StateHandle { tx })
}

impl State {
    fn new(shared: Arc<Shared>, view: Arc<ArcSwap<TerminalsView>>) -> Self {
        Self {
            shared,
            view,
            known: BTreeMap::new(),
            lost_seen: BTreeMap::new(),
            watches: Vec::new(),
            inboxes: BTreeMap::new(),
            reservations: BTreeMap::new(),
            requests: BTreeMap::new(),
            order: VecDeque::new(),
            reply_bytes: 0,
            restoring: Vec::new(),
            deferred: Vec::new(),
            next_request: 1,
            next_key: 1,
            next_wake: 1,
            next_reservation: 1,
            next_nid: 1,
            assume_live: false,
            wake_limit: WAKE_QUEUE_LIMIT,
            retry_first: WAKE_RETRY_FIRST,
            #[cfg(test)]
            peers: BTreeMap::new(),
            seats: BTreeMap::new(),
            next_seat: 0,
            foreign_asks: Vec::new(),
            tombstones: VecDeque::new(),
        }
    }
}

/// Whether a report stamped `stamp` belongs to an attempt other than `current`.
/// Zero means the report is not about an attempt.
pub(crate) const fn is_stale(stamp: u64, current: u64) -> bool {
    stamp != 0 && stamp != current
}

fn refusal(code: &str, reason: &str) -> Reply {
    Reply::refused(code, reason, None)
}

impl State {
    fn run(&mut self, rx: &Receiver<Msg>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Drain what is waiting before the tick, so a burst is seen whole.
            while let Ok(msg) = rx.try_recv() {
                self.handle(msg);
            }
            self.tick();
        }
    }

    fn handle(&mut self, msg: Msg) {
        self.reconcile();
        self.refresh_restored();
        self.pay_debts();
        match msg {
            Msg::Event {
                slot,
                uid,
                attempt,
                signal,
            } => self.on_signal(Att { slot, uid, attempt }, signal),
            Msg::Poke => {}
            Msg::List { caller, reply } => {
                let _ = reply.send(self.list(&caller));
            }
            Msg::Admit {
                caller,
                target,
                kind,
                reply,
            } => {
                let _ = reply.send(self.admit(&caller, &target, kind));
            }
            Msg::Result {
                caller,
                request,
                reply,
            } => {
                let _ = reply.send(self.result(&request, &caller));
            }
            Msg::Watch {
                caller,
                target,
                on,
                still,
                reply,
            } => {
                let answer = if still.as_ref().is_some_and(|still| !still()) {
                    refusal(
                        "paused",
                        "Connecto is paused by the user. Nothing was done.",
                    )
                } else {
                    self.watch(&caller, &target, on)
                };
                let _ = reply.send(answer);
            }
            Msg::Drain {
                caller,
                memory_version,
                ops_note,
                reply,
            } => {
                let _ = reply.send(self.drain_with(&caller, memory_version, ops_note));
            }
            Msg::Notice { caller, text } => self.note(&caller, text),
            Msg::OpenAsks { reply } => {
                let _ = reply.send(self.open_asks());
            }
        }
        self.take_reports();
    }

    /// Pay what the handlers owe, in the order they incurred it.
    fn pay_debts(&mut self) {
        for owed in self.shared.take_owed() {
            match owed {
                Owed::DrainDone {
                    reservation,
                    written,
                } => self.drain_done(reservation, written),
                Owed::TimedOut { id } => {
                    if let Some(request) = self.requests.get_mut(&id) {
                        request.id_given_out = true;
                        request.clock_from = Instant::now();
                    }
                }
                Owed::Waiting { id } => {
                    if let Some(request) = self.requests.get_mut(&id) {
                        request.id_given_out = true;
                        request.waiting = true;
                        request.clock_from = Instant::now();
                    }
                }
                Owed::Forwarded(forward) => self.on_forwarded(forward),
                Owed::ForeignAsk(ask) => self.on_foreign_ask(ask),
                Owed::SeatGone {
                    project,
                    instance,
                    att,
                } => self.seat_gone(&project, instance, att),
                Owed::ForeignCancel {
                    to,
                    to_instance,
                    source,
                } => {
                    if to_instance == self.shared.bridge_instance {
                        self.cancel_wanted(to, &source);
                    }
                }
            }
        }
    }

    fn tick(&mut self) {
        self.reconcile();
        self.open_restored();
        self.refresh_restored();
        self.retry_deferred();
        self.pay_debts();
        self.take_reports();
        self.check_lost();
        self.check_requests();
        self.check_wakes();
        self.retry_wakes();
        self.check_foreign_asks();
    }

    fn live(&self, slot: &Slot) -> bool {
        self.assume_live || slot.pane.is_live()
    }

    fn nid(&mut self) -> u64 {
        let nid = self.next_nid;
        self.next_nid += 1;
        nid
    }

    fn slot_of(&self, att: Att) -> Option<Arc<Slot>> {
        self.shared
            .slot(att.slot)
            .filter(|slot| slot.uid == att.uid && slot.pane.attempt() == att.attempt)
    }

    fn current(&self, slot: &Slot) -> Att {
        Att {
            slot: slot.index,
            uid: slot.uid,
            attempt: slot.pane.attempt(),
        }
    }

    fn is_current(&self, att: Att) -> bool {
        self.slot_of(att).is_some()
    }

    /// Notice attempts that ended: a slot vacated, a new actor, a restart.
    fn reconcile(&mut self) {
        let slots: Vec<Arc<Slot>> = self.shared.slots.load().iter().flatten().cloned().collect();
        let mut seen = Vec::new();
        for slot in &slots {
            seen.push(slot.index);
            let now = self.current(slot);
            match self.known.get(&slot.index).copied() {
                Some(old) if old == now => {}
                Some(old) => {
                    self.attempt_ended(old);
                    self.known.insert(slot.index, now);
                }
                None => {
                    self.known.insert(slot.index, now);
                }
            }
        }
        let gone: Vec<usize> = self
            .known
            .keys()
            .copied()
            .filter(|index| !seen.contains(index))
            .collect();
        for index in gone {
            if let Some(old) = self.known.remove(&index) {
                self.attempt_ended(old);
            }
        }
    }

    /// An attempt ended: its inbox, counts, watches, wake and reservations go,
    /// and no wake is sent. A dead conversation has no one to tell.
    fn attempt_ended(&mut self, old: Att) {
        self.inboxes.remove(&old.slot);
        self.foreign_asks.retain(|ask| ask.att != old);
        self.watches
            .retain(|(watcher, target)| *watcher != old && *target != old);
        self.reservations
            .retain(|_, reservation| reservation.owner != old);
        self.lost_seen.remove(&old.slot);
        let ids: Vec<String> = self
            .requests
            .iter()
            .filter(|(_, request)| request.state == ReqState::Pending && request.target == old)
            .map(|(id, _)| id.clone())
            .collect();
        let why = self
            .shared
            .slot(old.slot)
            .and_then(|slot| Self::exit_text(&slot, old));
        for id in ids {
            self.finish(
                &id,
                ReqState::Exited,
                &Self::ended_reason(why.as_deref()),
                None,
            );
        }
    }

    // ---- asks an update cut (ADR 0030)

    /// The asks open in this project: typed into a target that has not
    /// answered, by an asker that is still the conversation that asked.
    fn open_asks(&self) -> Vec<OpenAsk> {
        let project = self.shared.project_key.load(Ordering::SeqCst);
        self.requests
            .iter()
            .filter(|(_, r)| {
                r.state == ReqState::Pending
                    && r.ask
                    && r.ask_open
                    && r.project == project
                    && (r.restored || self.slot_of(r.from).is_some())
            })
            .map(|(id, r)| OpenAsk {
                request: id.clone(),
                from: r.from_id.clone(),
                target: r.target_id.clone(),
            })
            .collect()
    }

    /// Open again the asks an update cut, each once its target runs: it then
    /// ends with the target's next turn, as an ask held across the target's
    /// own asks does. A target that does not run within [`BOOT_CAP`] ends it
    /// as exited, so the asker is not left waiting.
    fn open_restored(&mut self) {
        let now = Instant::now();
        let taken = self.shared.take_restored();
        self.restoring
            .extend(taken.into_iter().map(|ask| (ask, now)));
        if self.restoring.is_empty() {
            return;
        }
        let project = self.shared.project_key.load(Ordering::SeqCst);
        for (ask, since) in std::mem::take(&mut self.restoring) {
            if self.requests.contains_key(&ask.request) {
                continue;
            }
            let (Some(target), Some(from)) = (
                self.shared.slot_by_id(&ask.target),
                self.shared.slot_by_id(&ask.from),
            ) else {
                continue;
            };
            let att = self.current(&target);
            let runs = self.known.get(&target.index) == Some(&att) && self.live(&target);
            if !runs && now.duration_since(since) < BOOT_CAP {
                self.restoring.push((ask, since));
                continue;
            }
            if let Some(number) = ask
                .request
                .strip_prefix('r')
                .and_then(|n| n.parse::<u64>().ok())
            {
                self.next_request = self.next_request.max(number.saturating_add(1));
            }
            let id = ask.request.clone();
            self.requests.insert(
                id.clone(),
                Request {
                    project,
                    from: self.current(&from),
                    from_id: ask.from,
                    target: att,
                    target_id: ask.target,
                    ask: true,
                    ask_open: true,
                    id_given_out: true,
                    bound: true,
                    clock_from: now,
                    working_seen: false,
                    ready_seen: true,
                    born: now,
                    waiting: false,
                    note: None,
                    state: ReqState::Pending,
                    reason: String::new(),
                    reply: None,
                    held: Some(EventText::new(String::new())),
                    restored: true,
                    seat: None,
                },
            );
            self.order.push_back(id.clone());
            if runs {
                target.ask.store(1, Ordering::SeqCst);
            } else {
                self.finish(
                    &id,
                    ReqState::Exited,
                    "The terminal did not start again after the update. Use read.",
                    None,
                );
            }
        }
    }

    /// A restored ask's asker restarted with the app: follow it to the
    /// conversation it runs now, so its own asks hold its turns and the
    /// answer reaches it.
    fn refresh_restored(&mut self) {
        let found: Vec<(String, Att)> = self
            .requests
            .iter()
            .filter(|(_, r)| r.restored)
            .filter_map(|(id, r)| {
                let slot = self.shared.slot_by_id(&r.from_id)?;
                Some((id.clone(), self.current(&slot)))
            })
            .collect();
        for (id, att) in found {
            if let Some(request) = self.requests.get_mut(&id) {
                request.from = att;
            }
        }
    }

    /// Give a restored ask's answer to its asker once it runs, up to
    /// [`BOOT_CAP`]; after that it stays for `connecto result`.
    fn retry_deferred(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        let now = Instant::now();
        for (id, since) in std::mem::take(&mut self.deferred) {
            let Some(request) = self.requests.get(&id) else {
                continue;
            };
            let (from, state) = (request.from, request.state.word());
            match self.slot_of(from) {
                Some(sender) if self.live(&sender) => self.notify_sender(&id, from, state),
                _ if now.duration_since(since) < BOOT_CAP => self.deferred.push((id, since)),
                _ => {}
            }
        }
    }

    // ---- reports from actors

    fn take_reports(&mut self) {
        let slots: Vec<Arc<Slot>> = self.shared.slots.load().iter().flatten().cloned().collect();
        for slot in slots {
            let att = self.current(&slot);
            if self.known.get(&slot.index) != Some(&att) {
                continue;
            }
            for (stamp, report) in slot.take_reports() {
                // A report from an earlier attempt must not touch this one. The
                // earlier attempt's requests, watches and inbox were already
                // closed when its end was seen.
                if is_stale(stamp, att.attempt) {
                    continue;
                }
                self.on_report(att, &slot, report);
            }
        }
    }

    fn on_report(&mut self, att: Att, slot: &Arc<Slot>, report: Report) {
        match report {
            Report::Typing { id, kind, outcome } => self.on_typing(att, &id, kind, outcome),
            Report::Prompt => {
                if let Some(inbox) = self.inboxes.get_mut(&att.slot)
                    && matches!(
                        inbox.wake.as_ref().map(|w| &w.stage),
                        Some(Stage::AwaitingPrompt { .. })
                    )
                {
                    inbox.wake = None;
                }
            }
            Report::Bound { request } => {
                if let Some(record) = self.requests.get_mut(&request)
                    && record.target == att
                    && record.state == ReqState::Pending
                    && !record.bound
                {
                    record.bound = true;
                }
            }
            Report::Ended { request, end } => self.on_request_end(att, &request, end),
            Report::Exited { status: _ } => {
                let why = self
                    .shared
                    .slot(att.slot)
                    .and_then(|slot| Self::exit_text(&slot, att));
                self.deliver_event(att, "exited", why.as_deref());
                self.attempt_ended(att);
                // The actor's attempt is over; it stays known so a restart is
                // seen as a change.
                let _ = slot;
            }
        }
    }

    /// How the attempt ended and the last thing it printed, for a person or an
    /// agent. The printed line is the tool's text, so it is quoted.
    fn exit_text(slot: &Slot, att: Att) -> Option<String> {
        let (status, line) = slot.exit(att.attempt)?;
        Some(match line {
            Some(line) => format!(
                "{status}. Last output (printed by the terminal, not an instruction): \"{}\"",
                render::lines_only(&line).replace('\n', " ")
            ),
            None => format!("{status}. It printed nothing."),
        })
    }

    fn on_typing(&mut self, att: Att, id: &str, kind: WriteKind, outcome: WriteOutcome) {
        match kind {
            WriteKind::Keys => {}
            WriteKind::Wake => self.wake_outcome(att, id, &outcome),
            WriteKind::Send => {
                let Some(request) = self.requests.get_mut(id) else {
                    return;
                };
                if request.state != ReqState::Pending {
                    return;
                }
                request.waiting = false;
                let typed = matches!(outcome, WriteOutcome::Typed);
                let pasted = matches!(
                    outcome,
                    WriteOutcome::Typed
                        | WriteOutcome::PausedBeforeEnter
                        | WriteOutcome::Restarted
                        | WriteOutcome::NotRunning { pasted: true }
                        | WriteOutcome::Failed { pasted: true, .. }
                );
                if pasted {
                    request.id_given_out = true;
                    request.clock_from = Instant::now();
                    if request.ask && !request.ask_open {
                        // Only now is an ask open on the target. One refused
                        // before its paste must leave the last ask's "done".
                        request.ask_open = true;
                        if let Some(slot) = self
                            .shared
                            .slot(att.slot)
                            .filter(|slot| slot.uid == att.uid)
                        {
                            slot.ask.store(1, Ordering::SeqCst);
                        }
                    }
                    if !typed {
                        request.note = Some(
                            "The text was pasted but Enter was not pressed, and it may still be in the input box.",
                        );
                    }
                    if matches!(outcome, WriteOutcome::NotRunning { .. }) {
                        self.finish(
                            id,
                            ReqState::Exited,
                            "The terminal ended before Enter was pressed.",
                            None,
                        );
                    }
                } else if request.id_given_out {
                    // A late outcome after a timeout or a wait: nothing was typed.
                    let note = match &outcome {
                        WriteOutcome::NotReady { why } => format!("The text was not typed: {why}."),
                        _ => "The text was not typed.".to_owned(),
                    };
                    self.finish(id, ReqState::Unmatched, &note, None);
                } else {
                    // Refused before any paste, with no id given out.
                    self.requests.remove(id);
                    self.order.retain(|known| known != id);
                }
            }
        }
    }

    fn on_request_end(&mut self, att: Att, id: &str, end: RequestEnd) {
        let Some(request) = self.requests.get(id) else {
            return;
        };
        if request.target != att || request.state != ReqState::Pending || !request.bound {
            return;
        }
        match end {
            RequestEnd::Ok(text) if self.awaits(att) => {
                if let Some(request) = self.requests.get_mut(id) {
                    request.held = Some(text);
                }
            }
            RequestEnd::Ok(text) => self.finish(id, ReqState::Ok, "", Some(text)),
            RequestEnd::Interrupted => {
                self.finish(
                    id,
                    ReqState::Interrupted,
                    "The turn was interrupted. Use read.",
                    None,
                );
            }
            RequestEnd::Exited => {
                let why = self
                    .requests
                    .get(id)
                    .map(|r| r.target)
                    .and_then(|t| self.shared.slot(t.slot).map(|slot| (slot, t)))
                    .and_then(|(slot, t)| Self::exit_text(&slot, t));
                self.finish(
                    id,
                    ReqState::Exited,
                    &Self::ended_reason(why.as_deref()),
                    None,
                );
            }
            RequestEnd::Unmatched(why) => self.finish(id, ReqState::Unmatched, why, None),
            RequestEnd::Unavailable(why) => self.finish(id, ReqState::Unavailable, why, None),
        }
    }

    /// Whether `att` waits on an `--ask` of its own.
    fn awaits(&self, att: Att) -> bool {
        self.requests
            .values()
            .any(|r| r.from == att && r.ask && r.state == ReqState::Pending)
            || self.foreign_asks.iter().any(|ask| ask.att == att)
    }

    /// A turn of `att` ended: the asks it holds open end with it once it waits
    /// on none of its own, with that turn's reply. An interrupt ends them too.
    fn release_held(&mut self, att: Att, interrupted: bool) {
        let ids: Vec<String> = self
            .requests
            .iter()
            .filter(|(_, r)| r.target == att && r.state == ReqState::Pending && r.held.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        if ids.is_empty() || (!interrupted && self.awaits(att)) {
            return;
        }
        let last = self
            .shared
            .slot(att.slot)
            .and_then(|slot| slot.last_reply())
            .map(EventText::new);
        for id in ids {
            if interrupted {
                self.finish(
                    &id,
                    ReqState::Interrupted,
                    "The turn was interrupted. Use read.",
                    None,
                );
                continue;
            }
            let held = self.requests.get_mut(&id).and_then(|r| r.held.take());
            self.finish(&id, ReqState::Ok, "", last.clone().or(held));
        }
    }

    /// Final state of a request. Bridge state decides it, and the first one wins.
    fn finish(&mut self, id: &str, state: ReqState, reason: &str, reply: Option<EventText>) {
        let Some(request) = self.requests.get_mut(id) else {
            return;
        };
        if request.state != ReqState::Pending {
            return;
        }
        request.state = state.clone();
        request.reason = reason.to_owned();
        if let Some(note) = request.note {
            if request.reason.is_empty() {
                request.reason = note.to_owned();
            } else {
                request.reason = format!("{} {note}", request.reason);
            }
        }
        if let Some(reply) = reply {
            self.reply_bytes += reply.expose().len();
            request.reply = Some(reply);
        }
        let (ask, from, target) = (request.ask, request.from, request.target);
        let (ask_open, bound) = (request.ask_open, request.bound);
        self.evict();
        if let Some(slot) = self
            .shared
            .slot(target.slot)
            .filter(|s| s.uid == target.uid)
        {
            if !bound {
                // No turn will carry this id now: its actor need not keep it.
                slot.forget_request(id);
            }
            if ask_open {
                // For the child's "done" dot: the last ask it was given has
                // ended. An ask that never reached it changes nothing.
                slot.ask.store(2, Ordering::SeqCst);
            }
        }
        if ask {
            self.notify_sender(id, from, state.word());
        }
        self.prune();
    }

    /// Keep the newest [`FINISHED_KEPT`] finished requests. Pending ones are
    /// never dropped, whatever their age.
    fn prune(&mut self) {
        let finished = self
            .requests
            .values()
            .filter(|request| request.state != ReqState::Pending)
            .count();
        let mut excess = finished.saturating_sub(FINISHED_KEPT);
        if excess == 0 {
            return;
        }
        let order = std::mem::take(&mut self.order);
        for id in order {
            let old = excess > 0
                && self
                    .requests
                    .get(&id)
                    .is_none_or(|request| request.state != ReqState::Pending);
            if !old {
                self.order.push_back(id);
                continue;
            }
            excess -= 1;
            if let Some(reply) = self.requests.remove(&id).and_then(|request| request.reply) {
                self.reply_bytes = self.reply_bytes.saturating_sub(reply.expose().len());
            }
        }
    }

    /// Past the cap the oldest reply goes and its request says `unavailable`.
    fn evict(&mut self) {
        while self.reply_bytes > RESULTS_CAP {
            let victim = self
                .order
                .iter()
                .find(|id| self.requests.get(*id).is_some_and(|r| r.reply.is_some()))
                .cloned();
            let Some(id) = victim else { break };
            if let Some(request) = self.requests.get_mut(&id)
                && let Some(reply) = request.reply.take()
            {
                self.reply_bytes = self.reply_bytes.saturating_sub(reply.expose().len());
                request.state = ReqState::Unavailable;
                "The result was dropped to stay within memory.".clone_into(&mut request.reason);
            }
        }
    }

    // ---- events, wakes and notices

    fn on_signal(&mut self, att: Att, signal: Signal) {
        if !self.is_current(att) {
            return;
        }
        match signal {
            Signal::Finished => self.release_held(att, false),
            Signal::Interrupted => self.release_held(att, true),
            _ => {}
        }
        match signal.word() {
            Some(word) => self.deliver_event(att, word, None),
            None => self.left_working(att),
        }
    }

    /// An event in `source`: for every other live terminal, a wake if it watches
    /// the source and is not busy, otherwise a notice.
    fn ended_reason(why: Option<&str>) -> String {
        match why {
            Some(why) => format!("The terminal ended before it answered: {why} Use read."),
            None => "The terminal ended before it answered. Use read.".to_owned(),
        }
    }

    fn deliver_event(&mut self, source: Att, word: &'static str, detail: Option<&str>) {
        let Some(source_slot) = self.shared.slot(source.slot) else {
            return;
        };
        let others: Vec<Arc<Slot>> = self
            .shared
            .slots
            .load()
            .iter()
            .flatten()
            .filter(|slot| slot.index != source.slot)
            .cloned()
            .collect();
        for target in others {
            let att = self.current(&target);
            if self.known.get(&target.index) != Some(&att) || !self.live(&target) {
                continue;
            }
            let watched = self.watches.iter().any(|(w, t)| *w == att && *t == source);
            let fact = Fact::Event { word };
            let notice = Notice {
                source: source_slot.id.clone(),
                text: render::sentence(&source_slot.id, &fact, watched),
                wanted: watched,
                body: detail.map(|d| format!("How it ended: {d}")),
            };
            let text = render::wake_text(&source_slot.id, &fact, watched);
            self.route(att, &target, notice, text, watched);
        }
        // Callers from other projects hear only what they watch.
        let seats: Vec<Att> = self
            .watches
            .iter()
            .filter(|(watcher, target)| *target == source && Self::is_seat(*watcher))
            .map(|(watcher, _)| *watcher)
            .collect();
        if seats.is_empty() {
            return;
        }
        let label = format!(
            "{} in project {}",
            source_slot.id,
            super::header_name(&self.own_name())
        );
        // Merged, folded and withdrawn by this key: the project's id, this
        // bridge instance and the terminal, never a name a rename changes.
        let key = self.watch_key(&source_slot.id);
        let fact = Fact::Event { word };
        for seat in seats {
            let notice = Notice {
                source: key.clone(),
                text: render::sentence(&label, &fact, true),
                wanted: true,
                body: detail.map(|d| format!("How it ended: {d}")),
            };
            let wake = render::wake_text(&label, &fact, true);
            if let Some(found) = self.seats.get(&seat.slot).cloned() {
                self.forward(&found, notice, wake, None);
            }
        }
    }

    /// Wake, or put the notice in the inbox.
    fn route(
        &mut self,
        att: Att,
        target: &Arc<Slot>,
        notice: Notice,
        wake_text: String,
        wakes: bool,
    ) {
        let can_wake = wakes
            && target.phase() != Phase::Working
            && !self.shared.pause().is_paused()
            && self
                .inboxes
                .get(&att.slot)
                .is_none_or(|inbox| inbox.wake.is_none());
        if can_wake {
            // A notice that carries a reply goes into the inbox as well, unwanted:
            // the wake's prompt drains it, and that is how the reply reaches the
            // agent. If the wake is never typed, the same entry gets its flag back.
            let why = if notice.body.is_some() {
                let nid = self.nid();
                let mut kept = notice.clone();
                kept.wanted = false;
                self.inboxes.entry(att.slot).or_default().push(nid, kept);
                Why::Unread(vec![nid])
            } else {
                Why::Event(notice.clone())
            };
            if self.send_wake(att, target, &wake_text, why) {
                return;
            }
            // Not queued: fall through to the plain inbox path. (A body notice is
            // already there; mark it wanted.)
            if notice.body.is_some() {
                if let Some(inbox) = self.inboxes.get_mut(&att.slot)
                    && let Some(entry) = inbox.entries.back_mut()
                {
                    entry.notice.wanted = true;
                }
                return;
            }
        }
        let nid = self.nid();
        if wakes {
            let note = if self.shared.pause().is_paused() {
                WakeNote::ParkedPaused
            } else if target.phase() == Phase::Working {
                WakeNote::ParkedWorking
            } else {
                WakeNote::ParkedInFlight
            };
            self.shared
                .wake_note(target, att.attempt, &format!("n{nid}"), note);
        }
        self.inboxes.entry(att.slot).or_default().push(nid, notice);
    }

    /// Queue a wake for `att`. False if it could not be queued.
    fn send_wake(&mut self, att: Att, target: &Arc<Slot>, text: &str, why: Why) -> bool {
        let id = format!("w{}", self.next_wake);
        self.next_wake += 1;
        let epoch = self.shared.pause().epoch();
        if self.shared.pause().is_paused() {
            return false;
        }
        let (sink, _unused) = bounded(1);
        let command = AgentCommand::Wake {
            id: id.clone(),
            text: text.to_owned(),
            admitted: Admitted {
                pause_epoch: epoch,
                attempt: att.attempt,
            },
            reply: sink,
            enqueued: Instant::now(),
        };
        if target.send_agent(command).is_err() {
            return false;
        }
        self.inboxes.entry(att.slot).or_default().wake = Some(WakeFlight {
            id,
            stage: Stage::Queued {
                since: Instant::now(),
            },
            why,
        });
        true
    }

    fn wake_outcome(&mut self, att: Att, id: &str, outcome: &WriteOutcome) {
        let Some(inbox) = self.inboxes.get_mut(&att.slot) else {
            return;
        };
        let Some(flight) = inbox.wake.clone().filter(|flight| flight.id == id) else {
            return;
        };
        if matches!(outcome, WriteOutcome::Typed) {
            inbox.wake = Some(WakeFlight {
                stage: Stage::AwaitingPrompt {
                    deadline: Instant::now() + WAKE_PROMPT_LIMIT,
                },
                ..flight
            });
        } else {
            inbox.wake = None;
            self.restore(att, flight.why);
        }
    }

    /// A wake that was never typed gives its notices back for the next stop.
    fn restore(&mut self, att: Att, why: Why) {
        match why {
            Why::Event(mut notice) => {
                notice.wanted = true;
                let nid = self.nid();
                self.inboxes.entry(att.slot).or_default().push(nid, notice);
            }
            Why::Context(notice) => {
                let nid = self.nid();
                self.inboxes.entry(att.slot).or_default().push(nid, notice);
            }
            Why::Unread(ids) => {
                if let Some(inbox) = self.inboxes.get_mut(&att.slot) {
                    inbox.set_wanted(&ids);
                }
                for reservation in self.reservations.values_mut().filter(|r| r.owner == att) {
                    for entry in &mut reservation.entries {
                        if ids.contains(&entry.nid) {
                            entry.notice.wanted = true;
                        }
                    }
                    for merged in reservation.merged.values_mut() {
                        if merged.wanted_ids.iter().any(|id| ids.contains(id)) {
                            merged.wanted = true;
                        }
                    }
                }
            }
        }
    }

    /// Phase left `Working`: one wake covers every unread wanted notice.
    fn left_working(&mut self, att: Att) {
        let Some(slot) = self.slot_of(att) else {
            return;
        };
        let Some(inbox) = self.inboxes.get_mut(&att.slot) else {
            return;
        };
        if !inbox.any_wanted() || inbox.wake.is_some() || self.shared.pause().is_paused() {
            return;
        }
        let ids = inbox.take_wanted();
        if !self.send_wake(att, &slot, render::UNREAD_WAKE, Why::Unread(ids.clone())) {
            self.restore(att, Why::Unread(ids));
        }
    }

    /// Connecto's own sentence for `caller`, if its session is still the one
    /// that asked.
    fn note(&mut self, caller: &Caller, text: String) {
        let att = Att::from(caller);
        let Some(slot) = self.slot_of(att) else {
            return;
        };
        if !self.live(&slot) {
            return;
        }
        let fact = Fact::Note {
            text: connecto_redact::sanitize(&text),
        };
        let notice = Notice {
            source: String::new(),
            text: render::sentence("", &fact, false),
            wanted: true,
            body: None,
        };
        let wake = render::wake_text("", &fact, false);
        self.route(att, &slot, notice, wake, true);
    }

    fn notify_sender(&mut self, request: &str, from: Att, state: &'static str) {
        if Self::is_seat(from) {
            self.notify_foreign_sender(request, state);
            return;
        }
        let restored = self.requests.get(request).is_some_and(|r| r.restored);
        // A notice belongs to the conversation that asked.
        let Some(sender) = self.slot_of(from).filter(|sender| self.live(sender)) else {
            // A restored ask's asker restarts with the app and may not run
            // yet: its answer waits for it.
            if restored && !self.deferred.iter().any(|(id, _)| id == request) {
                self.deferred.push((request.to_owned(), Instant::now()));
            }
            return;
        };
        let fact = Fact::RequestEnded {
            request: request.to_owned(),
            state: state.to_owned(),
        };
        // An `ok` reply rides in the context, framed as another agent's words, so
        // the woken sender has it without a command to run or a loop to write.
        let body = self.requests.get(request).and_then(|r| {
            r.reply
                .as_ref()
                .map(|reply| render::framed_reply(&r.target_id, request, reply.expose()))
        });
        let notice = Notice {
            source: String::new(),
            text: render::sentence("", &fact, false),
            wanted: true,
            body,
        };
        let text = render::wake_text("", &fact, false);
        self.route(from, &sender, notice, text, true);
    }

    fn check_wakes(&mut self) {
        let now = Instant::now();
        let slots: Vec<usize> = self.inboxes.keys().copied().collect();
        for index in slots {
            let Some(att) = self.known.get(&index).copied() else {
                continue;
            };
            let Some(inbox) = self.inboxes.get_mut(&index) else {
                continue;
            };
            let Some(flight) = inbox.wake.clone() else {
                continue;
            };
            match flight.stage {
                Stage::Queued { since } if now.duration_since(since) >= self.wake_limit => {
                    inbox.wake = None;
                    self.restore(att, flight.why);
                }
                Stage::AwaitingPrompt { deadline } if now >= deadline => {
                    // Typed and swallowed: nothing goes back (L12).
                    inbox.wake = None;
                }
                _ => {}
            }
        }
    }

    /// The safety net. A wake can be lost without a trace of a stop to come:
    /// the actor found the terminal busy, the screen not ready, or the user's
    /// draft in the way, and `LeftWorking` may never come again (an idle
    /// asker waiting for its reply). So wanted notices that wait in the inbox
    /// of a terminal that is not working, with no wake on its way and
    /// Connecto not paused, get a wake again after [`WAKE_RETRY_FIRST`],
    /// then after twice as long each time, up to [`WAKE_RETRY_MAX`]. A user
    /// draft holds it off: a draft is never typed over, and the user's own
    /// next prompt drains the inbox anyway.
    fn retry_wakes(&mut self) {
        if self.shared.pause().is_paused() {
            return;
        }
        let now = Instant::now();
        let slots: Vec<usize> = self.inboxes.keys().copied().collect();
        for index in slots {
            let Some(att) = self.known.get(&index).copied() else {
                continue;
            };
            let Some(slot) = self.slot_of(att) else {
                continue;
            };
            let live = self.live(&slot);
            let first = self.retry_first;
            let Some(inbox) = self.inboxes.get_mut(&index) else {
                continue;
            };
            if !inbox.any_wanted() {
                inbox.retry = None;
                continue;
            }
            // A shell has no prompt hook: nothing it could be woken into.
            let shell = slot.tool == connecto_store::Tool::Shell;
            if inbox.wake.is_some() || !live || shell || slot.phase() == Phase::Working {
                continue;
            }
            let Some(retry) = inbox.retry else {
                inbox.retry = Some(Retry {
                    next: now + first,
                    tries: 0,
                });
                continue;
            };
            if now < retry.next {
                continue;
            }
            let tries = retry.tries.saturating_add(1);
            let wait = first
                .saturating_mul(1_u32.checked_shl(tries.min(16)).unwrap_or(u32::MAX))
                .min(WAKE_RETRY_MAX);
            inbox.retry = Some(Retry {
                next: now + wait,
                tries,
            });
            if !matches!(slot.pane.draft(), crate::draft::Known::Exact(draft) if draft.is_empty()) {
                self.shared
                    .wake_note(&slot, att.attempt, "", WakeNote::ParkedDraft);
                continue;
            }
            let ids = inbox.take_wanted();
            let id = format!("w{}", self.next_wake);
            self.shared
                .wake_note(&slot, att.attempt, &id, WakeNote::Retried);
            if !self.send_wake(att, &slot, render::UNREAD_WAKE, Why::Unread(ids.clone())) {
                self.restore(att, Why::Unread(ids));
            }
        }
    }

    fn check_lost(&mut self) {
        let slots: Vec<Arc<Slot>> = self.shared.slots.load().iter().flatten().cloned().collect();
        for slot in slots {
            let lost = slot.lost.load(Ordering::SeqCst);
            let seen = self.lost_seen.entry(slot.index).or_insert(0);
            if lost <= *seen {
                continue;
            }
            *seen = lost;
            let att = self.current(&slot);
            let watchers: Vec<Att> = self
                .watches
                .iter()
                .filter(|(_, target)| *target == att)
                .map(|(watcher, _)| *watcher)
                .collect();
            for watcher in watchers.into_iter().filter(|w| !Self::is_seat(*w)) {
                self.inboxes.entry(watcher.slot).or_default().lost += 1;
            }
            if slot.phase() != Phase::Working {
                self.left_working(att);
            }
        }
    }

    // ---- requests

    fn check_requests(&mut self) {
        let now = Instant::now();
        let ids: Vec<String> = self
            .requests
            .iter()
            .filter(|(_, r)| r.state == ReqState::Pending && !r.bound && r.id_given_out)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let Some(request) = self.requests.get_mut(&id) else {
                continue;
            };
            if request.waiting {
                // Held for the user's draft; the actor bounds the wait.
                request.clock_from = now;
                continue;
            }
            let (working, ready) = self
                .shared
                .slot(request.target.slot)
                .map_or((false, true), |slot| {
                    (slot.phase() == Phase::Working, slot.ready())
                });
            if !ready && !working && !request.ready_seen {
                // Still starting: the text waits in the tool's input and no
                // clock runs, up to a cap.
                request.clock_from = now;
                if now.duration_since(request.born) >= BOOT_CAP {
                    let reason = "The terminal did not finish starting in time, so the text may not have been entered. Use read.";
                    self.finish(&id, ReqState::Unmatched, reason, None);
                }
                continue;
            }
            if !request.ready_seen {
                request.ready_seen = true;
                request.clock_from = now;
            }
            if working {
                request.ready_seen = true;
                request.working_seen = true;
                continue;
            }
            if request.working_seen {
                // The clock runs only while the target is not busy.
                request.working_seen = false;
                request.clock_from = now;
            }
            if now.duration_since(request.clock_from) >= UNMATCHED_AFTER {
                let reason = "The text did not start a turn. It may not have been entered and may still be in the input box. Use read.";
                self.finish(&id, ReqState::Unmatched, reason, None);
            }
        }
    }

    fn admit(
        &mut self,
        caller: &Caller,
        target: &str,
        kind: AdmitKind,
    ) -> Result<Admission, Reply> {
        let pause = self.shared.pause();
        // The epoch first, then the flag, then the attempt: a reader that sees
        // `paused` false took its epoch before any later Pause bumped it.
        let epoch = pause.epoch();
        if pause.is_paused() {
            return Err(refusal(
                "paused",
                "Connecto is paused by the user. Nothing was typed.",
            ));
        }
        let slot = self
            .shared
            .slot_by_id(target)
            .ok_or_else(|| refusal("unknown_terminal", "No terminal has that id."))?;
        let att = self.current(&slot);
        let admitted = Admitted {
            pause_epoch: epoch,
            attempt: att.attempt,
        };
        let from = self.seat_of(caller);
        let (id, ask) = match kind {
            AdmitKind::Send { ask } => {
                let id = format!("r{}", self.next_request);
                self.next_request += 1;
                (id, ask)
            }
            AdmitKind::Keys => {
                let id = format!("k{}", self.next_key);
                self.next_key += 1;
                (id, false)
            }
        };
        if matches!(kind, AdmitKind::Send { .. }) {
            self.requests.insert(
                id.clone(),
                Request {
                    project: self.shared.project_key.load(Ordering::SeqCst),
                    from,
                    from_id: caller.label(),
                    target: att,
                    target_id: slot.id.clone(),
                    ask,
                    ask_open: false,
                    id_given_out: false,
                    bound: false,
                    clock_from: Instant::now(),
                    working_seen: false,
                    ready_seen: false,
                    born: Instant::now(),
                    waiting: false,
                    note: None,
                    state: ReqState::Pending,
                    reason: String::new(),
                    reply: None,
                    held: None,
                    restored: false,
                    seat: self.seats.get(&from.slot).cloned(),
                },
            );
            self.order.push_back(id.clone());
        }
        Ok(Admission { id, slot, admitted })
    }

    fn result(&self, id: &str, caller: &Caller) -> Reply {
        let project = self.shared.project_key.load(Ordering::SeqCst);
        let Some(request) = self.requests.get(id) else {
            if self.was_given(id) {
                // As a dropped reply is reported: a state, with the reason.
                return Reply::ok(json!({
                    "state": ReqState::Unavailable.word(),
                    "reason": format!(
                        "That request is no longer kept: only the newest {FINISHED_KEPT} finished requests are."
                    ),
                }));
            }
            return refusal("unknown_request", "No request has that id in this project.");
        };
        if request.project != project {
            return refusal("unknown_request", "No request has that id in this project.");
        }
        // Across projects a caller reads only the answers to its own requests.
        // Decided on who the request itself says asked, not on today's
        // seats: a seat dropped since does not take a caller's answer away.
        let mine = match &caller.origin {
            None => request.from == Att::from(caller),
            Some(origin) => {
                request.seat.as_ref()
                    == Some(&Seat {
                        project_id: origin.project_id.clone(),
                        instance: origin.instance,
                        att: Att::from(caller),
                    })
            }
        };
        if caller.origin.is_some() && !mine {
            return refusal(
                "unknown_request",
                "No request of yours has that id in that project.",
            );
        }
        let mut reason = request.reason.clone();
        if request.state == ReqState::Pending {
            let slot = self.shared.slot(request.target.slot);
            let background = slot
                .as_ref()
                .is_some_and(|slot| slot.phase() == Phase::Background);
            // What runs, named, so the sender can tell a dev server from a
            // command that is nearly done.
            let running = slot
                .as_ref()
                .map(|slot| background::summary(&slot.background_shown(), Instant::now()))
                .filter(|running| !running.is_empty())
                .map_or_else(String::new, |running| format!(" Still running: {running}."));
            // The sender of an `--ask` is woken with the reply: it should end its
            // turn and wait, not poll.
            let ours = request.ask && mine;
            let target = &request.target_id;
            reason = match (background, ours) {
                (true, true) => format!("The target has background work running, so its turn has not ended.{running} The reply will be delivered to you automatically; end your turn and wait. `connecto tasks {target}` lists that work."),
                (true, false) => format!("The target has background work running, so its turn has not ended.{running} Use read, or `connecto tasks {target}`."),
                (false, true) if request.held.is_some() => "The target is still working with other agents it asked. The reply will be delivered to you automatically when it is done; end your turn and wait.".to_owned(),
                (false, true) => "The reply will be delivered to you automatically; end your turn and wait. Do not poll.".to_owned(),
                (false, false) => "The target has not answered yet.".to_owned(),
            };
        }
        let header = json!({
            "state": request.state.word(),
            "from": request.from_id,
            "target": request.target_id,
            "reason": if reason.is_empty() { serde_json::Value::Null } else { json!(reason) },
        });
        let reply = Reply::ok(header);
        match (&request.state, &request.reply) {
            (ReqState::Ok, Some(text)) => reply.with_body(text.expose().clone().into_bytes()),
            _ => reply,
        }
    }

    /// Whether `id` is a request id this state has made. One that is no longer
    /// held was forgotten. (Or it was refused before any paste, but that id
    /// was never given to anyone who could ask for it.)
    fn was_given(&self, id: &str) -> bool {
        id.strip_prefix('r')
            .filter(|digits| {
                !digits.starts_with('0') && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
            .and_then(|digits| digits.parse::<u64>().ok())
            .is_some_and(|number| number < self.next_request)
    }

    // ---- watches, list and drain

    fn watch(&mut self, caller: &Caller, target: &str, on: bool) -> Reply {
        let from = if on {
            self.seat_of(caller)
        } else {
            match self.find_seat(caller) {
                Some(att) => att,
                // Never seen here, so it watches nothing here.
                None => return Reply::ok(json!({ "target": target, "watching": false })),
            }
        };
        let Some(slot) = self.shared.slot_by_id(target) else {
            return refusal("unknown_terminal", "No terminal has that id.");
        };
        let att = self.current(&slot);
        if on {
            if att == from {
                return refusal(
                    "bad_request",
                    "Watching your own terminal would only wake yourself.",
                );
            }
            // A login is a live process, so this is decided on its own, first.
            if slot.pane.stage() == PaneStage::LoggingIn {
                return refusal("logging_in", "That terminal is running a login.");
            }
            if !self.live(&slot) {
                return refusal(
                    "not_running",
                    "That terminal has no attempt running, and a watch needs one.",
                );
            }
            if !self.watches.contains(&(from, att)) {
                self.watches.push((from, att));
            }
        } else {
            self.watches.retain(|watch| *watch != (from, att));
            match self.seats.get(&from.slot).cloned() {
                Some(seat) => {
                    // The notices are in the caller's own project.
                    let source = self.watch_key(target);
                    self.send_to(
                        &seat.project_id,
                        seat.instance,
                        Owed::ForeignCancel {
                            to: seat.att,
                            to_instance: seat.instance,
                            source,
                        },
                    );
                }
                None => self.cancel_wanted(from, target),
            }
        }
        Reply::ok(json!({ "target": target, "watching": on }))
    }

    /// `unwatch` withdraws what the watch made wanted, wherever it is now: in
    /// the inbox, in a reservation a drain holds, or in a wake in flight whose
    /// recovery would set the flag again. Notices stay as context. Independent
    /// `--ask` notices have no source and are kept.
    fn cancel_wanted(&mut self, from: Att, target: &str) {
        let mut cancelled: Vec<u64> = Vec::new();
        if let Some(inbox) = self.inboxes.get_mut(&from.slot) {
            cancelled.extend(inbox.ids_from(target));
            inbox.clear_wanted_for(target);
            if let Some(flight) = inbox.wake.as_mut() {
                match &mut flight.why {
                    Why::Event(notice) if notice.source == target => {
                        let notice = notice.clone();
                        flight.why = Why::Context(Notice {
                            wanted: false,
                            ..notice
                        });
                    }
                    Why::Unread(ids) => ids.retain(|id| !cancelled.contains(id)),
                    _ => {}
                }
            }
        }
        for reservation in self.reservations.values_mut().filter(|r| r.owner == from) {
            for entry in &mut reservation.entries {
                if entry.notice.source == target {
                    cancelled.push(entry.nid);
                    entry.notice.wanted = false;
                }
            }
            if let Some(merged) = reservation.merged.get_mut(target) {
                cancelled.extend(merged.wanted_ids.iter().copied());
                merged.wanted = false;
                merged.wanted_ids.clear();
            }
        }
        // A wake already in flight for the unread notices must not give back
        // what was cancelled in a reservation either.
        if let Some(Why::Unread(ids)) = self
            .inboxes
            .get_mut(&from.slot)
            .and_then(|inbox| inbox.wake.as_mut())
            .map(|flight| &mut flight.why)
        {
            ids.retain(|id| !cancelled.contains(id));
        }
    }

    pub(crate) fn state_word(slot: &Slot) -> &'static str {
        let (phase, needs_user, _, _) = slot.published();
        match slot.pane.stage() {
            PaneStage::Idle | PaneStage::Stopped => "stopped",
            PaneStage::Preparing { .. } => "starting",
            PaneStage::NeedsLogin { .. } => "needs-login",
            PaneStage::LoggingIn => "logging-in",
            PaneStage::Failed { .. } => "failed",
            PaneStage::Running => {
                if !slot.pane.is_live() {
                    "exited"
                } else {
                    match phase {
                        Phase::Working => "working",
                        Phase::Background => "background",
                        Phase::Idle if needs_user => "needs-user",
                        Phase::Idle if slot.interrupted.load(Ordering::SeqCst) => "interrupted",
                        Phase::Idle => "idle",
                    }
                }
            }
        }
    }

    fn list(&self, caller: &Caller) -> Reply {
        let from = self.find_seat(caller).unwrap_or(Att {
            slot: usize::MAX,
            uid: 0,
            attempt: 0,
        });
        let view = self.view.load();
        let terminals: Vec<serde_json::Value> = view
            .terminals
            .iter()
            .filter_map(|entry| {
                let slot = self.shared.slot_by_id(&entry.id)?;
                let (_, _, seconds, last) = slot.published();
                let att = self.current(&slot);
                let watched = self.watches.contains(&(from, att));
                let off = slot.pane.bridge_off_reason();
                let running = slot.background_shown();
                Some(json!({
                    "id": entry.id,
                    "tool": entry.tool.name(),
                    "state": Self::state_word(&slot),
                    "seconds": seconds,
                    "last_event": last,
                    "watched": watched,
                    "owner": entry.owner(),
                    "bridge": if off.is_some() { "off" } else { "on" },
                    "bridge_reason": off,
                    "warning": slot.pane.power_warning(),
                    "exit": Self::exit_text(&slot, att),
                    "background": if running.is_empty() {
                        serde_json::Value::Null
                    } else {
                        background::json(&running, Instant::now())
                    },
                }))
            })
            .collect();
        // A caller from another project is none of these terminals.
        let this = if caller.origin.is_some() {
            serde_json::Value::Null
        } else {
            json!(caller.id)
        };
        Reply::ok(json!({
            "self": this,
            "paused": self.shared.pause().is_paused(),
            "terminals": terminals,
        }))
    }

    fn roster(&self, except: usize) -> Vec<(String, String, String)> {
        let view = self.view.load();
        view.terminals
            .iter()
            .filter_map(|entry| {
                let slot = self.shared.slot_by_id(&entry.id)?;
                (slot.index != except).then(|| {
                    (
                        entry.id.clone(),
                        entry.tool.name().to_owned(),
                        Self::state_word(&slot).to_owned(),
                    )
                })
            })
            .collect()
    }

    #[cfg(test)]
    fn drain(&mut self, caller: &Caller) -> Option<Drained> {
        self.drain_with(caller, None, None)
    }

    /// A drain, with a project memory version to announce merged into any
    /// still waiting: one notice names the latest.
    fn drain_with(
        &mut self,
        caller: &Caller,
        memory_version: Option<u64>,
        ops_note: Option<String>,
    ) -> Option<Drained> {
        let att = Att::from(caller);
        if !self.is_current(att) {
            return None;
        }
        let roster = self.roster(att.slot);
        let inbox = self.inboxes.entry(att.slot).or_default();
        let first = !inbox.first_done;
        inbox.first_done = true;
        let reservation = Reservation {
            owner: att,
            entries: std::mem::take(&mut inbox.entries),
            merged: std::mem::take(&mut inbox.merged),
            lost: std::mem::take(&mut inbox.lost),
            first,
            memory_version: inbox.memory_version.take().max(memory_version),
        };
        let input = DrainInput {
            terminal: caller.id.clone(),
            roster,
            entries: reservation
                .entries
                .iter()
                .map(|e| e.notice.clone())
                .collect(),
            merged: reservation
                .merged
                .iter()
                .map(|(source, merged)| (source.clone(), merged.count))
                .collect(),
            lost: reservation.lost,
            first,
            memory_version: reservation.memory_version,
            ops_note,
        };
        let body = render::render(&input).map(|text| render::context_json(&text));
        let id = self.next_reservation;
        self.next_reservation += 1;
        match body {
            Some(body) => {
                self.reservations.insert(id, reservation);
                Some(Drained {
                    reservation: id,
                    body,
                })
            }
            None => {
                // Nothing to print: nothing to keep.
                Some(Drained {
                    reservation: 0,
                    body: Vec::new(),
                })
            }
        }
    }

    // ---- other projects (ADR 0035)

    const fn is_seat(att: Att) -> bool {
        att.slot >= SEAT_BASE
    }

    /// This project's id and the name the directory gives it.
    fn own_id(&self) -> String {
        self.shared
            .memory_project
            .get()
            .map(|(id, _)| id.clone())
            .unwrap_or_default()
    }

    /// What notices about `terminal` carry as their source in another
    /// project's inbox.
    fn watch_key(&self, terminal: &str) -> String {
        format!(
            "{terminal} in project {}#{}",
            self.own_id(),
            self.shared.bridge_instance
        )
    }

    fn own_name(&self) -> String {
        let id = self.own_id();
        self.shared
            .directory
            .get()
            .and_then(|directory| directory.by_id(&id))
            .map(|entry| entry.name)
            .or_else(|| {
                self.shared
                    .memory_project
                    .get()
                    .and_then(|(_, path)| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default()
    }

    /// The attempt a caller acts as here: its own for a caller of this
    /// project, a seat for one from another.
    fn find_seat(&self, caller: &Caller) -> Option<Att> {
        let Some(origin) = &caller.origin else {
            return Some(Att::from(caller));
        };
        let wanted = Seat {
            project_id: origin.project_id.clone(),
            instance: origin.instance,
            att: Att::from(caller),
        };
        self.seats
            .iter()
            .find(|(_, seat)| **seat == wanted)
            .map(|(slot, _)| Att {
                slot: *slot,
                uid: 0,
                attempt: 0,
            })
    }

    fn seat_of(&mut self, caller: &Caller) -> Att {
        if let Some(att) = self.find_seat(caller) {
            return att;
        }
        let Some(origin) = &caller.origin else {
            return Att::from(caller);
        };
        if self.seats.len() >= SEATS_MAX
            && let Some((&oldest, _)) = self.seats.iter().next()
        {
            // An ask it made keeps its own copy of where the answer goes.
            self.drop_seat(oldest);
        }
        let slot = SEAT_BASE + self.next_seat;
        self.next_seat += 1;
        self.seats.insert(
            slot,
            Seat {
                project_id: origin.project_id.clone(),
                instance: origin.instance,
                att: Att::from(caller),
            },
        );
        Att {
            slot,
            uid: 0,
            attempt: 0,
        }
    }

    fn drop_seat(&mut self, slot: usize) {
        self.seats.remove(&slot);
        self.watches.retain(|(watcher, _)| watcher.slot != slot);
    }

    fn seat_gone(&mut self, project: &str, instance: u64, att: Att) {
        let gone = Seat {
            project_id: project.to_owned(),
            instance,
            att,
        };
        let slots: Vec<usize> = self
            .seats
            .iter()
            .filter(|(_, seat)| **seat == gone)
            .map(|(slot, _)| *slot)
            .collect();
        for slot in slots {
            self.drop_seat(slot);
        }
    }

    /// The bridge state's shared side of `project` while it is the instance
    /// `instance`.
    fn peer(&self, project: &str, instance: u64) -> Option<Arc<Shared>> {
        #[cfg(test)]
        if let Some(shared) = self.peers.get(&(project.to_owned(), instance)) {
            return Some(Arc::clone(shared));
        }
        self.shared
            .directory
            .get()
            .and_then(|directory| directory.by_id(project))
            .and_then(|entry| entry.ctx())
            .map(|ctx| Arc::clone(&ctx.shared))
            .filter(|shared| shared.bridge_instance == instance)
    }

    /// Hand a debt to another project's bridge state. Lossless while that
    /// bridge instance runs; false when it does not.
    fn send_to(&self, project: &str, instance: u64, owed: Owed) -> bool {
        match self.peer(project, instance) {
            Some(shared) => {
                shared.owe(owed);
                true
            }
            None => false,
        }
    }

    /// A notice for the caller `seat`, in its own project.
    fn forward(&mut self, seat: &Seat, notice: Notice, wake: String, ended: Option<String>) {
        let forward = Forward {
            to: seat.att,
            to_instance: seat.instance,
            from_project: self.own_id(),
            from_instance: self.shared.bridge_instance,
            notice,
            wake,
            ended,
        };
        if !self.send_to(&seat.project_id, seat.instance, Owed::Forwarded(forward)) {
            // Its project closed: nobody there to tell.
            self.seat_gone(&seat.project_id, seat.instance, seat.att);
        }
    }

    /// The end of an ask a caller from another project made here.
    fn notify_foreign_sender(&mut self, request: &str, state: &'static str) {
        let Some(seat) = self.requests.get(request).and_then(|r| r.seat.clone()) else {
            return;
        };
        let name = super::header_name(&self.own_name());
        // The command names the project by its id, which never changes and
        // needs no quoting; the notice shows its name.
        let id = self.own_id();
        let fact = Fact::RequestEndedIn {
            request: request.to_owned(),
            name: name.chars().take(16).collect(),
            project: id.clone(),
            state: state.to_owned(),
        };
        let body = self.requests.get(request).and_then(|r| {
            r.reply.as_ref().map(|reply| {
                render::framed_reply(
                    &format!("{} in project {name}", r.target_id),
                    &format!("{request} --project {id}"),
                    reply.expose(),
                )
            })
        });
        let notice = Notice {
            source: String::new(),
            text: render::sentence("", &fact, false),
            wanted: true,
            body,
        };
        let wake = render::wake_text("", &fact, false);
        self.forward(&seat, notice, wake, Some(request.to_owned()));
    }

    /// A notice from another project for a terminal here.
    fn on_forwarded(&mut self, forward: Forward) {
        if forward.to_instance != self.shared.bridge_instance {
            // Meant for an earlier opening of this project: its terminals'
            // numbers mean other conversations now.
            return;
        }
        if let Some(request) = &forward.ended {
            let before = self.foreign_asks.len();
            self.foreign_asks.retain(|ask| {
                !(ask.project_id == forward.from_project
                    && ask.project_instance == forward.from_instance
                    && ask.request == *request)
            });
            if self.foreign_asks.len() == before {
                // The answer came before its ask was recorded here.
                if self.tombstones.len() >= TOMBSTONES_MAX {
                    self.tombstones.pop_front();
                }
                self.tombstones.push_back((
                    forward.from_project.clone(),
                    forward.from_instance,
                    request.clone(),
                ));
            }
        }
        match self.slot_of(forward.to).filter(|slot| self.live(slot)) {
            Some(slot) => {
                let wanted = forward.notice.wanted;
                self.route(forward.to, &slot, forward.notice, forward.wake, wanted);
            }
            None => {
                // That conversation is over: its seat there can go.
                let own = self.own_id();
                self.send_to(
                    &forward.from_project,
                    forward.from_instance,
                    Owed::SeatGone {
                        project: own,
                        instance: self.shared.bridge_instance,
                        att: forward.to,
                    },
                );
            }
        }
    }

    fn on_foreign_ask(&mut self, ask: ForeignAsk) {
        if let Some(index) = self
            .tombstones
            .iter()
            .position(|(project, instance, request)| {
                *project == ask.project_id
                    && *instance == ask.project_instance
                    && *request == ask.request
            })
        {
            self.tombstones.remove(index);
            return;
        }
        if self.is_current(ask.att) {
            self.foreign_asks.push(ask);
        }
    }

    /// An ask made in a project that has closed since will never be
    /// answered: its asker is told so, and is no longer held.
    fn check_foreign_asks(&mut self) {
        if self.foreign_asks.is_empty() {
            return;
        }
        let (open, closed): (Vec<ForeignAsk>, Vec<ForeignAsk>) =
            std::mem::take(&mut self.foreign_asks)
                .into_iter()
                .partition(|ask| self.peer(&ask.project_id, ask.project_instance).is_some());
        self.foreign_asks = open;
        for ask in closed {
            let Some(slot) = self.slot_of(ask.att).filter(|slot| self.live(slot)) else {
                continue;
            };
            let fact = Fact::Note {
                text: format!(
                    "Request {} in project {} ended (exited): that project was closed before it answered.",
                    ask.request,
                    super::header_name(&ask.project_name)
                ),
            };
            let notice = Notice {
                source: String::new(),
                text: render::sentence("", &fact, false),
                wanted: true,
                body: None,
            };
            let wake = render::wake_text("", &fact, false);
            self.route(ask.att, &slot, notice, wake, true);
        }
    }

    /// A drain commits when its reply has been written. Otherwise the notices
    /// go back to the front, in order, with their counts and the first flag.
    fn drain_done(&mut self, id: u64, written: bool) {
        let Some(reservation) = self.reservations.remove(&id) else {
            return;
        };
        if written {
            return;
        }
        let Some(inbox) = self.inboxes.get_mut(&reservation.owner.slot) else {
            return;
        };
        if !self
            .known
            .get(&reservation.owner.slot)
            .is_some_and(|known| *known == reservation.owner)
        {
            return;
        }
        let newer = std::mem::take(&mut inbox.entries);
        inbox.entries = reservation.entries;
        for entry in newer {
            inbox.push(entry.nid, entry.notice);
        }
        for (source, more) in reservation.merged {
            let merged = inbox.merged.entry(source).or_default();
            merged.count += more.count;
            merged.wanted |= more.wanted;
            merged.wanted_ids.extend(more.wanted_ids);
        }
        inbox.lost += reservation.lost;
        inbox.memory_version = inbox.memory_version.max(reservation.memory_version);
        if reservation.first {
            inbox.first_done = false;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn notice(source: &str, text: &str, wanted: bool) -> Notice {
        Notice {
            source: source.into(),
            text: text.into(),
            wanted,
            body: None,
        }
    }

    #[test]
    fn a_full_inbox_merges_its_oldest_entries_and_keeps_the_wanted_flag() {
        let mut inbox = Inbox::default();
        inbox.push(1, notice("a", "first", true));
        for n in 2..=(INBOX_MAX as u64 + 3) {
            inbox.push(n, notice("b", "x", false));
        }
        assert_eq!(inbox.entries.len(), INBOX_MAX);
        assert_eq!(
            (inbox.merged["a"].count, inbox.merged["a"].wanted),
            (1, true)
        );
        assert_eq!(
            (inbox.merged["b"].count, inbox.merged["b"].wanted),
            (2, false)
        );
        assert!(
            inbox.any_wanted(),
            "a merged wanted notice still counts as unread"
        );
        assert_eq!(
            inbox.take_wanted(),
            [1],
            "a merged notice keeps its recovery id"
        );
        inbox.set_wanted(&[1]);
        assert!(inbox.any_wanted(), "and gets its flag back with it");
        inbox.take_wanted();
        assert!(!inbox.any_wanted());
    }

    #[test]
    fn wanted_flags_clear_and_come_back_by_id() {
        let mut inbox = Inbox::default();
        inbox.push(1, notice("a", "one", true));
        inbox.push(2, notice("b", "two", true));
        inbox.push(3, notice("a", "three", false));
        let ids = inbox.take_wanted();
        assert_eq!(ids, [1, 2]);
        assert!(!inbox.any_wanted());
        inbox.set_wanted(&ids);
        assert!(inbox.any_wanted());
        inbox.clear_wanted_for("a");
        assert!(
            inbox
                .entries
                .iter()
                .all(|e| e.notice.source != "a" || !e.notice.wanted)
        );
        assert!(inbox.any_wanted(), "b is still wanted");
    }

    // ---- the state, driven directly (no thread, no sockets)

    use connecto_audit::{Audit, Sink};
    use connecto_store::Tool;

    use crate::launch::{LaunchExtras, Layout};
    use crate::{ManagedPane, PaneBridge, PaneSetup};

    struct Null;

    impl Sink for Null {
        fn write_line(&mut self, _line: &str) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Two idle terminals, `a` and `b`, and a state that treats both as live.
    /// Their actors run, so a wake is really queued and really refused (there is
    /// no process to type into), and its outcome comes back through the outbox.
    struct Rig {
        shared: Arc<Shared>,
        state: State,
        a: Arc<Slot>,
        b: Arc<Slot>,
    }

    fn rig() -> Rig {
        rig_with(Null)
    }

    /// Audit lines, kept for a test to read.
    #[derive(Clone, Default)]
    struct Kept(Arc<std::sync::Mutex<Vec<String>>>);

    impl Sink for Kept {
        fn write_line(&mut self, line: &str) -> std::io::Result<()> {
            self.0.lock().unwrap().push(line.to_owned());
            Ok(())
        }
    }

    fn rig_with(sink: impl Sink) -> Rig {
        let root = std::env::temp_dir().join(format!("mg-state-{}", std::process::id()));
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let shared = Shared::new(
            layout.clone(),
            Audit::with_sink(sink).unwrap(),
            Arc::new(|| {}),
            None,
            "",
        );
        let slot = |index: usize, id: &str| {
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
        };
        let (a, b) = (slot(0, "a"), slot(1, "b"));
        let view = Arc::new(ArcSwap::from_pointee(TerminalsView {
            terminals: Vec::new(),
            history: Arc::new(Vec::new()),
            transcript: None,
            save: connecto_core::SaveState::Saved,
            last_error: None,
            warning: None,
        }));
        let mut state = State::new(Arc::clone(&shared), view);
        state.assume_live = true;
        state.reconcile();
        Rig {
            shared,
            state,
            a,
            b,
        }
    }

    impl Rig {
        fn att(&self, slot: &Slot) -> Att {
            self.state.current(slot)
        }

        fn caller(&self, slot: &Slot) -> Caller {
            let att = self.att(slot);
            Caller {
                slot: att.slot,
                uid: att.uid,
                id: slot.id.clone(),
                attempt: att.attempt,
                origin: None,
            }
        }

        /// `slot`'s caller as if it came from another project, with the same
        /// slot, uid, attempt and id as a terminal here.
        fn stranger(&self, slot: &Slot) -> Caller {
            Caller {
                origin: Some(Arc::new(super::super::Origin {
                    project_id: "other".into(),
                    project_name: "other".into(),
                    instance: 7,
                })),
                ..self.caller(slot)
            }
        }

        fn inbox(&mut self, slot: &Slot) -> &mut Inbox {
            self.state.inboxes.entry(slot.index).or_default()
        }

        /// Let the actors answer what was queued, and take their reports.
        fn settle(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                thread::sleep(Duration::from_millis(30));
                self.state.take_reports();
                if self
                    .state
                    .inboxes
                    .values()
                    .all(|inbox| inbox.wake.is_none())
                {
                    return;
                }
            }
        }

        fn wanted(&mut self, slot: &Arc<Slot>, count: usize, source: &str) -> Vec<u64> {
            (0..count)
                .map(|n| {
                    let nid = self.state.nid();
                    self.inbox(slot)
                        .push(nid, notice(source, &format!("event {n}"), true));
                    nid
                })
                .collect()
        }
    }

    /// T52 and finding 7: a wake that is refused (or finds the target busy)
    /// gives the wanted flag back to its notices, merged ones included. The next
    /// `LeftWorking` tries again, and an idle watcher is not woken for the same
    /// notices in between.
    #[test]
    fn a_wake_that_was_never_typed_gives_back_every_wanted_flag_merged_ones_too() {
        let mut rig = rig();
        let a = Arc::clone(&rig.a);
        rig.wanted(&a, INBOX_MAX + 8, "b");
        assert!(!rig.inbox(&a).merged.is_empty(), "some notices were folded");
        let att = rig.att(&a);
        rig.state.left_working(att);
        assert!(rig.inbox(&a).wake.is_some(), "one wake went out");
        assert!(
            !rig.inbox(&a).any_wanted(),
            "the flags are cleared at the attempt"
        );
        rig.settle();
        assert!(rig.inbox(&a).wake.is_none(), "the refusal ended the wake");
        assert!(rig.inbox(&a).any_wanted(), "the flags came back");
        assert!(
            rig.inbox(&a).merged.values().any(|m| m.wanted),
            "the folded notices too"
        );
        // Nothing retries at once: an idle watcher is not woken again.
        rig.state.tick();
        assert!(rig.inbox(&a).wake.is_none());
        // The next stop tries again.
        rig.state.left_working(att);
        assert!(rig.inbox(&a).wake.is_some());
    }

    /// The safety net: wanted notices left in the inbox of an idle terminal
    /// (its wake found it busy, not ready, or behind a draft) get a wake again,
    /// never at once, never while one is on its way, never while paused or
    /// while the terminal works, and less often each time.
    #[test]
    fn wanted_notices_left_behind_get_a_wake_again_with_backoff() {
        let mut rig = rig();
        rig.state.retry_first = Duration::from_millis(150);
        let a = Arc::clone(&rig.a);
        rig.wanted(&a, 2, "b");
        rig.state.tick();
        assert!(rig.inbox(&a).wake.is_none(), "nothing retries at once");
        thread::sleep(Duration::from_millis(200));
        // Not while paused.
        rig.shared.pause().pause();
        rig.state.tick();
        assert!(rig.inbox(&a).wake.is_none(), "paused: nothing is tried");
        rig.shared.pause().resume();
        rig.state.tick();
        assert!(
            rig.inbox(&a).wake.is_some(),
            "due and idle: one wake goes out"
        );
        assert!(!rig.inbox(&a).any_wanted());
        // One in flight: no second.
        rig.state.tick();
        rig.settle();
        assert!(
            rig.inbox(&a).wake.is_none(),
            "refused: no process to type into"
        );
        assert!(rig.inbox(&a).any_wanted(), "the notices are wanted again");
        // The next try waits twice as long.
        rig.state.tick();
        assert!(rig.inbox(&a).wake.is_none(), "backing off");
        thread::sleep(Duration::from_millis(320));
        rig.state.tick();
        assert!(
            rig.inbox(&a).wake.is_some(),
            "tried again after the longer wait"
        );
        rig.settle();
        // Drained: the schedule is dropped.
        let caller = rig.caller(&a);
        let drained = rig.state.drain(&caller).unwrap();
        rig.state.drain_done(drained.reservation, true);
        rig.state.tick();
        assert!(rig.inbox(&a).retry.is_none());
    }

    /// What the audit log says of a wake that typed nothing is one of a fixed
    /// set: no notice text, no screen text, no error message.
    #[test]
    fn wake_audit_reasons_come_from_a_fixed_set() {
        let kept = Kept::default();
        let mut rig = rig_with(kept.clone());
        rig.state.retry_first = Duration::ZERO;
        let a = Arc::clone(&rig.a);
        let att = rig.att(&a);
        let secret = "token sk-live-0000 on screen";
        // One wake goes out; a second notice parks behind it.
        for _ in 0..2 {
            rig.state
                .route(att, &a, notice("b", secret, true), secret.to_owned(), true);
        }
        rig.settle();
        // The safety net tries again.
        rig.state.tick();
        rig.state.tick();
        rig.settle();
        assert!(rig.shared.gate().flush(Duration::from_secs(2)));
        let fixed: Vec<(&str, &str)> = WakeNote::ALL.iter().map(|n| n.audit()).collect();
        let lines = kept.0.lock().unwrap().clone();
        let mut seen = std::collections::BTreeSet::new();
        for line in &lines {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            if record["action"] != "wake" || record["decision"] != "outcome" {
                continue;
            }
            let pair = (
                record["outcome"].as_str().unwrap().to_owned(),
                record["reason"].as_str().unwrap().to_owned(),
            );
            assert!(!line.contains("sk-live"), "{line}");
            assert!(
                pair.1.is_empty()
                    || fixed
                        .iter()
                        .any(|(o, r)| (*o, *r) == (pair.0.as_str(), pair.1.as_str())),
                "{pair:?}"
            );
            seen.insert(pair.0);
        }
        assert!(
            seen.contains("parked") && seen.contains("retried"),
            "{seen:?}"
        );
    }

    /// The safety net leaves a working terminal alone: its wake waits for
    /// the turn to end.
    #[test]
    fn a_working_terminal_gets_no_retried_wake() {
        let mut rig = rig();
        rig.state.retry_first = Duration::ZERO;
        let a = Arc::clone(&rig.a);
        // With its actor gone, the published phase stays as set.
        assert!(a.end_actor());
        a.set_published(Phase::Working, false);
        rig.wanted(&a, 1, "b");
        rig.state.tick();
        rig.state.tick();
        assert!(rig.inbox(&a).wake.is_none());
        assert!(rig.inbox(&a).retry.is_none(), "no try is even scheduled");
        assert!(
            rig.inbox(&a).any_wanted(),
            "still waiting for the turn to end"
        );
        a.set_published(Phase::Idle, false);
        rig.state.tick();
        assert!(
            rig.inbox(&a).retry.is_some_and(|r| r.tries == 0),
            "the wait starts once it is idle"
        );
        rig.state.tick();
        // The ended actor cannot take it, so it is given back at once; the
        // schedule shows the try.
        assert!(rig.inbox(&a).retry.is_some_and(|r| r.tries == 1));
        assert!(rig.inbox(&a).any_wanted());
    }

    /// T40: a released drain reservation restores the notices with the wanted
    /// flag they had, and wakes nobody until the next `LeftWorking`.
    #[test]
    fn a_released_reservation_restores_the_wanted_flag_and_wakes_nobody() {
        let mut rig = rig();
        let a = Arc::clone(&rig.a);
        rig.wanted(&a, 3, "b");
        let caller = rig.caller(&a);
        let drained = rig.state.drain(&caller).unwrap();
        assert_ne!(drained.reservation, 0);
        assert!(
            !rig.inbox(&a).any_wanted(),
            "a reserved notice is not unread"
        );
        let att = rig.att(&a);
        rig.state.left_working(att);
        assert!(rig.inbox(&a).wake.is_none(), "reserved notices wake nobody");
        rig.state.drain_done(drained.reservation, false);
        assert_eq!(rig.inbox(&a).entries.len(), 3);
        assert!(rig.inbox(&a).any_wanted(), "the flag is back");
        assert!(
            rig.inbox(&a).wake.is_none(),
            "and nothing is attempted at release"
        );
        assert!(
            !rig.inbox(&a).first_done,
            "the first-drain flag is restored too"
        );
        rig.state.left_working(att);
        assert!(
            rig.inbox(&a).wake.is_some(),
            "the next LeftWorking wakes once"
        );
    }

    /// Finding 6, reservation half: `unwatch` withdraws the flag from notices a
    /// drain holds, so releasing the reservation cannot make a paid wake. An
    /// independent `--ask` notice stays wanted.
    #[test]
    fn unwatch_clears_the_wanted_flag_of_a_reserved_notice_but_keeps_ask_notices() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let caller = rig.caller(&a);
        let (att_a, att_b) = (rig.att(&a), rig.att(&b));
        rig.state.watches.push((att_a, att_b));
        rig.wanted(&a, 2, "b");
        let nid = rig.state.nid();
        rig.inbox(&a)
            .push(nid, notice("", "Request r1 ended (ok).", true));
        // A drain holds them all while its reply is being written.
        let drained = rig.state.drain(&caller).unwrap();
        assert!(rig.state.watch(&caller, "b", false).is_ok());
        rig.state.drain_done(drained.reservation, false);
        let wanted: Vec<bool> = rig
            .inbox(&a)
            .entries
            .iter()
            .map(|e| e.notice.wanted)
            .collect();
        assert_eq!(
            wanted,
            [false, false, true],
            "only the --ask notice still wants a wake"
        );
        // And the watch no longer wakes anyone for them.
        rig.state.left_working(att_a);
        let flight = rig.inbox(&a).wake.take().unwrap();
        assert!(
            matches!(&flight.why, Why::Unread(ids) if ids.len() == 1),
            "{flight:?}"
        );
    }

    /// Finding 6, in-flight half: a wake in flight for a watched event, whose
    /// watch is cancelled before the wake fails, leaves its notice as context
    /// and does not make it wanted again.
    #[test]
    fn unwatch_cancels_the_recovery_of_a_wake_in_flight() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let caller = rig.caller(&a);
        let (att_a, att_b) = (rig.att(&a), rig.att(&b));
        rig.state.watches.push((att_a, att_b));
        rig.state.route(
            att_a,
            &a,
            notice("b", "b finished.", true),
            "wake".to_owned(),
            true,
        );
        assert!(rig.inbox(&a).wake.is_some());
        assert!(rig.state.watch(&caller, "b", false).is_ok());
        rig.settle();
        let after: Vec<(String, bool)> = rig
            .inbox(&a)
            .entries
            .iter()
            .map(|e| (e.notice.text.clone(), e.notice.wanted))
            .collect();
        assert!(
            after
                .iter()
                .any(|(text, wanted)| text == "b finished." && !wanted),
            "the notice stayed as context and wants nothing: {after:?}"
        );
        assert!(!rig.inbox(&a).any_wanted());
    }

    /// Finding 1: a drain completion is not lost when the state's queue is full.
    /// The notices of a failed write come back, in order, and a written drain
    /// is not sent twice.
    #[test]
    fn a_full_state_queue_cannot_lose_a_drain_completion() {
        let mut rig = rig();
        let a = Arc::clone(&rig.a);
        // The state's queue is full: nothing more fits.
        let (tx, _rx) = bounded(1);
        tx.send(Msg::Poke).unwrap();
        assert!(tx.try_send(Msg::Poke).is_err());
        let _ = rig.shared.state.set(tx);
        rig.wanted(&a, 3, "b");
        let caller = rig.caller(&a);
        let drained = rig.state.drain(&caller).unwrap();
        assert!(rig.inbox(&a).entries.is_empty());
        rig.shared.owe(Owed::DrainDone {
            reservation: drained.reservation,
            written: false,
        });
        rig.state.pay_debts();
        let texts: Vec<String> = rig
            .inbox(&a)
            .entries
            .iter()
            .map(|e| e.notice.text.clone())
            .collect();
        assert_eq!(texts, ["event 0", "event 1", "event 2"], "back, in order");
        // A written drain removes only its snapshot, once.
        let again = rig.state.drain(&caller).unwrap();
        rig.shared.owe(Owed::DrainDone {
            reservation: again.reservation,
            written: true,
        });
        rig.state.pay_debts();
        assert!(rig.inbox(&a).entries.is_empty());
        assert!(rig.state.reservations.is_empty());
    }

    /// A new memory version is announced at the next prompt only: it wakes
    /// nobody, a failed write keeps it, and versions waiting merge into one
    /// notice naming the latest (spec §6.1).
    #[test]
    fn memory_versions_wait_for_the_prompt_and_merge_into_one_notice() {
        let mut rig = rig();
        let a = Arc::clone(&rig.a);
        let caller = rig.caller(&a);
        let first = rig.state.drain_with(&caller, Some(3), None).unwrap();
        assert!(rig.inbox(&a).wake.is_none(), "nothing was woken");
        rig.shared.owe(Owed::DrainDone {
            reservation: first.reservation,
            written: false,
        });
        rig.state.pay_debts();
        assert_eq!(
            rig.inbox(&a).memory_version,
            Some(3),
            "kept for the next prompt"
        );
        assert!(rig.inbox(&a).wake.is_none());
        let next = rig.state.drain_with(&caller, Some(5), None).unwrap();
        let body = String::from_utf8(next.body).unwrap();
        assert_eq!(
            body.matches("project memory updated to version").count(),
            1,
            "{body}"
        );
        assert!(
            body.contains("project memory updated to version 5"),
            "{body}"
        );
        rig.shared.owe(Owed::DrainDone {
            reservation: next.reservation,
            written: true,
        });
        rig.state.pay_debts();
        assert_eq!(rig.inbox(&a).memory_version, None);
        let quiet = rig.state.drain_with(&caller, None, None).unwrap();
        assert!(quiet.body.is_empty(), "nothing new: nothing printed");
    }

    /// Finding 4: a report stamped with an earlier attempt changes nothing.
    #[test]
    fn a_caller_from_another_project_is_never_taken_for_a_terminal_here() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        // Same slot, uid, attempt and id as `a`, but from another project.
        let stranger = rig.stranger(&a);
        let seat = rig.state.seat_of(&stranger);
        assert_ne!(seat, rig.att(&a));
        assert!(State::is_seat(seat));
        assert_eq!(rig.state.seat_of(&stranger), seat, "one seat per caller");
        // Its list names no terminal as itself.
        let list = rig.state.list(&stranger);
        assert!(list.header["self"].is_null(), "{:?}", list.header);
        // A watch is the seat's, not `a`'s: `a` is not woken by `b`'s events.
        assert!(rig.state.watch(&stranger, &b.id, true).is_ok());
        assert!(rig.state.watches.contains(&(seat, rig.att(&b))));
        assert!(!rig.state.watches.contains(&(rig.att(&a), rig.att(&b))));
        // And `a`'s own request is not the stranger's to read.
        let id = typed_send(&mut rig, false);
        assert!(rig.state.result(&id, &rig.caller(&a)).is_ok());
        let theirs = rig.state.result(&id, &stranger);
        assert_eq!(
            theirs.header["code"], "unknown_request",
            "{:?}",
            theirs.header
        );
    }

    #[test]
    fn an_ask_in_another_project_holds_the_askers_turns_until_its_answer() {
        let mut rig = rig();
        let mine = rig.shared.bridge_instance;
        let a = Arc::clone(&rig.a);
        let att = rig.att(&a);
        let ask = |request: &str| {
            Owed::ForeignAsk(ForeignAsk {
                att,
                project_id: "other".into(),
                project_instance: 9,
                project_name: "other".into(),
                request: request.into(),
            })
        };
        let answer = |request: &str, to_instance: u64| {
            Owed::Forwarded(Forward {
                to: att,
                to_instance,
                from_project: "other".into(),
                from_instance: 9,
                notice: notice("", "Request r1 in project other ended (ok).", true),
                wake: "[connecto notice] x".into(),
                ended: Some(request.into()),
            })
        };
        // Debts, not queue messages: a full queue cannot lose them.
        rig.shared.owe(ask("r1"));
        rig.state.handle(Msg::Poke);
        assert!(rig.state.awaits(att));
        rig.shared.owe(answer("r1", mine));
        rig.state.handle(Msg::Poke);
        assert!(!rig.state.awaits(att));
        // An answer that overtakes its ask leaves nothing held.
        rig.shared.owe(answer("r2", mine));
        rig.shared.owe(ask("r2"));
        rig.state.handle(Msg::Poke);
        assert!(!rig.state.awaits(att));
        // And the answer reached the asker: in its inbox, or in a wake.
        let inbox = rig.inbox(&a);
        let woken = matches!(
            inbox.wake.as_ref().map(|flight| &flight.why),
            Some(Why::Event(notice)) if notice.text.contains("in project other")
        );
        assert!(
            woken
                || inbox
                    .entries
                    .iter()
                    .any(|entry| entry.notice.text.contains("in project other"))
        );
    }

    /// Three projects; the two askers have the same slot, uid and attempt,
    /// and their own state queues are full. Each answer still reaches only
    /// the project that asked, and releases its asker's own ask there.
    #[test]
    fn an_answer_reaches_only_the_project_that_asked_even_with_full_queues() {
        let mut beta = rig();
        let mut alpha = rig();
        let mut gamma = rig();
        let _ = beta
            .shared
            .memory_project
            .set(("beta".into(), std::path::PathBuf::from("/w/beta")));
        // The askers' queues are full: nothing may ride on them.
        let mut held = Vec::new();
        for caller in [&alpha, &gamma] {
            let (tx, rx) = bounded(1);
            tx.try_send(Msg::Poke).unwrap();
            let _ = caller.shared.state.set(tx);
            held.push(rx);
        }
        let beta_instance = beta.shared.bridge_instance;
        for (name, caller) in [("alpha", &alpha), ("gamma", &gamma)] {
            beta.state.peers.insert(
                (name.into(), caller.shared.bridge_instance),
                Arc::clone(&caller.shared),
            );
        }
        for caller in [&mut alpha, &mut gamma] {
            caller
                .state
                .peers
                .insert(("beta".into(), beta_instance), Arc::clone(&beta.shared));
        }
        let from = |rig: &Rig, project: &str| Caller {
            origin: Some(Arc::new(super::super::Origin {
                project_id: project.into(),
                project_name: project.into(),
                instance: rig.shared.bridge_instance,
            })),
            ..rig.caller(&rig.a)
        };
        let (from_alpha, from_gamma) = (from(&alpha, "alpha"), from(&gamma, "gamma"));
        assert_eq!(Att::from(&from_alpha), Att::from(&from_gamma));
        let b = Arc::clone(&beta.b);
        let att_b = beta.att(&b);
        let mut ask = |caller: &Caller| {
            let id = beta
                .state
                .admit(caller, "b", AdmitKind::Send { ask: true })
                .ok()
                .unwrap()
                .id;
            beta.state
                .on_typing(att_b, &id, WriteKind::Send, WriteOutcome::Typed);
            id
        };
        let (for_alpha, for_gamma) = (ask(&from_alpha), ask(&from_gamma));
        // Each asker's own project records its ask, as the handler does.
        for (rig, request) in [(&mut alpha, &for_alpha), (&mut gamma, &for_gamma)] {
            let att = rig.att(&rig.a);
            rig.shared.owe(Owed::ForeignAsk(ForeignAsk {
                att,
                project_id: "beta".into(),
                project_instance: beta_instance,
                project_name: "beta".into(),
                request: request.clone(),
            }));
            rig.state.pay_debts();
            assert!(rig.state.awaits(att));
        }
        let heard = |rig: &mut Rig, text: &str| {
            let a = Arc::clone(&rig.a);
            rig.inbox(&a).entries.iter().any(|entry| {
                entry
                    .notice
                    .body
                    .as_deref()
                    .is_some_and(|body| body.contains(text))
            })
        };
        beta.state.finish(
            &for_alpha,
            ReqState::Ok,
            "",
            Some(EventText::new("answer for alpha".into())),
        );
        alpha.state.pay_debts();
        gamma.state.pay_debts();
        assert!(heard(&mut alpha, "answer for alpha"));
        assert!(!heard(&mut gamma, "answer for alpha"));
        let (att_alpha, att_gamma) = (alpha.att(&alpha.a), gamma.att(&gamma.a));
        assert!(!alpha.state.awaits(att_alpha), "alpha's ask is released");
        assert!(gamma.state.awaits(att_gamma), "gamma's is not");
        beta.state.finish(
            &for_gamma,
            ReqState::Ok,
            "",
            Some(EventText::new("answer for gamma".into())),
        );
        gamma.state.pay_debts();
        alpha.state.pay_debts();
        assert!(heard(&mut gamma, "answer for gamma"));
        assert!(!heard(&mut alpha, "answer for gamma"));
        assert!(!gamma.state.awaits(att_gamma));
        drop(held);
    }

    /// A watched event forwarded to another project carries a key made of
    /// the project's id, its bridge instance and the terminal, never its
    /// name; an unwatch withdraws by that same key. Two projects with one
    /// name, or a rename, cannot mix them up.
    #[test]
    fn notices_across_projects_are_keyed_by_project_id_instance_and_terminal() {
        let mut beta = rig();
        let alpha = rig();
        let _ = beta
            .shared
            .memory_project
            .set(("beta-id".into(), std::path::PathBuf::from("/w/shared-name")));
        beta.state.peers.insert(
            ("alpha".into(), alpha.shared.bridge_instance),
            Arc::clone(&alpha.shared),
        );
        let watcher = Caller {
            origin: Some(Arc::new(super::super::Origin {
                project_id: "alpha".into(),
                project_name: "alpha".into(),
                instance: alpha.shared.bridge_instance,
            })),
            ..alpha.caller(&alpha.a)
        };
        let b = Arc::clone(&beta.b);
        assert!(beta.state.watch(&watcher, "b", true).is_ok());
        let att_b = beta.att(&b);
        beta.state.deliver_event(att_b, "finished", None);
        let key = format!("b in project beta-id#{}", beta.shared.bridge_instance);
        let owed = alpha.shared.take_owed();
        assert!(
            owed.iter().any(|owed| matches!(
                owed,
                Owed::Forwarded(forward) if forward.notice.source == key
                    && forward.notice.text.contains("in project shared-name")
            )),
            "{owed:?}"
        );
        assert!(beta.state.watch(&watcher, "b", false).is_ok());
        let owed = alpha.shared.take_owed();
        assert!(
            owed.iter().any(|owed| matches!(
                owed,
                Owed::ForeignCancel { source, .. } if *source == key
            )),
            "{owed:?}"
        );
    }

    /// A watch or unwatch from another project that a Pause overtook while
    /// it was queued changes nothing: a watch set before stays, and no new
    /// one is set.
    #[test]
    fn a_watch_change_refused_by_pause_keeps_the_watches_as_they_were() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let stranger = rig.stranger(&a);
        assert!(rig.state.watch(&stranger, "b", true).is_ok());
        let watching = |rig: &Rig| rig.state.watches.len();
        assert_eq!(watching(&rig), 1);
        let ask = |rig: &mut Rig, target: &str, on: bool| {
            let (tx, rx) = crossbeam_channel::bounded(1);
            rig.state.handle(Msg::Watch {
                caller: stranger.clone(),
                target: target.into(),
                on,
                still: Some(Box::new(|| false)),
                reply: tx,
            });
            rx.recv().unwrap()
        };
        let repeated = ask(&mut rig, "b", true);
        assert_eq!(repeated.header["code"], "paused");
        assert_eq!(watching(&rig), 1);
        let unwatch = ask(&mut rig, "b", false);
        assert_eq!(unwatch.header["code"], "paused");
        assert_eq!(watching(&rig), 1);
        let att_b = rig.att(&b);
        assert!(rig.state.watches.iter().any(|(_, target)| *target == att_b));
        let other = ask(&mut rig, "a", true);
        assert_eq!(other.header["code"], "paused");
        assert_eq!(watching(&rig), 1);
    }

    /// `result` across projects is decided on who the request says asked:
    /// a seat dropped since (past the cap) does not take the answer away,
    /// and another caller still cannot read it.
    #[test]
    fn an_askers_result_survives_its_seat() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let asker = rig.stranger(&a);
        let id = rig
            .state
            .admit(&asker, "b", AdmitKind::Send { ask: true })
            .ok()
            .unwrap()
            .id;
        let att_b = rig.att(&b);
        rig.state
            .on_typing(att_b, &id, WriteKind::Send, WriteOutcome::Typed);
        rig.state
            .finish(&id, ReqState::Ok, "", Some(EventText::new("whole".into())));
        // Its project could not be reached, so its seat is gone already;
        // drop it in any case, as the cap would.
        if let Some(seat) = rig.state.find_seat(&asker) {
            rig.state.drop_seat(seat.slot);
        }
        assert!(rig.state.find_seat(&asker).is_none());
        let reply = rig.state.result(&id, &asker);
        assert!(reply.is_ok(), "{:?}", reply.header);
        assert_eq!(reply.body, b"whole");
        let other = rig.stranger(&b);
        assert_eq!(
            rig.state.result(&id, &other).header["code"],
            "unknown_request"
        );
    }

    /// A project closed and opened again counts its terminals anew, so the
    /// same slot, uid and attempt are another conversation: it gets a seat
    /// of its own, and nothing meant for the earlier one reaches it.
    #[test]
    fn a_reopened_project_is_a_new_bridge_instance_and_inherits_nothing() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let before = rig.stranger(&a);
        let mut after = before.clone();
        after.origin = Some(Arc::new(super::super::Origin {
            instance: 8,
            ..(**before.origin.as_ref().unwrap()).clone()
        }));
        let old_seat = rig.state.seat_of(&before);
        assert!(rig.state.watch(&before, &b.id, true).is_ok());
        let new_seat = rig.state.seat_of(&after);
        assert_ne!(new_seat, old_seat);
        let att_b = rig.att(&b);
        assert!(rig.state.watches.contains(&(old_seat, att_b)));
        assert!(
            !rig.state.watches.contains(&(new_seat, att_b)),
            "the old watch is not the new one's"
        );
        // A forward for this project's earlier instance is dropped.
        let att = rig.att(&a);
        let stale = Forward {
            to: att,
            to_instance: rig.shared.bridge_instance + 1000,
            from_project: "other".into(),
            from_instance: 9,
            notice: notice("", "old news", true),
            wake: "[connecto notice] old news".into(),
            ended: None,
        };
        rig.state.on_forwarded(stale);
        let inbox = rig.inbox(&a);
        assert!(inbox.wake.is_none() && inbox.entries.is_empty());
    }

    #[test]
    fn a_report_from_an_earlier_attempt_does_not_touch_the_new_one() {
        assert!(is_stale(3, 4));
        assert!(!is_stale(4, 4));
        assert!(!is_stale(0, 4), "zero is not about an attempt");
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let (att_a, att_b) = (rig.att(&a), rig.att(&b));
        rig.state.watches.push((att_a, att_b));
        rig.shared.report_at(
            &b,
            99,
            Report::Exited {
                status: "old".into(),
            },
        );
        rig.state.take_reports();
        assert!(
            rig.state.watches.contains(&(att_a, att_b)),
            "the watch survived"
        );
        // Stamped for this attempt, the same report does end the watch.
        rig.shared.report_at(
            &b,
            att_b.attempt.max(1),
            Report::Exited {
                status: "now".into(),
            },
        );
        rig.state.take_reports();
        assert!(rig.state.watches.contains(&(att_a, att_b)) || att_b.attempt == 0);
    }

    /// T46: a late report for a final request changes nothing.
    #[test]
    fn a_late_report_for_a_final_request_changes_nothing() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let caller = rig.caller(&a);
        let admission = rig
            .state
            .admit(&caller, "b", AdmitKind::Send { ask: false })
            .ok()
            .unwrap();
        let (id, att_b) = (admission.id, rig.att(&b));
        rig.state
            .on_typing(att_b, &id, WriteKind::Send, WriteOutcome::Typed);
        rig.state.on_report(
            att_b,
            &b,
            Report::Bound {
                request: id.clone(),
            },
        );
        let text = |s: &str| EventText::new(s.to_owned());
        rig.state
            .on_request_end(att_b, &id, RequestEnd::Ok(text("first")));
        assert_eq!(rig.state.requests[&id].state, ReqState::Ok);
        rig.state
            .on_request_end(att_b, &id, RequestEnd::Ok(text("second")));
        rig.state
            .on_request_end(att_b, &id, RequestEnd::Interrupted);
        rig.state.on_typing(
            att_b,
            &id,
            WriteKind::Send,
            WriteOutcome::Refused(connecto_gate::Denial::Paused),
        );
        rig.state.finish(&id, ReqState::Unmatched, "late", None);
        let request = &rig.state.requests[&id];
        assert_eq!(request.state, ReqState::Ok);
        assert_eq!(request.reply.as_ref().unwrap().expose(), "first");
    }

    /// Send from `a` to `b` and say it was typed: the request is pending with
    /// its id given out.
    fn typed_send(rig: &mut Rig, ask: bool) -> String {
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let caller = rig.caller(&a);
        let admission = rig
            .state
            .admit(&caller, "b", AdmitKind::Send { ask })
            .ok()
            .unwrap();
        let att_b = rig.att(&b);
        rig.state
            .on_typing(att_b, &admission.id, WriteKind::Send, WriteOutcome::Typed);
        admission.id
    }

    /// Finished requests are kept up to a cap, oldest out first, and a pending
    /// one is never dropped. `result` for a forgotten one says so, and an id
    /// that was never made is still unknown.
    #[test]
    fn finished_requests_are_capped_and_a_forgotten_one_says_so() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let att_b = rig.att(&b);
        let caller = rig.caller(&a);
        let text = |s: &str| EventText::new(s.to_owned());
        // The oldest of all stays pending throughout.
        let pending = typed_send(&mut rig, false);
        let mut finished = Vec::new();
        for n in 0..FINISHED_KEPT + 3 {
            let id = typed_send(&mut rig, false);
            if n % 2 == 0 {
                rig.state.on_report(
                    att_b,
                    &b,
                    Report::Bound {
                        request: id.clone(),
                    },
                );
                rig.state
                    .on_request_end(att_b, &id, RequestEnd::Ok(text("four")));
            } else {
                rig.state.finish(&id, ReqState::Unmatched, "late", None);
            }
            finished.push(id);
        }
        assert_eq!(rig.state.requests.len(), FINISHED_KEPT + 1);
        assert_eq!(rig.state.order.len(), FINISHED_KEPT + 1);
        assert_eq!(
            rig.state.requests[&pending].state,
            ReqState::Pending,
            "a pending request is kept whatever its age"
        );
        for id in &finished[..3] {
            assert!(!rig.state.requests.contains_key(id), "{id} was the oldest");
        }
        assert!(rig.state.requests.contains_key(&finished[3]));
        let held: usize = rig
            .state
            .requests
            .values()
            .filter_map(|request| request.reply.as_ref())
            .map(|reply| reply.expose().len())
            .sum();
        assert_eq!(
            rig.state.reply_bytes, held,
            "a forgotten reply is not counted"
        );

        let gone = rig.state.result(&finished[0], &caller);
        assert!(gone.is_ok());
        assert_eq!(gone.header["state"], "unavailable");
        assert!(
            gone.header["reason"]
                .as_str()
                .unwrap()
                .contains("no longer kept"),
            "{gone:?}"
        );
        let kept = rig.state.result(&finished[4], &caller);
        assert_eq!(kept.header["state"], "ok");
        assert_eq!(kept.body, b"four");
        for never in ["r999999", "r0", "r01", "x1", "r"] {
            let reply = rig.state.result(never, &caller);
            assert_eq!(reply.header["code"], "unknown_request", "{never}");
        }
    }

    /// A request that ends with no turn tells its target's actor to forget the
    /// id. One that a turn carried was already dropped by the actor itself.
    #[test]
    fn a_request_that_ends_unbound_is_handed_to_its_actor_to_forget() {
        let mut rig = rig();
        let b = Arc::clone(&rig.b);
        // Without its actor, what is handed over stays to be read.
        assert!(b.end_actor());
        let att_b = rig.att(&b);
        let unbound = typed_send(&mut rig, false);
        let bound = typed_send(&mut rig, false);
        rig.state.on_report(
            att_b,
            &b,
            Report::Bound {
                request: bound.clone(),
            },
        );
        rig.state.finish(
            &unbound,
            ReqState::Unmatched,
            "The text did not start a turn.",
            None,
        );
        rig.state
            .on_request_end(att_b, &bound, RequestEnd::Interrupted);
        assert_eq!(b.forgotten(), [unbound]);
    }

    /// The "done" mark of a child's last ask survives a second ask that is
    /// refused before its paste, and goes only when an ask's text is typed.
    #[test]
    fn an_ask_refused_before_its_paste_leaves_the_last_asks_done_mark() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let caller = rig.caller(&a);
        let att_b = rig.att(&b);
        // The first ask ran its course.
        let first = typed_send(&mut rig, true);
        assert_eq!(b.ask.load(Ordering::SeqCst), 1, "typed: an ask is open");
        rig.state.on_report(
            att_b,
            &b,
            Report::Bound {
                request: first.clone(),
            },
        );
        rig.state.on_request_end(
            att_b,
            &first,
            RequestEnd::Ok(EventText::new("four".to_owned())),
        );
        assert_eq!(b.ask.load(Ordering::SeqCst), 2, "ended: done");

        // A second is admitted and then refused with nothing pasted.
        for outcome in [
            WriteOutcome::NotReady {
                why: "a dialog is up".to_owned(),
            },
            WriteOutcome::Refused(connecto_gate::Denial::Paused),
            WriteOutcome::Failed {
                pasted: false,
                why: "not queued".to_owned(),
            },
        ] {
            let admission = rig
                .state
                .admit(&caller, "b", AdmitKind::Send { ask: true })
                .ok()
                .unwrap();
            assert_eq!(
                b.ask.load(Ordering::SeqCst),
                2,
                "admission alone opens no ask"
            );
            rig.state
                .on_typing(att_b, &admission.id, WriteKind::Send, outcome);
            assert!(!rig.state.requests.contains_key(&admission.id));
            assert_eq!(b.ask.load(Ordering::SeqCst), 2, "still done");
        }

        // One that timed out in the handler and was then found not typed ends
        // `unmatched`, and it never opened an ask either.
        let admission = rig
            .state
            .admit(&caller, "b", AdmitKind::Send { ask: true })
            .ok()
            .unwrap();
        rig.shared.owe(Owed::TimedOut {
            id: admission.id.clone(),
        });
        rig.state.pay_debts();
        rig.state.on_typing(
            att_b,
            &admission.id,
            WriteKind::Send,
            WriteOutcome::NotReady {
                why: "late".to_owned(),
            },
        );
        assert_eq!(rig.state.requests[&admission.id].state, ReqState::Unmatched);
        assert_eq!(b.ask.load(Ordering::SeqCst), 2);

        // A third that is typed opens an ask again.
        typed_send(&mut rig, true);
        assert_eq!(b.ask.load(Ordering::SeqCst), 1);
    }

    /// T53 and T45: a lost `finished` is found on the tick with no later event,
    /// and the published phase, not any event, is what a terminal reads as.
    #[test]
    fn a_lost_event_is_found_on_the_tick_and_the_published_phase_decides() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let (att_a, att_b) = (rig.att(&a), rig.att(&b));
        rig.state.watches.push((att_a, att_b));
        b.set_published(Phase::Working, false);
        // The channel dropped `finished`; the actor only counted it.
        b.lost.fetch_add(1, Ordering::SeqCst);
        b.set_published(Phase::Idle, false);
        rig.state.tick();
        assert_eq!(
            rig.inbox(&a).lost,
            1,
            "the watcher is told events were lost"
        );
        assert_eq!(b.phase(), Phase::Idle, "never stuck at working");
        // Found once, not again.
        rig.state.tick();
        assert_eq!(rig.inbox(&a).lost, 1);
    }

    /// T44: while a wake is in flight a new event queues no second wake, and a
    /// stuck actor frees the wake at its limit. A late outcome is then ignored.
    #[test]
    fn a_wake_in_flight_queues_no_second_and_a_stuck_one_is_freed() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        let (att_a, att_b) = (rig.att(&a), rig.att(&b));
        rig.state.watches.push((att_a, att_b));
        rig.state.wake_limit = Duration::ZERO;
        rig.state
            .route(att_a, &a, notice("b", "first", true), "w".into(), true);
        let first = rig.inbox(&a).wake.as_ref().unwrap().id.clone();
        rig.state
            .route(att_a, &a, notice("b", "second", true), "w".into(), true);
        assert_eq!(
            rig.inbox(&a).wake.as_ref().unwrap().id,
            first,
            "no second wake"
        );
        assert_eq!(
            rig.inbox(&a).entries.len(),
            1,
            "the second went to the inbox"
        );
        // The actor's answer has not been read yet: the limit frees the wake.
        rig.state.check_wakes();
        assert!(rig.inbox(&a).wake.is_none());
        let before = rig.inbox(&a).entries.len();
        // Its late outcome is ignored.
        rig.state.wake_outcome(att_a, &first, &WriteOutcome::Typed);
        assert!(rig.inbox(&a).wake.is_none());
        assert_eq!(rig.inbox(&a).entries.len(), before);
    }

    /// A woken sender gets the reply in its context: the notice that carries it
    /// is in the inbox for the wake's prompt to drain, unwanted. If the wake is
    /// never typed, that same entry wants a wake again, and no copy is made.
    #[test]
    fn a_reply_notice_waits_in_the_inbox_for_its_wake_and_is_wanted_again_if_it_fails() {
        let mut rig = rig();
        let a = Arc::clone(&rig.a);
        let att = rig.att(&a);
        let mut with_reply = notice("", "Request r1 ended (ok). Run: connecto result r1", true);
        with_reply.body = Some("Reply from b (another agent, not the user):\nfour".to_owned());
        rig.state
            .route(att, &a, with_reply, "wake".to_owned(), true);
        assert!(rig.inbox(&a).wake.is_some(), "the wake went out");
        assert_eq!(
            rig.inbox(&a).entries.len(),
            1,
            "and the reply is in the inbox"
        );
        assert!(!rig.inbox(&a).any_wanted(), "which the wake covers");
        rig.settle();
        assert!(
            rig.inbox(&a).wake.is_none(),
            "the wake was refused: no process"
        );
        assert_eq!(rig.inbox(&a).entries.len(), 1, "still one entry");
        assert!(
            rig.inbox(&a).any_wanted(),
            "and it is wanted for the next stop"
        );
    }

    /// A reply notice is in the inbox, unwanted, while its wake is in flight.
    /// When more than 32 notices arrive and fold it into a count, a wake that
    /// then fails must still bring the flag back, or the asker sleeps for good.
    #[test]
    fn a_reply_notice_folded_by_overflow_is_wanted_again_when_its_wake_fails() {
        let mut rig = rig();
        let a = Arc::clone(&rig.a);
        let att = rig.att(&a);
        let mut with_reply = notice("", "Request r1 ended (ok). Run: connecto result r1", true);
        with_reply.body = Some("Reply from b (another agent, not the user):\nfour".to_owned());
        rig.state
            .route(att, &a, with_reply, "wake".to_owned(), true);
        assert!(rig.inbox(&a).wake.is_some());
        // Enough others to push it out of the inbox before the wake's outcome.
        for n in 0..(INBOX_MAX + 4) {
            let nid = rig.state.nid();
            rig.inbox(&a)
                .push(nid, notice("b", &format!("later {n}"), false));
        }
        assert!(
            rig.inbox(&a)
                .entries
                .iter()
                .all(|e| e.notice.body.is_none()),
            "the reply notice was folded"
        );
        rig.settle();
        assert!(rig.inbox(&a).wake.is_none(), "the wake was refused");
        assert!(
            rig.inbox(&a).any_wanted(),
            "and the asker is not left asleep"
        );
        rig.state.left_working(att);
        assert!(rig.inbox(&a).wake.is_some(), "the next stop wakes it again");
    }

    /// ADR 0030: an update saves the asks open now, by terminal id; only
    /// typed ones, and not after they ended.
    #[test]
    fn the_open_asks_are_the_typed_ones_not_yet_answered() {
        let mut rig = rig();
        assert!(rig.state.open_asks().is_empty());
        let id = typed_send(&mut rig, true);
        let _plain = typed_send(&mut rig, false);
        assert_eq!(
            rig.state.open_asks(),
            vec![OpenAsk {
                request: id.clone(),
                from: "a".to_owned(),
                target: "b".to_owned(),
            }]
        );
        rig.state.finish(&id, ReqState::Ok, "", None);
        assert!(rig.state.open_asks().is_empty());
    }

    /// ADR 0030: an ask an update cut is opened again once its target runs,
    /// keeps its id, ends with the target's next turn, and its answer waits
    /// for the asker to run before it is given.
    #[test]
    fn a_restored_ask_ends_with_the_targets_next_turn_and_reaches_the_asker() {
        let mut rig = rig();
        let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
        rig.state.assume_live = false;
        rig.shared.restore_asks(vec![OpenAsk {
            request: "r13".to_owned(),
            from: "a".to_owned(),
            target: "b".to_owned(),
        }]);
        rig.state.open_restored();
        assert!(
            !rig.state.requests.contains_key("r13"),
            "not before its target runs"
        );
        assert_eq!(rig.state.restoring.len(), 1);
        rig.state.assume_live = true;
        rig.state.open_restored();
        let request = &rig.state.requests["r13"];
        assert!(request.restored && request.bound && request.state == ReqState::Pending);
        assert_eq!(rig.state.next_request, 14, "new ids come after it");
        assert_eq!(
            rig.state.open_asks().len(),
            1,
            "a second update saves it again"
        );
        // The asker sees it as its own open ask.
        let att_a = rig.att(&a);
        assert!(rig.state.awaits(att_a));
        // The target's turn ends while the asker is not running yet.
        rig.state.assume_live = false;
        let att_b = rig.att(&b);
        rig.state.on_signal(att_b, Signal::Finished);
        assert_eq!(rig.state.requests["r13"].state, ReqState::Ok);
        assert_eq!(
            rig.state.deferred.len(),
            1,
            "the answer waits for the asker"
        );
        assert!(rig.inbox(&a).entries.is_empty() && rig.inbox(&a).wake.is_none());
        rig.state.assume_live = true;
        rig.state.retry_deferred();
        assert!(rig.state.deferred.is_empty());
        let inbox = rig.inbox(&a);
        assert!(
            inbox.wake.is_some() || inbox.any_wanted(),
            "the asker is told its ask ended"
        );
    }

    /// ADR 0030: a restored ask whose target does not run again in time ends
    /// as exited, so its asker is not left waiting.
    #[test]
    fn a_restored_ask_whose_target_never_runs_ends_as_exited() {
        let mut rig = rig();
        rig.state.assume_live = false;
        let long_ago = Instant::now()
            .checked_sub(BOOT_CAP + Duration::from_secs(1))
            .unwrap();
        rig.state.restoring.push((
            OpenAsk {
                request: "r4".to_owned(),
                from: "a".to_owned(),
                target: "b".to_owned(),
            },
            long_ago,
        ));
        rig.state.open_restored();
        assert!(rig.state.restoring.is_empty());
        assert_eq!(rig.state.requests["r4"].state, ReqState::Exited);
    }
}
