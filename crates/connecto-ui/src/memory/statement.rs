//! What the user states about the memory from the Connecto window: accept,
//! reject, correct, or add a note.
//!
//! Terminal capture can credit a prompt to the user only in rare, provably
//! clean cases, so most approvals typed into terminals are not recorded as
//! the user's. This window is the one place where the statement is certainly
//! the user's (an agent cannot click it), so these actions are how a proposal
//! becomes an accepted decision. The wording of each action follows the
//! memory's kind and standing, so its meaning is never in doubt: accepting a
//! plan approves it, it does not make it implemented.
//!
//! A draft is bound to what it is about. A draft about a memory or a note
//! remembers the exact subject and what it looked like (kind, standing,
//! summary or text) when the action was chosen; it can be sent only while
//! that memory is the one on show and still looks the same. When that stops
//! being so (Back, another memory, a room or a search, another tab, memory
//! turned off, or the memory changed under it after a refresh) the draft is
//! suspended: kept with its text, labelled with what it was about, not
//! sendable, and offered back ("Resume" reopens that memory; if it changed,
//! the action must be chosen again) or discarded. It is never retargeted. A
//! free note, about no memory, follows the user around, but is suspended too
//! when memory is turned off. Every button that sends is enabled only while
//! the statement passes its checks, and a press sends once.

use connecto_core::memory::{
    Annotation, AnnotationKind, AnnotationOrigin, Lookup, MAX_STATEMENT_BYTES, Memory,
    MemoryDetail, MemoryId, MemoryKind, ReadRequest, Room, Seen, Standing, Subject, UserStatement,
    UserStatementKind, Verification,
};
use egui::{Id, RichText, TextEdit, Ui};

use super::{MemoryAction, Slot, text};
use crate::forms::{Out, card, muted, problem, warning};
use crate::theme;

/// Said next to every set of actions.
pub(crate) const PROVENANCE: &str = "What you state here is recorded as yours. What agents or automations type into terminals is not.";

/// How a source is labelled when it is a statement made in this window.
pub(crate) const WINDOW_SOURCE: &str = "stated in the Connecto window";

/// The label of a note stated in this window.
pub(crate) const FROM_WINDOW: &str = "stated by you in the Connecto window";

/// How the actions on one memory or note are worded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Wording {
    pub(crate) accept: String,
    pub(crate) reject: String,
    pub(crate) correct: String,
    /// What accepting and rejecting mean here, in one line.
    pub(crate) meaning: String,
}

/// What a memory of this kind is called.
const fn noun(kind: MemoryKind, standing: Standing) -> &'static str {
    match (kind, standing) {
        (MemoryKind::Decision, _) => "decision",
        (MemoryKind::Convention, _) => "convention",
        (MemoryKind::Work, Standing::Planned) => "plan",
        (MemoryKind::Work, _) => "work",
        (MemoryKind::Fact, _) => "fact",
        (MemoryKind::Problem, _) => "problem",
        (MemoryKind::Procedure, _) => "procedure",
    }
}

