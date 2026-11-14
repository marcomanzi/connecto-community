//! The turn tracker: tool events in, phases, bridge events and request endings
//! out (section 5.8).
//!
//! It is a pure state machine. The session actor feeds it hook events, marks
//! read from the tool's turn log, and the clock, and acts on what it returns.
//! Nothing here reads a file or a clock by itself, so every rule can be tested
//! by playing events in.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use connecto_adapter::{LogMark, ToolAdapter};
use connecto_core::{EventText, Reply, ToolEvent};

/// How long a Codex `task_complete` waits for the `notify` that normally
/// reports the same end.
pub const TASK_COMPLETE_WAIT: Duration = Duration::from_secs(5);
/// Ended Codex turn ids remembered per attempt.
const ENDED_KEEP: usize = 64;
/// Codex turn starts remembered, for the stale-fallback guard.
const STARTED_KEEP: usize = 64;
/// Log marks kept for Claude's pairing.
const MARKS_KEEP: usize = 512;
/// How much of a prompt identifies it.
const HEAD: usize = 256;

/// What a running terminal is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// A turn is running.
    Working,
    /// The tool is at its prompt and background work continues.
    Background,
}

/// The five events the bridge reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeEvent {
    Began,
    Finished,
    Interrupted,
    NeedsUser,
    Exited(String),
}

impl BridgeEvent {
    /// The word `list` and the notices use.
    #[must_use]
    pub const fn word(&self) -> &'static str {
        match self {
            Self::Began => "began",
            Self::Finished => "finished",
            Self::Interrupted => "interrupted",
            Self::NeedsUser => "needs-user",
            Self::Exited(_) => "exited",
        }
    }
}

/// How a request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestEnd {
    Ok(EventText),
    Interrupted,
    Exited,
    /// The evidence could not show which turn answered.
    Unmatched(&'static str),
    /// The reply was over the limit, a frame was lost, or there was none.
    Unavailable(&'static str),
}

/// What the tracker tells the actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    Event(BridgeEvent),
    /// The phase left `Working`. Not an event: never reported.
    LeftWorking,
    /// A request id was found in a submitted prompt.
    Bound(String),
    Ended {
        request: String,
        end: RequestEnd,
    },
    /// An accepted turn end, for the manual relay.
    Turn {
        input: Option<EventText>,
        reply: Reply,
    },
    /// A prompt was accepted.
    Prompt,
}

/// Request ids in `prompt`: `[connecto from <id>, request r<n>.` at the start of
/// any line. An id counts only if that request was sent to this terminal and
/// is unbound, which the bridge decides.
#[must_use]
pub fn request_ids(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("[connecto from ")?;
            let (from, rest) = rest.split_once(", request r")?;
            let valid_from = crate::terminal_list::valid_id(from);
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            let after = &rest[digits.len()..];
            (valid_from && !digits.is_empty() && after.starts_with('.'))
                .then(|| format!("r{digits}"))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Claude,
    Codex,
}

/// A prompt the tool accepted and whose turn has not ended.
#[derive(Debug, Clone)]
struct Open {
    seq: u64,
    requests: Vec<String>,
    turn: Option<String>,
    ambiguous: bool,
    head: String,
    /// A self-started turn that continues a waiting turn.
    continues: Option<u64>,
}

/// A closed turn whose background work is still running.
#[derive(Debug, Clone)]
struct Waiting {
    seq: u64,
    requests: Vec<String>,
    ambiguous: bool,
    reply: Reply,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mark {
    Prompt(String),
    Final,
}

struct PendingComplete {
    turn: String,
    due: Instant,
    reply: Reply,
}

/// One terminal attempt's tracker.
pub struct Tracker {
    adapter: &'static dyn ToolAdapter,
    kind: Kind,
    phase: Phase,
    needs_user: bool,
    open: Vec<Open>,
    waiting: Vec<Waiting>,
    next_seq: u64,
    /// Claude: task id to the turn that owns it.
    task_owner: BTreeMap<String, u64>,
    marks: Vec<Mark>,
    /// Codex.
    ended: VecDeque<String>,
    /// Codex: turn ids the log shows as started, most recent last.
    started: VecDeque<String>,
    late_expected: u32,
    pinned: Option<String>,
    input_seen: usize,
    pending: Vec<PendingComplete>,
    /// Whether a `task_complete` with no `notify` is an end. Off when the two
    /// ids are found to differ (Check 1), because a late `notify` could then
    /// not be recognised.
    fallback: bool,
}

fn head_of(adapter: &dyn ToolAdapter, text: &str) -> String {
    adapter.ack_form(&text.chars().take(HEAD).collect::<String>())
}

fn same_head(a: &str, b: &str) -> bool {
    let len = a.len().min(b.len());
    len > 0 && a.is_char_boundary(len) && b.is_char_boundary(len) && a[..len] == b[..len]
}

impl Tracker {
    #[must_use]
    pub fn new(adapter: &'static dyn ToolAdapter, codex: bool) -> Self {
        Self {
            adapter,
            kind: if codex { Kind::Codex } else { Kind::Claude },
            phase: Phase::Idle,
            needs_user: false,
            open: Vec::new(),
            waiting: Vec::new(),
            next_seq: 1,
            task_owner: BTreeMap::new(),
            marks: Vec::new(),
            ended: VecDeque::new(),
            started: VecDeque::new(),
            late_expected: 0,
            pinned: None,
            input_seen: 0,
            pending: Vec::new(),
            fallback: true,
        }
    }

    /// Switch the `task_complete` fallback off.
    pub fn disable_fallback(&mut self) {
        self.fallback = false;
    }

    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    #[must_use]
    pub const fn needs_user(&self) -> bool {
        self.needs_user
    }

    /// Prompts accepted and not yet ended.
    #[must_use]
    pub fn open_prompts(&self) -> usize {
        self.open.len()
    }

    /// Whether a turn log should be tailed now.
    #[must_use]
    pub fn wants_log(&self) -> bool {
        !self.open.is_empty() || !self.pending.is_empty()
    }

    fn is_codex(&self) -> bool {
        self.kind == Kind::Codex
    }

    fn end_requests(out: &mut Vec<Out>, requests: &[String], end: &RequestEnd) {
        for request in requests {
            out.push(Out::Ended {
                request: request.clone(),
                end: end.clone(),
            });
        }
    }

    fn reply_end(reply: &Reply, ambiguous: bool) -> RequestEnd {
        if ambiguous {
            return RequestEnd::Unmatched("the evidence could not show which turn answered");
        }
        match reply {
            Reply::Text(text) => RequestEnd::Ok(text.clone()),
            Reply::Dropped => RequestEnd::Unavailable("the reply was over the limit or cut"),
            Reply::None => RequestEnd::Unavailable("the turn reported no reply"),
        }
    }

    fn set_phase(&mut self, phase: Phase, out: &mut Vec<Out>) {
        let was = self.phase;
        self.phase = phase;
        if was == Phase::Working && phase != Phase::Working {
            out.push(Out::LeftWorking);
        }
    }

    // ---- events from the tool's hooks

    /// A hook event.
    pub fn on_event(&mut self, event: ToolEvent, _now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        match event {
            ToolEvent::Started { tool_session, .. } => {
                if self.is_codex() {
                    // Every SessionStart updates the pinned thread, and a new
                    // pin resets the count of inputs seen.
                    let thread = tool_session.expose().clone();
                    if self.pinned.as_deref() != Some(thread.as_str()) {
                        self.input_seen = 0;
                    }
                    self.pinned = Some(thread);
                }
            }
            ToolEvent::PromptSubmitted { prompt, turn } => {
                self.prompt(&prompt, turn.map(|turn| turn.expose().clone()), &mut out);
            }
            ToolEvent::TurnEnded {
                input,
                input_total,
                reply,
                lifecycle_known,
                background,
                thread,
                turn,
            } => {
                if !lifecycle_known {
                    self.incomplete(&mut out);
                } else if self.is_codex() {
                    self.codex_notify(
                        input,
                        input_total,
                        reply,
                        thread.map(|t| t.expose().clone()),
                        turn.map(|t| t.expose().clone()),
                        &mut out,
                    );
                } else {
                    self.claude_stop(reply, &background, &mut out);
                }
            }
            ToolEvent::NeedsUser => {
                self.needs_user = true;
                out.push(Out::Event(BridgeEvent::NeedsUser));
            }
        }
        out
    }

