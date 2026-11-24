//! Who typed each prompt of a registry terminal (project memory's prompt
//! provenance, spec §4.5: a user statement counts as independent evidence and
//! as approval, so a wrong credit is a defect and a lost one is not).
//!
//! # One way into the PTY
//!
//! Every byte for a registry terminal's PTY goes through [`InputGate::write`]
//! with its [`InputSource`]. Under one lock it works out what the bytes do to
//! the [`Ledger`], hands them to the PTY queue, and commits the ledger change
//! only if the queue took them; a refused write changes nothing but the taint.
//! So the ledger sees exactly the bytes the PTY gets, in the PTY's order.
//! `LiveSession` owns its gate; the raw enqueue happens only inside the
//! closures it passes to [`InputGate::write`]:
//! - `LiveSession::write_input(bytes, source)`; its `TerminalSession::
//!   send_input` writes as [`InputSource::Connecto`] (relay, utility runs);
//! - the emulator's answers to terminal queries, as [`InputSource::Protocol`];
//! - `ManagedPane::send_input` (the window, and remote peers) writes as
//!   [`InputSource::Window`], or [`InputSource::Remote`] on a thread the remote
//!   guard marked ([`mark_remote_input`]; the mark never leaves the thread).
//!
//! # Two modes ([`TerminalCredit`], chosen where the app sets the capture sink)
//!
//! - **`Off`** (the default): no prompt typed in a terminal is ever recorded
//!   as the user's. It assumes nothing about the tools. The ledger below
//!   still runs (so ordering and accounting stay exercised); its `User`
//!   verdict is turned into not-the-user at the capture tap.
//! - **`Quarantine`**: the rule below. It is sound **only if the tool never
//!   emits a prompt hook on its own**, without a key from the PTY. Tool
//!   behaviours known or likely to do so: Claude's `/loop` and scheduled
//!   runs, a background task's completion notice delivered as a prompt,
//!   Stop hooks or plugins that start a new prompt, Codex continuing on its
//!   own, a slash command that expands into several prompts. Such a hook
//!   can carry any text, including a waiting window submission's, and no
//!   rule over bytes and hooks can tell it from the user's (the test
//!   `autonomous_hooks_defeat_quarantine` shows it).
//!
//! # The quarantine rule (what a reviewer checks against the code)
//!
//! 1. The ledger is CLEAN or TAINTED, and starts TAINTED (a new attempt).
//! 2. [`Ledger::taint`] runs, whatever the state, on: any committed byte not
//!    from the window; a window byte that makes the composer uncertain
//!    (escape sequences other than paste markers, control keys, Tab, a line
//!    feed, a backslash-Enter, a line break or control byte inside a paste,
//!    an overlong text, an Enter on an empty or blank composer); a refused
//!    write; more submissions than the ledger keeps; a hook of another
//!    attempt, a cut hook, a hook with nothing waiting, a hook before the
//!    previous hook's turn ended, a hook it cannot credit while CLEAN; the
//!    tool's (re)start; an unreadable turn end; lost hook events. Each one
//!    also ends any recovery under way (rule 5 starts over).
//! 3. Every *possible* submission waits for a hook, and their number is an
//!    upper bound on the prompts the bytes can cause **under any parse**
//!    (paste mode on or off, any newline key, any way of resynchronizing on
//!    a broken escape sequence). Counting never parses; it over-counts:
//!    - every CR and every LF byte, wherever it is and whoever wrote it;
//!    - inside an escape stretch (from an ESC or C1 control up to a byte in
//!      0x40..=0x7E that does not directly follow one; see [`Scan`]), every
//!      `u`, `~` and `M`, which may end an Enter sequence (`CSI 13 u`,
//!      `CSI 27;5;13 ~`, `ESC O M`), except the `~` of an exact
//!      bracketed-paste marker that is the whole stretch;
//!    - the prompt on the tool's command line, if Connecto launched it with
//!      one.
//!
//!    Only a *pure* submission has a known text: plain text typed in the
//!    window (no paste, no line break, no ESC or other control, not blank)
//!    and one window CR. Every other possible submission is of unknown origin. Every
//!    hook takes one off, oldest first, except a repeat (same text, before
//!    the previous hook's turn ended).
//! 4. While TAINTED every hook is not the user's.
//! 5. TAINTED becomes CLEAN only at a turn end of this attempt that closes a
//!    hook which came after a pure submission made after the last taint,
//!    with *nothing* waiting and no byte written since the last submit key.
//!    A possible submission is cleared only by a hook: one that never gets
//!    its hook (a dialog's Enter, a lost hook) keeps the ledger TAINTED until
//!    the terminal restarts.
//! 6. While CLEAN a hook is the user's only if: of this attempt, not cut, not
//!    before the previous hook's turn ended, exactly one submission waiting,
//!    that submission pure, the hook's text equal to its text (line ends
//!    unified). Otherwise: not the user's, and rule 2.
//!
//! **Invariant:** hooks <= real prompts <= possible submissions. Repeats
//! aside, each counted hook stands for at least one real prompt and no two
//! for the same one; so nothing waiting means every possible submission was
//! a real prompt that has had its one hook, and no hook is still on its way.
//! After that point every possible submission is pure, or the ledger is
//! TAINTED again. What `Quarantine` still assumes of the tool:
//! - no prompt the tool starts by itself (the list under "Two modes"): every
//!   prompt, and so every hook, comes from a key the ledger counts under
//!   rule 3, at most one per counted key, or from the registered
//!   command-line prompt;
//! - a hook is delivered twice only back to back;
//! - a turn end comes after every hook of every submission up to its own
//!   prompt (the last hook).
//!
//! Hooks may otherwise be late, reordered, lost, merged, trimmed or altered.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