/// The wording for a memory, from its kind and standing (the standings each
/// kind can have are [`Standing::allowed_for`]). Each line says exactly what
/// the engine does with the statement, no more:
///
/// | Kind | Accept | Reject |
/// | - | - | - |
/// | decision, convention | accepted, verified | discarded |
/// | fact, problem | established, verified | conflict, back to hypothesis, objection left open |
/// | work, procedure | standing unchanged, verified | conflict, back to planned / documented, objection left open |
///
/// Correct and Note are kept as open notes for the next dream.
pub(crate) fn for_memory(kind: MemoryKind, standing: Standing) -> Wording {
    let n = noun(kind, standing);
    let w = |accept: String, reject: String, meaning: String| Wording {
        accept,
        reject,
        correct: format!("Correct this {n}…"),
        meaning,
    };
    // Reject on anything but a decision or convention: what follows.
    let conflict = |weakest: &str| {
        format!(
            "\"This is wrong\" marks it in conflict, back to {weakest}, with your objection left open for the next dream."
        )
    };
    match standing {
        Standing::Proposed => w(
            format!("Accept this {n}"),
            format!("Reject this {n}"),
            format!(
                "It is only proposed. Accepting makes it an accepted {n}, verified on your word; rejecting marks it discarded."
            ),
        ),
        Standing::Accepted => w(
            format!("Confirm this {n}"),
            format!("Reject this {n}"),
            "It is accepted. Confirming keeps it accepted and marks it verified on your word; rejecting marks it discarded.".to_owned(),
        ),
        Standing::Discarded => w(
            format!("Accept this {n} after all"),
            "Confirm it stays rejected".to_owned(),
            format!(
                "This {n} was turned down. Accepting makes it an accepted {n}, verified on your word; confirming keeps it discarded."
            ),
        ),
        Standing::Planned => w(
            "Confirm this plan is right".to_owned(),
            "This plan is wrong".to_owned(),
            format!(
                "Confirming marks the plan verified on your word. It stays a plan: nothing marks the work started or implemented. {}",
                conflict("planned")
            ),
        ),
        Standing::InProgress | Standing::Implemented => w(
            "Confirm this description is right".to_owned(),
            "This is wrong".to_owned(),
            format!(
                "Confirming marks it verified on your word; it does not change whether the work is in progress or implemented. {}",
                conflict("planned")
            ),
        ),
        Standing::Hypothesis => w(
            "Confirm this is established".to_owned(),
            "This is wrong".to_owned(),
            format!(
                "It is a hypothesis. Confirming makes it an established {n}, verified on your word. {}",
                conflict("a hypothesis")
            ),
        ),
        Standing::Established => w(
            "Confirm this still holds".to_owned(),
            "This is wrong".to_owned(),
            format!(
                "It is established. Confirming keeps it so and marks it verified on your word. {}",
                conflict("a hypothesis")
            ),
        ),
        Standing::Documented | Standing::Exercised => w(
            "Confirm this procedure is right".to_owned(),
            "This procedure is wrong".to_owned(),
            format!(
                "Confirming marks it verified on your word; its standing ({}) does not change, and it does not mean a run was seen. {}",
                standing.label().to_lowercase(),
                conflict("documented")
            ),
        ),
    }
}

/// What Correct and Note do, said in their composer.
pub(crate) const KEPT_FOR_DREAM: &str =
    "Kept as an open note: the next dream considers it, it changes nothing at once.";

/// The wording for an open note: from what it would become when that is
/// known, else from what kind of note it is.
pub(crate) fn for_annotation(a: &Annotation) -> Wording {
    // Labels follow what the note claims; the explanation follows what the
    // engine does with a judgment on a note: it records it, nothing more.
    let (accept, reject, what) = match (a.memory_kind, a.standing) {
        (Some(kind), Some(standing)) if standing.applies_to(kind) => {
            let w = for_memory(kind, standing);
            (
                w.accept,
                w.reject,
                format!(
                    "This note proposes a {} {}.",
                    standing.label().to_lowercase(),
                    kind.label().to_lowercase()
                ),
            )
        }
        _ => {
            let (accept, reject, what) = match a.kind {
                AnnotationKind::Integration => (
                    "Accept this addition",
                    "Reject this addition",
                    "Accepting says the addition is right; rejecting says it is not.",
                ),
                AnnotationKind::Correction => (
                    "Accept this correction",
                    "Reject this correction",
                    "Accepting says the memory should be corrected this way; rejecting says it should not.",
                ),
                AnnotationKind::Contradiction => (
                    "The contradiction is right",
                    "The contradiction is wrong",
                    "Say which side is right: the contradiction, or the memory it contradicts.",
                ),
                AnnotationKind::Task => (
                    "Accept this task",
                    "Reject this task",
                    "Accepting approves the task; it does not mark it done.",
                ),
            };
            (accept.to_owned(), reject.to_owned(), what.to_owned())
        }
    };
    Wording {
        accept,
        reject,
        correct: "Correct this note…".to_owned(),
        meaning: format!("{what} {ON_A_NOTE}"),
    }
}

