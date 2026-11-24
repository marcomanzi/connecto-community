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
use connecto_core::{BackgroundReport, EventText, Reply, ToolEvent};

/// How long a Codex `task_complete` waits for the `notify` that normally
/// reports the same end.
pub const TASK_COMPLETE_WAIT: Duration = Duration::from_secs(5);
/// Ended Codex turn ids remembered per attempt.
const ENDED_KEEP: usize = 64;
/// Codex turn starts remembered, for the stale-fallback guard.
const STARTED_KEEP: usize = 64;
/// Claude: end records remembered, and prompt records waiting for their hook.
const ENDS_KEEP: usize = 64;
const UNCLAIMED_KEEP: usize = 64;
/// How much of a prompt identifies it.
const HEAD: usize = 256;
/// How long the turn log is still read after a Claude `Stop` with nothing
/// left open: long enough for another `Stop` hook to keep the turn going and
/// the log to show it (Claude's default timeout for a command hook is 600 s,
/// and a hook that blocks does so within it). After that an idle terminal's
/// log is not read until its next prompt.
pub const LOG_AFTER_STOP: Duration = Duration::from_secs(600);
/// How long a Claude `Stop` with several prompts open waits for the turn
/// log to show its `end_turn` record. Claude Code can run the hook before the
/// record is flushed; deciding on the log as it was then closes the wrong
/// prompt and leaves the terminal `working` (the stuck-wake bug).
pub const STOP_LOG_WAIT: Duration = Duration::from_secs(2);

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
    /// A turn end that failed (`StopFailure`): it has no reply, and no reply
    /// an earlier stop kept is its. Not a turn for the manual relay.
    TurnFailed,
}

/// Request ids in `prompt`: `[connecto from <id>, request r<n>.` at the start of
/// any line, or `[connecto from <id> in project <name>, request r<n>.` for a
/// sender from another project (ADR 0035). An id counts only if that request
/// was sent to this terminal and is unbound, which the bridge decides.
#[must_use]
pub fn request_ids(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("[connecto from ")?;
            let (from, rest) = rest.split_once(", request r")?;
            let valid_from = match from.split_once(" in project ") {
                Some((id, project)) => {
                    crate::terminal_list::valid_id(id)
                        && crate::bridge::header_name(project) == project
                }
                None => crate::terminal_list::valid_id(from),
            };
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
    /// A task notification that came while a `Stop` waited: its owner is
    /// known once that `Stop` is decided. The task it names, if any.
    awaits_owner: Option<Option<String>>,
    /// Claude: the read position of the log record assigned to this prompt.
    /// Assigned once, when the record is read, and never changed.
    record: Option<u64>,
}

/// What kind of turn end a Claude `Stop` hook reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StopKind {
    /// `StopFailure`: the turn ended in an API error.
    failed: bool,
    /// `stop_hook_active`: the end of a turn that another `Stop` hook kept
    /// going, not of a new turn.
    continued: bool,
}

/// A Claude `Stop` not decided yet.
struct DeferredStop {
    reply: Reply,
    background: Vec<BackgroundReport>,
    /// When it arrived: a task it is the first to list started then.
    at: Instant,
    kind: StopKind,
    /// The first prompt sequence number accepted after it: only prompts
    /// before it can be its turn's.
    boundary: u64,
    due: Instant,
}

/// Why a Claude `Stop` is being decided.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Decide {
    /// When it arrives, or when a record is read while it waits: only what
    /// the log already shows can decide it.
    AtStop,
    /// The first end record read after it, at this position.
    EndRead(u64),
    /// Its wait is over, or another event needs it decided: it closes its
    /// own turn's first prompt at least.
    Now,
}

/// A `Stop` that closed its turn's first prompt by count, before the log
/// showed which other prompts open then were in that turn. They are closed
/// with its reply when the log shows it.
struct LateTurn {
    /// The record of the turn's first prompt, if it was read.
    head: Option<u64>,
    /// Records read when it was decided: with no head record, the first end
    /// read after this is the turn's.
    read_at: u64,
    boundary: u64,
    /// It ended in an API error.
    failed: bool,
    /// The sequence number of the prompt it closed.
    first_seq: u64,
}

/// A closed turn whose background work is still running.
#[derive(Debug, Clone)]
struct Waiting {
    seq: u64,
    requests: Vec<String>,
    ambiguous: bool,
    reply: Reply,
}

/// Claude: background work a `Stop` listed as running, and when Connecto
/// first saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub report: BackgroundReport,
    /// The `Stop` that first listed it.
    pub since: Instant,
}

impl Running {
    /// `shell`, `subagent`, ...; `task` when the tool did not say.
    #[must_use]
    pub fn kind(&self) -> &str {
        self.report
            .kind
            .as_ref()
            .map_or("task", |kind| kind.expose().as_str())
    }

    /// What to call it: a shell's command, otherwise its description, its
    /// type, or its id. Raw tool text: redact it before it is shown.
    #[must_use]
    pub fn label(&self) -> &str {
        let report = &self.report;
        let command = report.command.as_ref().filter(|_| self.kind() == "shell");
        command
            .or(report.description.as_ref())
            .or(report.kind.as_ref())
            .unwrap_or(&report.id)
            .expose()
    }