/// Who wrote some bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    /// This app's window: keys, pastes, dropped paths, mouse, wheel.
    Window,
    /// A remote peer.
    Remote,
    /// Connecto itself: relay, `send`, `keys`, automations, `/rename`, or any
    /// direct write to a session.
    Connecto,
    /// The emulator answering a terminal query.
    Protocol,
}

/// What a submitted prompt was, as the ledger knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Typed in the window, every byte, in a clean terminal (rule 6).
    User,
    /// Not the user's; a peer or Connecto wrote to the terminal since it was
    /// last clean.
    Foreign,
    /// Not the user's, nothing foreign known.
    Unknown,
}

/// Whether a terminal prompt may ever be recorded as the user's: the one
/// switch, applied where prompts are captured (`memory_capture`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalCredit {
    /// Never: no prompt typed in a terminal is the user's statement. Assumes
    /// nothing about the tools. The ledger still runs; its `User` is dropped.
    #[default]
    Off,
    /// The quarantine rule of this module: sound only if the tool never
    /// starts a prompt by itself (see the module doc).
    Quarantine,
}

impl TerminalCredit {
    /// The verdict recorded for a prompt the ledger judged `verdict`.
    #[must_use]
    pub fn apply(self, verdict: Verdict) -> Verdict {
        match (self, verdict) {
            (Self::Off, Verdict::User) => Verdict::Unknown,
            _ => verdict,
        }
    }
}

thread_local! {
    /// Set by the remote guard on a peer's thread, and never cleared: every
    /// pane write from that thread is the peer's.
    static REMOTE_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// The remote guard saw a peer's input on this thread: from now on every
/// pane write on this thread is remote. A peer's reader thread does nothing
/// else, so a sticky mark can only ever make a write foreign.
pub(crate) fn mark_remote_input() {
    REMOTE_THREAD.with(|m| m.set(true));
}

/// The source of a write reaching `ManagedPane::send_input` on this thread.
pub(crate) fn pane_source() -> InputSource {
    if REMOTE_THREAD.with(Cell::get) {
        InputSource::Remote
    } else {
        InputSource::Window
    }
}

const ESC: u8 = 0x1b;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Waiting submissions kept in full; more are only counted (and taint).
const MAX_PENDING: usize = 64;
/// Longest text followed for one submission; more taints.
const MAX_TEXT: usize = 256 * 1024;
/// Bytes of an escape stretch kept (enough to tell a paste marker).
const STRETCH_KEPT: usize = 8;

/// The composer since the last submit key, as the bytes say.
#[derive(Debug, Clone)]
struct Draft {
    /// Its bytes, Backspace applied.
    text: Vec<u8>,
    /// A byte of any source was written since the last submit key.
    any: bool,
    /// Every byte was the window's.
    window_only: bool,
    /// Its content cannot be followed from the bytes.
    uncertain: bool,
    /// It holds something other than whitespace.
    content: bool,
    /// A paste marker went in: never pure (the tool may not be in paste mode).
    pasted: bool,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            text: Vec::new(),
            any: false,
            window_only: true,
            uncertain: false,
            content: false,
            pasted: false,
        }
    }
}