/// What the engine does with Accept or Reject on a note: no immediate
/// effect.
pub(crate) const ON_A_NOTE: &str =
    "Recorded as your judgment on this note; the next dream applies it. Nothing changes at once.";

/// Whether a note was stated by the user in this window.
pub(crate) fn from_window(a: &Annotation) -> bool {
    a.origin == AnnotationOrigin::User && a.verification == Verification::UserStated
}

/// What a subject looked like when an action on it was chosen, taken from
/// the answer drawn at that moment. `seen` is the contract's [`Seen`]: what
/// the statement is sent against, and what "changed" means, for the window
/// and the engine alike (it covers status and details too). The other fields
/// only word the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) seen: Seen,
    kind: Option<MemoryKind>,
    standing: Option<Standing>,
    note: Option<AnnotationKind>,
    /// The summary of a memory, the text of a note.
    pub(crate) text: String,
}

impl Snapshot {
    pub(crate) fn of_memory(m: &Memory) -> Self {
        Self {
            seen: m.seen(),
            kind: Some(m.kind),
            standing: Some(m.standing),
            note: None,
            text: m.summary.clone(),
        }
    }

    /// A note drawn under `page`: it is seen together with that memory when
    /// it is about it, alone when it is a note for a new memory.
    pub(crate) fn of_annotation(a: &Annotation, page: &Memory) -> Self {
        let target = a.target_memory().filter(|id| **id == page.id).map(|_| page);
        Self {
            seen: a.seen(target),
            kind: a.memory_kind,
            standing: a.standing,
            note: Some(a.kind),
            text: a.text.clone(),
        }
    }

    /// "proposed decision", "contradiction" ...
    pub(crate) fn describe(&self) -> String {
        match (self.note, self.kind, self.standing) {
            (Some(note), _, _) => note.label().to_lowercase(),
            (None, Some(kind), Some(standing)) => format!(
                "{} {}",
                standing.label().to_lowercase(),
                kind.label().to_lowercase()
            ),
            _ => "memory".to_owned(),
        }
    }
}

/// What a draft is about, and on which memory's page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bound {
    /// The memory on show when it was started.
    pub(crate) shown: MemoryId,
    /// The memory itself, or one of its open notes.
    pub(crate) subject: Subject,
    pub(crate) snapshot: Snapshot,
}

impl Bound {
    pub(crate) fn memory(m: &Memory) -> Self {
        Self {
            shown: m.id.clone(),
            subject: Subject::Memory(m.id.clone()),
            snapshot: Snapshot::of_memory(m),
        }
    }

    pub(crate) fn annotation(page: &Memory, a: &Annotation) -> Self {
        Self {
            shown: page.id.clone(),
            subject: Subject::Annotation(a.id.clone()),
            snapshot: Snapshot::of_annotation(a, page),
        }
    }

    /// The subject as `d` shows it now: `None` when `d` is another memory's
    /// page; `Some(None)` when the note is no longer open on it.
    fn now(&self, d: &MemoryDetail) -> Option<Option<Snapshot>> {
        if d.memory.id != self.shown {
            return None;
        }
        Some(match &self.subject {
            Subject::Memory(_) => Some(Snapshot::of_memory(&d.memory)),
            Subject::Annotation(id) => d
                .annotations
                .iter()
                .find(|a| a.id == *id && a.is_open())
                .map(|a| Snapshot::of_annotation(a, &d.memory)),
        })
    }
}

/// Where a draft stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Draft {
    /// On show under its subject, and sendable.
    Active,
    /// Kept, not sendable, offered back.
    Suspended {
        why: String,
        /// What the subject is now, when it changed under the draft.
        changed: Option<Snapshot>,
    },
    /// Its memory is being reopened.
    Resuming,
}

