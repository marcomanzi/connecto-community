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
use crate::relay::{Shared, Slot};
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
}

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
        reply: Sender<Reply>,
    },
    Drain {
        caller: Caller,
        reply: Sender<Option<Drained>>,
    },
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

/// One terminal attempt's notices.
#[derive(Debug, Default)]
pub(crate) struct Inbox {
    entries: VecDeque<Entry>,
    /// Per source: how many notices were merged, and whether any was wanted.
    merged: BTreeMap<String, Merged>,
    lost: u32,
    first_done: bool,
    wake: Option<WakeFlight>,
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
    note: Option<&'static str>,
    state: ReqState,
    reason: String,
    reply: Option<EventText>,
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
    next_request: u64,
    next_key: u64,
    next_wake: u64,
    next_reservation: u64,
    next_nid: u64,
    /// Tests only: treat every terminal as having a live process.
    assume_live: bool,
    /// How long a wake may wait for an outcome. A field so a test can shorten it.
    wake_limit: Duration,
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
            next_request: 1,
            next_key: 1,
            next_wake: 1,
            next_reservation: 1,
            next_nid: 1,
            assume_live: false,
            wake_limit: WAKE_QUEUE_LIMIT,
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
                reply,
            } => {
                let _ = reply.send(self.watch(&caller, &target, on));
            }
            Msg::Drain { caller, reply } => {
                let _ = reply.send(self.drain(&caller));
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
            }
        }
    }

    fn tick(&mut self) {
        self.reconcile();
        self.pay_debts();
        self.take_reports();
        self.check_lost();
        self.check_requests();
        self.check_wakes();
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
                    // A late outcome after a timeout: nothing was typed.
                    let note = "The text was not typed.";
                    self.finish(id, ReqState::Unmatched, note, None);
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

    fn notify_sender(&mut self, request: &str, from: Att, state: &'static str) {
        // A notice belongs to the conversation that asked.
        let Some(sender) = self.slot_of(from) else {
            return;
        };
        if !self.live(&sender) {
            return;
        }
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
            for watcher in watchers {
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
        let from = Att::from(caller);
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
                    from_id: caller.id.clone(),
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
                    note: None,
                    state: ReqState::Pending,
                    reason: String::new(),
                    reply: None,
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
        let mut reason = request.reason.clone();
        if request.state == ReqState::Pending {
            let background = self
                .shared
                .slot(request.target.slot)
                .is_some_and(|slot| slot.phase() == Phase::Background);
            // The sender of an `--ask` is woken with the reply: it should end its
            // turn and wait, not poll.
            let ours = request.ask && request.from == Att::from(caller);
            reason = match (background, ours) {
                (true, true) => "The target has background work running, so its turn has not ended. The reply will be delivered to you automatically; end your turn and wait.",
                (true, false) => "The target has background work running, so its turn has not ended. Use read.",
                (false, true) => "The reply will be delivered to you automatically; end your turn and wait. Do not poll.",
                (false, false) => "The target has not answered yet.",
            }
            .to_owned();
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
        let from = Att::from(caller);
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
            self.cancel_wanted(from, target);
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
        let from = Att::from(caller);
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
                Some(json!({
                    "id": entry.id,
                    "tool": entry.tool.name(),
                    "state": Self::state_word(&slot),
                    "seconds": seconds,
                    "last_event": last,
                    "watched": watched,
                    "bridge": if off.is_some() { "off" } else { "on" },
                    "bridge_reason": off,
                    "warning": slot.pane.power_warning(),
                    "exit": Self::exit_text(&slot, att),
                }))
            })
            .collect();
        Reply::ok(json!({
            "self": caller.id,
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

    fn drain(&mut self, caller: &Caller) -> Option<Drained> {
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
        let root = std::env::temp_dir().join(format!("mg-state-{}", std::process::id()));
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

    /// Finding 4: a report stamped with an earlier attempt changes nothing.
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
}
