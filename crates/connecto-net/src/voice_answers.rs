//! The questions a project's terminals ask now (a permission, a menu, a
//! question that wants words, a blocked task's question in the app
//! session's chat), and answering one of them, for a voice call on the
//! phone (ADR 0034, "answers by voice").
//!
//! Only the terminals the peer's latest list grants it are read or written,
//! each by its wire id, and the grant is checked again before every write
//! (and, for a list of questions, while it is delivered). Every read is a
//! capture of the terminal's live bottom (never the view's scroll position)
//! taken whole: one a synchronized redraw is still holding back is not
//! taken, but read again once the redraw is over. Every write is
//! conditional: the host reads the terminal again right before it (the
//! terminal as it is now, not as listed), goes on only while it shows that
//! very instance of that question, and each write (every arrow, paste and
//! Enter) goes in only under the terminal's input lock, on the input
//! generation the instance was read at (nobody else typed, nothing
//! restarted) and, for the agent's prompt, on its exact contents as typed
//! and as drawn ([`TerminalSession::send_input_if`]); never held back while
//! Connecto types, to be replayed later.
//!
//! A dialog's instance is a token the host issues ([`Dialogs`]): it holds
//! while the dialog stays on screen and nobody else types into the
//! terminal, and ends when the dialog goes, is submitted, or the terminal
//! takes other input or restarts. The same dialog shown again is another
//! instance.
//!
//! What is not positively recognised is shown and left to a tap
//! ([`connecto_core::question`]): only a known layout's one-shot choices and
//! known text fields are answered by voice, and that is checked here, at
//! the write, whatever the phone asked.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use connecto_core::question::{self, ChooseRefused, Kind, Question};
use connecto_core::session::{APP_SESSION, ChatHistory};
use connecto_core::{
    CellFlags, InputCondition, InputGeneration, InputModes, SnapshotOutcome, TermActivity,
    TermSnapshot, TerminalEntry, TerminalSession,
};
use serde_json::{Value, json};

/// What an answer does: take a choice (by its index), or type words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnswerChoice {
    Option(usize),
    Text(String),
}

/// Writes bytes into a terminal (by its wire id) as the peer, only if the
/// peer still holds that terminal and `condition` holds, both checked with
/// the write itself; at once or not at all.
pub type Typer<'a> =
    &'a dyn Fn(&TerminalEntry, u32, &InputCondition, Vec<u8>) -> Result<(), String>;

/// One write of an answer to the terminal it is for, made only if the
/// condition holds then.
type ConditionalWrite<'a> = &'a dyn Fn(&InputCondition, &[u8]) -> Result<(), String>;

/// The project's granted terminals as they are now, each with its wire id
/// (read again at every step of an answer).
pub type Terminals<'a> = &'a dyn Fn() -> Vec<(TerminalEntry, u32)>;

/// The longest words an answer may type: one line, read back whole.
pub const MAX_TEXT: usize = 500;

/// How long to wait between reads of the screen, how many reads one key may
/// take to show (or a held-back redraw to end), and how long the question
/// may take to go once Enter was pressed.
const STEP: Duration = Duration::from_millis(40);
const TRIES: usize = 25;
const GONE_WITHIN: Duration = Duration::from_secs(2);

/// `entries` (one project's terminals) that the peer is granted now, each
/// with its wire id: `wire_of` gives a terminal's (by its host id), and
/// `granted` says whether the peer's latest list holds it. A terminal with
/// no wire id, or not in that list (one started since), is left out.
pub fn granted_terminals(
    entries: Vec<TerminalEntry>,
    wire_of: impl Fn(&str) -> Option<u32>,
    granted: impl Fn(u32) -> bool,
) -> Vec<(TerminalEntry, u32)> {
    entries
        .into_iter()
        .filter_map(|t| {
            let wire = wire_of(&t.id)?;
            granted(wire).then_some((t, wire))
        })
        .collect()
}