/// A statement being written.
#[derive(Debug, Clone)]
pub(crate) struct Composer {
    pub(crate) kind: UserStatementKind,
    /// What it is about; `None` for a free note.
    pub(crate) bound: Option<Bound>,
    pub(crate) room: Option<Room>,
    pub(crate) text: String,
    /// What the send button says (the action's wording).
    pub(crate) title: String,
    /// A rejection waits for a second yes.
    pub(crate) confirming: bool,
    /// Why the engine refused it. The text stays.
    pub(crate) error: Option<String>,
    /// The engine accepted it for processing with this id; the draft stays,
    /// not editable, until that statement's result is final in the view.
    pub(crate) pending: Option<u64>,
    /// Pressed, not yet answered: a second press does nothing.
    pub(crate) sending: bool,
    pub(crate) draft: Draft,
}

impl Composer {
    pub(crate) fn new(kind: UserStatementKind, bound: Option<Bound>, title: &str) -> Self {
        Self {
            kind,
            bound,
            room: None,
            text: String::new(),
            title: title.to_owned(),
            confirming: false,
            error: None,
            sending: false,
            pending: None,
            draft: Draft::Active,
        }
    }

    pub(crate) fn about(&self) -> Option<&Subject> {
        self.bound.as_ref().map(|b| &b.subject)
    }

    pub(crate) fn is_active(&self) -> bool {
        self.draft == Draft::Active
    }

    pub(crate) fn statement(&self) -> UserStatement {
        UserStatement {
            kind: self.kind,
            about: self.about().cloned(),
            // What the user saw when choosing the action, never recomputed.
            seen: self.bound.as_ref().map(|b| b.snapshot.seen.clone()),
            room: if self.bound.is_some() {
                None
            } else {
                self.room
            },
            text: if self.text.trim().is_empty() && !self.needs_text() {
                String::new()
            } else {
                self.text.clone()
            },
        }
    }

    const fn needs_text(&self) -> bool {
        matches!(
            self.kind,
            UserStatementKind::Correct | UserStatementKind::Note
        )
    }

    /// Whether it is drawn under this subject (a note is drawn by the "Add a
    /// note" button, whatever it is about).
    pub(crate) fn under(&self, subject: &Subject) -> bool {
        self.kind != UserStatementKind::Note && self.is_active() && self.about() == Some(subject)
    }

    pub(crate) fn suspend(&mut self, why: &str) {
        if self.draft != Draft::Active && self.draft != Draft::Resuming {
            return;
        }
        self.draft = Draft::Suspended {
            why: why.to_owned(),
            changed: None,
        };
        self.confirming = false;
    }
}

/// Hold a draft to what is on show, once per frame on the Memories tab: a
/// bound draft is active only while its memory is on show unchanged.
pub(crate) fn settle(composer: &mut Option<Composer>, on_show: Option<&MemoryDetail>) {
    let Some(c) = composer else {
        return;
    };
    let Some(bound) = c.bound.clone() else {
        return;
    };
    let now = on_show.and_then(|d| bound.now(d));
    match (&c.draft, now) {
        (Draft::Suspended { .. }, _) | (Draft::Resuming, None) => {}
        (Draft::Active | Draft::Resuming, Some(Some(s))) if s == bound.snapshot => {
            c.draft = Draft::Active;
        }
        (Draft::Active, None) => c.suspend("What it is about is no longer on show."),
        (Draft::Active | Draft::Resuming, Some(None)) => {
            c.draft = Draft::Suspended {
                why: "The note it is about is no longer open.".to_owned(),
                changed: None,
            };
            c.confirming = false;
        }
        (Draft::Active | Draft::Resuming, Some(Some(s))) => {
            c.draft = Draft::Suspended {
                why: "What it is about changed since you started.".to_owned(),
                changed: Some(s),
            };
            c.confirming = false;
        }
    }
}