    fn prompt(&mut self, prompt: &EventText, turn: Option<String>, out: &mut Vec<Out>) {
        let text = prompt.expose();
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut open = Open {
            seq,
            requests: Vec::new(),
            turn,
            ambiguous: false,
            head: head_of(self.adapter, text),
            continues: None,
        };
        if !self.is_codex()
            && let Some(continuation) = self.adapter.continuation(text)
        {
            self.continuation(&mut open, continuation.task.as_deref());
        } else {
            open.requests = request_ids(text);
            for request in &open.requests {
                out.push(Out::Bound(request.clone()));
            }
        }
        self.open.push(open);
        self.needs_user = false;
        out.push(Out::Prompt);
        if self.phase != Phase::Working {
            self.phase = Phase::Working;
            out.push(Out::Event(BridgeEvent::Began));
        }
    }

    /// A self-started turn continues a waiting one only when ownership is
    /// clear: it names a task owned by that turn, or names none and every
    /// running task belongs to one turn. Otherwise each waiting turn it might
    /// belong to is marked ambiguous.
    fn continuation(&mut self, open: &mut Open, task: Option<&str>) {
        let owner = match task {
            Some(task) => self.task_owner.get(task).copied(),
            None => {
                let mut owners = self.task_owner.values().copied();
                owners
                    .next()
                    .filter(|first| owners.all(|other| other == *first))
            }
        };
        match owner.filter(|seq| self.waiting.iter().any(|w| w.seq == *seq)) {
            Some(seq) => open.continues = Some(seq),
            None => {
                for waiting in &mut self.waiting {
                    waiting.ambiguous = true;
                }
            }
        }
    }

    // ---- Claude

    /// Which open prompts a Claude `Stop` closes: `None` means no evidence.
    fn claude_split(&self) -> Option<(Vec<usize>, Vec<usize>, Vec<usize>)> {
        // (belong, older, later) as indexes into `open`.
        let finals: Vec<usize> = self
            .marks
            .iter()
            .enumerate()
            .filter(|(_, mark)| **mark == Mark::Final)
            .map(|(index, _)| index)
            .collect();
        let last = *finals.last()?;
        let previous = finals.len().checked_sub(2).map(|i| finals[i]);
        let mut belong = Vec::new();
        let mut older = Vec::new();
        let mut later = Vec::new();
        let mut cursor = 0;
        for (index, open) in self.open.iter().enumerate() {
            let at =
                self.marks
                    .iter()
                    .enumerate()
                    .skip(cursor)
                    .find_map(|(i, mark)| match mark {
                        Mark::Prompt(text) if same_head(text, &open.head) => Some(i),
                        _ => None,
                    })?;
            cursor = at + 1;
            if at > last {
                later.push(index);
            } else if previous.is_some_and(|p| at < p) {
                older.push(index);
            } else {
                belong.push(index);
            }
        }
        Some((belong, older, later))
    }

    fn claude_stop(&mut self, reply: Reply, background: &[EventText], out: &mut Vec<Out>) {
        out.push(Out::Turn {
            input: None,
            reply: reply.clone(),
        });
        if self.open.is_empty() {
            // A self-started turn whose prompt hook was lost, or a stray end.
            self.after_close(background, None, out);
            return;
        }
        let mut closed: Vec<Open> = Vec::new();
        if self.open.len() == 1 {
            closed.push(self.open.remove(0));
        } else if let Some((belong, older, _later)) =
            self.claude_split().filter(|(b, _, _)| !b.is_empty())
        {
            // Highest index first, so removals do not shift the rest.
            let mut remove: Vec<usize> = belong.iter().chain(&older).copied().collect();
            remove.sort_unstable_by(|a, b| b.cmp(a));
            let mut taken: Vec<(usize, Open)> = remove
                .into_iter()
                .map(|index| (index, self.open.remove(index)))
                .collect();
            taken.sort_by_key(|(index, _)| *index);
            for (index, mut prompt) in taken {
                if older.contains(&index) {
                    // It had its own turn, and that end was never reported.
                    Self::end_requests(
                        out,
                        &prompt.requests,
                        &RequestEnd::Unavailable("that turn's end was never reported"),
                    );
                    prompt.requests.clear();
                }
                if belong.contains(&index) {
                    closed.push(prompt);
                }
            }
        } else {
            // No evidence: the oldest closes, and every prompt open is ambiguous.
            for open in &mut self.open {
                open.ambiguous = true;
            }
            closed.push(self.open.remove(0));
        }
        let owner = closed
            .iter()
            .find_map(|prompt| prompt.continues)
            .unwrap_or_else(|| closed.first().map_or(0, |prompt| prompt.seq));
        // Each closed prompt's requests wait on background work or end now.
        let mut requests: Vec<String> = Vec::new();
        let mut ambiguous = false;
        for prompt in &closed {
            requests.extend(prompt.requests.iter().cloned());
            ambiguous |= prompt.ambiguous;
        }
        if closed.iter().all(|prompt| prompt.continues.is_some()) && !closed.is_empty() {
            // The reply replaces the continued turn's latest.
            if let Some(waiting) = self.waiting.iter_mut().find(|w| w.seq == owner) {
                waiting.reply = reply;
                waiting.ambiguous |= ambiguous;
            }
        } else if background.is_empty() {
            Self::end_requests(out, &requests, &Self::reply_end(&reply, ambiguous));
        } else {
            self.waiting.push(Waiting {
                seq: owner,
                requests,
                ambiguous,
                reply,
            });
        }
        self.after_close(background, Some(owner), out);
    }

    /// Own the new tasks, forget the gone ones, and settle the phase.
    fn after_close(&mut self, background: &[EventText], owner: Option<u64>, out: &mut Vec<Out>) {
        let running: Vec<String> = background
            .iter()
            .map(|task| task.expose().clone())
            .collect();
        self.task_owner.retain(|task, _| running.contains(task));
        if let Some(owner) = owner {
            for task in &running {
                self.task_owner.entry(task.clone()).or_insert(owner);
            }
        }
        if !self.open.is_empty() {
            return;
        }
        if running.is_empty() {
            // The end with no work left: waiting turns end with `finished`.
            for waiting in std::mem::take(&mut self.waiting) {
                Self::end_requests(
                    out,
                    &waiting.requests,
                    &Self::reply_end(&waiting.reply, waiting.ambiguous),
                );
            }
            self.set_phase(Phase::Idle, out);
            out.push(Out::Event(BridgeEvent::Finished));
        } else {
            self.set_phase(Phase::Background, out);
        }
    }

    /// Feed marks read from the turn log, in order.
    pub fn on_log(&mut self, marks: Vec<LogMark>, now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        for mark in marks {
            match mark {
                LogMark::Prompt { text } => {
                    self.marks.push(Mark::Prompt(self.adapter.ack_form(&text)))
                }
                LogMark::FinalReply => self.marks.push(Mark::Final),
                LogMark::Interrupt { turn } => {
                    if self.is_codex() {
                        self.codex_abort(turn, &mut out);
                    } else {
                        self.claude_interrupt(&mut out);
                    }
                }
                LogMark::TurnComplete { turn, reply } => {
                    if self.is_codex() {
                        self.codex_complete(turn, reply, now);
                    }
                }
                LogMark::TurnStarted { turn } => {
                    if let Some(turn) = turn {
                        self.started.push_back(turn);
                        if self.started.len() > STARTED_KEEP {
                            self.started.pop_front();
                        }
                    }
                }
                LogMark::Other => {}
            }
        }
        if self.marks.len() > MARKS_KEEP {
            let drop = self.marks.len() - MARKS_KEEP;
            self.marks.drain(..drop);
        }
        out
    }

    fn claude_interrupt(&mut self, out: &mut Vec<Out>) {
        // The prompts whose user record the tail has already passed.
        let passed: Vec<usize> =
            {
                let mut cursor = 0;
                let mut found = Vec::new();
                for (index, open) in self.open.iter().enumerate() {
                    let at =
                        self.marks.iter().enumerate().skip(cursor).find_map(
                            |(i, mark)| match mark {
                                Mark::Prompt(text) if same_head(text, &open.head) => Some(i),
                                _ => None,
                            },
                        );
                    if let Some(at) = at {
                        cursor = at + 1;
                        found.push(index);
                    }
                }
                found
            };
        let victims = if passed.is_empty() && self.open.len() == 1 {
            vec![0]
        } else {
            passed
        };
        for index in victims.into_iter().rev() {
            let prompt = self.open.remove(index);
            Self::end_requests(out, &prompt.requests, &RequestEnd::Interrupted);
        }
        // What the interrupted turn left in the log is behind us.
        self.marks.retain(|mark| *mark != Mark::Final);
        self.after_interrupt(out);
    }