/// A submission waiting for its hook.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    /// Plain typed text and one Enter, all the window's, nothing uncertain.
    user: bool,
    /// Its text, in [`normal`] form; `None` when it cannot be known.
    text: Option<String>,
}

/// What one write would do to the ledger, committed only if it was enqueued.
struct Plan {
    draft: Draft,
    in_paste: bool,
    scan: Scan,
    submits: Vec<Pending>,
    taint: bool,
    /// The write's source is a peer or Connecto.
    foreign: bool,
}

/// The provenance ledger of one session (one attempt of a terminal). Its
/// default is TAINTED (`clean: false`, rule 1).
#[derive(Debug, Clone, Default)]
pub(crate) struct Ledger {
    /// The writes the session's input took, by anyone (the window, a peer,
    /// Connecto; not the terminal's own protocol replies): counted with the
    /// enqueue, under the gate's lock (see [`InputGate::write_if`]).
    writes: u64,
    /// The pane attempt this ledger belongs to.
    attempt: u64,
    clean: bool,
    /// A pure submission was committed after the last taint.
    fresh_submit: bool,
    /// A hook arrived after that submit key.
    fresh_hook: bool,
    /// The text of the last hook of this attempt whose turn has not ended.
    open_hook: Option<String>,
    /// A peer or Connecto wrote since the ledger was last clean (the label of
    /// a verdict that is not the user's).
    foreign: bool,
    draft: Draft,
    /// Inside a paste, as the markers say (for the text only: counting never
    /// depends on it).
    in_paste: bool,
    /// Escape stretches and UTF-8, across writes.
    scan: Scan,
    /// The newest waiting submissions.
    pending: VecDeque<Pending>,
    /// Older waiting submissions, counted but not kept.
    unkept: u64,
    /// Every byte committed, in order (tests prove order with it).
    #[cfg(test)]
    log: Vec<u8>,
}