/// The wire ids of the terminals whose questions an answer to
/// [`questions`] carries: what its delivery must still be granted.
#[must_use]
pub fn carried_terminals(answer: &[u8]) -> Vec<u32> {
    serde_json::from_slice::<Value>(answer)
        .ok()
        .and_then(|v| v["questions"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|q| q["terminal"].as_u64().and_then(|n| u32::try_from(n).ok()))
        .collect()
}

fn fnv(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in *part {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash & 0xffff_ffff_ffff
}

/// The id of a blocked task's question in the chat: from the message's
/// text, so another message (even about the same task) is another id.
#[must_use]
pub fn chat_id(message: &str) -> String {
    format!("chat-{:012x}", fnv(&[message.as_bytes()]))
}

/// The occurrence of a chat question: the message that asked it (its place
/// in the chat), so turns about other tasks leave it the same.
fn chat_occurrence(index: u64) -> String {
    format!("m{index}")
}

/// The task a "Question [t-…]." message asks about.
fn question_task(message: &str) -> Option<String> {
    let s = message.trim_start_matches(|c: char| c.is_whitespace() || "*_#>".contains(c));
    let rest = s
        .get(..8)?
        .eq_ignore_ascii_case("question")
        .then(|| &s[8..])?;
    let rest = rest.trim_start().strip_prefix(['[', '('])?;
    let end = rest.find([']', ')'])?;
    let id = &rest[..end];
    (id.starts_with("t-") && id.len() > 2 && id[2..].chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| id.to_lowercase())
}

/// What a call's own prompt says of itself (`newRequestNote` in the app):
/// a prompt carrying it is never an answer to a task's question.
pub const NEW_REQUEST: &str = "a new request, not an answer to any";

/// The start of an answer the call routes to task `task`'s question.
#[must_use]
pub fn answer_lead(task: &str) -> String {
    format!("Answer to Question [{task}]:")
}

/// The blocked tasks' questions the chat still waits on, each task's latest,
/// in the order asked: a "Question [t-…]." of the agent's is pending until
/// the agent says anything else naming that task (a report, or another
/// question that takes its place), a prompt answers it by name
/// ([`answer_lead`]), or a prompt right after it answers it as before (one
/// that says it is a new request, [`NEW_REQUEST`], does not). Unrelated
/// turns in between (another task started, a question asked) leave it
/// pending.
#[must_use]
pub fn pending_chat_questions(history: &ChatHistory) -> Vec<(String, &str)> {
    pending_chat_questions_at(history)
        .into_iter()
        .map(|(task, text, _)| (task, text))
        .collect()
}

/// The task a report of the agent's is about, by its leading marker
/// ("Task complete [t-…]", "Task failed [t-…]", "Queued [t-…]", or their
/// Italian forms): a task merely mentioned in a report's body is not.
fn report_task(message: &str) -> Option<String> {
    let s = message
        .trim_start_matches(|c: char| c.is_whitespace() || "*_#>".contains(c))
        .to_lowercase();
    let rest = [
        "task complete",
        "attività completata",
        "compito completato",
        "task failed",
        "compito non riuscito",
        "attività non riuscita",
        "queued",
        "in coda",
    ]
    .iter()
    .find_map(|marker| s.strip_prefix(marker))?;
    let rest = rest.trim_start().strip_prefix(['[', '('])?;
    let end = rest.find([']', ')'])?;
    let id = &rest[..end];
    (id.starts_with("t-") && id.len() > 2 && id[2..].chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| id.to_owned())
}

/// [`pending_chat_questions`], each with the index of the message that
/// asked it: what tells this asking from another, whatever else the chat
/// says since. The position is absolute (among everything the chat ever
/// held), so it stays the same when the oldest messages are dropped, and a
/// later asking never takes an earlier one's.
#[must_use]
pub fn pending_chat_questions_at(history: &ChatHistory) -> Vec<(String, &str, u64)> {
    let first = connecto_core::siri_reply::first_seq(history);
    let mut pending: Vec<(String, &str, u64)> = Vec::new();
    let mut last_asked: Option<String> = None;
    for (i, (from_user, text)) in history.messages.iter().enumerate() {
        let index = first + i as u64;
        if *from_user {
            let answered = pending
                .iter()
                .position(|(task, ..)| text.trim_start().starts_with(&answer_lead(task)))
                .or_else(|| {
                    // Right after its question, a plain prompt answers it.
                    let task = last_asked.as_ref()?;
                    (!text.contains(NEW_REQUEST))
                        .then(|| pending.iter().position(|(t, ..)| t == task))
                        .flatten()
                });
            if let Some(at) = answered {
                pending.remove(at);
            }
            last_asked = None;
            continue;
        }
        if let Some(task) = question_task(text) {
            pending.retain(|(t, ..)| *t != task);
            pending.push((task.clone(), text.as_str(), index));
            last_asked = Some(task);
            continue;
        }
        // A report about a task, by its leading marker: it moved on. One
        // that only mentions it ("waiting for [t-…]") does not close it.
        if let Some(task) = report_task(text) {
            pending.retain(|(t, ..)| *t != task);
        }
        last_asked = None;
    }
    pending
}

/// `entry`'s live bottom now, taken whole: its lines (a row the terminal
/// soft-wrapped joined with the next) and input modes. None when no whole
/// capture could be had now (the emulator busy, or a synchronized redraw
/// still held back: never ended early to read it half drawn).
fn capture(entry: &TerminalEntry) -> Option<(String, InputModes, u64, u16)> {
    let mut snap = TermSnapshot::default();
    let mut history = TermSnapshot::default();
    if entry
        .session
        .poll_capture(0, false, &mut snap, &mut history)
        != SnapshotOutcome::Updated
        || snap.rows == 0
    {
        return None;
    }
    let mut screen = String::new();
    for r in 0..snap.rows {
        let row = snap.row(r);
        let text: String = row.iter().map(|c| c.ch).collect();
        let wrapped = row
            .last()
            .is_some_and(|c| c.flags.contains(CellFlags::WRAPPED));
        if wrapped {
            screen.push_str(&text);
        } else {
            screen.push_str(text.trim_end());
            screen.push('\n');
        }
    }
    Some((screen, snap.modes, snap.generation, snap.cols))
}

/// What a terminal shows at its live bottom now, the input generation it
/// was read at (read before the screen: a write after it moves it), and the
/// screen's own generation (it moves with any output drawn).
pub struct Seen {
    pub question: Option<Question>,
    pub modes: InputModes,
    pub input: Option<InputGeneration>,
    pub output: u64,
    /// The screen's width.
    pub cols: u16,
}

/// The dialog `entry` shows at its live bottom now, if any. None: no whole
/// capture now.
#[must_use]
pub fn look(entry: &TerminalEntry) -> Option<Seen> {
    let input = entry.session.input_generation();
    let (screen, modes, output, cols) = capture(entry)?;
    let question = if !entry.tool.is_agent()
        || matches!(
            entry.state,
            TermActivity::NotStarted | TermActivity::Failed | TermActivity::Exited
        ) {
        None
    } else {
        question::active(entry.tool, &screen)
    };
    Some(Seen {
        question,
        modes,
        input,
        output,
        cols,
    })
}

/// [`look`], again while the capture cannot be taken whole.
fn look_hard(entry: &TerminalEntry, wait: &dyn Fn()) -> Option<Seen> {
    for _ in 0..TRIES {
        if let Some(seen) = look(entry) {
            return Some(seen);
        }
        wait();
    }
    None
}

fn is_app_session(entry: &TerminalEntry) -> bool {
    entry.name.as_deref() == Some(APP_SESSION) && entry.parent.is_none()
}

/// The dialog instances the host issued tokens for ([`questions`]): an
/// answer names one, claims it before its first key, and spends it with
/// Enter.
#[derive(Default)]
pub struct Dialogs {
    inner: Mutex<HashMap<String, Instance>>,
    watching: AtomicBool,
}

/// Where an instance stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    /// Issued, not yet answered.
    Free,
    /// An answer has it: no other may.
    Claimed,
    /// Its answer pressed Enter: no other may, for as long as the
    /// terminal still shows that dialog.
    Spent,
}

/// One dialog as it stays on one terminal: its exact operation, and the
/// input and output generations the host last saw it at (moved on only by
/// the answer's own writes, and by what the answer itself then read).
struct Instance {
    wire: u32,
    entry: TerminalEntry,
    id: String,
    operation: Option<String>,
    input: InputGeneration,
    output: u64,
    issued: Instant,
    used: Use,
    /// The choice selected, as the host last saw it.
    selected: Option<usize>,
    /// What the answer's last key is to draw, not yet seen: until it is,
    /// nothing else drawn is taken for the instance.
    expect: Option<Expect>,
}

/// The one change an answer's key may draw.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    /// An arrow: this choice selected, nothing else changed.
    Select(usize),
    /// A paste into the question's field: it holds exactly these words.
    Field(String),
}