    fn after_interrupt(&mut self, out: &mut Vec<Out>) {
        if self.open.is_empty() {
            self.set_phase(Phase::Idle, out);
            out.push(Out::Event(BridgeEvent::Interrupted));
        }
    }

    // ---- Codex

    fn remember_ended(&mut self, turn: &str) {
        if !self.ended.iter().any(|id| id == turn) {
            self.ended.push_back(turn.to_owned());
            while self.ended.len() > ENDED_KEEP {
                self.ended.pop_front();
            }
        }
    }

    fn has_ended(&self, turn: &str) -> bool {
        self.ended.iter().any(|id| id == turn)
    }

    #[allow(clippy::too_many_arguments)]
    fn codex_notify(
        &mut self,
        input: Vec<EventText>,
        input_total: usize,
        reply: Reply,
        thread: Option<String>,
        turn: Option<String>,
        out: &mut Vec<Out>,
    ) {
        if let (Some(thread), Some(pinned)) = (&thread, &self.pinned)
            && thread != pinned
        {
            // A child thread's end is not the main turn's.
            return;
        }
        let first_absolute = input_total.saturating_sub(input.len());
        let new_inputs: Vec<&EventText> = input
            .iter()
            .enumerate()
            .filter(|(index, _)| first_absolute + index >= self.input_seen)
            .map(|(_, text)| text)
            .collect();
        let total_seen = input_total.max(self.input_seen);
        let last_input = input.last().cloned();
        if let Some(turn_id) = &turn {
            self.pending.retain(|pending| pending.turn != *turn_id);
            if self.has_ended(turn_id) {
                // A later end for an ended id changes nothing, except that a
                // fallback end was waiting for exactly this notify.
                self.input_seen = total_seen;
                self.late_expected = self.late_expected.saturating_sub(1);
                return;
            }
        }
        let rule2: Option<usize> = self.open.iter().position(|open| {
            new_inputs.iter().any(|entry| {
                request_ids(entry.expose())
                    .iter()
                    .any(|id| open.requests.contains(id))
            })
        });
        self.input_seen = total_seen;
        out.push(Out::Turn {
            input: last_input,
            reply: reply.clone(),
        });
        self.codex_end(turn.as_deref(), &reply, rule2, out);
    }

    /// Close the prompt this end belongs to, and settle the phase.
    fn codex_end(
        &mut self,
        turn: Option<&str>,
        reply: &Reply,
        by_input: Option<usize>,
        out: &mut Vec<Out>,
    ) {
        let by_turn = turn.and_then(|id| {
            self.open
                .iter()
                .position(|open| open.turn.as_deref() == Some(id))
        });
        let mut ambiguous_all = false;
        let index = if let Some(index) = by_turn {
            Some(index)
        } else if self.open.len() == 1 && (turn.is_none() || self.open[0].turn.is_none()) {
            Some(0)
        } else if self.open.is_empty() {
            None
        } else if let Some(index) = by_input {
            Some(index)
        } else {
            ambiguous_all = true;
            Some(0)
        };
        if ambiguous_all {
            for open in &mut self.open {
                open.ambiguous = true;
            }
        }
        if let Some(turn) = turn {
            self.remember_ended(turn);
        }
        match index {
            Some(index) => {
                let prompt = self.open.remove(index);
                if let Some(own) = &prompt.turn {
                    let own = own.clone();
                    self.remember_ended(&own);
                }
                Self::end_requests(
                    out,
                    &prompt.requests,
                    &Self::reply_end(reply, prompt.ambiguous),
                );
                if self.open.is_empty() {
                    self.set_phase(Phase::Idle, out);
                    out.push(Out::Event(BridgeEvent::Finished));
                }
            }
            None => {
                // The prompt hook was lost: the end is still an end.
                self.set_phase(Phase::Idle, out);
                out.push(Out::Event(BridgeEvent::Finished));
            }
        }
    }

    fn codex_complete(&mut self, turn: Option<String>, reply: Option<String>, now: Instant) {
        let Some(turn) = turn else { return };
        if !self.fallback || self.has_ended(&turn) || self.pending.iter().any(|p| p.turn == turn) {
            return;
        }
        let reply = match reply {
            Some(text) => {
                let text = EventText::reply(text);
                if text.truncated() {
                    Reply::Dropped
                } else {
                    Reply::Text(text)
                }
            }
            None => Reply::Dropped,
        };
        self.pending.push(PendingComplete {
            turn,
            due: now + TASK_COMPLETE_WAIT,
            reply,
        });
    }

    fn codex_abort(&mut self, turn: Option<String>, out: &mut Vec<Out>) {
        if let Some(turn) = &turn {
            if self.has_ended(turn) {
                return;
            }
            self.pending.retain(|pending| pending.turn != *turn);
        }
        let index = turn
            .as_deref()
            .and_then(|id| {
                self.open
                    .iter()
                    .position(|open| open.turn.as_deref() == Some(id))
            })
            .or_else(|| (!self.open.is_empty()).then_some(0));
        let Some(index) = index else { return };
        let prompt = self.open.remove(index);
        // Both the prompt's own turn id and the marker's are ended.
        if let Some(own) = prompt.turn.clone() {
            self.remember_ended(&own);
        }
        if let Some(turn) = turn {
            self.remember_ended(&turn);
        }
        Self::end_requests(out, &prompt.requests, &RequestEnd::Interrupted);
        self.after_interrupt(out);
    }

    /// An end whose payload could not be read: nothing is known about which
    /// turn ended or what still runs.
    fn incomplete(&mut self, out: &mut Vec<Out>) {
        if self.is_codex() && self.late_expected > 0 {
            self.late_expected -= 1;
            return;
        }
        for open in &mut self.open {
            let requests = std::mem::take(&mut open.requests);
            Self::end_requests(
                out,
                &requests,
                &RequestEnd::Unavailable("the turn end could not be read"),
            );
        }
    }

    // ---- clock, gaps, exit

    /// Whether a fallback end for `turn` must be ignored: it is another turn's,
    /// while the only open prompt's own turn is known to have started in this
    /// log and has not ended. Codex runs one turn at a time per thread, so that
    /// end cannot be this prompt's, and closing the prompt on it would end a
    /// running turn. It is narrow on purpose: the prompt's own id must have
    /// been seen as started. When the hook id differs from the rollout's
    /// (Check 1), that id is never seen, and the end still closes the prompt.
    fn stale_fallback(&self, turn: &str) -> bool {
        let [open] = self.open.as_slice() else {
            return false;
        };
        open.turn
            .as_deref()
            .is_some_and(|own| own != turn && self.started.iter().any(|id| id == own))
    }

    /// The clock: a `task_complete` that no `notify` followed is the end.
    pub fn on_tick(&mut self, now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        let due: Vec<PendingComplete> = {
            let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
                .into_iter()
                .partition(|pending| pending.due <= now);
            self.pending = keep;
            due
        };
        for pending in due {
            if self.has_ended(&pending.turn) {
                continue;
            }
            if self.stale_fallback(&pending.turn) {
                // Not this prompt's end. Remembered as ended, so that a notify
                // for the same id later is recognised and closes nothing.
                self.remember_ended(&pending.turn);
                continue;
            }
            self.late_expected += 1;
            out.push(Out::Turn {
                input: None,
                reply: pending.reply.clone(),
            });
            self.codex_end(Some(&pending.turn), &pending.reply, None, &mut out);
        }
        out
    }

    /// The turn log could not be read in full. What it said is no evidence.
    pub fn log_gap(&mut self) {
        self.marks.clear();
    }

    /// The user ended a delivery whose turn will never end (End delivery). It
    /// is the user's word that the terminal is not working: open prompts are
    /// forgotten, their requests end `unavailable`, and the phase is `Idle`. No
    /// event is reported, because nothing was observed.
    pub fn user_ended(&mut self) -> Vec<Out> {
        let mut out = Vec::new();
        for open in std::mem::take(&mut self.open) {
            Self::end_requests(
                &mut out,
                &open.requests,
                &RequestEnd::Unavailable("the user ended the delivery"),
            );
        }
        self.set_phase(Phase::Idle, &mut out);
        out
    }