/// A short key for a subject.
pub(crate) fn key(subject: &Subject) -> String {
    subject.to_string()
}

/// Whether a new draft may be started for `subject` (`None`: a free note)
/// without dropping text typed elsewhere.
/// Never while a statement waits for its result ([`locked`]).
pub(crate) fn may_start(composer: &Option<Composer>, subject: Option<&Subject>) -> bool {
    !locked(composer)
        && composer
            .as_ref()
            .is_none_or(|c| c.text.trim().is_empty() || (subject.is_some() && c.about() == subject))
}

/// The one guard on the draft slot. While a draft holds a pending statement
/// id, nothing may create, replace, renew, resume, discard or cancel a draft,
/// nor send again: every button that would is drawn disabled. Writers that
/// remain allowed: suspension by navigation, tab or memory turned off (state
/// only, the id is kept), the application of the result, and the fallback
/// when no result comes ([`super::follow`]). A project switch resets the
/// panel; it only happens defensively, a real switch opens a new sheet.
pub(crate) fn locked(composer: &Option<Composer>) -> bool {
    composer.as_ref().is_some_and(|c| c.pending.is_some())
}

/// Said by every button the guard turns off.
pub(crate) const WAITING: &str = "Waiting for your last statement to be recorded…";

const BUSY: &str = "Send or discard your draft first.";

/// Why a draft-starting button is off.
pub(crate) fn why_not(composer: &Option<Composer>) -> &'static str {
    if locked(composer) { WAITING } else { BUSY }
}

/// The action buttons for a memory or note, and the line that says what they
/// mean. Opens the composer, carrying text already typed about the same
/// subject.
pub(crate) fn actions(
    ui: &mut Ui,
    composer: &mut Option<Composer>,
    bound: &Bound,
    wording: &Wording,
    verdicts: bool,
    out: &mut Out<MemoryAction>,
) {
    let k = key(&bound.subject);
    let free = may_start(composer, Some(&bound.subject));
    ui.horizontal_wrapped(|ui| {
        for (kind, label, name) in [
            (UserStatementKind::Accept, &wording.accept, "accept"),
            (UserStatementKind::Reject, &wording.reject, "reject"),
            (UserStatementKind::Correct, &wording.correct, "correct"),
        ] {
            if !verdicts && kind != UserStatementKind::Correct {
                continue;
            }
            let text = if kind == UserStatementKind::Reject {
                format!("{label}…")
            } else {
                label.clone()
            };
            let key = format!("mem-say-{name}:{k}");
            if out.button_hint(ui, &key, &text, free, why_not(composer)) && free {
                let title = label.trim_end_matches('…');
                let kept = composer
                    .take()
                    .filter(|c| c.about() == Some(&bound.subject))
                    .map(|c| c.text)
                    .unwrap_or_default();
                let mut c = Composer::new(kind, Some(bound.clone()), title);
                c.text = kept;
                *composer = Some(c);
            }
        }
    });
    if verdicts {
        out.say(ui, muted(wording.meaning.clone()));
    }
}