/// What an answer writes to a dialog.
pub(crate) enum Key<'a> {
    Down,
    Up,
    Paste(&'a str),
    Enter,
}

impl Instance {
    /// Whether `seen` shows this very instance, nothing having happened
    /// since the host last saw it: the same dialog and operation, no input
    /// and no output since.
    fn unchanged(&self, seen: &Seen) -> bool {
        seen.input == Some(self.input) && seen.output == self.output && self.shows(seen)
    }

    /// Whether `seen` shows exactly the change the answer's last key was
    /// to draw (and the same dialog, no other input).
    fn drew(&self, seen: &Seen, expect: &Expect) -> bool {
        let tool = self.entry.tool;
        seen.input == Some(self.input)
            && self.shows(seen)
            && seen.question.as_ref().is_some_and(|q| match expect {
                Expect::Select(k) => q.choices(tool).1 == Some(*k),
                Expect::Field(words) => q.field(tool).is_some_and(|f| f.value == *words),
            })
    }

    /// Whether `seen` shows this dialog, with this operation.
    fn shows(&self, seen: &Seen) -> bool {
        seen.question
            .as_ref()
            .is_some_and(|q| q.id(self.entry.tool) == self.id && q.operation == self.operation)
    }
}

/// How often the host looks at the terminals whose dialogs it issued tokens
/// for, and how long a token can live.
const WATCH_EVERY: Duration = Duration::from_millis(100);
const TOKEN_LIFE: Duration = Duration::from_secs(15 * 60);