    /// A hook frame was lost.
    pub fn on_gap(&mut self) -> Vec<Out> {
        let mut out = Vec::new();
        for open in &mut self.open {
            let requests = std::mem::take(&mut open.requests);
            Self::end_requests(
                &mut out,
                &requests,
                &RequestEnd::Unavailable("a hook frame was lost"),
            );
        }
        out
    }

    /// The process ended.
    pub fn on_exit(&mut self, status: &str) -> Vec<Out> {
        let mut out = Vec::new();
        for open in std::mem::take(&mut self.open) {
            Self::end_requests(&mut out, &open.requests, &RequestEnd::Exited);
        }
        for waiting in std::mem::take(&mut self.waiting) {
            Self::end_requests(&mut out, &waiting.requests, &RequestEnd::Exited);
        }
        self.pending.clear();
        self.task_owner.clear();
        self.phase = Phase::Idle;
        out.push(Out::Event(BridgeEvent::Exited(status.to_owned())));
        out
    }
}

// ---- the turn log tail

/// Most a poll reads while a turn runs.
pub const POLL_BYTES: u64 = 256 * 1024;
/// Most a catch-up reads at a turn end.
pub const CATCH_UP_BYTES: u64 = 4 * 1024 * 1024;
/// A line over this is classified from its first [`HEAD_BYTES`].
const LONG_LINE: usize = 1024 * 1024;
const HEAD_BYTES: usize = 4096;

/// Reads new lines of a tool's turn log, to spot interrupts and activity. It
/// holds a partial last line for the next read. Hitting a bound sets a gap,
/// which counts as no evidence for that turn: a cut log can never look like an
/// answer.
///
/// The tail starts where the file ends when it is attached, so only records
/// written after that are ever classified. Records already in a resumed log
/// belong to earlier turns. A file that does not exist yet starts at 0.
pub struct LogTail {
    path: std::path::PathBuf,
    position: u64,
    partial: Vec<u8>,
    /// Inside a line over the limit: only its head was kept.
    skipping: Option<String>,
}

/// A turn log and its length at one moment: the boundary between what an
/// earlier turn wrote and what the attempt may read. A missing file is length 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogBoundary {
    pub path: std::path::PathBuf,
    pub length: u64,
}

impl LogBoundary {
    /// The log at `path` as it is now.
    #[must_use]
    pub fn sample(path: std::path::PathBuf) -> Self {
        let length = std::fs::metadata(&path).map_or(0, |meta| meta.len());
        Self { path, length }
    }
}

impl LogTail {
    /// Attach to `path` at its current end.
    #[must_use]
    pub fn new(path: std::path::PathBuf) -> Self {
        Self::from_boundary(LogBoundary::sample(path))
    }