/// The composer, with its checks, counter and confirmation. `on_show`: the
/// memory on show, which a note may be about.
pub(crate) fn compose(
    ui: &mut Ui,
    composer: &mut Option<Composer>,
    on_show: Option<&MemoryDetail>,
    out: &mut Out<MemoryAction>,
) {
    let Some(c) = composer else {
        return;
    };
    if !c.is_active() {
        return;
    }
    if c.pending.is_some() {
        sending(ui, c, out);
        return;
    }
    let mut close = false;
    card(ui, |ui| {
        out.say(
            ui,
            RichText::new(c.title.clone()).strong().color(theme::TEXT),
        );
        if c.kind == UserStatementKind::Note {
            note_target(ui, c, on_show, out);
        }
        let label = match c.kind {
            UserStatementKind::Accept | UserStatementKind::Reject => "Reason (optional)",
            UserStatementKind::Correct => "What is right instead",
            UserStatementKind::Note => "Your note",
        };
        out.say(ui, RichText::new(label).size(12.0).color(theme::TEXT_ROW));
        if c.needs_text() {
            out.say(ui, muted(KEPT_FOR_DREAM));
        }
        let response = ui.add(
            TextEdit::multiline(&mut c.text)
                .id(Id::unique(("memory-statement", c.kind.word())))
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .margin(theme::FIELD_MARGIN),
        );
        out.mark("mem-say-text", response.rect);
        counter(ui, &c.text, out);
        let statement = c.statement();
        let check = statement.check();
        if let (Err(why), false) = (&check, c.text.is_empty() && c.needs_text()) {
            out.say(ui, problem(format!("{why}.")));
        }
        if let Some(why) = &c.error {
            out.say(ui, problem(format!("Not recorded: {why}")));
        }
        out.say(ui, muted(PROVENANCE));
        let ok = check.is_ok() && !c.sending;
        let why = match &check {
            Err(why) => format!("{why}."),
            Ok(()) => String::new(),
        };
        if c.confirming {
            out.say(
                ui,
                warning(format!("Record, as your statement: \"{}\"?", c.title)),
            );
            ui.horizontal_wrapped(|ui| {
                let response = ui.add_enabled(ok, theme::destructive(&format!("Yes: {}", c.title)));
                let response = if ok {
                    response
                } else {
                    response.on_disabled_hover_text(&why)
                };
                out.mark("mem-say-confirm", response.rect);
                if response.clicked() && ok {
                    c.sending = true;
                    out.actions.push(MemoryAction::Statement(statement.clone()));
                }
                if out.button(ui, "mem-say-back", "Back", true) {
                    c.confirming = false;
                }
            });
            return;
        }
        ui.horizontal_wrapped(|ui| {
            let send = if c.kind == UserStatementKind::Reject {
                format!("{}…", c.title)
            } else {
                c.title.clone()
            };
            if out.button_hint(ui, "mem-say-send", &send, ok, &why) && ok {
                if c.kind == UserStatementKind::Reject {
                    c.confirming = true;
                } else {
                    c.sending = true;
                    out.actions.push(MemoryAction::Statement(statement.clone()));
                }
            }
            if out.button(ui, "mem-say-cancel", "Cancel", true) && c.pending.is_none() {
                close = true;
            }
        });
    });
    if close {
        *composer = None;
    }
}

/// A draft sent and waiting for its result: shown, not editable.
fn sending(ui: &mut Ui, c: &Composer, out: &mut Out<MemoryAction>) {
    card(ui, |ui| {
        out.say(
            ui,
            RichText::new(c.title.clone()).strong().color(theme::TEXT),
        );
        if !c.text.is_empty() {
            out.say(
                ui,
                RichText::new(c.text.clone())
                    .size(12.0)
                    .color(theme::TEXT_ROW),
            );
        }
        out.say(ui, muted("Sending…"));
    });
}