impl Dialogs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instance>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The token of the dialog `q` that `entry` (wire id `wire`) shows in
    /// `seen`: the one already issued while nothing changed there since, or
    /// a new one. None for a terminal that does not count its input
    /// (nothing can be written to it conditionally).
    fn issue(&self, entry: &TerminalEntry, wire: u32, q: &Question, seen: &Seen) -> Option<String> {
        let input = seen.input?;
        let id = q.id(entry.tool);
        let mut inner = self.lock();
        // Being answered, or submitted and still on screen (its redraw not
        // come): no new token for it, so it cannot be answered twice.
        if inner.values().any(|i| {
            i.used != Use::Free
                && i.wire == wire
                && i.id == id
                && Arc::ptr_eq(&i.entry.session, &entry.session)
        }) {
            return None;
        }
        if let Some((token, _)) = inner.iter().find(|(_, i)| {
            i.used == Use::Free
                && i.wire == wire
                && Arc::ptr_eq(&i.entry.session, &entry.session)
                && i.unchanged(seen)
        }) {
            return Some(token.clone());
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).ok()?;
        let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let token = format!("d-{token}");
        inner.insert(
            token.clone(),
            Instance {
                wire,
                entry: entry.clone(),
                id,
                operation: q.operation.clone(),
                input,
                output: seen.output,
                issued: Instant::now(),
                used: Use::Free,
                selected: q.choices(entry.tool).1,
                expect: None,
            },
        );
        Some(token)
    }

    /// Ends every instance that is over. One not being answered: any
    /// input, any output since the host last saw it (the dialog could have
    /// gone and come back between two looks, so a change not seen is taken
    /// for one), or too old. One spent: once its dialog is no longer shown.
    /// One being answered is its answer's to follow. A screen that cannot
    /// be read now is looked at next time.
    pub fn observe(&self) {
        let mut inner = self.lock();
        inner.retain(|_, i| match i.used {
            Use::Claimed => true,
            _ if i.issued.elapsed() > TOKEN_LIFE => false,
            Use::Spent => look(&i.entry).is_none_or(|seen| i.shows(&seen)),
            Use::Free => {
                if i.entry.session.input_generation() != Some(i.input) {
                    return false;
                }
                look(&i.entry).is_none_or(|seen| i.unchanged(&seen))
            }
        });
    }

    /// Looks at the issued instances every [`WATCH_EVERY`] from a thread of
    /// its own, while there are any.
    pub fn watch(self: &Arc<Self>) {
        if self.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let dialogs = Arc::downgrade(self);
        let started = std::thread::Builder::new()
            .name("voice-dialogs".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(WATCH_EVERY);
                    let Some(dialogs) = dialogs.upgrade() else {
                        return;
                    };
                    dialogs.observe();
                    let inner = dialogs.lock();
                    if inner.is_empty() {
                        dialogs.watching.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            });
        if started.is_err() {
            self.watching.store(false, Ordering::SeqCst);
        }
    }

    /// Claims instance `token` (dialog `id` on `wire`, that very session)
    /// for one answer, before its first key: only if no answer has it or
    /// spent it, and the terminal shows it now, nothing having happened
    /// since the host last saw it.
    fn claim(
        &self,
        token: &str,
        wire: u32,
        id: &str,
        session: &Arc<dyn TerminalSession>,
    ) -> Result<(), String> {
        let mut inner = self.lock();
        let changed = || ChooseRefused::Changed.to_string();
        let instance = inner.get_mut(token).ok_or_else(changed)?;
        if instance.used != Use::Free {
            return Err("That question is being answered already.".into());
        }
        // Asked for another question or terminal: refused, the instance
        // left as it is.
        if instance.wire != wire
            || instance.id != id
            || !Arc::ptr_eq(&instance.entry.session, session)
        {
            return Err(changed());
        }
        // Changed since the host last saw it: over.
        if !look(&instance.entry).is_some_and(|seen| instance.unchanged(&seen)) {
            inner.remove(token);
            return Err(changed());
        }
        instance.used = Use::Claimed;
        Ok(())
    }

    /// The claiming answer read the dialog again (after its own key): what
    /// it saw is what the instance is now, or the instance is over.
    fn saw(&self, token: &str, seen: &Seen) -> bool {
        let mut inner = self.lock();
        let Some(instance) = inner.get_mut(token) else {
            return false;
        };
        if instance.used != Use::Claimed {
            return false;
        }
        // Nothing drawn since: as it was.
        if instance.unchanged(seen) {
            return true;
        }
        // Drawn since: only the change the answer's own last key was to
        // draw is taken; anything else (or anything before a key) ends it.
        match instance.expect.take() {
            Some(expect) if instance.drew(seen, &expect) => {
                instance.output = seen.output;
                if let Expect::Select(k) = expect {
                    instance.selected = Some(k);
                }
                true
            }
            _ => {
                inner.remove(token);
                false
            }
        }
    }

    /// Makes one write of the claiming answer on instance `token`: only if
    /// the terminal still shows it exactly as the answer last saw it (no
    /// input, no output since), and conditional on the input generation it
    /// holds at (`write` gets it). Once it went in, the instance holds at
    /// the next one; Enter spends it.
    fn write(
        &self,
        token: &str,
        key: &Key<'_>,
        write: impl FnOnce(&InputCondition) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut inner = self.lock();
        let changed = || ChooseRefused::Changed.to_string();
        let instance = inner.get_mut(token).ok_or_else(changed)?;
        // Not while the last key's change is still to be seen.
        if instance.used != Use::Claimed
            || instance.expect.is_some()
            || !look(&instance.entry).is_some_and(|seen| instance.unchanged(&seen))
        {
            return Err(changed());
        }
        // Checked again with the enqueue: no other input, and the screen
        // still at the generation the answer last saw.
        write(&InputCondition {
            generation: Some(instance.input),
            output: Some(instance.output),
            composer: None,
            typed: None,
            unread_box_ok: false,
        })?;
        instance.input = instance.input.next();
        let at = instance.selected;
        match key {
            Key::Down => instance.expect = at.map(|s| Expect::Select(s + 1)),
            Key::Up => instance.expect = at.and_then(|s| s.checked_sub(1)).map(Expect::Select),
            Key::Paste(words) => instance.expect = Some(Expect::Field((*words).to_owned())),
            Key::Enter => instance.used = Use::Spent,
        }
        Ok(())
    }

    /// The answer is over: an instance it did not spend ends with it; a
    /// spent one stays (refusing any other answer) until its dialog goes.
    fn release(&self, token: &str) {
        let mut inner = self.lock();
        if inner.get(token).is_some_and(|i| i.used != Use::Spent) {
            inner.remove(token);
        }
    }
}