    #[must_use]
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.since)
    }
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
    /// Claude: the background work last listed, by task id.
    tasks: BTreeMap<String, Running>,
    /// Claude: read positions of log records (prompt and end records), from 1.
    read_pos: u64,
    /// Claude: positions of the end records read, oldest first.
    ends: VecDeque<u64>,
    /// Claude: prompt records read before their prompt's hook, since the last
    /// end record, with their text.
    unclaimed: Vec<(u64, String)>,
    /// Claude: turns closed by count whose other prompts the log has not
    /// placed yet.
    late: Vec<LateTurn>,
    /// Claude: prompts closed by count before their record was read, with
    /// their text. Each still takes its own record when it is read, so no
    /// later prompt with the same text is given it. Until the next end record.
    ghosts: Vec<(u64, String)>,
    /// Claude: the end record of the turn the last `Stop` closed, once read.
    turn_end: Option<u64>,
    /// Claude: the last `Stop` was decided before its end record was read.
    awaiting_end: bool,
    /// A turn log is attached: a `Stop` may wait for its records.
    has_log: bool,
    /// Claude: where the last main-conversation work was read.
    last_activity: Option<u64>,
    /// Claude: the log is read until then after the last `Stop`.
    read_until: Option<Instant>,
    /// Claude: after a `Stop`, the log showed the turn going on (another
    /// `Stop` hook kept it running): `working` until its next `Stop`.
    continuing: bool,
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
    /// Claude: a `Stop` waiting for its `end_turn` record.
    deferred: Option<DeferredStop>,
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
            tasks: BTreeMap::new(),
            read_pos: 0,
            ends: VecDeque::new(),
            unclaimed: Vec::new(),
            late: Vec::new(),
            ghosts: Vec::new(),
            turn_end: None,
            awaiting_end: false,
            has_log: false,
            last_activity: None,
            read_until: None,
            continuing: false,
            ended: VecDeque::new(),
            started: VecDeque::new(),
            late_expected: 0,
            pinned: None,
            input_seen: 0,
            pending: Vec::new(),
            fallback: true,
            deferred: None,
        }
    }

    /// A turn log is attached: `Stop`s are paired with prompts by its records.
    /// Without one, each `Stop` closes the oldest open prompt at once.
    pub fn log_attached(&mut self) {
        self.has_log = true;
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

    /// Claude: the background work last listed as running, oldest first.
    #[must_use]
    pub fn background(&self) -> Vec<Running> {
        let mut running: Vec<Running> = self.tasks.values().cloned().collect();
        running.sort_by_key(|task| task.since);
        running
    }

    /// The owned tasks, as the last `Stop`s reported them.
    fn owned_tasks(&self) -> Vec<BackgroundReport> {
        self.task_owner
            .keys()
            .map(|task| {
                self.tasks.get(task).map_or_else(
                    || BackgroundReport::id_only(EventText::new(task.clone())),
                    |running| running.report.clone(),
                )
            })
            .collect()
    }

    /// Prompts accepted and not yet ended.
    #[must_use]
    pub fn open_prompts(&self) -> usize {
        self.open.len()
    }

    /// Whether a turn log should be tailed now: while anything is open,
    /// waiting or running, and for [`LOG_AFTER_STOP`] after a Claude `Stop`,
    /// so a turn another `Stop` hook keeps going (and an end record read
    /// late) is seen.
    #[must_use]
    pub fn wants_log(&self, now: Instant) -> bool {
        !self.open.is_empty()
            || !self.pending.is_empty()
            || self.deferred.is_some()
            || self.phase == Phase::Working
            || self.read_until.is_some_and(|until| now < until)
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
    pub fn on_event(&mut self, event: ToolEvent, now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        if matches!(event, ToolEvent::TurnEnded { .. }) {
            // An earlier `Stop` still waiting is decided first, on the log as
            // read now: ends are taken in order.
            self.flush_deferred(&mut out);
        }
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
                failure,
                continued,
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
                    let end = StopKind {
                        failed: failure.is_some(),
                        continued,
                    };
                    self.claude_stop(reply, &background, end, now, &mut out);
                }
            }
            ToolEvent::NeedsUser => {
                self.needs_user = true;
                out.push(Out::Event(BridgeEvent::NeedsUser));
            }
            // Only the session to resume: no turn begins or ends.
            ToolEvent::SessionNamed { .. } => {}
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
            awaits_owner: None,
            record: None,
        };
        if !self.is_codex() {
            // Its record may have been read before its hook arrived.
            if let Some(at) = self
                .unclaimed
                .iter()
                .position(|(_, head)| same_head(head, &open.head))
            {
                open.record = Some(self.unclaimed.remove(at).0);
            }
        }
        // Codex takes a message sent during a turn into that turn: its prompt
        // hook names the running turn, and the turn ends with one notify. The
        // message joins the prompt already open for that turn, so that one
        // end closes both; a second open prompt would keep the terminal
        // `working` for good.
        if self.is_codex()
            && let Some(joined) = open.turn.as_deref().and_then(|id| {
                self.open
                    .iter_mut()
                    .find(|other| other.turn.as_deref() == Some(id))
            })
        {
            for request in request_ids(text) {
                out.push(Out::Bound(request.clone()));
                joined.requests.push(request);
            }
            self.needs_user = false;
            out.push(Out::Prompt);
            return;
        }
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
        if self.deferred.is_some() {
            // Which turn owns which task is known once the waiting `Stop` is
            // decided (it may start tasks, and it forgets the ones that ended).
            // Ask then, as if its end had been read in time.
            open.awaits_owner = Some(task.map(str::to_owned));
            return;
        }
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
    //
    // Pairing a `Stop` with its prompts.
    //
    // Turns end in order, and each fires one `Stop`: a `Stop` belongs to the
    // oldest turn that has not had one, and that turn holds the oldest open
    // prompt. So a `Stop` always closes the oldest open prompt (by count, not
    // by text or timing), and never a prompt accepted after it.
    //
    // Which other prompts open then were in its turn (messages Claude queued
    // into the running turn) only the log shows. The hook carries only the
    // prompt's text, and the log's records carry ids the hook never sees, so
    // a prompt and its record are linked by text and order: hooks and records
    // come in the same order. Each prompt record, when read, is assigned once
    // to the earliest open prompt with no record and the same text, and never
    // reassigned. A turn ends at the first end record after its first
    // prompt's record; the prompts whose records come before that end are in
    // it. A `Stop` waits up to [`STOP_LOG_WAIT`] for that end; past it, it
    // closes its first prompt alone, and the others are closed with its reply
    // when the log places them (that end read late, or a prompt accepted after
    // the `Stop` showing its record, which a separate queued turn could not
    // yet have).
    //
    // Invariants: no `Stop` closes a prompt accepted after it; no prompt is
    // closed by an end record that precedes its own record; a prompt is
    // closed without a record only as its turn's first prompt (by count), or
    // when a prompt it was accepted before is closed by the same end. And a
    // wrong `ok` is worse than `unmatched`: a request whose turn the log
    // cannot show for sure (left open by a turn closed by count, or past an
    // end record that is a later turn's because a prompt accepted after this
    // `Stop` is logged before it) ends `unmatched`, never with a reply that
    // may be another turn's.
    //
    // Turn ends Claude reports (hooks reference, checked 2026-11-15):
    // - `Stop`: the main agent finished responding. Not fired on a user
    //   interrupt; not fired on an API error. `stop_hook_active` is true when
    //   the turn is going on because a `Stop` hook kept it going
    //   (`decision: "block"` or `additionalContext`; at most 8 in a row).
    // - `StopFailure`: fired instead of `Stop` when the turn ends in an API
    //   error (`error`: rate_limit, overloaded, ..., unknown). Its
    //   `last_assistant_message` is the error text. It ends the turn: the
    //   requests end `unavailable`. From Claude Code 2.1.78.
    // - `SubagentStop`: a subagent finished; not the main turn. Not used.
    // `StopFailure` is registered only for a CLI whose probed version is
    // 2.1.78 or later. On an older or unknown one, an API-error turn has an
    // end record in the log but may fire no turn end at all; the tracker does
    // not settle it from the log alone, because a `Stop` that such a CLI does
    // fire, late, would then be counted twice and could close a later, running
    // turn's prompt. Its prompts stay open (`working`) until the next `Stop`
    // or the user's next prompt. Connecto's stored CLIs are far newer.
    // A turn kept going by another `Stop` hook fires `Stop` again
    // (`stop_hook_active`), with no new prompt: that `Stop` closes no prompt
    // by count. Meanwhile the log shows main-conversation work after the
    // turn's end record, and the terminal reads `working` again from then
    // until that `Stop` (a wake typed before that shows is withdrawn before
    // Enter, or lands queued and is closed with the turn).
    //
    // Where counting can be wrong, and how it resolves:
    // - An interrupted turn fires no `Stop`: the interrupt mark in the log
    //   closes its prompts.
    // - A turn whose prompt hook was lost fires a `Stop` with no prompt of
    //   its own: with nothing open, it settles the phase; with a prompt queued
    //   for the next turn open, it closes that one early (the gap that was
    //   there before; the next `Stop` then finds nothing and settles).
    // - A background task's notification is a turn with its own hook and
    //   `Stop`, counted like any other.
    // - A queued message absorbed into a turn whose end record is absent or
    //   unreadable, with no later prompt logged, cannot be told from a queued
    //   separate turn still to run: it stays open (`working`) until a later
    //   prompt's record, a `Stop` or the user's next prompt settles it. It is
    //   never closed while it might be running.

    /// A prompt record read from the log.
    fn read_prompt_record(&mut self, text: &str, out: &mut Vec<Out>) {
        self.read_pos += 1;
        let head = self
            .adapter
            .ack_form(&text.chars().take(HEAD).collect::<String>());
        let at = self.read_pos;
        let open_seq = self
            .open
            .iter()
            .find(|open| open.record.is_none() && same_head(&open.head, &head))
            .map(|open| open.seq);
        let ghost = self
            .ghosts
            .iter()
            .position(|(seq, text)| same_head(text, &head) && open_seq.is_none_or(|o| *seq < o));
        if let Some(index) = ghost {
            // The record of a prompt already closed by count.
            let (seq, _) = self.ghosts.remove(index);
            for turn in &mut self.late {
                if turn.first_seq == seq {
                    turn.head = Some(at);
                }
            }
            return;
        }
        match self.open.iter_mut().find(|open| Some(open.seq) == open_seq) {
            Some(open) => open.record = Some(at),
            None => {
                self.unclaimed.push((at, head));
                if self.unclaimed.len() > UNCLAIMED_KEEP {
                    self.unclaimed.remove(0);
                }
                return;
            }
        }
        // A waiting `Stop` may have been waiting for exactly this record.
        if let Some(stop) = self.deferred.take() {
            self.deferred = self.run_decision(stop, Decide::AtStop, out);
        }
    }

    /// An end record read from the log.
    fn read_end_record(&mut self, out: &mut Vec<Out>) {
        self.read_pos += 1;
        let end = self.read_pos;
        let previous = self.ends.back().copied();
        self.ends.push_back(end);
        if self.ends.len() > ENDS_KEEP {
            self.ends.pop_front();
        }
        // A prompt record before an end with no hook by now has none coming,
        // and a closed prompt whose record is not before it has none readable.
        self.unclaimed.clear();
        if std::mem::take(&mut self.awaiting_end) {
            self.turn_end = Some(end);
        }
        // Turns closed by count: this end, if it is theirs, places the rest.
        for turn in std::mem::take(&mut self.late) {
            let theirs = match turn.head {
                Some(head) => head < end && previous.is_none_or(|p| p < head),
                None => turn.read_at < end && previous.is_none_or(|p| p <= turn.read_at),
            };
            if theirs {
                self.close_late(&turn, end, out);
            } else {
                self.late.push(turn);
            }
        }
        self.ghosts.clear();
        self.prune_late();
        if let Some(stop) = self.deferred.take() {
            self.deferred = self.run_decision(stop, Decide::EndRead(end), out);
        } else if self.open.is_empty() {
            self.settle(out);
        }
    }

    /// Close the prompts `turn` left open whose record comes before `end`.
    /// They were in that turn or in a separate one queued before its `Stop`;
    /// with that turn's own end not seen in time, nothing tells which, so
    /// they end `unmatched` (`unavailable` for a failed turn), never with a
    /// reply that may be another turn's. Prompts accepted after its `Stop` are
    /// set aside meanwhile, so its end, and the next turn's start, are
    /// reported in order.
    fn close_late(&mut self, turn: &LateTurn, end: u64, out: &mut Vec<Out>) {
        let split = self
            .open
            .iter()
            .position(|open| open.seq >= turn.boundary)
            .unwrap_or(self.open.len());
        let rest = self.open.split_off(split);
        let mut kept = Vec::new();
        for prompt in std::mem::take(&mut self.open) {
            if prompt.record.is_some_and(|at| at < end) {
                Self::end_requests(out, &prompt.requests, &Self::uncertain(turn.failed));
            } else {
                kept.push(prompt);
            }
        }
        self.open = kept;
        if !rest.is_empty() {
            self.settle(out);
            self.open.extend(rest);
            if self.phase != Phase::Working {
                self.phase = Phase::Working;
                out.push(Out::Event(BridgeEvent::Began));
            }
        }
    }

    /// The end of a request whose turn is not certain.
    const fn uncertain(failed: bool) -> RequestEnd {
        if failed {
            RequestEnd::Unavailable("the turn ended in an API error")
        } else {
            RequestEnd::Unmatched("the evidence could not show which turn answered")
        }
    }

    /// Main-conversation work read from the log. After a `Stop`, with nothing
    /// open, it means the turn went on: another `Stop` hook kept it running
    /// (or a turn started with no prompt hook). It reads `working` until the
    /// next `Stop`. Only work logged after that turn's end record counts: the
    /// turn's own records, read late, come before it.
    fn read_activity(&mut self, out: &mut Vec<Out>) {
        self.read_pos += 1;
        self.last_activity = Some(self.read_pos);
        let after_end = self.turn_end.is_some_and(|end| self.read_pos > end) && !self.awaiting_end;
        if after_end
            && self.open.is_empty()
            && self.deferred.is_none()
            && self.phase != Phase::Working
        {
            self.continuing = true;
            self.phase = Phase::Working;
            out.push(Out::Event(BridgeEvent::Began));
        }
    }

    /// Forget turns closed by count with none of their prompts open.
    fn prune_late(&mut self) {
        let oldest = self.open.first().map(|open| open.seq);
        self.late
            .retain(|turn| oldest.is_some_and(|seq| seq < turn.boundary));
    }

    /// Settle the phase with nothing open, keeping the tasks known to run.
    fn settle(&mut self, out: &mut Vec<Out>) {
        if self.phase == Phase::Working && self.open.is_empty() {
            let running = self.owned_tasks();
            self.after_close(&running, None, None, out);
        }
    }

    fn claude_stop(
        &mut self,
        reply: Reply,
        background: &[BackgroundReport],
        kind: StopKind,
        now: Instant,
        out: &mut Vec<Out>,
    ) {
        if kind.failed {
            out.push(Out::TurnFailed);
        } else {
            out.push(Out::Turn {
                input: None,
                reply: reply.clone(),
            });
        }
        // A failed turn lists no background work: what ran still runs.
        let background: Vec<BackgroundReport> = if kind.failed {
            self.owned_tasks()
        } else {
            background.to_vec()
        };
        self.continuing = false;
        self.read_until = Some(now + LOG_AFTER_STOP);
        let stop = DeferredStop {
            reply,
            kind,
            background,
            at: now,
            boundary: self.next_seq,
            due: now + STOP_LOG_WAIT,
        };
        self.deferred = self.run_decision(stop, Decide::AtStop, out);
    }

    /// Decide `stop` with prompts accepted after it set aside, so it, and the
    /// phase, are settled as if its end had been read in time; then take them
    /// back as they were then. Gives the `Stop` back when it must wait.
    fn run_decision(
        &mut self,
        stop: DeferredStop,
        why: Decide,
        out: &mut Vec<Out>,
    ) -> Option<DeferredStop> {
        let split = self
            .open
            .iter()
            .position(|open| open.seq >= stop.boundary)
            .unwrap_or(self.open.len());
        let later = self.open.split_off(split);
        if !self.decide(&stop, &later, why, out) {
            self.open.extend(later);
            return Some(stop);
        }
        for mut prompt in later {
            if let Some(task) = prompt.awaits_owner.take() {
                self.continuation(&mut prompt, task.as_deref());
            }
            self.open.push(prompt);
            if self.phase != Phase::Working {
                self.phase = Phase::Working;
                out.push(Out::Event(BridgeEvent::Began));
            }
        }
        self.prune_late();
        None
    }

    /// Close `stop`'s turn: its first prompt by count, and the others open
    /// before it that the log places in it. False when it must wait.
    fn decide(
        &mut self,
        stop: &DeferredStop,
        later: &[Open],
        why: Decide,
        out: &mut Vec<Out>,
    ) -> bool {
        // Prompts a turn closed by count left open: nothing tells whether
        // they were in it, so they are ambiguous, and never get a reply.
        let leftover: Vec<u64> = self
            .open
            .iter()
            .filter(|open| self.late.iter().any(|turn| open.seq < turn.boundary))
            .map(|open| open.seq)
            .collect();
        for open in &mut self.open {
            if leftover.contains(&open.seq) {
                open.ambiguous = true;
            }
        }
        let Some(first) = self.open.first() else {
            // A turn whose prompt hook was lost, a continued turn ending, or a
            // stray end.
            self.awaiting_end = true;
            self.after_close(&stop.background, None, Some(stop.at), out);
            return true;
        };
        if !self.has_log && self.read_pos == 0 && why != Decide::Now {
            // No log read at all: the rule from before it was used. The one
            // turn is the first prompt's.
            self.close_by_count(stop, out);
            return true;
        }
        // The end of this turn: the first end record after its first prompt's
        // record; that record unreadable, after the record of the earliest
        // prompt that has one (it came after the first prompt); or the first
        // end read after the `Stop`. A continued turn has no prompt of its
        // own: its end is the latest.
        let anchor = if stop.kind.continued {
            None
        } else {
            first
                .record
                .or_else(|| self.open.iter().find_map(|open| open.record))
        };
        let end = match (anchor, why) {
            (Some(head), _) => self.ends.iter().copied().find(|end| *end > head),
            (None, Decide::EndRead(end)) => Some(end),
            (None, _) if stop.kind.continued => self.ends.back().copied(),
            (None, _) => None,
        };
        // An end after the record of a prompt accepted after this `Stop` is a
        // later turn's: this turn's own end is missing or unreadable. Its
        // first prompt closes by count; the rest cannot be placed.
        let foreign = end.is_some_and(|end| {
            later
                .iter()
                .any(|open| open.record.is_some_and(|at| at < end))
        });
        if foreign {
            if stop.kind.continued {
                self.after_close(&stop.background, None, Some(stop.at), out);
            } else {
                self.close_by_count(stop, out);
            }
            return true;
        }
        if let Some(end) = end {
            // A later turn of prompts open now has ended too: the `Stop`s
            // between were lost (a turn ends, and fires its `Stop`, before the
            // next one starts). This `Stop` is the latest ended turn's.
            let latest = self
                .open
                .iter()
                .filter_map(|open| open.record)
                .filter_map(|at| self.ends.iter().copied().find(|end| *end > at))
                .max()
                .unwrap_or(end);
            self.close_turn(stop, latest.max(end), out);
            return true;
        }
        if why == Decide::AtStop {
            return false;
        }
        if stop.kind.continued {
            // Nothing new was placed in the continued turn: it ends here.
            self.awaiting_end = true;
            self.after_close(&stop.background, None, Some(stop.at), out);
            return true;
        }
        self.close_by_count(stop, out);
        true
    }

    /// By count: the first prompt open is this turn's. The others stay open
    /// until the log places them.
    fn close_by_count(&mut self, stop: &DeferredStop, out: &mut Vec<Out>) {
        let first = self.open.remove(0);
        if first.record.is_none() {
            self.ghosts.push((first.seq, first.head.clone()));
        }
        if !self.open.is_empty() {
            self.late.push(LateTurn {
                head: first.record,
                read_at: self.read_pos,
                boundary: stop.boundary,
                failed: stop.kind.failed,
                first_seq: first.seq,
            });
        }
        self.awaiting_end = true;
        self.close_claude(vec![first], stop, out);
    }

    /// Close the turn that `end` ends: the prompts whose record comes before
    /// `end`, its first prompt (by count; not for a continued turn, which has
    /// none of its own), and those with no record accepted before one of them.
    fn close_turn(&mut self, stop: &DeferredStop, end: u64, out: &mut Vec<Out>) {
        let previous = self.ends.iter().copied().filter(|at| *at < end).max();
        let last = self
            .open
            .iter()
            .rposition(|open| open.record.is_some_and(|at| at < end));
        let by_count = !stop.kind.continued;
        let mut closed = Vec::new();
        let mut kept = Vec::new();
        for (index, prompt) in std::mem::take(&mut self.open).into_iter().enumerate() {
            match prompt.record {
                Some(at) if at < end => {
                    if previous.is_some_and(|p| at < p) {
                        // It had its own turn, and that end was never reported.
                        Self::end_requests(
                            out,
                            &prompt.requests,
                            &RequestEnd::Unavailable("that turn's end was never reported"),
                        );
                    } else {
                        closed.push(prompt);
                    }
                }
                None if index == 0 && by_count => closed.push(prompt),
                // Accepted before a prompt this end closes, so logged before
                // it, though its record could not be read.
                None if last.is_some_and(|last| index <= last) => Self::end_requests(
                    out,
                    &prompt.requests,
                    &RequestEnd::Unmatched("the evidence could not show which turn answered"),
                ),
                _ => kept.push(prompt),
            }
        }
        self.open = kept;
        self.turn_end = Some(end);
        self.awaiting_end = false;
        self.close_claude(closed, stop, out);
        // The relay reads the log just before it hands over a `Stop`: work
        // after this turn's end may be read already. The turn went on.
        if self.last_activity.is_some_and(|at| at > end)
            && self.open.is_empty()
            && self.phase != Phase::Working
        {
            self.continuing = true;
            self.phase = Phase::Working;
            out.push(Out::Event(BridgeEvent::Began));
        }
    }

    /// Decide a waiting `Stop` now, on the log as read.
    fn flush_deferred(&mut self, out: &mut Vec<Out>) {
        let Some(stop) = self.deferred.take() else {
            return;
        };
        // `Now` always decides.
        let _ = self.run_decision(stop, Decide::Now, out);
    }

    /// Each closed prompt's requests wait on background work or end now, and
    /// the phase settles. An ambiguous prompt's requests end `unmatched` now,
    /// never sharing the reply; a failed turn's end `unavailable`.
    fn close_claude(&mut self, closed: Vec<Open>, stop: &DeferredStop, out: &mut Vec<Out>) {
        let background = stop.background.as_slice();
        let reply = stop.reply.clone();
        let owner = closed
            .iter()
            .find_map(|prompt| prompt.continues)
            .unwrap_or_else(|| closed.first().map_or(0, |prompt| prompt.seq));
        let mut requests: Vec<String> = Vec::new();
        for prompt in &closed {
            if prompt.ambiguous || stop.kind.failed {
                Self::end_requests(out, &prompt.requests, &Self::uncertain(stop.kind.failed));
            } else {
                requests.extend(prompt.requests.iter().cloned());
            }
        }
        if stop.kind.failed {
            // A continued turn that failed: what waited on it is not answered.
            if let Some(index) = self.waiting.iter().position(|w| w.seq == owner) {
                let waiting = self.waiting.remove(index);
                Self::end_requests(out, &waiting.requests, &Self::uncertain(true));
            }
        } else if closed.iter().all(|prompt| prompt.continues.is_some()) && !closed.is_empty() {
            // The reply replaces the continued turn's latest.
            if let Some(waiting) = self.waiting.iter_mut().find(|w| w.seq == owner) {
                waiting.reply = reply;
            }
        } else if background.is_empty() {
            Self::end_requests(out, &requests, &Self::reply_end(&reply, false));
        } else {
            self.waiting.push(Waiting {
                seq: owner,
                requests,
                ambiguous: false,
                reply,
            });
        }
        self.after_close(background, Some(owner), Some(stop.at), out);
    }

    /// Own the new tasks, forget the gone ones, and settle the phase. A task
    /// first listed now is taken to have started `at`.
    fn after_close(
        &mut self,
        background: &[BackgroundReport],
        owner: Option<u64>,
        at: Option<Instant>,
        out: &mut Vec<Out>,
    ) {
        let running: Vec<String> = background
            .iter()
            .map(|task| task.id.expose().clone())
            .collect();
        self.task_owner.retain(|task, _| running.contains(task));
        self.tasks.retain(|task, _| running.contains(task));
        for report in background {
            let id = report.id.expose();
            if let Some(known) = self.tasks.get_mut(id) {
                // The latest words for it; the time it was first seen stays.
                known.report = report.clone();
            } else if let Some(since) = at {
                self.tasks.insert(
                    id.clone(),
                    Running {
                        report: report.clone(),
                        since,
                    },
                );
            }
        }
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
                    if !self.is_codex() {
                        self.read_prompt_record(&text, &mut out);
                    }
                }
                LogMark::FinalReply => {
                    if !self.is_codex() {
                        self.read_end_record(&mut out);
                    }
                }
                LogMark::Interrupt { turn } => {
                    if self.is_codex() {
                        self.codex_abort(turn, &mut out);
                    } else {
                        self.flush_deferred(&mut out);
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
                LogMark::Activity => {
                    if !self.is_codex() {
                        self.read_activity(&mut out);
                    }
                }
                LogMark::Other => {}
            }
        }
        out
    }

    fn claude_interrupt(&mut self, out: &mut Vec<Out>) {
        // The prompts whose record the tail has already passed.
        let passed: Vec<usize> = self
            .open
            .iter()
            .enumerate()
            .filter(|(_, open)| open.record.is_some())
            .map(|(index, _)| index)
            .collect();
        let victims = if passed.is_empty() && self.open.len() == 1 {
            vec![0]
        } else {
            passed
        };
        for index in victims.into_iter().rev() {
            let prompt = self.open.remove(index);
            Self::end_requests(out, &prompt.requests, &RequestEnd::Interrupted);
        }
        self.prune_late();
        self.continuing = false;
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
        if self.deferred.as_ref().is_some_and(|stop| now >= stop.due) {
            self.flush_deferred(&mut out);
        }
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
        // Records in the gap were never read: their prompts stay unassigned,
        // and only a `Stop`'s fallback can close them.
        self.unclaimed.clear();
    }

    /// The user ended a delivery whose turn will never end (End delivery). It
    /// is the user's word that the terminal is not working: open prompts are
    /// forgotten, their requests end `unavailable`, and the phase is `Idle`. No
    /// event is reported, because nothing was observed.
    pub fn user_ended(&mut self) -> Vec<Out> {
        let mut out = Vec::new();
        self.flush_deferred(&mut out);
        self.late.clear();
        self.ghosts.clear();
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
        self.flush_deferred(&mut out);
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
        self.flush_deferred(&mut out);
        for open in std::mem::take(&mut self.open) {
            Self::end_requests(&mut out, &open.requests, &RequestEnd::Exited);
        }
        for waiting in std::mem::take(&mut self.waiting) {
            Self::end_requests(&mut out, &waiting.requests, &RequestEnd::Exited);
        }
        self.pending.clear();
        self.late.clear();
        self.ghosts.clear();
        self.task_owner.clear();
        self.tasks.clear();
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
                background: background
                    .iter()
                    .map(|id| BackgroundReport::id_only(text(id)))
                    .collect(),
                thread: None,
                turn: None,
                failure: None,
                continued: false,
            },
            Instant::now(),
        )
    }

    /// A Claude `Stop` at `now` listing `tasks` with their details.
    fn stop_with(tracker: &mut Tracker, tasks: &[BackgroundReport], now: Instant) -> Vec<Out> {
        tracker.on_event(
            ToolEvent::TurnEnded {
                input: Vec::new(),
                input_total: 0,
                reply: Reply::None,
                lifecycle_known: true,
                background: tasks.to_vec(),
                thread: None,
                turn: None,
                failure: None,
                continued: false,
            },
            now,
        )
    }

    fn task(
        id: &str,
        kind: Option<&str>,
        description: Option<&str>,
        command: Option<&str>,
    ) -> BackgroundReport {
        BackgroundReport {
            id: text(id),
            kind: kind.map(text),
            description: description.map(text),
            command: command.map(text),
        }
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
                failure: None,
                continued: false,
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
        // A sender from another project is named with its project.
        assert_eq!(
            request_ids(&header("r7", "claude-55 in project harbor app")),
            ["r7"]
        );
        assert!(request_ids("[connecto from a in project x]y, request r1.]").is_empty());
        assert!(request_ids("[connecto from a in project , request r1.]").is_empty());
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

    /// Each running task keeps the time of the `Stop` that first listed it,
    /// and is named by its command (a shell), else its description, type or
    /// id. A task no longer listed is forgotten.
    #[test]
    fn background_tasks_keep_when_they_were_first_seen_and_a_label() {
        let mut t = claude();
        let start = Instant::now();
        prompt(&mut t, "grind the log", None);
        let shell = task("b1", Some("shell"), Some("Grind"), Some("python3 grind.py"));
        stop_with(&mut t, std::slice::from_ref(&shell), start);
        assert_eq!(t.phase(), Phase::Background);
        prompt(&mut t, "and review it", None);
        let later = start + Duration::from_secs(600);
        let agent = task("a2", Some("subagent"), Some("Review"), None);
        stop_with(
            &mut t,
            &[shell.clone(), agent.clone(), task("x", None, None, None)],
            later,
        );
        let running = t.background();
        let seen: Vec<(&str, &str, &str, Duration)> = running
            .iter()
            .map(|task| {
                (
                    task.report.id.expose().as_str(),
                    task.kind(),
                    task.label(),
                    task.age(later + Duration::from_secs(60)),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("b1", "shell", "python3 grind.py", Duration::from_secs(660)),
                ("a2", "subagent", "Review", Duration::from_secs(60)),
                ("x", "task", "x", Duration::from_secs(60)),
            ]
        );
        // A task's type alone names it; a command only names a shell.
        let monitor = task("m", Some("monitor"), None, Some("tail -f log"));
        let named = Running {
            report: monitor,
            since: start,
        };
        assert_eq!(named.label(), "monitor");
        // The shell ended: only the others are left, still Background.
        stop_with(&mut t, &[agent], later + Duration::from_secs(5));
        let ids: Vec<String> = t
            .background()
            .iter()
            .map(|task| task.report.id.expose().clone())
            .collect();
        assert_eq!(ids, ["a2"]);
        assert_eq!(t.phase(), Phase::Background);
        stop_with(&mut t, &[], later + Duration::from_secs(9));
        assert!(t.background().is_empty());
        assert_eq!(t.phase(), Phase::Idle);
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

    fn marks(t: &mut Tracker, marks: Vec<LogMark>) -> Vec<Out> {
        t.on_log(marks, Instant::now())
    }

    fn log_prompt(text_in: &str) -> LogMark {
        LogMark::Prompt {
            text: text_in.to_owned(),
        }
    }

    const NOTE: &str = "<task-notification>\n<task-id>acf0</task-id>\ndone";
    const WAKE: &str = "[connecto notice] Request r58 ended (ok). Run: connecto result r58";

    /// The stuck-wake bug, replayed from a real session: a background task's
    /// notification starts a turn by itself, a wake typed a moment later is
    /// taken into that turn as a queued message (both hooks fire, so two
    /// prompts are open), and Claude Code runs `Stop` before the turn's
    /// `end_turn` record reaches the log. Deciding then closed the older
    /// prompt and left the newer one open; every later `Stop` did the same,
    /// so the terminal read `working` for hours and every wake was parked.
    /// Now the `Stop` waits for its record and closes both.
    #[test]
    fn a_stop_that_runs_before_its_end_turn_is_logged_waits_and_closes_its_turn() {
        let mut t = claude();
        // The previous turn, fully logged.
        prompt(&mut t, "earlier", None);
        marks(&mut t, vec![log_prompt("earlier"), LogMark::FinalReply]);
        stop(&mut t, "ok", &[]);
        assert_eq!(t.phase(), Phase::Idle);
        // The self-started turn and the wake absorbed into it.
        prompt(&mut t, NOTE, None);
        prompt(&mut t, WAKE, None);
        marks(
            &mut t,
            vec![log_prompt(NOTE), log_prompt(WAKE), LogMark::Other],
        );
        let out = stop(&mut t, "done", &[]);
        assert!(events(&out).is_empty(), "nothing is guessed: {out:?}");
        assert!(
            t.wants_log(Instant::now()),
            "the log is read while the Stop waits"
        );
        // The next poll reads the end record.
        let out = marks(&mut t, vec![LogMark::FinalReply]);
        assert_eq!(events(&out), ["finished"]);
        assert!(out.contains(&Out::LeftWorking));
        assert_eq!(t.phase(), Phase::Idle);
        assert_eq!(t.open_prompts(), 0);
        // And the following turns, logged just as late, settle each time.
        for turn in ["next", "and next"] {
            prompt(&mut t, turn, None);
            marks(&mut t, vec![log_prompt(turn)]);
            stop(&mut t, "ok", &[]);
            marks(&mut t, vec![LogMark::FinalReply]);
            assert_eq!(t.phase(), Phase::Idle, "{turn}");
        }
    }

    /// Past the wait a `Stop` closes its turn's first prompt by count; a
    /// queued prompt the log has not placed stays open (`working`) until the
    /// turn's end record, however late, closes it, `unmatched`.
    #[test]
    fn placed_prompts_wait_for_their_end_past_the_limit() {
        let mut t = claude();
        // An earlier turn's end shows the log's end records can be read.
        marks(&mut t, vec![log_prompt("earlier"), LogMark::FinalReply]);
        prompt(&mut t, NOTE, None);
        prompt(&mut t, &header("r1", "a"), None);
        marks(
            &mut t,
            vec![log_prompt(NOTE), log_prompt(&header("r1", "a"))],
        );
        stop(&mut t, "done", &[]);
        let out = t.on_tick(Instant::now() + STOP_LOG_WAIT);
        assert!(events(&out).is_empty(), "{out:?}");
        assert_eq!(t.open_prompts(), 1, "the first prompt closed by count");
        assert_eq!(t.phase(), Phase::Working);
        assert!(t.on_tick(Instant::now() + STOP_LOG_WAIT * 100).is_empty());
        let out = marks(&mut t, vec![LogMark::FinalReply]);
        // The end, read after the wait, places r1 in the turn, but a separate
        // turn queued before the `Stop` would look the same: never a reply
        // that may be another turn's.
        assert!(
            matches!(ended(&out)["r1"], RequestEnd::Unmatched(_)),
            "{out:?}"
        );
        assert_eq!(events(&out), ["finished"]);
    }

    /// Prompts with no record read: once the wait is over the `Stop` closes
    /// the oldest by count, with its reply.
    #[test]
    fn unplaced_prompts_take_the_fallback_after_the_wait() {
        let mut t = claude();
        marks(&mut t, vec![log_prompt("earlier"), LogMark::FinalReply]);
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, &header("r2", "a"), None);
        stop(&mut t, "done", &[]);
        assert!(t.on_tick(Instant::now()).is_empty(), "not yet due");
        let out = t.on_tick(Instant::now() + STOP_LOG_WAIT);
        assert_eq!(ended(&out)["r1"], ok("done"), "{out:?}");
        assert_eq!(t.open_prompts(), 1);
    }

    /// A prompt accepted while a `Stop` waits is a new turn: the late record
    /// closes the waiting turn's prompts only, and the terminal stays
    /// `working` for the new one.
    #[test]
    fn a_prompt_that_arrives_while_a_stop_waits_keeps_its_own_turn() {
        let mut t = claude();
        prompt(&mut t, NOTE, None);
        prompt(&mut t, &header("r1", "a"), None);
        marks(
            &mut t,
            vec![log_prompt(NOTE), log_prompt(&header("r1", "a"))],
        );
        stop(&mut t, "the answer", &[]);
        prompt(&mut t, "a new question", None);
        let out = marks(
            &mut t,
            vec![LogMark::FinalReply, log_prompt("a new question")],
        );
        assert_eq!(ended(&out)["r1"], ok("the answer"));
        assert_eq!(t.open_prompts(), 1);
        assert_eq!(t.phase(), Phase::Working, "the new turn runs");
        marks(&mut t, vec![LogMark::FinalReply]);
        let out = stop(&mut t, "later", &[]);
        assert_eq!(events(&out), ["finished"]);
    }

    /// A second `Stop` while one waits decides the first on the log as read.
    #[test]
    fn a_second_stop_decides_the_waiting_one_first() {
        let mut t = claude();
        prompt(&mut t, NOTE, None);
        prompt(&mut t, WAKE, None);
        marks(&mut t, vec![log_prompt(NOTE), log_prompt(WAKE)]);
        stop(&mut t, "one", &[]);
        prompt(&mut t, "two", None);
        marks(
            &mut t,
            vec![LogMark::FinalReply, log_prompt("two"), LogMark::FinalReply],
        );
        assert_eq!(
            t.open_prompts(),
            1,
            "the first record settled the first Stop"
        );
        let out = stop(&mut t, "two", &[]);
        assert_eq!(events(&out), ["finished"]);
        assert_eq!(t.phase(), Phase::Idle);
    }

    /// One step of a scripted session, for the late-record comparison.
    #[derive(Clone, Copy)]
    enum Step {
        /// A prompt hook.
        P(&'static str),
        /// The prompt's record read from the log.
        R(&'static str),
        /// A turn's end: its `end_turn` record and its `Stop`, with the
        /// background work it lists.
        S(&'static str, &'static [&'static str]),
    }

    /// How the end records reach the tracker.
    #[derive(Clone, Copy, Debug)]
    enum Timing {
        /// Read before the `Stop` is handled.
        OnTime,
        /// Read after the `Stop`: with the next records read, in file order.
        Late,
    }

    #[derive(PartialEq)]
    struct Result {
        events: Vec<&'static str>,
        ends: BTreeMap<String, RequestEnd>,
        phase: Phase,
        open: usize,
    }

    impl std::fmt::Debug for Result {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let ends: Vec<String> = self
                .ends
                .iter()
                .map(|(id, end)| match end {
                    RequestEnd::Ok(text) => format!("{id}: ok({})", text.expose()),
                    other => format!("{id}: {other:?}"),
                })
                .collect();
            write!(
                f,
                "events {:?} ends {ends:?} phase {:?} open {}",
                self.events, self.phase, self.open
            )
        }
    }

    fn play(script: &[Step], timing: Timing) -> Result {
        let mut t = claude();
        let mut out: Vec<Out> = Vec::new();
        let mut late: Vec<LogMark> = Vec::new();
        for step in script {
            match *step {
                Step::P(text) => out.extend(prompt(&mut t, text, None)),
                Step::R(text) => {
                    let mut batch = std::mem::take(&mut late);
                    batch.push(log_prompt(text));
                    out.extend(t.on_log(batch, Instant::now()));
                }
                Step::S(reply, background) => {
                    // The relay catches the log up before a `Stop`.
                    if !late.is_empty() {
                        out.extend(t.on_log(std::mem::take(&mut late), Instant::now()));
                    }
                    match timing {
                        Timing::OnTime => {
                            out.extend(t.on_log(vec![LogMark::FinalReply], Instant::now()));
                            out.extend(stop(&mut t, reply, background));
                        }
                        Timing::Late => {
                            out.extend(stop(&mut t, reply, background));
                            late.push(LogMark::FinalReply);
                        }
                    }
                }
            }
        }
        out.extend(t.on_log(late, Instant::now()));
        Result {
            events: events(&out),
            ends: ended(&out),
            phase: t.phase(),
            open: t.open_prompts(),
        }
    }

    /// The reference: what the tracker decides with every end read in time.
    /// A late record, within the wait, must not change any outcome.
    fn same_late_or_on_time(script: &[Step]) -> Result {
        let reference = play(script, Timing::OnTime);
        assert_eq!(play(script, Timing::Late), reference, "late");
        reference
    }

    /// Round 3: A holds "continue" and the queued request r1; while A's
    /// `Stop` waits, B is "continue" again and its record is read first.
    const REPEATED: &[Step] = &[
        Step::P("continue"),
        Step::P(R1),
        Step::R("continue"),
        Step::R(R1),
        Step::S("first answer", &[]),
        Step::P("continue"),
        Step::R("continue"),
        Step::S("second", &[]),
    ];

    /// The same text three times, before, in and after the waiting turn.
    const THREE: &[Step] = &[
        Step::P("go"),
        Step::R("go"),
        Step::S("a", &[]),
        Step::P("go"),
        Step::P(R1),
        Step::R("go"),
        Step::R(R1),
        Step::S("b", &[]),
        Step::P("go"),
        Step::R("go"),
        Step::S("c", &[]),
    ];

    /// Empty and whitespace-only prompts, which no record can be matched to.
    const BLANK: &[Step] = &[
        Step::P(""),
        Step::P(R1),
        Step::R(""),
        Step::R(R1),
        Step::S("x", &[]),
        Step::P("   "),
        Step::R("   "),
        Step::S("y", &[]),
        Step::P("after"),
        Step::R("after"),
        Step::S("z", &[]),
    ];

    const R1: &str = "[connecto from a, request r1. Sent by another agent, not the user.]\nbody";
    const R2: &str = "[connecto from a, request r2. Sent by another agent, not the user.]\nbody";
    const BG_B1: &str = "<task-notification><task-id>b1</task-id>done";
    const BG_ANON: &str = "<task-notification>done";

    #[test]
    fn late_records_change_no_outcome_in_any_order_tested() {
        use Step::{P, R, S};
        let scripts: &[(&str, &[Step])] = &[
            // The incident: a self-started turn absorbs a wake.
            (
                "absorbed wake",
                &[
                    P("earlier"),
                    R("earlier"),
                    S("ok", &[]),
                    P(NOTE),
                    P(WAKE),
                    R(NOTE),
                    R(WAKE),
                    S("done", &[]),
                    P("next"),
                    R("next"),
                    S("ok", &[]),
                ],
            ),
            // A prompt accepted while the Stop waits is its own turn.
            (
                "prompt during wait",
                &[
                    P(NOTE),
                    P(R1),
                    R(NOTE),
                    R(R1),
                    S("the answer", &[]),
                    P("new"),
                    R("new"),
                    S("later", &[]),
                ],
            ),
            // Round 2, finding 1: B (two prompts) runs after A's Stop.
            (
                "next turn of two",
                &[
                    P(NOTE),
                    P(WAKE),
                    R(NOTE),
                    R(WAKE),
                    S("a", &[]),
                    P(R2),
                    P("queued"),
                    R(R2),
                    R("queued"),
                    S("b", &[]),
                ],
            ),
            // Round 1, finding 3: a named notification during the wait.
            (
                "named notification",
                &[
                    P(R1),
                    P(NOTE),
                    R(R1),
                    R(NOTE),
                    S("started task", &["b1"]),
                    P(BG_B1),
                    R(BG_B1),
                    S("actual answer", &[]),
                ],
            ),
            // Round 2, finding 2: an anonymous notification with b0 owned
            // and b1 started by the waiting Stop.
            (
                "anonymous notification",
                &[
                    P(R1),
                    R(R1),
                    S("one", &["b0"]),
                    P(R2),
                    P("x"),
                    R(R2),
                    R("x"),
                    S("started b1", &["b0", "b1"]),
                    P(BG_ANON),
                    R(BG_ANON),
                    S("notification answer", &[]),
                ],
            ),
            // A task the waiting Stop no longer lists has ended.
            (
                "ended task",
                &[
                    P(R1),
                    R(R1),
                    S("one", &["b0"]),
                    P("y"),
                    P("z"),
                    R("y"),
                    R("z"),
                    S("b0 gone", &[]),
                    P("<task-notification><task-id>b0</task-id>done"),
                    R("<task-notification><task-id>b0</task-id>done"),
                    S("late note", &[]),
                ],
            ),
            // Background still running at the waiting Stop.
            (
                "background holds",
                &[
                    P(NOTE),
                    P(R1),
                    R(NOTE),
                    R(R1),
                    S("started", &["b1"]),
                    P(BG_B1),
                    R(BG_B1),
                    S("final", &[]),
                ],
            ),
            // Round 3: the same text before and after the boundary.
            ("repeated text", REPEATED),
            ("text three times", THREE),
            (
                "later is a prefix",
                &[
                    P("continue please"),
                    P(R1),
                    R("continue please"),
                    R(R1),
                    S("x", &[]),
                    P("continue"),
                    R("continue"),
                    S("y", &[]),
                ],
            ),
            (
                "earlier is a prefix",
                &[
                    P("go"),
                    P(R1),
                    R("go"),
                    R(R1),
                    S("x", &[]),
                    P("go on"),
                    R("go on"),
                    S("y", &[]),
                ],
            ),
            (
                "later is a suffix",
                &[
                    P("continue please"),
                    P(R1),
                    R("continue please"),
                    R(R1),
                    S("x", &[]),
                    P("please"),
                    R("please"),
                    S("y", &[]),
                ],
            ),
            ("empty and blank", BLANK),
        ];
        for (name, script) in scripts {
            let result = std::panic::catch_unwind(|| same_late_or_on_time(script));
            assert!(result.is_ok(), "{name}");
        }
        // And the outcomes the findings name.
        let anon = same_late_or_on_time(&[
            P(R1),
            R(R1),
            S("one", &["b0"]),
            P(R2),
            P("x"),
            R(R2),
            R("x"),
            S("started b1", &["b0", "b1"]),
            P(BG_ANON),
            R(BG_ANON),
            S("notification answer", &[]),
        ]);
        assert!(
            matches!(anon.ends["r1"], RequestEnd::Unmatched(_)),
            "{anon:?}"
        );
        assert!(
            matches!(anon.ends["r2"], RequestEnd::Unmatched(_)),
            "{anon:?}"
        );
        let named = same_late_or_on_time(&[
            P(R1),
            P(NOTE),
            R(R1),
            R(NOTE),
            S("started task", &["b1"]),
            P(BG_B1),
            R(BG_B1),
            S("actual answer", &[]),
        ]);
        assert_eq!(named.ends["r1"], ok("actual answer"));
        assert_eq!(
            same_late_or_on_time(REPEATED).ends["r1"],
            ok("first answer")
        );
        assert_eq!(same_late_or_on_time(THREE).ends["r1"], ok("b"));
        let blank = same_late_or_on_time(BLANK);
        assert!(blank.ends.contains_key("r1"), "{blank:?}");
        let next = same_late_or_on_time(&[
            P(NOTE),
            P(WAKE),
            R(NOTE),
            R(WAKE),
            S("a", &[]),
            P(R2),
            P("queued"),
            R(R2),
            R("queued"),
            S("b", &[]),
        ]);
        assert_eq!(next.ends["r2"], ok("b"));
        assert_eq!(next.phase, Phase::Idle);
    }

    // ---- generated interleavings
    //
    // A session of 2-3 turns is fixed: which prompts each turn holds (texts
    // repeat, requests are queued into turns), and so the order of the hook
    // stream (prompt hooks, `Stop`s) and of the log (prompt records, end
    // records). Only when each log record is read varies: never before it is
    // written, always in file order, and each turn's records no later than
    // the next turn's `Stop` (the relay's catch-up). Every such merge must
    // give the on-time outcome, never read idle while a hooked prompt's turn
    // has not reported its `Stop`, and end idle with nothing open.

    /// One prompt: its text, and whether its hook came before the previous
    /// turn's `Stop` (typed during that turn, run as its own turn after it).
    type Spec = (&'static str, bool);

    #[derive(Clone, Copy, Debug)]
    enum Hs {
        Hook(&'static str, usize),
        Stop(usize),
        /// The `Stop` after another hook kept turn k going.
        Stop2(usize),
    }

    #[derive(Clone, Copy, Debug)]
    enum Ls {
        Rec(&'static str),
        End,
        /// Turn k's work after its first end: it was kept going.
        Act(usize),
        /// Turn k's second end record.
        End2(usize),
    }

    #[derive(Clone, Copy, Debug)]
    enum Item {
        H(Hs),
        L(Ls),
    }

    struct Session {
        /// Turns whose end record is missing.
        absent: Vec<bool>,
        hs: Vec<Hs>,
        /// With the hook-stream index it may be read from, and before which
        /// it must be read.
        ls: Vec<(Ls, usize, usize)>,
        turns: usize,
    }

    /// `absent`: the turns whose end record is missing or unreadable.
    fn session(turns: &[&[Spec]], absent: &[bool]) -> Session {
        session_with(turns, absent, &[])
    }

    /// `continued`: the turns another `Stop` hook keeps going once.
    fn session_with(turns: &[&[Spec]], absent: &[bool], continued: &[bool]) -> Session {
        let kept = |k: usize| continued.get(k).copied().unwrap_or(false);
        let mut stop2_at = vec![0; turns.len()];
        let mut hs = Vec::new();
        let mut hook_at: Vec<Vec<usize>> = vec![Vec::new(); turns.len()];
        let mut stop_at = vec![0; turns.len()];
        for (k, turn) in turns.iter().enumerate() {
            for (j, (text, early)) in turn.iter().enumerate() {
                if !early {
                    hook_at[k].push(hs.len());
                    hs.push(Hs::Hook(text, k));
                } else {
                    let _ = j;
                }
            }
            if let Some(next) = turns.get(k + 1) {
                for (text, early) in next.iter() {
                    if *early {
                        hook_at[k + 1].push(hs.len());
                        hs.push(Hs::Hook(text, k + 1));
                    }
                }
            }
            stop_at[k] = hs.len();
            hs.push(Hs::Stop(k));
            if kept(k) {
                stop2_at[k] = hs.len();
                hs.push(Hs::Stop2(k));
            }
        }
        let mut ls = Vec::new();
        for (k, turn) in turns.iter().enumerate() {
            let deadline = stop_at.get(k + 1).copied().unwrap_or(hs.len());
            // Early prompts' hooks are listed first in their turn.
            let mut early_hooks = hook_at[k].iter();
            let mut order: Vec<&Spec> = turn.iter().filter(|(_, e)| *e).collect();
            order.extend(turn.iter().filter(|(_, e)| !*e));
            for (text, early) in order {
                let hook = *early_hooks.next().unwrap_or(&0);
                let from = if *early { stop_at[k - 1] } else { hook };
                ls.push((Ls::Rec(text), from, deadline));
            }
            if !absent.get(k).copied().unwrap_or(false) {
                ls.push((Ls::End, stop_at[k], deadline));
            }
            if kept(k) {
                // Written after the first `Stop`, and read by the second's
                // catch-up at the latest; the second end may come later.
                ls.push((Ls::Act(k), stop_at[k], stop2_at[k]));
                ls.push((Ls::End2(k), stop2_at[k], deadline));
            }
        }
        let _ = &stop2_at;
        Session {
            absent: absent.to_vec(),
            hs,
            ls,
            turns: turns.len(),
        }
    }

    /// Every merge of the two streams the constraints allow.
    fn merges(session: &Session) -> Vec<Vec<Item>> {
        fn walk(s: &Session, i: usize, j: usize, path: &mut Vec<Item>, all: &mut Vec<Vec<Item>>) {
            if i == s.hs.len() && j == s.ls.len() {
                all.push(path.clone());
                return;
            }
            if j < s.ls.len() && s.ls[j].1 <= i {
                path.push(Item::L(s.ls[j].0));
                walk(s, i, j + 1, path, all);
                path.pop();
            }
            // No record still unread may be due before this event.
            if i < s.hs.len() && s.ls[j..].iter().all(|(_, _, due)| *due > i) {
                path.push(Item::H(s.hs[i]));
                walk(s, i + 1, j, path, all);
                path.pop();
            }
        }
        let mut all = Vec::new();
        walk(session, 0, 0, &mut Vec::new(), &mut all);
        all
    }

    const REPLIES: [&str; 3] = ["reply 0", "reply 1", "reply 2"];

    /// When the 2 s wait of a waiting `Stop` runs out.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Timeouts {
        Never,
        AfterEvery,
        After(usize),
    }

    fn run_merge(session: &Session, merge: &[Item], timeouts: Timeouts) -> Result {
        let mut t = claude();
        t.log_attached();
        let mut out = Vec::new();
        let mut hooked = vec![false; session.turns];
        let mut stopped = vec![false; session.turns];
        // A turn is over once its end record is in the log, or its `Stop` came.
        let mut ended_in_log = vec![false; session.turns];
        let end_turns: Vec<usize> = (0..session.turns)
            .filter(|k| !session.absent.get(*k).copied().unwrap_or(false))
            .collect();
        let mut ends_read = 0;
        // Turn k shown at work again after its first `Stop`, until its next.
        let mut kept_going = vec![false; session.turns];
        let later = || Instant::now() + STOP_LOG_WAIT + Duration::from_secs(1);
        // Hook-stream events and log records taken so far, and the hook-stream
        // index of the last `Stop`.
        let (mut hs_done, mut ls_done, mut last_stop) = (0, 0, None);
        for (step, item) in merge.iter().enumerate() {
            match item {
                Item::H(Hs::Stop(_) | Hs::Stop2(_)) => {
                    last_stop = Some(hs_done);
                    hs_done += 1;
                }
                Item::H(_) => hs_done += 1,
                Item::L(_) => ls_done += 1,
            }
            match *item {
                Item::H(Hs::Hook(text, k)) => {
                    hooked[k] = true;
                    out.extend(prompt(&mut t, text, None));
                }
                Item::H(Hs::Stop(k)) => {
                    stopped[k] = true;
                    out.extend(stop(&mut t, REPLIES[k], &[]));
                }
                Item::L(Ls::Rec(text)) => out.extend(marks(&mut t, vec![log_prompt(text)])),
                Item::L(Ls::End) => {
                    ended_in_log[end_turns[ends_read]] = true;
                    ends_read += 1;
                    out.extend(marks(&mut t, vec![LogMark::FinalReply]));
                }
                Item::H(Hs::Stop2(k)) => {
                    kept_going[k] = false;
                    out.extend(end_of(&mut t, "continued", None, true));
                }
                Item::L(Ls::Act(k)) => {
                    kept_going[k] = true;
                    out.extend(marks(&mut t, vec![LogMark::Activity]));
                }
                Item::L(Ls::End2(k)) => {
                    kept_going[k] = false;
                    out.extend(marks(&mut t, vec![LogMark::FinalReply]));
                }
            }
            // A wait runs out only after the prompt records written before
            // the last `Stop` were read: they are written at the start of their
            // turn, long before it ends. An end record, written last, may be
            // read after the wait.
            let records_in = last_stop.is_none_or(|stop| {
                session.ls[ls_done..]
                    .iter()
                    .all(|(item, from, _)| matches!(item, Ls::End | Ls::End2(_)) || *from > stop)
            });
            let due = timeouts == Timeouts::AfterEvery || timeouts == Timeouts::After(step);
            if due && records_in {
                out.extend(t.on_tick(later()));
            }
            let running = (0..session.turns)
                .any(|k| (hooked[k] && !stopped[k] && !ended_in_log[k]) || kept_going[k]);
            assert!(
                !running || t.phase() == Phase::Working,
                "idle at step {step} with a turn running ({timeouts:?}): {merge:?}"
            );
        }
        // Every record and `Stop` has arrived; the waits run out.
        out.extend(t.on_tick(later()));
        Result {
            events: events(&out),
            ends: ended(&out),
            phase: t.phase(),
            open: t.open_prompts(),
        }
    }

    const C: &str = "continue";

    const SESSIONS: &[&[&[Spec]]] = &[
        &[&[(C, false)], &[(C, false)]],
        &[&[(C, false), (R1, false)], &[(C, false)]],
        &[
            &[(R1, false), (C, false)],
            &[(C, false)],
            &[(C, false), (R2, false)],
        ],
        &[&[(C, false), (C, false)], &[(R1, false)]],
        // A prompt typed during turn 0 and run as its own turn after it.
        &[&[(C, false)], &[(C, true)]],
        &[&[(R1, false)], &[(R2, true), (C, false)]],
        &[
            &[(C, false), (R1, false)],
            &[(C, true), (R2, false)],
            &[(C, false)],
        ],
    ];

    /// Every request ended, and an `ok` is always its own turn's reply: where
    /// the log cannot tell turns apart, `unmatched`, never another's answer.
    fn assert_own_or_unmatched(turns: &[&[Spec]], result: &Result, context: &str) {
        for (k, turn) in turns.iter().enumerate() {
            for (text, _) in turn.iter() {
                for id in request_ids(text) {
                    match result.ends.get(&id) {
                        Some(RequestEnd::Ok(reply)) => {
                            assert_eq!(
                                reply.expose(),
                                REPLIES[k],
                                "{id} took another turn's reply: {context}"
                            );
                        }
                        Some(RequestEnd::Unmatched(_)) => {}
                        other => panic!("{id} ended {other:?}: {context}"),
                    }
                }
            }
        }
    }

    /// Whether the outcome cannot be decided from the hooks and the log: a
    /// turn holds a queued message and no end record is read from that turn
    /// on. Nothing tells the message from a queued separate turn still to
    /// run, so `Stop`s cannot be paired past it: what is left stays open,
    /// `working`, until a later end record or prompt (see the pairing notes).
    fn undecidable(turns: &[&[Spec]], absent: &[bool]) -> bool {
        // A turn holding a queued message, with no end record from it on.
        (0..turns.len()).any(|k| turns[k].len() > 1 && absent[k..].iter().all(|a| *a))
    }

    #[test]
    fn every_read_timing_of_a_fixed_log_gives_the_on_time_outcome() {
        let mut total = 0;
        for turns in SESSIONS {
            let session = session(turns, &vec![false; turns.len()]);
            let all = merges(&session);
            // The reference reads every record as soon as it is written.
            let reference = run_merge(&session, &all[0], Timeouts::Never);
            assert_eq!(reference.phase, Phase::Idle, "{turns:?}");
            assert_eq!(reference.open, 0, "{turns:?}");
            for (k, turn) in turns.iter().enumerate() {
                for (text, _) in turn.iter() {
                    for id in request_ids(text) {
                        assert_eq!(reference.ends[&id], ok(REPLIES[k]), "{turns:?}");
                    }
                }
            }
            for merge in &all {
                assert_eq!(
                    run_merge(&session, merge, Timeouts::Never),
                    reference,
                    "{merge:?}"
                );
                // A wait that runs out gives no request another turn's reply:
                // each ends with its own, or `unmatched` when its turn's end
                // came too late to be sure.
                let timed = run_merge(&session, merge, Timeouts::AfterEvery);
                assert_own_or_unmatched(turns, &timed, &format!("{merge:?}"));
                assert_eq!((timed.phase, timed.open), (Phase::Idle, 0), "{merge:?}");
            }
            total += all.len();
        }
        assert!(total > 100, "{total} interleavings");
    }

    /// End records missing or unreadable, for any set of turns, with the wait
    /// running out never, after every event, or at any one point: the
    /// terminal is never idle while a turn runs, and once every `Stop` has
    /// arrived and the waits have run out it is idle with nothing open (but
    /// for the one undecidable shape), every request ended with its own
    /// turn's reply.
    #[test]
    fn missing_end_records_and_timeouts_still_settle_every_turn() {
        let mut runs = 0;
        for turns in SESSIONS {
            for mask in 0..(1u32 << turns.len()) {
                let absent: Vec<bool> = (0..turns.len()).map(|k| mask & (1 << k) != 0).collect();
                let session = session(turns, &absent);
                let all = merges(&session);
                let steps = session.hs.len() + session.ls.len();
                for (n, merge) in all.iter().enumerate() {
                    let mut modes = vec![Timeouts::Never, Timeouts::AfterEvery];
                    if n < 40 {
                        modes.extend((0..steps).map(Timeouts::After));
                    }
                    for mode in modes {
                        runs += 1;
                        let result = run_merge(&session, merge, mode);
                        if undecidable(turns, &absent) {
                            continue;
                        }
                        let context = format!("{absent:?} {mode:?} {merge:?}");
                        assert_eq!((result.phase, result.open), (Phase::Idle, 0), "{context}");
                        assert_own_or_unmatched(turns, &result, &context);
                    }
                }
            }
        }
        assert!(runs > 10_000, "{runs} runs");
    }

    /// A Claude turn end of the given kind.
    fn end_of(
        tracker: &mut Tracker,
        reply: &str,
        failure: Option<&str>,
        continued: bool,
    ) -> Vec<Out> {
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
                background: Vec::new(),
                thread: None,
                turn: None,
                failure: failure.map(text),
                continued,
            },
            Instant::now(),
        )
    }

    /// Review round 6, finding 1, the real lifecycle: a turn that fails on
    /// an API error logs an error record (`stop_sequence`,
    /// `isApiErrorMessage`) and fires `StopFailure`, not `Stop`. It ends the
    /// turn; its requests end `unavailable`, never with the error text as a
    /// reply; and the terminal settles.
    #[test]
    fn a_turn_ending_in_an_api_failure_settles() {
        let mut t = claude();
        let read = |line: &str| Claude.read_log(line, true);
        prompt(&mut t, "first", None);
        t.on_log(
            vec![
                read(r#"{"type":"user","message":{"role":"user","content":"first"}}"#),
                read(r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[]}}"#),
            ],
            Instant::now(),
        );
        stop(&mut t, "ok", &[]);
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, C, None);
        let failure = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"stop_reason":"stop_sequence","content":[{"type":"text","text":"API Error: 529 overloaded"}]}}"#;
        t.on_log(
            vec![log_prompt(&header("r1", "a")), log_prompt(C), read(failure)],
            Instant::now(),
        );
        let out = end_of(&mut t, "", Some("overloaded"), false);
        assert!(
            matches!(ended(&out)["r1"], RequestEnd::Unavailable(_)),
            "{out:?}"
        );
        assert!(
            !out.iter().any(|o| matches!(o, Out::Turn { .. })),
            "no reply is captured"
        );
        assert_eq!((t.phase(), t.open_prompts()), (Phase::Idle, 0));
        // Its end record read after the failure: the same.
        let mut t = claude();
        t.log_attached();
        prompt(&mut t, &header("r2", "a"), None);
        let mut out = end_of(&mut t, "", Some("rate_limit"), false);
        out.extend(t.on_log(
            vec![log_prompt(&header("r2", "a")), read(failure)],
            Instant::now(),
        ));
        out.extend(t.on_tick(Instant::now() + STOP_LOG_WAIT));
        assert!(
            matches!(ended(&out)["r2"], RequestEnd::Unavailable(_)),
            "{out:?}"
        );
        assert_eq!((t.phase(), t.open_prompts()), (Phase::Idle, 0));
    }

    /// Review round 6, finding 2: another `Stop` hook blocks the first `Stop`,
    /// and the turn goes on with no new prompt, then fires `Stop` again with
    /// `stop_hook_active`. Once the log shows the turn at work after its end,
    /// the terminal reads `working` until that second `Stop`, which closes no
    /// other turn's prompt by count; a message queued into the continued turn
    /// closes with it.
    #[test]
    fn a_turn_kept_going_by_a_stop_hook_reads_working_until_its_next_stop() {
        let mut t = claude();
        let read = |line: &str| Claude.read_log(line, true);
        let work = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[]}}"#;
        let end = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[]}}"#;
        prompt(&mut t, &header("r1", "a"), None);
        t.on_log(
            vec![log_prompt(&header("r1", "a")), read(end)],
            Instant::now(),
        );
        let out = stop(&mut t, "first answer", &[]);
        assert_eq!(ended(&out)["r1"], ok("first answer"));
        assert_eq!(t.phase(), Phase::Idle);
        // The blocked `Stop`: the turn goes on.
        let out = t.on_log(vec![read(work)], Instant::now());
        assert_eq!(events(&out), ["began"]);
        assert_eq!(t.phase(), Phase::Working, "no wake is typed into it");
        // A message queued into the continued turn, and a prompt for later.
        prompt(&mut t, &header("r2", "b"), None);
        t.on_log(
            vec![log_prompt(&header("r2", "b")), read(end)],
            Instant::now(),
        );
        let out = end_of(&mut t, "second answer", None, true);
        assert_eq!(ended(&out)["r2"], ok("second answer"));
        assert_eq!((t.phase(), t.open_prompts()), (Phase::Idle, 0));
        // The turn's own records read late, after its `Stop`, are not taken
        // for a continuation: they come before its end record.
        let mut t = claude();
        t.log_attached();
        prompt(&mut t, "x", None);
        t.on_log(vec![log_prompt("x")], Instant::now());
        stop(&mut t, "x", &[]);
        t.on_tick(Instant::now() + STOP_LOG_WAIT);
        assert_eq!(t.phase(), Phase::Idle);
        t.on_log(vec![read(work), read(end)], Instant::now());
        assert_eq!(t.phase(), Phase::Idle, "late records of the ended turn");
    }

    /// Review round 6, finding 3: A's end record is missing; B was queued as
    /// a separate turn before A's `Stop`; A's wait runs out and its `Stop`
    /// closes A by count; C is queued into B; B's `Stop` and end arrive. B's
    /// request never gets A's reply: which turn B was in is not known, so it
    /// is `unmatched`, and C, closed by B's end, gets B's reply.
    #[test]
    fn a_later_record_never_gives_an_older_queued_prompt_another_turns_reply() {
        let mut t = claude();
        t.log_attached();
        let mut out = Vec::new();
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, &header("r2", "b"), None);
        out.extend(marks(&mut t, vec![log_prompt(&header("r1", "a"))]));
        out.extend(stop(&mut t, "A answer", &[]));
        out.extend(t.on_tick(Instant::now() + STOP_LOG_WAIT));
        out.extend(marks(&mut t, vec![log_prompt(&header("r2", "b"))]));
        prompt(&mut t, &header("r3", "c"), None);
        out.extend(marks(
            &mut t,
            vec![log_prompt(&header("r3", "c")), LogMark::FinalReply],
        ));
        out.extend(stop(&mut t, "B answer", &[]));
        let ends = ended(&out);
        assert_eq!(ends["r1"], ok("A answer"));
        assert!(matches!(ends["r2"], RequestEnd::Unmatched(_)), "{ends:?}");
        assert_eq!(ends["r3"], ok("B answer"));
        assert_eq!((t.phase(), t.open_prompts()), (Phase::Idle, 0));
    }

    /// Review round 5, finding 2: the attempt's first turn has two prompts and
    /// its end record comes after the wait ran out (the first closed by count,
    /// the second left); B's record and `Stop` arrive before B's end. B's
    /// `Stop` is B's, by count: the leftover goes with the first turn, and B
    /// closes when its end arrives. Nothing stays open.
    #[test]
    fn a_turn_whose_end_comes_after_the_wait_leaves_the_next_stop_its_own() {
        let mut t = claude();
        let mut out = Vec::new();
        prompt(&mut t, NOTE, None);
        prompt(&mut t, &header("r1", "a"), None);
        marks(
            &mut t,
            vec![log_prompt(NOTE), log_prompt(&header("r1", "a"))],
        );
        out.extend(stop(&mut t, "a", &[]));
        out.extend(t.on_tick(Instant::now() + STOP_LOG_WAIT));
        assert_eq!(t.open_prompts(), 1);
        out.extend(marks(&mut t, vec![LogMark::FinalReply]));
        prompt(&mut t, &header("r2", "b"), None);
        out.extend(marks(&mut t, vec![log_prompt(&header("r2", "b"))]));
        out.extend(stop(&mut t, "b", &[]));
        assert_eq!(t.phase(), Phase::Working, "B's end is not read yet");
        out.extend(marks(&mut t, vec![LogMark::FinalReply]));
        let ends = ended(&out);
        assert!(matches!(ends["r1"], RequestEnd::Unmatched(_)), "{ends:?}");
        assert_eq!(ends["r2"], ok("b"));
        assert_eq!((t.phase(), t.open_prompts()), (Phase::Idle, 0));
        // The same with no end record for either turn: which turn B's `Stop`
        // belongs to cannot be told (r1 may have been a turn of its own), so
        // B stays open, `working`, never closed with a guess, until an end
        // record places it; then `unmatched`, as nothing shows whose it is.
        let mut t = claude();
        let mut out = Vec::new();
        prompt(&mut t, NOTE, None);
        prompt(&mut t, &header("r1", "a"), None);
        marks(
            &mut t,
            vec![log_prompt(NOTE), log_prompt(&header("r1", "a"))],
        );
        out.extend(stop(&mut t, "a", &[]));
        out.extend(t.on_tick(Instant::now() + STOP_LOG_WAIT));
        prompt(&mut t, &header("r2", "b"), None);
        out.extend(marks(&mut t, vec![log_prompt(&header("r2", "b"))]));
        out.extend(stop(&mut t, "b", &[]));
        out.extend(t.on_tick(Instant::now() + STOP_LOG_WAIT));
        assert!(
            matches!(ended(&out)["r1"], RequestEnd::Unmatched(_)),
            "{out:?}"
        );
        assert_eq!((t.phase(), t.open_prompts()), (Phase::Working, 1));
        out.extend(marks(&mut t, vec![LogMark::FinalReply]));
        assert!(
            matches!(ended(&out)["r2"], RequestEnd::Unmatched(_)),
            "{out:?}"
        );
        assert_eq!((t.phase(), t.open_prompts()), (Phase::Idle, 0));
    }

    /// Review round 6, finding 2, generated: turns another `Stop` hook keeps
    /// going once, with every read timing and the wait running out never,
    /// after every event, or at any one point. The terminal is never idle
    /// while a turn runs, which now includes a kept-going turn once its work
    /// shows in the log; every `Stop` arrived, it is idle with nothing open;
    /// every request has its own turn's reply or `unmatched`.
    #[test]
    fn turns_kept_going_by_a_stop_hook_never_read_idle_while_they_run() {
        let cases: &[(&[&[Spec]], &[bool])] = &[
            (&[&[(C, false), (R1, false)]], &[true]),
            (
                &[&[(R1, false)], &[(C, false), (R2, false)]],
                &[true, false],
            ),
            (&[&[(C, false)], &[(R1, false)]], &[false, true]),
            (&[&[(R1, false)], &[(C, true)]], &[true, false]),
        ];
        let mut runs = 0;
        for (turns, continued) in cases {
            let session = session_with(turns, &vec![false; turns.len()], continued);
            let all = merges(&session);
            let steps = session.hs.len() + session.ls.len();
            for (n, merge) in all.iter().enumerate() {
                let mut modes = vec![Timeouts::Never, Timeouts::AfterEvery];
                if n < 40 {
                    modes.extend((0..steps).map(Timeouts::After));
                }
                for mode in modes {
                    runs += 1;
                    let result = run_merge(&session, merge, mode);
                    let context = format!("{continued:?} {mode:?} {merge:?}");
                    assert_eq!((result.phase, result.open), (Phase::Idle, 0), "{context}");
                    assert_own_or_unmatched(turns, &result, &context);
                }
            }
        }
        assert!(runs > 1_000, "{runs} runs");
    }

    /// Review round 4, finding 1: A's records (prompt and end) are read; B,
    /// with the same text, is accepted; A's `Stop` comes before B's record.
    /// A closes; B stays open and the terminal `working` until B's own end.
    #[test]
    fn a_late_stop_never_gives_the_next_prompts_record_away() {
        let mut t = claude();
        prompt(&mut t, C, None);
        marks(&mut t, vec![log_prompt(C), LogMark::FinalReply]);
        prompt(&mut t, &header("r2", "b"), None);
        prompt(&mut t, C, None);
        let out = stop(&mut t, "a", &[]);
        assert!(!events(&out).contains(&"finished"), "{out:?}");
        assert_eq!(t.open_prompts(), 2);
        assert_eq!(t.phase(), Phase::Working);
        marks(&mut t, vec![log_prompt(&header("r2", "b")), log_prompt(C)]);
        assert_eq!(t.phase(), Phase::Working);
        marks(&mut t, vec![LogMark::FinalReply]);
        let out = stop(&mut t, "b", &[]);
        assert_eq!(ended(&out)["r2"], ok("b"));
        assert_eq!(t.phase(), Phase::Idle);
    }

    /// Review round 4, finding 2: A holds r1 and "continue"; its `Stop` waits;
    /// B ("continue") is accepted and its record read before A's end record;
    /// B's `Stop` comes before B's end record. Nothing is left open for ever.
    #[test]
    fn a_trailing_prompt_never_takes_the_next_prompts_record() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, C, None);
        marks(&mut t, vec![log_prompt(&header("r1", "a")), log_prompt(C)]);
        // A's `Stop` closes A's first prompt by count.
        let mut out = stop(&mut t, "a", &[]);
        prompt(&mut t, C, None);
        out.extend(marks(&mut t, vec![log_prompt(C)]));
        out.extend(marks(&mut t, vec![LogMark::FinalReply]));
        assert_eq!(ended(&out)["r1"], ok("a"));
        assert_eq!(t.phase(), Phase::Working, "B runs");
        stop(&mut t, "b", &[]);
        marks(&mut t, vec![LogMark::FinalReply]);
        t.on_tick(Instant::now() + STOP_LOG_WAIT);
        assert_eq!(t.open_prompts(), 0);
        assert_eq!(t.phase(), Phase::Idle);
    }

    const HOUR: Duration = Duration::from_secs(3600);

    /// Review round 2, finding 1: A has two prompts open and its `Stop` waits;
    /// B is accepted and its record is read; then an end record. Read in this
    /// order (a file tail cannot: A's end is written before B starts), that
    /// end is after B's record, so it is B's, and A's own is taken as missing:
    /// A's `Stop` closes its first prompt by count. B's turn is never ended by
    /// A's `Stop` or by time, and B's request never gets A's reply.
    #[test]
    fn a_prompt_accepted_while_a_stop_waits_is_never_ended_by_that_stop_or_by_time() {
        let mut t = claude();
        let at = Instant::now();
        prompt(&mut t, NOTE, None);
        prompt(&mut t, WAKE, None);
        t.on_log(vec![log_prompt(NOTE), log_prompt(WAKE)], at);
        let mut out = stop(&mut t, "a done", &[]);
        prompt(&mut t, &header("r9", "b"), None);
        out.extend(t.on_log(vec![log_prompt(&header("r9", "b"))], at));
        out.extend(t.on_log(vec![LogMark::FinalReply], at));
        for later in [HOUR, HOUR * 10] {
            out.extend(t.on_tick(at + later));
        }
        assert!(!ended(&out).contains_key("r9"));
        assert_eq!(t.phase(), Phase::Working);
        out.extend(stop(&mut t, "b done", &[]));
        out.extend(t.on_tick(at + HOUR * 20));
        assert!(
            matches!(&ended(&out)["r9"], RequestEnd::Ok(text) if text.expose() == "b done")
                || matches!(ended(&out)["r9"], RequestEnd::Unmatched(_)),
            "{out:?}"
        );
    }

    /// Review finding 1: B is accepted before A's late `end_turn` is read,
    /// and B's record is one the adapter cannot place (`Other`). A's end then
    /// arrives, and B runs a tool that keeps the log silent past the limit.
    /// Silence and an unplaced prompt are never evidence of an end: B stays
    /// open and the terminal `working`, so no wake is typed into B's turn.
    #[test]
    fn an_unplaced_prompt_and_silence_never_end_a_turn() {
        for stop_comes in [true, false] {
            let mut t = claude();
            let at = Instant::now();
            prompt(&mut t, "A", None);
            t.on_log(vec![log_prompt("A")], at);
            prompt(&mut t, "B", None);
            t.on_log(vec![LogMark::Other], at);
            if stop_comes {
                stop(&mut t, "a done", &[]);
            }
            t.on_log(vec![LogMark::FinalReply], at);
            if !stop_comes {
                // B is placed nowhere, so the lost-Stop rule never fires.
                assert_eq!(t.open_prompts(), 2);
            }
            for later in [HOUR, HOUR * 10] {
                let out = t.on_tick(at + later);
                assert!(events(&out).is_empty(), "{stop_comes}: {out:?}");
                assert_eq!(t.phase(), Phase::Working, "{stop_comes}");
            }
        }
    }

    /// The quiet rule never ends a running turn: a prompt whose record comes
    /// after the last `end_turn` is a turn under way, however long its tool
    /// call keeps the log silent.
    #[test]
    fn a_long_silent_turn_is_never_taken_for_an_ended_one() {
        let mut t = claude();
        prompt(&mut t, "first", None);
        let at = Instant::now();
        t.on_log(vec![log_prompt("first"), LogMark::FinalReply], at);
        stop(&mut t, "ok", &[]);
        prompt(&mut t, "build everything", None);
        t.on_log(vec![log_prompt("build everything"), LogMark::Other], at);
        assert!(t.on_tick(at + HOUR * 10).is_empty());
        assert_eq!(t.phase(), Phase::Working);
        // Nor one whose prompt has no record yet.
        let mut t = claude();
        prompt(&mut t, "fresh", None);
        t.on_log(vec![LogMark::FinalReply], at);
        prompt(&mut t, "no record yet", None);
        assert!(t.on_tick(at + HOUR * 10).is_empty());
        assert_eq!(t.phase(), Phase::Working);
    }

    /// Review finding 3, in its exact order: request r1's turn starts task
    /// b1 and its `Stop` waits for the log; b1's notification starts a turn
    /// while it waits; the parent's `end_turn` is read; the notification
    /// turn answers. The answer is r1's, as it is without the wait.
    #[test]
    fn a_task_notification_during_a_waiting_stop_still_continues_its_owner() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, NOTE, None);
        marks(
            &mut t,
            vec![log_prompt(&header("r1", "a")), log_prompt(NOTE)],
        );
        stop(&mut t, "started task", &["b1"]);
        let bg = "<task-notification><task-id>b1</task-id>done";
        prompt(&mut t, bg, None);
        let out = marks(&mut t, vec![LogMark::FinalReply, log_prompt(bg)]);
        assert!(ended(&out).is_empty(), "r1 waits on b1: {out:?}");
        marks(&mut t, vec![LogMark::FinalReply]);
        let out = stop(&mut t, "actual answer", &[]);
        assert_eq!(ended(&out)["r1"], ok("actual answer"));
        assert_eq!(events(&out), ["finished"]);
        assert_eq!(t.phase(), Phase::Idle);
    }

    /// The same, when the notification's own `Stop` comes before a poll read
    /// the parent's end: the catch-up read at that `Stop` reads both ends.
    #[test]
    fn a_notification_stop_before_the_parent_end_is_read_still_continues_its_owner() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, NOTE, None);
        marks(
            &mut t,
            vec![log_prompt(&header("r1", "a")), log_prompt(NOTE)],
        );
        stop(&mut t, "started task", &["b1"]);
        let bg = "<task-notification><task-id>b1</task-id>done";
        prompt(&mut t, bg, None);
        marks(
            &mut t,
            vec![LogMark::FinalReply, log_prompt(bg), LogMark::FinalReply],
        );
        let out = stop(&mut t, "actual answer", &[]);
        assert_eq!(ended(&out)["r1"], ok("actual answer"));
        assert_eq!(t.phase(), Phase::Idle);
    }

    /// A notification for a task the waiting `Stop` did not list, with an
    /// owner already known, is taken as before.
    #[test]
    fn a_notification_for_a_known_task_is_placed_at_once_during_a_wait() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        marks(
            &mut t,
            vec![log_prompt(&header("r1", "a")), LogMark::FinalReply],
        );
        stop(&mut t, "started b0", &["b0"]);
        prompt(&mut t, "x", None);
        prompt(&mut t, "y", None);
        marks(&mut t, vec![log_prompt("x"), log_prompt("y")]);
        stop(&mut t, "x done", &["b0"]);
        let bg = "<task-notification><task-id>b0</task-id>done";
        prompt(&mut t, bg, None);
        marks(
            &mut t,
            vec![LogMark::FinalReply, log_prompt(bg), LogMark::FinalReply],
        );
        let out = stop(&mut t, "b0 answer", &[]);
        assert_eq!(ended(&out)["r1"], ok("b0 answer"));
    }

    /// A user prompt, and then a message queued into its turn, while a
    /// `Stop` waits: both are the new turn's, and its own `Stop` closes them.
    #[test]
    fn a_prompt_and_a_queued_message_during_a_wait_are_the_next_turn() {
        let mut t = claude();
        prompt(&mut t, NOTE, None);
        prompt(&mut t, WAKE, None);
        marks(&mut t, vec![log_prompt(NOTE), log_prompt(WAKE)]);
        stop(&mut t, "one", &[]);
        prompt(&mut t, "user asks", None);
        prompt(&mut t, "queued more", None);
        marks(
            &mut t,
            vec![
                LogMark::FinalReply,
                log_prompt("user asks"),
                log_prompt("queued more"),
            ],
        );
        assert_eq!(t.open_prompts(), 2);
        assert_eq!(t.phase(), Phase::Working);
        marks(&mut t, vec![LogMark::FinalReply]);
        let out = stop(&mut t, "two", &[]);
        assert_eq!(events(&out), ["finished"]);
        assert_eq!(t.open_prompts(), 0);
    }

    /// An interrupt marker while a `Stop` waits: the waiting `Stop` is decided
    /// first, then the interrupt applies to what is still open.
    #[test]
    fn an_interrupt_during_a_wait_decides_the_stop_first() {
        let mut t = claude();
        prompt(&mut t, NOTE, None);
        prompt(&mut t, WAKE, None);
        marks(&mut t, vec![log_prompt(NOTE), log_prompt(WAKE)]);
        stop(&mut t, "one", &[]);
        prompt(&mut t, &header("r2", "a"), None);
        let out = marks(
            &mut t,
            vec![
                LogMark::FinalReply,
                log_prompt(&header("r2", "a")),
                LogMark::Interrupt { turn: None },
            ],
        );
        // As if read in time: the first turn finished, r2's began.
        assert_eq!(events(&out), ["finished", "began", "interrupted"]);
        assert_eq!(ended(&out)["r2"], RequestEnd::Interrupted);
        assert_eq!(t.open_prompts(), 0);
        assert_eq!(t.phase(), Phase::Idle);
    }

    /// T38, by count: with no log, each `Stop` closes the oldest open prompt
    /// with its reply. The rest stay open (`working`); the next `Stop` closes
    /// the next, but whether it was in the first turn is not known, so it is
    /// `unmatched`.
    #[test]
    fn with_several_prompts_and_no_log_each_stop_closes_the_oldest() {
        let mut t = claude();
        prompt(&mut t, &header("r1", "a"), None);
        prompt(&mut t, &header("r2", "a"), None);
        let out = stop(&mut t, "reply", &[]);
        let ends = ended(&out);
        assert_eq!(ends["r1"], ok("reply"));
        assert!(!ends.contains_key("r2"));
        assert_eq!(t.phase(), Phase::Working, "the terminal reads working");
        let out = stop(&mut t, "reply 2", &[]);
        assert!(
            matches!(ended(&out)["r2"], RequestEnd::Unmatched(_)),
            "{out:?}"
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
                failure: None,
                continued: false,
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
                failure: None,
                continued: false,
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

    /// Messages Codex takes into a running turn fire their own prompt hook
    /// with that turn's id, and the turn ends with one notify: it closes them
    /// all, their requests end with its reply, and the terminal is idle again
    /// (it had stayed `working` for good, one prompt left open per message).
    #[test]
    fn messages_taken_into_a_running_codex_turn_end_with_it() {
        let mut t = codex();
        start_codex(&mut t, MAIN);
        let out = prompt(&mut t, &header("r1", "a"), Some("tu1"));
        assert_eq!(events(&out), ["began"]);
        for id in ["r2", "r3"] {
            let out = prompt(&mut t, &header(id, "b"), Some("tu1"));
            assert!(out.contains(&Out::Bound(id.to_owned())), "{out:?}");
            assert!(events(&out).is_empty(), "still the same turn: {out:?}");
        }
        assert_eq!(t.open_prompts(), 1);
        let out = notify(
            &mut t,
            MAIN,
            "tu1",
            &[],
            0,
            Reply::Text(text("all three")),
            Instant::now(),
        );
        let ends = ended(&out);
        for id in ["r1", "r2", "r3"] {
            assert_eq!(ends[id], ok("all three"), "{id}");
        }
        assert_eq!(events(&out), ["finished"]);
        assert_eq!(t.phase(), Phase::Idle);
        // The next turn is a turn of its own again.
        let out = prompt(&mut t, "next", Some("tu2"));
        assert_eq!(events(&out), ["began"]);
        let out = notify(&mut t, MAIN, "tu2", &[], 0, Reply::None, Instant::now());
        assert_eq!(events(&out), ["finished"]);
        assert_eq!(t.phase(), Phase::Idle);
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
                failure: None,
                continued: false,
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
        assert!(!t.wants_log(t0) || t.on_tick(t0 + Duration::from_secs(10)).is_empty());
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