    /// Attach at a boundary sampled earlier, so nothing written after it is
    /// skipped.
    #[must_use]
    pub fn from_boundary(boundary: LogBoundary) -> Self {
        Self {
            path: boundary.path,
            position: boundary.length,
            partial: Vec::new(),
            skipping: None,
        }
    }

    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Read up to `budget` new bytes and classify the complete lines. The bool
    /// is true when a bound was hit: bytes were skipped or are still unread.
    pub fn read(&mut self, adapter: &dyn ToolAdapter, budget: u64) -> (Vec<LogMark>, bool) {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return (Vec::new(), false);
        };
        let Ok(len) = file.metadata().map(|meta| meta.len()) else {
            return (Vec::new(), false);
        };
        let mut gap = false;
        if len < self.position {
            // The file was replaced: all of it is new.
            self.position = 0;
            self.partial.clear();
            self.skipping = None;
        }
        if len - self.position > budget {
            gap = true;
        }
        let take = (len - self.position).min(budget);
        if file.seek(SeekFrom::Start(self.position)).is_err() {
            return (Vec::new(), gap);
        }
        let mut bytes = Vec::new();
        if (&mut file).take(take).read_to_end(&mut bytes).is_err() {
            return (Vec::new(), gap);
        }
        self.position += bytes.len() as u64;
        (self.lines(adapter, &bytes), gap)
    }

    fn lines(&mut self, adapter: &dyn ToolAdapter, bytes: &[u8]) -> Vec<LogMark> {
        let mut marks = Vec::new();
        for piece in bytes.split_inclusive(|byte| *byte == b'\n') {
            let complete = piece.last() == Some(&b'\n');
            let body = if complete {
                &piece[..piece.len() - 1]
            } else {
                piece
            };
            if self.skipping.is_some() {
                // Inside a line that was cut or too long: nothing more of it is
                // kept, and its end brings its head to be classified.
                if complete
                    && let Some(head) = self.skipping.take()
                    && !head.is_empty()
                {
                    marks.push(adapter.read_log(&head, false));
                }
                continue;
            }
            self.partial.extend_from_slice(body);
            if self.partial.len() > LONG_LINE {
                let head = String::from_utf8_lossy(&self.partial[..HEAD_BYTES]).into_owned();
                self.partial.clear();
                if complete {
                    marks.push(adapter.read_log(&head, false));
                } else {
                    self.skipping = Some(head);
                }
                continue;
            }
            if complete {
                let line = String::from_utf8_lossy(&self.partial).into_owned();
                self.partial.clear();
                if !line.trim().is_empty() {
                    marks.push(adapter.read_log(&line, true));
                }
            }
        }
        marks
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use connecto_adapter::{Claude, Codex};

    use super::*;

    fn text(value: &str) -> EventText {
        EventText::new(value.to_owned())
    }

    fn header(id: &str, from: &str) -> String {
        format!("[connecto from {from}, request {id}. Sent by another agent, not the user.]\nbody")
    }

    fn claude() -> Tracker {
        Tracker::new(&Claude, false)
    }

    fn codex() -> Tracker {
        Tracker::new(&Codex, true)
    }

    fn prompt(tracker: &mut Tracker, text_in: &str, turn: Option<&str>) -> Vec<Out> {
        tracker.on_event(
            ToolEvent::PromptSubmitted {
                prompt: text(text_in),
                turn: turn.map(text),
            },
            Instant::now(),
        )
    }

    fn stop(tracker: &mut Tracker, reply: &str, background: &[&str]) -> Vec<Out> {
        tracker.on_event(
            ToolEvent::TurnEnded {
                input: Vec::new(),
                input_total: 0,
                reply: if reply.is_empty() {
                    Reply::None
                } else {
                    Reply::Text(text(reply))
                },
                lifecycle_known: true,
                background: background.iter().map(|id| text(id)).collect(),
                thread: None,
                turn: None,
            },
            Instant::now(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        tracker: &mut Tracker,
        thread: &str,
        turn: &str,
        inputs: &[&str],
        total: usize,
        reply: Reply,
        now: Instant,
    ) -> Vec<Out> {
        tracker.on_event(
            ToolEvent::TurnEnded {
                input: inputs.iter().map(|i| text(i)).collect(),
                input_total: total,
                reply,
                lifecycle_known: true,
                background: Vec::new(),
                thread: Some(text(thread)),
                turn: Some(text(turn)),
            },
            now,
        )
    }

    fn start_codex(tracker: &mut Tracker, thread: &str) {
        tracker.on_event(
            ToolEvent::Started {
                tool_session: text(thread),
                turn_log: None,
            },
            Instant::now(),
        );
    }

    fn events(out: &[Out]) -> Vec<&'static str> {
        out.iter()
            .filter_map(|out| match out {
                Out::Event(event) => Some(event.word()),
                _ => None,
            })
            .collect()
    }

    fn ended(out: &[Out]) -> BTreeMap<String, RequestEnd> {
        out.iter()
            .filter_map(|out| match out {
                Out::Ended { request, end } => Some((request.clone(), end.clone())),
                _ => None,
            })
            .collect()
    }

    fn ok(reply: &str) -> RequestEnd {
        RequestEnd::Ok(text(reply))
    }

    #[test]
    fn request_ids_are_found_in_the_generated_header_only() {
        assert_eq!(request_ids(&header("r12", "claude-2")), ["r12"]);
        assert!(request_ids("[connecto from X Y, request r1.]").is_empty());
        assert!(request_ids("[connecto from a, request 12.]").is_empty());
        assert!(request_ids("not at the start [connecto from a, request r1.]").is_empty());
        assert_eq!(
            request_ids("hello\n[connecto from a, request r3.]\n[connecto from b, request r4.]"),
            ["r3", "r4"]
        );
    }

    #[test]
    fn a_plain_turn_goes_working_and_finished_and_a_request_takes_the_reply() {
        let mut t = claude();
        let out = prompt(&mut t, &header("r1", "codex"), None);
        assert_eq!(events(&out), ["began"]);
        assert!(out.contains(&Out::Bound("r1".into())));
        assert_eq!(t.phase(), Phase::Working);
        let out = stop(&mut t, "the answer", &[]);
        assert_eq!(events(&out), ["finished"]);
        assert!(out.contains(&Out::LeftWorking));
        assert_eq!(ended(&out)["r1"], ok("the answer"));
        assert_eq!(t.phase(), Phase::Idle);
    }

    /// T19: a Stop with background work gives no `finished`, the request waits,
    /// and the Stop with an empty list ends both.
    #[test]
    fn background_work_holds_finished_and_the_request_until_the_list_is_empty() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "codex"), None);
        let out = stop(&mut t, "first", &["b1"]);
        assert!(events(&out).is_empty(), "{out:?}");
        assert!(ended(&out).is_empty());
        assert!(
            out.contains(&Out::LeftWorking),
            "Background counts as not busy"
        );
        assert_eq!(t.phase(), Phase::Background);
        // Claude starts a turn by itself when the work reports back.
        let out = prompt(&mut t, "<task-notification><task-id>b1</task-id>done", None);
        assert_eq!(events(&out), ["began"]);
        let out = stop(&mut t, "final answer", &[]);
        assert_eq!(events(&out), ["finished"]);
        assert_eq!(ended(&out)["r1"], ok("final answer"));
    }

    /// T21: a user's prompt during Background does not become a waiting
    /// request's result.
    #[test]
    fn an_unrelated_prompt_during_background_is_its_own_turn() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "codex"), None);
        stop(&mut t, "first", &["b1"]);
        prompt(&mut t, "what time is it", None);
        let out = stop(&mut t, "noon", &["b1"]);
        assert!(ended(&out).is_empty(), "{out:?}");
        assert_eq!(t.phase(), Phase::Background);
        let out = stop(&mut t, "later", &[]);
        let ends = ended(&out);
        assert_eq!(ends["r1"], ok("first"), "the request keeps its own reply");
    }

    /// A task notification the tool queued into a running turn rides with it:
    /// one `Stop` ends the user's prompt and the notification together, and the
    /// phase goes idle. The log shows both prompts before the turn's end.
    #[test]
    fn a_notification_queued_into_a_turn_ends_with_that_turn() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "codex"), None);
        stop(&mut t, "first", &["b1"]);
        assert_eq!(t.phase(), Phase::Background);
        prompt(&mut t, "can you check with codex", None);
        prompt(&mut t, "<task-notification><task-id>b1</task-id>done", None);
        t.on_log(
            vec![
                LogMark::Prompt {
                    text: "can you check with codex".into(),
                },
                LogMark::Prompt {
                    text: "<task-notification><task-id>b1</task-id>done".into(),
                },
                LogMark::FinalReply,
            ],
            Instant::now(),
        );
        let out = stop(&mut t, "done", &[]);
        assert_eq!(events(&out), ["finished"]);
        assert_eq!(t.phase(), Phase::Idle);
        assert_eq!(t.open_prompts(), 0);
        assert!(
            ended(&out).contains_key("r1"),
            "the waiting request ends too"
        );
    }

    /// T23: two requests that each leave work running cannot overwrite each
    /// other, and unclear ownership gives `unmatched`.
    #[test]
    fn two_waiting_requests_keep_their_own_results_and_unclear_ownership_is_unmatched() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        stop(&mut t, "one", &["b1"]);
        prompt(&mut t, &header("r2", "a"), None);
        stop(&mut t, "two", &["b1", "b2"]);
        // A notification naming b2 continues the second turn only.
        prompt(&mut t, "<task-notification><task-id>b2</task-id>x", None);
        stop(&mut t, "two-continued", &["b1", "b2"]);
        // One that names no task, with two owners running, is unclear.
        prompt(&mut t, "<task-notification>x", None);
        let out = stop(&mut t, "mystery", &[]);
        let ends = ended(&out);
        assert!(matches!(ends["r1"], RequestEnd::Unmatched(_)), "{ends:?}");
        assert!(matches!(ends["r2"], RequestEnd::Unmatched(_)), "{ends:?}");
    }

    #[test]
    fn a_single_waiting_turn_with_a_clear_owner_gets_the_continued_reply() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        stop(&mut t, "one", &["b1"]);
        prompt(&mut t, "<task-notification>x", None);
        let out = stop(&mut t, "continued", &[]);
        assert_eq!(ended(&out)["r1"], ok("continued"));
    }

    /// T22: several prompts open, with evidence, no evidence, and the ordering
    /// case from Pairing.
    #[test]
    fn several_open_prompts_close_by_evidence_and_never_by_time() {
        let mut t = claude();
        // The measured case: a self-started turn's Stop lands just after a
        // typed prompt. Its record sits after the last end_turn: not ours.
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, &header("r2", "a"), None);
        t.on_log(
            vec![
                LogMark::Prompt {
                    text: header("r1", "a"),
                },
                LogMark::FinalReply,
                LogMark::Prompt {
                    text: header("r2", "a"),
                },
            ],
            Instant::now(),
        );
        let out = stop(&mut t, "reply to r1", &[]);
        let ends = ended(&out);
        assert_eq!(ends["r1"], ok("reply to r1"));
        assert!(
            !ends.contains_key("r2"),
            "r2 is a separate turn and stays open"
        );
        assert!(
            events(&out).is_empty(),
            "a prompt is still open: no finished"
        );
        assert_eq!(t.phase(), Phase::Working);
        t.on_log(vec![LogMark::FinalReply], Instant::now());
        let out = stop(&mut t, "reply to r2", &[]);
        assert_eq!(ended(&out)["r2"], ok("reply to r2"));
        assert_eq!(events(&out), ["finished"]);
    }

    #[test]
    fn merged_prompts_with_evidence_share_a_reply() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, &header("r2", "a"), None);
        t.on_log(
            vec![
                LogMark::FinalReply,
                LogMark::Prompt {
                    text: header("r1", "a"),
                },
                LogMark::Prompt {
                    text: header("r2", "a"),
                },
                LogMark::FinalReply,
            ],
            Instant::now(),
        );
        let out = stop(&mut t, "both", &[]);
        let ends = ended(&out);
        assert_eq!(ends["r1"], ok("both"));
        assert_eq!(ends["r2"], ok("both"));
        assert_eq!(events(&out), ["finished"]);
    }

    /// T38: no evidence closes the oldest and marks every open prompt
    /// ambiguous, so their requests end `unmatched`.
    #[test]
    fn with_several_prompts_and_no_evidence_the_oldest_closes_and_all_are_unmatched() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, &header("r2", "a"), None);
        let out = stop(&mut t, "reply", &[]);
        let ends = ended(&out);
        assert!(matches!(ends["r1"], RequestEnd::Unmatched(_)), "{ends:?}");
        assert!(!ends.contains_key("r2"));
        assert_eq!(t.phase(), Phase::Working, "the terminal reads working");
        let out = stop(&mut t, "reply 2", &[]);
        assert!(
            matches!(ended(&out)["r2"], RequestEnd::Unmatched(_)),
            "being ambiguous lasts"
        );
        assert_eq!(events(&out), ["finished"]);
    }

    #[test]
    fn a_prompt_older_than_the_previous_end_turn_had_its_own_unreported_turn() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, &header("r2", "a"), None);
        t.on_log(
            vec![
                LogMark::Prompt {
                    text: header("r1", "a"),
                },
                LogMark::FinalReply,
                LogMark::Prompt {
                    text: header("r2", "a"),
                },
                LogMark::FinalReply,
            ],
            Instant::now(),
        );
        let out = stop(&mut t, "r2 answer", &[]);
        let ends = ended(&out);
        assert!(matches!(ends["r1"], RequestEnd::Unavailable(_)), "{ends:?}");
        assert_eq!(ends["r2"], ok("r2 answer"));
    }

    /// T24: a reply is `ok` only if whole; a cut or missing one is `unavailable`.
    #[test]
    fn a_dropped_or_missing_reply_is_unavailable_and_never_ok() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        let out = t.on_event(
            ToolEvent::TurnEnded {
                input: Vec::new(),
                input_total: 0,
                reply: Reply::Dropped,
                lifecycle_known: true,
                background: Vec::new(),
                thread: None,
                turn: None,
            },
            Instant::now(),
        );
        assert!(matches!(ended(&out)["r1"], RequestEnd::Unavailable(_)));
        prompt(&mut t, &header("r2", "a"), None);
        let out = stop(&mut t, "", &[]);
        assert!(matches!(ended(&out)["r2"], RequestEnd::Unavailable(_)));
        assert_eq!(
            events(&out),
            ["finished"],
            "lifecycle survives a lost reply"
        );
    }

    /// T26: an end that could not be read gives `unavailable` and no `finished`.
    #[test]
    fn a_lifecycle_unknown_end_and_a_hook_gap_end_requests_without_finishing() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        let out = t.on_event(
            ToolEvent::TurnEnded {
                input: Vec::new(),
                input_total: 0,
                reply: Reply::None,
                lifecycle_known: false,
                background: Vec::new(),
                thread: None,
                turn: None,
            },
            Instant::now(),
        );
        assert!(matches!(ended(&out)["r1"], RequestEnd::Unavailable(_)));
        assert!(events(&out).is_empty());
        assert_eq!(t.phase(), Phase::Working);
        prompt(&mut t, &header("r2", "a"), None);
        let out = t.on_gap();
        assert!(matches!(ended(&out)["r2"], RequestEnd::Unavailable(_)));
        assert!(events(&out).is_empty());
    }

    /// T43 on Claude: a late interrupt marker leaves a newer prompt open.
    #[test]
    fn a_claude_interrupt_ends_the_passed_prompts_and_leaves_a_newer_one() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        let out = t.on_log(
            vec![
                LogMark::Prompt {
                    text: header("r1", "a"),
                },
                LogMark::Interrupt { turn: None },
            ],
            Instant::now(),
        );
        assert_eq!(ended(&out)["r1"], RequestEnd::Interrupted);
        assert_eq!(events(&out), ["interrupted"]);
        assert_eq!(t.phase(), Phase::Idle);

        prompt(&mut t, &header("r2", "a"), None);
        prompt(&mut t, &header("r3", "a"), None);
        let out = t.on_log(
            vec![
                LogMark::Prompt {
                    text: header("r2", "a"),
                },
                LogMark::Interrupt { turn: None },
            ],
            Instant::now(),
        );
        assert_eq!(ended(&out)["r2"], RequestEnd::Interrupted);
        assert!(!ended(&out).contains_key("r3"));
        assert!(events(&out).is_empty(), "r3 is still open");
        assert_eq!(t.phase(), Phase::Working);
    }

    #[test]
    fn needs_user_is_reported_and_cleared_by_the_next_prompt() {
        let mut t = claude();
        let out = t.on_event(ToolEvent::NeedsUser, Instant::now());
        assert_eq!(events(&out), ["needs-user"]);
        assert!(t.needs_user());
        prompt(&mut t, "go on", None);
        assert!(!t.needs_user());
    }

    #[test]
    fn exit_ends_every_request_and_reports_the_status() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        stop(&mut t, "x", &["b1"]);
        prompt(&mut t, &header("r2", "a"), None);
        let out = t.on_exit("exited with code 0");
        let ends = ended(&out);
        assert_eq!(ends["r1"], RequestEnd::Exited);
        assert_eq!(ends["r2"], RequestEnd::Exited);
        assert_eq!(
            out.last(),
            Some(&Out::Event(BridgeEvent::Exited(
                "exited with code 0".into()
            )))
        );
    }

    // ---- Codex

    const MAIN: &str = "11111111-aaaa";
    const CHILD: &str = "22222222-bbbb";

    /// T20: a child thread's notify is ignored.
    #[test]
    fn a_child_threads_notify_is_not_a_turn_end() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        let out = notify(&mut t, CHILD, "tu9", &[], 0, Reply::None, Instant::now());
        assert!(out.is_empty());
        assert_eq!(t.phase(), Phase::Working);
        let out = notify(
            &mut t,
            MAIN,
            "tu1",
            &[],
            0,
            Reply::Text(text("done")),
            Instant::now(),
        );
        assert_eq!(ended(&out)["r1"], ok("done"));
        assert_eq!(events(&out), ["finished"]);
    }

    #[test]
    fn a_codex_end_closes_only_the_prompt_with_its_turn_id() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        prompt(&mut t, &header("r2", "a"), Some("tu2"));
        // The second turn ends first: only its prompt closes.
        let out = notify(
            &mut t,
            MAIN,
            "tu2",
            &[],
            0,
            Reply::Text(text("two")),
            Instant::now(),
        );
        let ends = ended(&out);
        assert_eq!(ends["r2"], ok("two"));
        assert!(!ends.contains_key("r1"));
        assert!(events(&out).is_empty());
        let out = notify(
            &mut t,
            MAIN,
            "tu1",
            &[],
            0,
            Reply::Text(text("one")),
            Instant::now(),
        );
        assert_eq!(ended(&out)["r1"], ok("one"));
        assert_eq!(events(&out), ["finished"]);
    }

    /// T41: a notify after the task_complete fallback closes nothing and emits
    /// no second `finished`; the reverse order is also one end; an incomplete
    /// frame after a fallback end is ignored.
    #[test]
    fn a_late_notify_after_the_fallback_end_changes_nothing() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        t.on_log(
            vec![LogMark::TurnComplete {
                turn: Some("tu1".into()),
                reply: Some("late".into()),
            }],
            t0,
        );
        assert!(
            t.on_tick(t0 + Duration::from_secs(1)).is_empty(),
            "the wait is 5 s"
        );
        let out = t.on_tick(t0 + TASK_COMPLETE_WAIT + Duration::from_millis(1));
        assert_eq!(ended(&out)["r1"], ok("late"));
        assert_eq!(events(&out), ["finished"]);
        // The next turn starts, and the old notify arrives during it.
        prompt(&mut t, "next", Some("tu2"));
        let out = notify(&mut t, MAIN, "tu1", &[], 0, Reply::Text(text("late")), t0);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(t.open_prompts(), 1, "the newer prompt stays open");
        assert_eq!(t.phase(), Phase::Working);
        // An incomplete frame for the missing notify would have been ignored,
        // but `late_expected` was taken by the real notify: now it acts.
        let mut t = codex();
        start_codex(&mut t, MAIN);
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        t.on_log(
            vec![LogMark::TurnComplete {
                turn: Some("tu1".into()),
                reply: Some("x".into()),
            }],
            t0,
        );
        t.on_tick(t0 + TASK_COMPLETE_WAIT + Duration::from_millis(1));
        prompt(&mut t, &header("r2", "a"), Some("tu2"));
        let out = t.on_event(
            ToolEvent::TurnEnded {
                input: Vec::new(),
                input_total: 0,
                reply: Reply::None,
                lifecycle_known: false,
                background: Vec::new(),
                thread: None,
                turn: None,
            },
            t0,
        );
        assert!(
            out.is_empty(),
            "the incomplete frame belongs to the fallback end"
        );
        assert_eq!(t.open_prompts(), 1);
    }

    #[test]
    fn a_notify_then_the_task_complete_is_one_end() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        let out = notify(&mut t, MAIN, "tu1", &[], 0, Reply::Text(text("a")), t0);
        assert_eq!(events(&out), ["finished"]);
        t.on_log(
            vec![LogMark::TurnComplete {
                turn: Some("tu1".into()),
                reply: Some("a".into()),
            }],
            t0,
        );
        assert!(!t.wants_log() || t.on_tick(t0 + Duration::from_secs(10)).is_empty());
    }

    /// T51: with differing ids the fallback is off.
    #[test]
    fn with_the_fallback_off_a_task_complete_ends_nothing() {
        let mut t = codex();
        t.disable_fallback();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, "x", Some("tu1"));
        t.on_log(
            vec![LogMark::TurnComplete {
                turn: Some("tu1".into()),
                reply: None,
            }],
            t0,
        );
        assert!(t.on_tick(t0 + Duration::from_secs(60)).is_empty());
        assert_eq!(t.phase(), Phase::Working);
    }

    /// T42: a reply too long to read whole still ends the turn.
    #[test]
    fn a_task_complete_whose_reply_could_not_be_read_ends_the_turn_as_unavailable() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        t.on_log(
            vec![LogMark::TurnComplete {
                turn: Some("tu1".into()),
                reply: None,
            }],
            t0,
        );
        let out = t.on_tick(t0 + TASK_COMPLETE_WAIT + Duration::from_millis(1));
        assert!(matches!(ended(&out)["r1"], RequestEnd::Unavailable(_)));
        assert_eq!(events(&out), ["finished"]);
    }

    /// T50: with a hook turn id that differs from notify's, the turn still
    /// ends. A request found in the new `input-messages` is `ok`, and with no
    /// evidence the prompt closes ambiguous.
    #[test]
    fn mismatched_turn_ids_still_end_the_turn() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        prompt(&mut t, &header("r1", "a"), Some("hook-id"));
        prompt(&mut t, &header("r2", "a"), Some("hook-id-2"));
        let header_r2 = header("r2", "a");
        let out = notify(
            &mut t,
            MAIN,
            "notify-id",
            &[&header_r2],
            1,
            Reply::Text(text("for r2")),
            Instant::now(),
        );
        let ends = ended(&out);
        assert_eq!(
            ends["r2"],
            ok("for r2"),
            "rule 2: the request id is in the inputs"
        );
        assert!(!ends.contains_key("r1"));
        // r1 alone, no evidence and a different id: it still closes.
        let out = notify(
            &mut t,
            MAIN,
            "notify-id-2",
            &[],
            1,
            Reply::Text(text("late")),
            Instant::now(),
        );
        assert!(matches!(ended(&out)["r1"], RequestEnd::Unmatched(_)));
        assert_eq!(events(&out), ["finished"]);
        assert_eq!(
            t.phase(),
            Phase::Idle,
            "no Codex terminal stays working after its end"
        );
    }

    /// T27: an old id in accumulated `input-messages` proves nothing.
    #[test]
    fn an_old_input_id_proves_nothing_and_positions_are_absolute() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let old = header("r1", "a");
        prompt(&mut t, &old, Some("tu1"));
        notify(
            &mut t,
            MAIN,
            "tu1",
            &[&old],
            1,
            Reply::Text(text("a")),
            Instant::now(),
        );
        // Next turn: two prompts, ids differ from the hook's. The list now
        // holds the old entry first, then the new ones.
        prompt(&mut t, &header("r2", "a"), Some("hook-a"));
        prompt(&mut t, &header("r3", "a"), Some("hook-b"));
        let (h2, h3) = (header("r2", "a"), header("r3", "a"));
        let out = notify(
            &mut t,
            MAIN,
            "other",
            &[&old, &h2, &h3],
            3,
            Reply::Text(text("x")),
            Instant::now(),
        );
        // r2 and r3 are both new: the first open prompt whose id is new wins.
        let ends = ended(&out);
        assert_eq!(ends["r2"], ok("x"));
        assert!(!ends.contains_key("r1"));
    }

    /// T29: a shrunk notify keeps absolute positions.
    #[test]
    fn a_shrunk_notify_finds_new_ids_by_absolute_position() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        // 30 entries were seen before.
        prompt(&mut t, "warm", Some("w"));
        notify(
            &mut t,
            MAIN,
            "w",
            &[],
            30,
            Reply::Text(text("w")),
            Instant::now(),
        );
        prompt(&mut t, &header("r7", "a"), Some("hook-x"));
        prompt(&mut t, &header("r8", "a"), Some("hook-y"));
        let h8 = header("r8", "a");
        // 40 now; only the last entry is kept, and it is new (position 39).
        let out = notify(
            &mut t,
            MAIN,
            "unseen",
            &[&h8],
            40,
            Reply::Text(text("eight")),
            Instant::now(),
        );
        assert_eq!(ended(&out)["r8"], ok("eight"));
    }

    #[test]
    fn a_codex_interrupt_with_its_turn_id_closes_that_prompt_and_an_unseen_id_the_oldest() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        prompt(&mut t, &header("r2", "a"), Some("tu2"));
        let out = t.on_log(
            vec![LogMark::Interrupt {
                turn: Some("tu2".into()),
            }],
            Instant::now(),
        );
        assert_eq!(ended(&out)["r2"], RequestEnd::Interrupted);
        assert!(events(&out).is_empty(), "r1 is still open");
        let out = t.on_log(
            vec![LogMark::Interrupt {
                turn: Some("never-seen".into()),
            }],
            Instant::now(),
        );
        assert_eq!(ended(&out)["r1"], RequestEnd::Interrupted);
        assert_eq!(events(&out), ["interrupted"]);
        // A late marker for an ended turn changes nothing.
        prompt(&mut t, "newer", Some("tu3"));
        let out = t.on_log(
            vec![LogMark::Interrupt {
                turn: Some("tu1".into()),
            }],
            Instant::now(),
        );
        assert!(out.is_empty());
        assert_eq!(t.open_prompts(), 1);
    }

    #[test]
    fn an_end_with_nothing_open_still_finishes() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let out = notify(&mut t, MAIN, "unseen", &[], 0, Reply::None, Instant::now());
        assert_eq!(events(&out), ["finished"], "the prompt hook was lost");
    }

    /// The stale-fallback guard: a fallback end for another id does not close a
    /// prompt whose own turn the log shows as started. Its own end still does.
    #[test]
    fn a_fallback_end_for_another_turn_does_not_close_a_running_prompt() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        t.on_log(
            vec![LogMark::TurnStarted {
                turn: Some("tu1".into()),
            }],
            t0,
        );
        t.on_log(
            vec![LogMark::TurnComplete {
                turn: Some("tu-other".into()),
                reply: Some("x".into()),
            }],
            t0,
        );
        let out = t.on_tick(t0 + TASK_COMPLETE_WAIT + Duration::from_millis(1));
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(t.open_prompts(), 1, "the running prompt stays open");
        assert_eq!(t.phase(), Phase::Working);
        let out = notify(&mut t, MAIN, "tu1", &[], 0, Reply::Text(text("mine")), t0);
        assert_eq!(ended(&out)["r1"], ok("mine"));
        assert_eq!(events(&out), ["finished"]);
    }

    /// A dropped stale end is remembered: a notify later for the same other id
    /// closes nothing, and the running prompt still ends by its own turn.
    #[test]
    fn a_notify_for_a_dropped_stale_end_closes_nothing() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        t.on_log(
            vec![LogMark::TurnStarted {
                turn: Some("tu1".into()),
            }],
            t0,
        );
        t.on_log(
            vec![LogMark::TurnComplete {
                turn: Some("tuX".into()),
                reply: Some("x".into()),
            }],
            t0,
        );
        let out = t.on_tick(t0 + TASK_COMPLETE_WAIT + Duration::from_millis(1));
        assert!(out.is_empty(), "{out:?}");
        let out = notify(&mut t, MAIN, "tuX", &[], 0, Reply::Text(text("x")), t0);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(t.open_prompts(), 1, "the running prompt stays open");
        assert_eq!(t.phase(), Phase::Working);
        let out = notify(&mut t, MAIN, "tu1", &[], 0, Reply::Text(text("mine")), t0);
        assert_eq!(ended(&out)["r1"], ok("mine"));
        assert_eq!(events(&out), ["finished"]);
    }

    /// Without the prompt's own start in the log, the fallback end closes it as
    /// it did before the guard.
    #[test]
    fn without_its_own_start_a_fallback_end_still_closes_the_prompt() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        t.on_log(
            vec![LogMark::TurnComplete {
                turn: Some("tu-other".into()),
                reply: Some("x".into()),
            }],
            t0,
        );
        let out = t.on_tick(t0 + TASK_COMPLETE_WAIT + Duration::from_millis(1));
        assert!(matches!(ended(&out)["r1"], RequestEnd::Unmatched(_)));
        assert_eq!(events(&out), ["finished"]);
    }

    /// Check 1 with differing ids: the log shows the rollout's id as started,
    /// not the hook's, so the guard does not apply and the fallback ends the turn.
    #[test]
    fn a_hook_id_that_differs_from_the_rollouts_still_ends_by_fallback() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, &header("r1", "a"), Some("hook-1"));
        t.on_log(
            vec![
                LogMark::TurnStarted {
                    turn: Some("rollout-1".into()),
                },
                LogMark::TurnComplete {
                    turn: Some("rollout-1".into()),
                    reply: Some("done".into()),
                },
            ],
            t0,
        );
        let out = t.on_tick(t0 + TASK_COMPLETE_WAIT + Duration::from_millis(1));
        assert!(matches!(ended(&out)["r1"], RequestEnd::Unmatched(_)));
        assert_eq!(events(&out), ["finished"]);
    }

    /// A started turn whose own `task_complete` arrives ends its prompt with
    /// the reply, as before.
    #[test]
    fn a_started_turn_ends_on_its_own_task_complete() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, &header("r1", "a"), Some("tu1"));
        t.on_log(
            vec![
                LogMark::TurnStarted {
                    turn: Some("tu1".into()),
                },
                LogMark::TurnComplete {
                    turn: Some("tu1".into()),
                    reply: Some("yes".into()),
                },
            ],
            t0,
        );
        let out = t.on_tick(t0 + TASK_COMPLETE_WAIT + Duration::from_millis(1));
        assert_eq!(ended(&out)["r1"], ok("yes"));
        assert_eq!(events(&out), ["finished"]);
    }

    // ---- the tail

    use std::io::Write;

    fn log_file(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("mg-tail-{name}-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn append(path: &std::path::Path, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    #[test]
    fn the_tail_reads_new_lines_and_holds_a_partial_one() {
        let path = log_file("partial");
        let mut tail = LogTail::new(path.clone());
        append(&path, br#"{"type":"user","message":{"content":"hello"}}"#);
        append(&path, b"\n{\"type\":\"assistant\",\"message\":{\"stop_reas");
        let (marks, gap) = tail.read(&Claude, POLL_BYTES);
        assert!(!gap);
        assert_eq!(
            marks,
            [LogMark::Prompt {
                text: "hello".into()
            }]
        );
        append(&path, b"on\":\"end_turn\"}}\n");
        let (marks, _) = tail.read(&Claude, POLL_BYTES);
        assert_eq!(marks, [LogMark::FinalReply]);
        let _ = std::fs::remove_file(path);
    }

    /// T42 and T27: a line over the limit is classified from its head, and
    /// hitting the read bound sets a gap.
    #[test]
    fn a_very_long_line_is_classified_from_its_head_and_a_bound_sets_a_gap() {
        let path = log_file("long");
        let mut tail = LogTail::new(path.clone());
        // The file did not exist when the tail attached: it reads from 0.
        append(&path, b"{}\n");
        tail.read(&Codex, POLL_BYTES);
        let mut line = br#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"tu7","last_agent_message":""#.to_vec();
        line.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
        line.extend_from_slice(b"\"}}\n");
        append(&path, &line);
        let (marks, _) = tail.read(&Codex, CATCH_UP_BYTES);
        assert_eq!(
            marks,
            [LogMark::TurnComplete {
                turn: Some("tu7".into()),
                reply: None
            }]
        );
        // More than the budget unread: a gap, and reading goes on.
        let path2 = log_file("bound");
        let mut tail = LogTail::new(path2.clone());
        append(&path2, b"{}\n");
        tail.read(&Claude, POLL_BYTES);
        append(&path2, &vec![b'\n'; 300 * 1024]);
        let (_, gap) = tail.read(&Claude, POLL_BYTES);
        assert!(gap);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path2);
    }

    /// One Codex rollout record, with its newline.
    fn codex_event(kind: &str, turn: &str, reply: Option<&str>) -> String {
        let reply = reply.map_or_else(String::new, |text| {
            format!(r#","last_agent_message":"{text}""#)
        });
        format!(r#"{{"type":"event_msg","payload":{{"type":"{kind}","turn_id":"{turn}"{reply}}}}}"#)
            + "\n"
    }

    /// A rollout that a resumed session already holds: one earlier turn.
    fn resumed_log(name: &str) -> std::path::PathBuf {
        let path = log_file(name);
        append(
            &path,
            codex_event("task_started", "tu-old", None).as_bytes(),
        );
        append(
            &path,
            codex_event("task_complete", "tu-old", Some("old reply")).as_bytes(),
        );
        path
    }

    /// The reported case: a restarted terminal resumes a log that already
    /// holds an old turn's `task_complete`. The new prompt has its own turn id
    /// and the real turn runs for minutes. The old end is never read, so no
    /// fallback end fires, and the request ends only with the real end.
    #[test]
    fn a_resumed_log_s_old_turn_never_ends_a_new_prompt() {
        let path = resumed_log("resumed-fallback");
        let mut tail = LogTail::new(path.clone());
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, &header("r1", "a"), Some("tu-new"));
        append(
            &path,
            codex_event("task_started", "tu-new", None).as_bytes(),
        );
        for secs in [1, 3, 6] {
            let now = t0 + Duration::from_secs(secs);
            let (marks, gap) = tail.read(&Codex, POLL_BYTES);
            assert!(!gap);
            let mut out = t.on_log(marks, now);
            out.extend(t.on_tick(now));
            assert!(out.is_empty(), "{out:?}");
        }
        assert_eq!(t.open_prompts(), 1, "the new prompt stays open");
        assert_eq!(t.phase(), Phase::Working);
        // Two minutes later the real turn completes, and its notify ends it.
        let later = t0 + Duration::from_secs(120);
        append(
            &path,
            codex_event("task_complete", "tu-new", Some("new reply")).as_bytes(),
        );
        let (marks, _) = tail.read(&Codex, POLL_BYTES);
        let out = t.on_log(marks, later);
        assert!(out.is_empty(), "{out:?}");
        let out = notify(
            &mut t,
            MAIN,
            "tu-new",
            &[],
            0,
            Reply::Text(text("new reply")),
            later,
        );
        assert_eq!(ended(&out)["r1"], ok("new reply"));
        assert_eq!(events(&out), ["finished"]);
        let _ = std::fs::remove_file(path);
    }

    /// The same resumed log, with no notify: the real `task_complete` is the
    /// end once its wait has passed, and it is the new turn's reply.
    #[test]
    fn a_real_task_complete_ends_the_new_prompt_after_the_wait() {
        let path = resumed_log("resumed-real");
        let mut tail = LogTail::new(path.clone());
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let t0 = Instant::now();
        prompt(&mut t, &header("r1", "a"), Some("tu-new"));
        append(
            &path,
            codex_event("task_complete", "tu-new", Some("new reply")).as_bytes(),
        );
        let (marks, _) = tail.read(&Codex, POLL_BYTES);
        assert!(t.on_log(marks, t0).is_empty());
        assert!(t.on_tick(t0 + Duration::from_secs(1)).is_empty());
        let out = t.on_tick(t0 + TASK_COMPLETE_WAIT + Duration::from_millis(1));
        assert_eq!(ended(&out)["r1"], ok("new reply"));
        assert_eq!(events(&out), ["finished"]);
        let _ = std::fs::remove_file(path);
    }

    /// A tail on an existing log starts at its end; on a file that does not
    /// exist yet it starts at 0.
    #[test]
    fn a_tail_starts_at_the_end_of_an_existing_log_and_at_zero_for_a_new_one() {
        let existing = resumed_log("attach-existing");
        let mut tail = LogTail::new(existing.clone());
        let (marks, gap) = tail.read(&Codex, POLL_BYTES);
        assert!(marks.is_empty(), "{marks:?}");
        assert!(!gap);
        append(
            &existing,
            codex_event("task_complete", "tu-new", Some("r")).as_bytes(),
        );
        let (marks, _) = tail.read(&Codex, POLL_BYTES);
        assert_eq!(
            marks,
            [LogMark::TurnComplete {
                turn: Some("tu-new".into()),
                reply: Some("r".into())
            }]
        );
        let missing = log_file("attach-missing");
        let mut tail = LogTail::new(missing.clone());
        append(
            &missing,
            codex_event("task_complete", "tu-first", Some("first")).as_bytes(),
        );
        let (marks, _) = tail.read(&Codex, POLL_BYTES);
        assert_eq!(
            marks,
            [LogMark::TurnComplete {
                turn: Some("tu-first".into()),
                reply: Some("first".into())
            }]
        );
        let _ = std::fs::remove_file(existing);
        let _ = std::fs::remove_file(missing);
    }

    /// A log replaced by a shorter file is read from its start.
    #[test]
    fn a_replaced_shorter_log_is_read_from_its_start() {
        let path = resumed_log("replaced");
        let mut tail = LogTail::new(path.clone());
        std::fs::write(
            &path,
            codex_event("task_complete", "tu-next", Some("next")).as_bytes(),
        )
        .unwrap();
        let (marks, _) = tail.read(&Codex, POLL_BYTES);
        assert_eq!(
            marks,
            [LogMark::TurnComplete {
                turn: Some("tu-next".into()),
                reply: Some("next".into())
            }]
        );
        let _ = std::fs::remove_file(path);
    }
}