/// One terminal's question, as the phone reads it. `terminal` is the wire
/// id the answer names, with `occurrence`: the instance's token.
#[must_use]
pub fn question_json(
    entry: &TerminalEntry,
    wire: u32,
    q: &Question,
    occurrence: Option<&str>,
) -> Value {
    let (choices, selected) = q.choices(entry.tool);
    let kind = q.kind(entry.tool);
    // No instance to write on: shown, nothing by voice.
    let voice = occurrence.is_some();
    json!({
        "terminal": wire,
        "shown": entry.display_id(),
        "name": entry.name,
        "tool": entry.tool.label(),
        "appSession": is_app_session(entry),
        "id": q.id(entry.tool),
        "occurrence": occurrence,
        "kind": kind.name(),
        "text": q.text,
        "prompt": q.prompt(entry.tool),
        "voiceText": voice && kind == Kind::Text,
        "chat": false,
        "options": choices.iter().map(|c| json!({
            "index": c.index,
            "label": c.label,
            "tapOnly": !voice || c.tap_only,
        })).collect::<Vec<_>>(),
        "selected": selected,
    })
}

fn chat_json(entry: &TerminalEntry, wire: u32, task: &str, text: &str, index: u64) -> Value {
    json!({
        "terminal": wire,
        "shown": entry.display_id(),
        "name": entry.name,
        "tool": entry.tool.label(),
        "appSession": true,
        "id": chat_id(text),
        "occurrence": chat_occurrence(index),
        "kind": "text",
        "text": text,
        "prompt": text,
        // Words go by voice only where the app session's input line can be
        // read back exactly (Claude, Codex); elsewhere: on screen.
        "voiceText": matches!(
            entry.tool,
            connecto_core::ToolKind::Claude | connecto_core::ToolKind::Codex
        ),
        "chat": true,
        "task": task,
        "options": [],
        "selected": null,
    })
}

/// The questions `terminals` (each with its wire id, those the peer is
/// granted) ask now: `{"questions": [...]}`, each dialog with the token of
/// its instance (issued in `dialogs`). A terminal whose screen could not be
/// captured whole within a few tries is left out.
#[must_use]
pub fn questions(dialogs: &Arc<Dialogs>, terminals: &[(TerminalEntry, u32)]) -> Value {
    let v = questions_with(dialogs, terminals, &|| {
        std::thread::sleep(Duration::from_millis(10));
    });
    dialogs.watch();
    v
}

fn questions_with(dialogs: &Dialogs, terminals: &[(TerminalEntry, u32)], wait: &dyn Fn()) -> Value {
    dialogs.observe();
    let mut rows = Vec::new();
    for (t, wire) in terminals {
        let Some(seen) = (0..5).find_map(|i| {
            if i > 0 {
                wait();
            }
            look(t)
        }) else {
            continue;
        };
        match &seen.question {
            Some(q) => {
                let token = dialogs.issue(t, *wire, q, &seen);
                rows.push(question_json(t, *wire, q, token.as_deref()));
            }
            None if is_app_session(t) && t.tool.is_agent() => {
                if let Some(history) = t.session.chat_history() {
                    for (task, text, index) in pending_chat_questions_at(&history) {
                        rows.push(chat_json(t, *wire, &task, text, index));
                    }
                }
            }
            None => {}
        }
    }
    json!({ "questions": rows })
}

/// Words a voice may type: one line, as given (no spaces around it), no
/// control characters at all.
fn valid_words(text: &str) -> bool {
    !text.is_empty()
        && text.trim() == text
        && text.len() <= MAX_TEXT
        && !text.chars().any(char::is_control)
}

/// Answers instance `occurrence` (a token [`questions`] issued, or a chat
/// revision) of question `id` of the terminal with wire id `wire`, writing
/// through `type_into`. `terminals` gives the granted terminals as they are
/// now: each step reads that terminal anew (the same session, or nothing is
/// written). `{"outcome": "answered"}` once the question went away;
/// `{"outcome": "sent"}` when Enter was pressed but it still shows.
///
/// # Errors
/// Why nothing was answered, in words for the call. After words were typed
/// into a field and the question then changed, Enter is not pressed and the
/// error says the words are still typed there.
pub fn answer(
    dialogs: &Dialogs,
    terminals: Terminals<'_>,
    wire: u32,
    id: &str,
    occurrence: &str,
    choice: &AnswerChoice,
    type_into: Typer<'_>,
) -> Result<Value, String> {
    answer_with(
        dialogs,
        terminals,
        wire,
        id,
        occurrence,
        choice,
        type_into,
        &|| {
            std::thread::sleep(STEP);
        },
    )
}