/// The form texts are compared in: line ends as `\n`, nothing else changed.
fn normal(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// A line break: a submit key in some input mode of some tool.
fn line_break(b: u8) -> bool {
    matches!(b, b'\r' | b'\n')
}

/// What the byte stream says about escape sequences, without parsing them
/// (rule 3: counting is an upper bound under any parse).
///
/// An *escape stretch* opens at an introducer: ESC, a C1 control (U+0080 to
/// U+009F in UTF-8, or a stray byte 0x80 to 0x9F). It stays open over every
/// byte except one in 0x40..=0x7E that does not directly follow an
/// introducer: under any parser (ignoring stray ESCs until a final byte,
/// restarting at each ESC, executing controls inside a sequence) no Enter
/// sequence can still be in progress after that byte. Every CR, LF, `u`, `~`
/// or `M` while a stretch is open is counted as a possible submission (it
/// may end `CSI 13 u`, `CSI 27;5;13 ~` or `ESC O M`, or be Enter itself),
/// except the final `~` of an exact bracketed-paste marker that is the whole
/// stretch.
#[derive(Debug, Clone, Default)]
struct Scan {
    /// The open stretch's first bytes.
    stretch: Option<Vec<u8>>,
    /// The last byte was an introducer.
    after_intro: bool,
    /// UTF-8: continuation bytes still due, and the lead byte.
    utf8_left: u8,
    utf8_lead: u8,
}

impl Scan {
    /// Whether `b` is an introducer, following UTF-8 to tell a C1 control
    /// from a continuation byte.
    fn introducer(&mut self, b: u8) -> bool {
        match b {
            ESC => {
                self.utf8_left = 0;
                true
            }
            0x80..=0xbf if self.utf8_left > 0 => {
                self.utf8_left -= 1;
                self.utf8_lead == 0xc2 && b <= 0x9f
            }
            0x80..=0x9f => true,
            0xc0..=0xf7 => {
                self.utf8_lead = b;
                self.utf8_left = match b {
                    0xc0..=0xdf => 1,
                    0xe0..=0xef => 2,
                    _ => 3,
                };
                false
            }
            _ => {
                self.utf8_left = 0;
                false
            }
        }
    }
}

impl Plan {
    fn mark(&mut self, window: bool) {
        self.draft.any = true;
        self.draft.window_only &= window;
    }

    fn uncertain(&mut self) {
        self.draft.uncertain = true;
        self.taint = true;
    }

    fn text(&mut self, b: u8, window: bool) {
        self.mark(window);
        if self.draft.text.len() >= MAX_TEXT {
            self.uncertain();
        } else {
            self.draft.text.push(b);
        }
        if !b.is_ascii_whitespace() {
            self.draft.content = true;
        }
    }

    fn backspace(&mut self, window: bool) {
        self.mark(window);
        // Remove one character (a UTF-8 sequence).
        while let Some(last) = self.draft.text.pop() {
            if last & 0xc0 != 0x80 {
                break;
            }
        }
    }

    /// A key that may have submitted something unknown (rule 3).
    fn possible(&mut self) {
        self.submits.push(Pending {
            user: false,
            text: None,
        });
    }

    /// A CR outside a paste.
    fn submit(&mut self, window: bool) {
        let draft = std::mem::take(&mut self.draft);
        let pure = window
            && draft.window_only
            && !draft.uncertain
            && !draft.pasted
            && draft.content
            && !draft.text.iter().copied().any(line_break);
        if !draft.any || (draft.window_only && !draft.uncertain && !draft.content) {
            // An Enter on an empty or blank composer: a dialog's answer, or
            // text the tool filled in (a suggestion, a recalled prompt).
            self.taint = true;
        }
        if pure {
            self.submits.push(Pending {
                user: true,
                text: Some(normal(&String::from_utf8_lossy(&draft.text))),
            });
        } else {
            self.possible();
        }
    }

    /// One byte, `window` if the window wrote it.
    fn byte(&mut self, b: u8, window: bool) {
        if self.scan.introducer(b) {
            // ESC or C1, anywhere: never pure, and the composer may hold
            // anything.
            self.mark(window);
            self.uncertain();
            match &mut self.scan.stretch {
                Some(kept) if kept.len() < STRETCH_KEPT => kept.push(b),
                Some(_) => {}
                None => self.scan.stretch = Some(vec![b]),
            }
            self.scan.after_intro = true;
            return;
        }
        if let Some(kept) = &mut self.scan.stretch {
            if kept.len() < STRETCH_KEPT {
                kept.push(b);
            }
            let closes = (0x40..=0x7e).contains(&b) && !self.scan.after_intro;
            self.scan.after_intro = false;
            let marker = closes && (kept.as_slice() == PASTE_START || kept.as_slice() == PASTE_END);
            if marker {
                self.in_paste = kept.as_slice() == PASTE_START;
                self.draft.pasted = true;
            } else if line_break(b) || matches!(b, b'u' | b'~' | b'M') {
                // It may end an Enter sequence, or be Enter itself.
                self.possible();
            }
            if closes {
                self.scan.stretch = None;
            }
            self.mark(window);
            return;
        }
        if self.in_paste {
            if line_break(b) {
                // A tool out of paste mode, or one that splits pastes, may
                // take it as Enter (round 7): counted, and uncertain.
                self.possible();
                self.uncertain();
            } else if (b < 0x20 && b != b'\t') || b == 0x7f {
                // A control byte in a paste (finding 6).
                self.uncertain();
            }
            self.text(b, window);
            return;
        }
        match b {
            b'\r' if self.draft.text.last() == Some(&b'\\') => {
                // A newline in Claude, maybe a submission elsewhere.
                self.draft.text.pop();
                self.text(b'\n', window);
                self.uncertain();
                self.possible();
            }
            b'\r' => self.submit(window),
            b'\n' => {
                // A newline in some tools, a submission in others.
                self.mark(window);
                self.uncertain();
                self.possible();
            }
            0x7f | 0x08 => self.backspace(window),
            // Tab, Ctrl- keys: the composer may now hold anything.
            0x00..=0x1f => {
                self.mark(window);
                self.uncertain();
            }
            _ => self.text(b, window),
        }
    }
}

impl Ledger {
    /// What `bytes` from `source` would do; nothing changes yet.
    fn plan(&self, bytes: &[u8], source: InputSource) -> Plan {
        let window = source == InputSource::Window;
        let mut plan = Plan {
            draft: self.draft.clone(),
            in_paste: self.in_paste,
            scan: self.scan.clone(),
            submits: Vec::new(),
            // Rule 2: any byte not from the window.
            taint: !window,
            foreign: matches!(source, InputSource::Remote | InputSource::Connecto),
        };
        for &b in bytes {
            plan.byte(b, window);
        }
        plan
    }

    /// Commit a write the PTY queue took.
    fn commit(&mut self, plan: Plan) {
        self.draft = plan.draft;
        self.in_paste = plan.in_paste;
        self.scan = plan.scan;
        if plan.taint {
            self.taint(plan.foreign);
        }
        if plan.submits.iter().any(|s| s.user) && !plan.taint {
            // Rule 5: a pure submission after the last taint.
            self.fresh_submit = true;
        }
        for submit in plan.submits {
            if self.pending.len() >= MAX_PENDING {
                // Counted, not kept: the count stays exact (finding 5).
                self.pending.pop_front();
                self.unkept = self.unkept.saturating_add(1);
                self.taint(false);
            }
            self.pending.push_back(submit);
        }
    }

    /// Account for a write that was enqueued (tests; the gate plans first).
    #[cfg(test)]
    pub(crate) fn write(&mut self, bytes: &[u8], source: InputSource) {
        self.log.extend_from_slice(bytes);
        let plan = self.plan(bytes, source);
        self.commit(plan);
    }

    /// Rule 2.
    pub(crate) fn taint(&mut self, foreign: bool) {
        self.clean = false;
        self.fresh_submit = false;
        self.fresh_hook = false;
        self.foreign |= foreign;
    }

    fn waiting(&self) -> u64 {
        self.unkept.saturating_add(self.pending.len() as u64)
    }

    /// Take the oldest waiting submission off the count.
    fn take(&mut self) -> Option<Pending> {
        if self.unkept > 0 {
            self.unkept -= 1;
            return Some(Pending {
                user: false,
                text: None,
            });
        }
        self.pending.pop_front()
    }

    fn not_user(&self) -> Verdict {
        if self.foreign {
            Verdict::Foreign
        } else {
            Verdict::Unknown
        }
    }

    pub(crate) fn set_attempt(&mut self, attempt: u64) {
        self.attempt = attempt;
    }

    /// The tool was started with a prompt on its command line: its hook is
    /// one more possible submission, before any key (rule 3).
    pub(crate) fn expect_startup_prompt(&mut self) {
        self.taint(false);
        self.pending.push_front(Pending {
            user: false,
            text: None,
        });
    }

    /// A prompt hook of `attempt` with `text` (cut when `truncated`).
    pub(crate) fn prompt(&mut self, attempt: u64, text: &str, truncated: bool) -> Verdict {
        if attempt != self.attempt {
            // Another attempt's hook: it only taints this ledger.
            self.taint(false);
            return self.not_user();
        }
        let text = normal(text);
        if let Some(open) = self.open_hook.replace(text.clone()) {
            // Before the previous hook's turn ended: an interrupted turn, a
            // prompt the tool took mid-turn, or the same hook twice. Only a
            // different text takes a submission off (rule 3).
            self.taint(false);
            if open != text {
                let _ = self.take();
            }
            return self.not_user();
        }
        let single = self.waiting() == 1;
        let Some(taken) = self.take() else {
            // More hooks than submissions.
            self.taint(false);
            return self.not_user();
        };
        if !self.clean {
            // Rule 4; rule 5 notes the hook, unless it is cut (rule 2).
            if truncated {
                self.taint(false);
            } else {
                self.fresh_hook = self.fresh_submit;
            }
            return self.not_user();
        }
        // Rule 6.
        let matches = taken.text.as_deref() == Some(text.as_str());
        if single && taken.user && !truncated && matches {
            Verdict::User
        } else {
            self.taint(false);
            self.not_user()
        }
    }

    /// The tool ended a turn of `attempt`; `known` is false when the event
    /// could not be read.
    pub(crate) fn turn_ended(&mut self, attempt: u64, known: bool) {
        if attempt != self.attempt {
            self.taint(false);
            return;
        }
        let closes = self.open_hook.take().is_some();
        if !known {
            self.taint(false);
            return;
        }
        // Rule 5.
        if !self.clean
            && closes
            && self.fresh_submit
            && self.fresh_hook
            && self.waiting() == 0
            && !self.draft.any
            && !self.in_paste
            && self.scan.stretch.is_none()
        {
            self.clean = true;
            self.foreign = false;
        }
    }

    #[cfg(test)]
    pub(crate) fn is_clean(&self) -> bool {
        self.clean
    }
}

/// The one way into a session's PTY: provenance and enqueueing in one order.
#[derive(Debug, Default)]
pub struct InputGate {
    ledger: Mutex<Ledger>,
}

impl InputGate {
    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hand `bytes` to `send` (the PTY enqueue) and account for them, both
    /// under one lock: no other write can come between them. The ledger
    /// changes only if `send` took the bytes; a refusal taints it.
    pub(crate) fn write<E>(
        &self,
        bytes: Vec<u8>,
        source: InputSource,
        send: impl FnOnce(Vec<u8>) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut ledger = self.ledger();
        let plan = ledger.plan(&bytes, source);
        #[cfg(test)]
        let copy = bytes.clone();
        match send(bytes) {
            Ok(()) => {
                #[cfg(test)]
                ledger.log.extend_from_slice(&copy);
                ledger.commit(plan);
                // The terminal's own replies to the program's queries are
                // not input anyone gave: they do not move it.
                if source != InputSource::Protocol {
                    ledger.writes += 1;
                }
                Ok(())
            }
            Err(err) => {
                ledger.taint(plan.foreign);
                Err(err)
            }
        }
    }

    /// [`Self::write`], only if the session's input has taken exactly
    /// `writes` writes (when given): the comparison, the enqueue and the
    /// count all under the gate's one lock, so no other writer's bytes can
    /// come between the check and these. Otherwise `stale()`, and nothing
    /// is written.
    pub(crate) fn write_if<E>(
        &self,
        writes: Option<u64>,
        bytes: Vec<u8>,
        source: InputSource,
        send: impl FnOnce(Vec<u8>) -> Result<(), E>,
        stale: impl FnOnce() -> E,
    ) -> Result<(), E> {
        let mut ledger = self.ledger();
        if writes.is_some_and(|w| w != ledger.writes) {
            return Err(stale());
        }
        let plan = ledger.plan(&bytes, source);
        #[cfg(test)]
        let copy = bytes.clone();
        match send(bytes) {
            Ok(()) => {
                #[cfg(test)]
                ledger.log.extend_from_slice(&copy);
                ledger.commit(plan);
                // The terminal's own replies to the program's queries are
                // not input anyone gave: they do not move it.
                if source != InputSource::Protocol {
                    ledger.writes += 1;
                }
                Ok(())
            }
            Err(err) => {
                ledger.taint(plan.foreign);
                Err(err)
            }
        }
    }

    /// The writes the session's input took so far.
    pub(crate) fn writes(&self) -> u64 {
        self.ledger().writes
    }

    /// The pane attempt this session is.
    pub(crate) fn set_attempt(&self, attempt: u64) {
        self.ledger().set_attempt(attempt);
    }

    /// A prompt hook of `attempt` arrived with `text`.
    pub(crate) fn prompt(&self, attempt: u64, text: &str, truncated: bool) -> Verdict {
        self.ledger().prompt(attempt, text, truncated)
    }

    /// A turn of `attempt` ended (`known`: the event could be read).
    pub(crate) fn turn_ended(&self, attempt: u64, known: bool) {
        self.ledger().turn_ended(attempt, known);
    }

    /// The tool was started with a prompt on its command line.
    pub(crate) fn expect_startup_prompt(&self) {
        self.ledger().expect_startup_prompt();
    }

    /// Something happened the ledger cannot account for: the tool's (re)start,
    /// lost hook events.
    pub(crate) fn taint(&self) {
        self.ledger().taint(false);
    }

    #[cfg(test)]
    pub(crate) fn log(&self) -> Vec<u8> {
        self.ledger().log.clone()
    }

    #[cfg(test)]
    pub(crate) fn is_clean(&self) -> bool {
        self.ledger().is_clean()
    }
}

#[cfg(test)]
#[path = "input_ledger_tests.rs"]
mod tests;