/// A draft that is not on show: what it was about, why it waits, and Resume
/// or Discard. Nothing here sends it.
pub(crate) fn suspended(ui: &mut Ui, composer: &mut Option<Composer>, out: &mut Out<MemoryAction>) {
    let Some(c) = composer else {
        return;
    };
    if c.is_active() {
        return;
    }
    let mut discard = false;
    card(ui, |ui| {
        let what = match &c.bound {
            Some(b) => format!(
                "Draft about {}: \"{}\" ({}, as it was)",
                b.subject,
                b.snapshot.text,
                b.snapshot.describe()
            ),
            None => "Draft note".to_owned(),
        };
        out.say(ui, RichText::new(what).strong().color(theme::TEXT));
        out.say(ui, muted(format!("{} · not sent", c.title)));
        match &c.draft {
            Draft::Suspended { why, changed } => {
                out.say(ui, warning(why.clone()));
                if let Some(now) = changed {
                    let again = if c.kind == UserStatementKind::Note {
                        "Press \"Add a note…\" again to write it about the memory as it is now"
                    } else {
                        "Choose the action again under it"
                    };
                    out.say(
                        ui,
                        warning(format!(
                            "Now: {}, \"{}\". {again}; your text is kept.",
                            now.describe(),
                            now.text
                        )),
                    );
                }
            }
            Draft::Resuming => out.say(ui, muted("Reopening…")),
            Draft::Active => {}
        }
        if c.pending.is_some() {
            out.say(ui, muted("Sent: waiting for the answer…"));
        }
        if let Some(why) = &c.error {
            out.say(ui, problem(why.clone()));
        }
        if !c.text.is_empty() {
            let preview: String = c.text.chars().take(300).collect();
            out.say(ui, muted(format!("Your text: {preview}")));
        }
        let waiting = c.pending.is_some();
        if waiting {
            out.say(ui, muted(WAITING));
        }
        ui.horizontal_wrapped(|ui| {
            let resumable = matches!(c.draft, Draft::Suspended { .. });
            if resumable
                && out.button_hint(ui, "mem-draft-resume", "Resume", !waiting, WAITING)
                && !waiting
            {
                match &c.bound {
                    Some(b) => {
                        c.draft = Draft::Resuming;
                        out.actions.push(MemoryAction::Read(
                            ReadRequest::Open(Lookup::Memory(b.shown.clone())),
                            Slot::Detail,
                        ));
                    }
                    None => c.draft = Draft::Active,
                }
            }
            if out.button_hint(ui, "mem-draft-discard", "Discard", !waiting, WAITING) && !waiting {
                discard = true;
            }
        });
    });
    if discard {
        *composer = None;
    }
}

/// What a note is about: the memory on show (explicitly ticked), or a room,
/// or neither.
fn note_target(
    ui: &mut Ui,
    c: &mut Composer,
    on_show: Option<&MemoryDetail>,
    out: &mut Out<MemoryAction>,
) {
    let target = c
        .bound
        .as_ref()
        .map(|b| b.shown.clone())
        .or_else(|| on_show.map(|d| d.memory.id.clone()));
    if let Some(memory) = target {
        let mut about = c.bound.is_some();
        let response = ui.checkbox(&mut about, format!("About {memory}"));
        out.mark("mem-say-about", response.rect);
        if response.changed() {
            c.bound = if about {
                on_show
                    .filter(|d| d.memory.id == memory)
                    .map(|d| Bound::memory(&d.memory))
            } else {
                None
            };
        }
    }
    if c.bound.is_some() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        out.say(ui, RichText::new("Room").color(theme::TEXT));
        if out.choice(ui, "mem-say-room:any", c.room.is_none(), "None") {
            c.room = None;
        }
        for room in Room::ALL.iter().filter(|r| r.holds_memories()) {
            let key = format!("mem-say-room:{}", room.word());
            if out.choice(ui, &key, c.room == Some(*room), room.label()) {
                c.room = Some(*room);
            }
        }
    });
}

/// Bytes used, shown from 80% of the engine's limit for a statement
/// ([`MAX_STATEMENT_BYTES`], below the note limit); over it, by how much.
fn counter(ui: &mut Ui, typed: &str, out: &mut Out<MemoryAction>) {
    let used = typed.len();
    if used * 5 < MAX_STATEMENT_BYTES * 4 {
        return;
    }
    let line = format!(
        "{} of {} bytes",
        text::count(used as u64),
        text::count(MAX_STATEMENT_BYTES as u64)
    );
    if used > MAX_STATEMENT_BYTES {
        out.say(
            ui,
            problem(format!(
                "{line}: {} too many. Shorten it; it is never cut.",
                text::count((used - MAX_STATEMENT_BYTES) as u64)
            )),
        );
    } else {
        out.say(ui, warning(line));
    }
}