const GONE: &str = "That terminal is no longer open, or not yours to answer.";

/// The terminal with wire id `wire` among `terminals` as they are now, on
/// `session` (the one first read), if it is still there.
fn now_on(
    terminals: Terminals<'_>,
    wire: u32,
    session: &Arc<dyn TerminalSession>,
) -> Option<TerminalEntry> {
    terminals()
        .into_iter()
        .find(|(t, w)| *w == wire && Arc::ptr_eq(&t.session, session))
        .map(|(t, _)| t)
}

#[allow(clippy::too_many_arguments)]
fn answer_with(
    dialogs: &Dialogs,
    terminals: Terminals<'_>,
    wire: u32,
    id: &str,
    occurrence: &str,
    choice: &AnswerChoice,
    type_into: Typer<'_>,
    wait: &dyn Fn(),
) -> Result<Value, String> {
    let first = terminals()
        .into_iter()
        .find(|(_, w)| *w == wire)
        .map(|(t, _)| t)
        .ok_or(GONE)?;
    let session = Arc::clone(&first.session);
    let now = || now_on(terminals, wire, &session);
    if id.starts_with("chat-") {
        let AnswerChoice::Text(text) = choice else {
            return Err("A task's question takes words.".into());
        };
        let write = |condition: &InputCondition, bytes: &[u8]| {
            let entry = now().ok_or(GONE)?;
            type_into(&entry, wire, condition, bytes.to_vec())
        };
        return answer_chat(&now, id, occurrence, text, &write, wait);
    }
    dialogs.observe();
    // One answer only, from its first key: claimed now, or refused.
    let token = occurrence;
    dialogs.claim(token, wire, id, &session)?;
    let result = answer_dialog(dialogs, token, &now, wire, id, choice, type_into, wait);
    dialogs.release(token);
    result
}

#[allow(clippy::too_many_arguments)]
fn answer_dialog(
    dialogs: &Dialogs,
    token: &str,
    now: &dyn Fn() -> Option<TerminalEntry>,
    wire: u32,
    id: &str,
    choice: &AnswerChoice,
    type_into: Typer<'_>,
    wait: &dyn Fn(),
) -> Result<Value, String> {
    let changed = || ChooseRefused::Changed.to_string();
    let tool = now().ok_or(GONE)?.tool;
    // Each write on the claimed instance, as the answer last saw it, at the
    // input generation it holds at. Enter spends it.
    let write = |key: Key<'_>, bytes: &[u8]| {
        dialogs.write(token, &key, |condition| {
            let entry = now().ok_or(GONE)?;
            type_into(&entry, wire, condition, bytes.to_vec())
        })
    };
    let key_write = |bytes: &[u8]| {
        let key = match bytes {
            b"\x1b[B" => Key::Down,
            b"\x1b[A" => Key::Up,
            b"\r" => Key::Enter,
            _ => return Err(changed()),
        };
        write(key, bytes)
    };
    // The question as it is now, read whole, on the terminal as it is now,
    // while the instance still holds.
    // What the answer reads is what the instance is now: its own keys'
    // redraws are seen, never skipped.
    let read = || {
        let entry = now()?;
        let seen = look_hard(&entry, wait)?;
        if !dialogs.saw(token, &seen) {
            return None;
        }
        let modes = seen.modes;
        seen.question
            .filter(|q| q.id(tool) == id)
            .map(|q| (q, modes))
    };
    match choice {
        AnswerChoice::Option(index) => question::choose(
            tool,
            id,
            *index,
            || read().map(|(q, _)| q),
            |bytes| key_write(bytes),
            wait,
            TRIES,
        )
        .map_err(|why| match why {
            ChooseRefused::Input(why) => format!("The answer could not be typed: {why}"),
            other => other.to_string(),
        })?,
        AnswerChoice::Text(text) => {
            if !valid_words(text) {
                return Err("Give one line of words, up to 500 bytes.".into());
            }
            let (q, modes) = read().ok_or_else(changed)?;
            let field = q
                .field(tool)
                .ok_or("That question does not take words by voice: answer it on screen.")?;
            if !field.value.is_empty() {
                return Err(
                    "Something is already typed in that question's field: answer it on screen."
                        .into(),
                );
            }
            write(Key::Paste(text), &connecto_core::encode_paste(text, modes))?;
            // The field must show exactly these words, on the same question.
            let mut shown = false;
            for _ in 0..TRIES {
                wait();
                match read() {
                    Some((q, _)) if q.field(tool).is_some_and(|f| f.value == *text) => {
                        shown = true;
                        break;
                    }
                    Some(_) => {}
                    None => {
                        return Err(
                            "The question changed while the words were typed: they are still in its field, and nothing was submitted."
                                .into(),
                        );
                    }
                }
            }
            if !shown {
                return Err(
                    "The field did not show exactly the words: nothing was submitted. Check it on screen."
                        .into(),
                );
            }
            write(Key::Enter, b"\r")?;
        }
    }
    let until = Instant::now() + GONE_WITHIN;
    loop {
        // Gone: the question changed, or it is answered.
        if now()
            .and_then(|e| look(&e))
            .is_some_and(|s| s.question.is_none_or(|q| q.id(tool) != id))
        {
            return Ok(json!({ "outcome": "answered" }));
        }
        if Instant::now() >= until {
            return Ok(json!({ "outcome": "sent" }));
        }
        wait();
    }
}

/// The app session as it is now, ready for a prompt: between turns, no
/// dialog on screen; its chat history, the modes its input takes, and the
/// input generation it was read at.
fn idle_app_session(
    now: &dyn Fn() -> Option<TerminalEntry>,
    wait: &dyn Fn(),
) -> Option<(Arc<ChatHistory>, InputModes, InputGeneration, u16, u64)> {
    let entry = now()?;
    if !is_app_session(&entry)
        || !entry.tool.is_agent()
        || !matches!(
            entry.state,
            TermActivity::Idle | TermActivity::Done | TermActivity::Interrupted
        )
    {
        return None;
    }
    let seen = look_hard(&entry, wait)?;
    if seen.question.is_some() {
        return None;
    }
    Some((
        entry.session.chat_history()?,
        seen.modes,
        seen.input?,
        seen.cols,
        seen.output,
    ))
}

/// Whether `text` can be read back exactly from the agent's input line once
/// pasted, on a screen `cols` wide: one line that fits on the line's first
/// row (with its glyph and margins). Anything longer is drawn over several
/// rows or collapsed into a placeholder, and what Enter would submit could
/// not be proven: it is not typed by voice at all.
fn readable_back(text: &str, cols: u16) -> bool {
    !text.contains('\n') && text.chars().count() + 6 <= usize::from(cols)
}

/// Why a text is not typed: it could not be read back.
const UNREADABLE: &str = "That is too long to check on the Mac's prompt before sending it: nothing was typed. Do it on screen.";

/// Words to a blocked task's question in the app session's chat: only
/// while the app session, as it is now, is between turns, shows no dialog,
/// and its chat still ends on that very message at the revision it was read
/// at. The paste goes in only on an input line known empty (as typed and as
/// drawn) and on the generation read; Enter only on the line holding exactly
/// these words and on the generation after the paste, the chat unchanged.
fn answer_chat(
    now: &dyn Fn() -> Option<TerminalEntry>,
    id: &str,
    occurrence: &str,
    text: &str,
    write: ConditionalWrite<'_>,
    wait: &dyn Fn(),
) -> Result<Value, String> {
    if !valid_words(text) {
        return Err("Give one line of words, up to 500 bytes.".into());
    }
    let changed = || ChooseRefused::Changed.to_string();
    // The question as the chat asks it now, with the screen's generation
    // at that look: each write goes in only on it.
    let asked = || {
        let (history, modes, input, cols, screen) = idle_app_session(now, wait)?;
        // That very asking, still waiting: turns about other tasks since
        // do not change it.
        let (task, ..) =
            pending_chat_questions_at(&history)
                .into_iter()
                .find(|(_, asked, index)| {
                    chat_id(asked) == id && chat_occurrence(*index) == occurrence
                })?;
        Some((history.revision, modes, input, cols, screen, task))
    };
    let (revision, modes, input, cols, screen, task) = asked().ok_or_else(changed)?;
    // Routed by name, as the phone read it back: the words must already
    // carry this task's lead, so what is typed is what was confirmed.
    if !text.starts_with(&format!("{} ", answer_lead(&task))) {
        return Err(format!(
            "An answer to that question begins with \"{}\": nothing was typed.",
            answer_lead(&task)
        ));
    }
    if !readable_back(text, cols) {
        return Err(UNREADABLE.into());
    }
    write(
        &InputCondition {
            generation: Some(input),
            composer: Some(String::new()),
            typed: None,
            output: Some(screen),
            unread_box_ok: false,
        },
        &connecto_core::encode_paste(text, modes),
    )
    .map_err(|why| format!("Nothing was typed: {why}"))?;
    // Enter only on a look that still finds that question, nothing else
    // typed since the paste, the line showing exactly the words, and the
    // screen held at that look through the enqueue. The paste's own redraw
    // may still be coming: looked at again while nothing else changed.
    let came_up = || {
        "The chat changed while the words were typed: they may still be in the prompt, and nothing was submitted."
            .to_owned()
    };
    let mut last = Err(came_up());
    for _ in 0..TRIES {
        wait();
        let (_, _, _, _, looked, _) = asked()
            .filter(|(now, _, at, ..)| *now == revision && *at == input.next())
            .ok_or_else(came_up)?;
        last = write(
            &InputCondition {
                generation: Some(input.next()),
                composer: Some(text.to_owned()),
                typed: None,
                output: Some(looked),
                unread_box_ok: false,
            },
            b"\r",
        );
        if last.is_ok() {
            break;
        }
    }
    last.map_err(|why| {
        format!(
            "The prompt changed: the words may still be in it, and nothing was submitted ({why})."
        )
    })?;
    submitted(now, revision, wait)
}

/// Waits a little for the chat to move on from `revision`.
fn submitted(
    now: &dyn Fn() -> Option<TerminalEntry>,
    revision: u64,
    wait: &dyn Fn(),
) -> Result<Value, String> {
    let until = Instant::now() + GONE_WITHIN;
    loop {
        if now()
            .and_then(|e| e.session.chat_history())
            .is_some_and(|h| h.revision != revision)
        {
            return Ok(json!({ "outcome": "answered" }));
        }
        if Instant::now() >= until {
            return Ok(json!({ "outcome": "sent" }));
        }
        wait();
    }
}

/// The longest prompt a call may send.
pub const MAX_PROMPT: usize = 60_000;

/// Sends a call's prompt (a task to start, a question to analyse) to the
/// app session with wire id `wire`. What Enter submits is known by
/// construction, not read back (a long prompt is drawn wrapped or collapsed
/// into a placeholder, which cannot be read back):
/// 1. The app session, as it is now, is between turns with no dialog and no
///    question on screen, and its input line is empty as typed and as
///    drawn, at input generation G and screen generation S.
/// 2. The paste goes in only at G and S: it moves the input to G+1.
/// 3. Enter goes in only at exactly G+1 (nobody else wrote since our own
///    paste: no key on the Mac, no other answer, no Connecto typing), with
///    the input line holding exactly the prompt as typed, and only after a
///    fresh look found it still between turns with no dialog and its chat
///    unmoved, the screen held at that look's generation through the
///    enqueue (the paste's own redraw is allowed; a dialog come since is
///    not).
///
/// The line was empty, only our paste went in: it holds exactly the
/// prompt. The one case left is the agent itself putting an old draft back
/// between the paste and Enter, with no dialog and no input (accepted, ADR
/// 0034). A question that came meanwhile never gets the prompt.
/// `{"outcome": "answered"}` once the chat moved on, `{"outcome": "sent"}`
/// when Enter went in but it has not yet.
///
/// # Errors
/// Why it was not sent, or not submitted.
pub fn send_prompt(
    terminals: Terminals<'_>,
    wire: u32,
    text: &str,
    type_into: Typer<'_>,
) -> Result<Value, String> {
    send_prompt_with(terminals, wire, text, type_into, &|| {
        std::thread::sleep(STEP)
    })
}

fn send_prompt_with(
    terminals: Terminals<'_>,
    wire: u32,
    text: &str,
    type_into: Typer<'_>,
    wait: &dyn Fn(),
) -> Result<Value, String> {
    if text.trim().is_empty()
        || text.len() > MAX_PROMPT
        || text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("Enter a message of up to 60,000 bytes.".into());
    }
    let first = terminals()
        .into_iter()
        .find(|(_, w)| *w == wire)
        .map(|(t, _)| t)
        .ok_or(GONE)?;
    let session = Arc::clone(&first.session);
    let now = || now_on(terminals, wire, &session);
    let busy = || {
        "The project's agent is busy, asks something, or has something typed: nothing was sent."
            .to_owned()
    };
    let (history, modes, input, _, screen) = idle_app_session(&now, wait).ok_or_else(busy)?;
    if text.contains('\n') && !modes.bracketed_paste {
        return Err("This prompt accepts one line at a time.".into());
    }
    let write = |condition: &InputCondition, bytes: Vec<u8>| {
        let entry = now().ok_or(GONE)?;
        type_into(&entry, wire, condition, bytes)
    };
    // 2. The paste, on the empty line, at G and S.
    write(
        &InputCondition {
            generation: Some(input),
            composer: Some(String::new()),
            typed: None,
            output: Some(screen),
            // Pi, OpenCode, Antigravity: their box is not read; the empty
            // line rests on the tracker and Enter on G+1 alone.
            unread_box_ok: true,
        },
        connecto_core::encode_paste(text, modes),
    )
    .map_err(|why| format!("Nothing was sent: {why}"))?;
    wait();
    // 3. Still between turns, no dialog or question come meanwhile, the
    // chat unmoved: and Enter only at G+1, on that screen.
    let came_up = || {
        "Something came up while the prompt was typed: it may still be in the agent's prompt, and nothing was submitted."
            .to_owned()
    };
    // The paste's own redraw may still be coming: looked at again while
    // nothing else changed (still G+1, no dialog, the chat unmoved).
    let mut last = Err(came_up());
    for _ in 0..TRIES {
        let (_, _, _, _, looked) = idle_app_session(&now, wait)
            .filter(|(h, _, at, ..)| h.revision == history.revision && *at == input.next())
            .ok_or_else(came_up)?;
        last = write(
            &InputCondition {
                generation: Some(input.next()),
                composer: None,
                typed: Some(text.to_owned()),
                output: Some(looked),
                unread_box_ok: false,
            },
            b"\r".to_vec(),
        );
        if last.is_ok() {
            break;
        }
        wait();
    }
    last.map_err(|why| {
        format!("The prompt changed: it may still be in the agent's prompt, and nothing was submitted ({why}).")
    })?;
    submitted(&now, history.revision, wait)
}

#[cfg(test)]
#[path = "voice_answers/tests.rs"]
mod tests;
