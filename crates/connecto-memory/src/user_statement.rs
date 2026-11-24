//! What the user states in the Connecto window itself (spec §4.5, §5.10):
//! accept, reject, correct, or a note. Its provenance is certain, unlike a
//! terminal prompt credited to the user (§5.3): agents can type into
//! terminals but cannot click the app, [`connecto_core::memory::MemoryControl`]
//! is called only by the UI, and no `connecto memory` verb leads here.
//!
//! One statement is **one writer transaction** ([`Store::user_statement`]):
//!
//! - first, the subject is loaded by its exact id (no redirect or merge
//!   followed) and must still be what the user saw
//!   ([`connecto_core::memory::Seen`]): live, open for a note, in the same
//!   version, with the same content fingerprint; otherwise the statement is
//!   refused with "changed since you saw it: review it and confirm again"
//!   and nothing is written. A version that differs refuses even when the
//!   content is the same again (a revert that restores it): the user saw an
//!   earlier state, and confirming again is cheap. The same statement made
//!   twice while its note is open is refused too ([`StoreError::AlreadyStated`]);
//! - a `dichiarazione_utente` evidence with the window's provenance
//!   ([`is_window_statement`]): no terminal, no tool session, no log, the turn
//!   [`WINDOW_TURN`], `exposed_to` empty (it is the user's own act); its
//!   excerpt is a fixed sentence naming the subject, the subject's summary
//!   (or note text) as seen, quoted, redacted and bounded, and the user's
//!   redacted words;
//! - an annotation with origin `utente` and verification
//!   `dichiarata_dall_utente`, sourced on that evidence, with the same text;
//! - for **Accept** and **Reject** about a memory, the effect, applied at once
//!   through the dream's final judgement ([`operations::recheck`] →
//!   [`Final::publication`](crate::operations::Final::publication) → the
//!   store's publication, in a savepoint of the same transaction) as one new
//!   memory version whose cycle is `dream` and run is none. If the judgement
//!   or the store does not allow it, nothing of it is applied and the
//!   annotation stays open for the next dream ([`Applied::NotAllowed`]).
//!
//! | Statement | decision, convention | fact, problem | work, procedure |
//! | - | - | - | - |
//! | Accept a memory | `accettata`, `verificato` | `accertato`, `verificato` | standing kept, `verificato` |
//! | Reject a memory | `scartata` | `conflitto`, standing lowered to the weakest, open `contraddizione` | `conflitto`, standing lowered to the weakest, open `contraddizione` |
//! | Correct a memory | open `correzione` | same | same |
//! | Note | open `integrazione` | same | same |
//!
//! Accepting a plan or a procedure confirms it as it stands: the user did not
//! say the work is done or the procedure was run, so no standing is raised.
//! An Accept or Reject about a memory that is `superato` or lives in
//! `archivio` is refused (act on the one that replaced it); a Correct or a
//! Note about it is recorded. An Accept or Reject that is applied
//! absorbs its annotation into no memory (the version's operations are its
//! effect and its evidence becomes a source of the memory, so `read` shows it
//! under the memory as an absorbed note and a source); a Reject of a fact,
//! problem, work or procedure leaves its `contraddizione` open, shown under
//! the memory until a dream resolves it. Absorbing into no memory keeps the
//! statement out of the memory's lineage: a user statement is never the
//! self-confirmation the lineage rule guards against.
//!
//! About an open annotation, the statement is a new annotation on the same
//! target, its text naming the annotation; nothing is applied at once (the
//! dream consolidates text into memories). Accept of a note for a new memory
//! carries the standing the user backs (a decision `accettata`, a fact
//! `accertato`); Reject of a proposed decision or convention carries
//! `scartata`. Correct and Note only add the open annotation.
//!
//! No event is queued: the development cycle never sees the statement, so it
//! is never evidenced twice. It is allowed during a pause (it is the user's
//! act, not a model run) and starts no run.

use connecto_core::Redacted;
use connecto_core::memory::{
    Annotation, AnnotationId, AnnotationKind, AnnotationOrigin, AnnotationTarget, Cycle, Evidence,
    EvidenceId, EvidenceKind, Memory, MemoryError, MemoryId, MemoryKind, MemoryStatus, Room, Scope,
    Seen, Standing, StatementRefusal, StatementState, Subject, UserStatement, UserStatementKind,
    Verification, check_note_text,
};

use crate::error::{Result, StoreError};
use crate::operations::{self, Staged};
use crate::publish::{Absorb, MemoryOp, MemoryUpdate, publish_in};
use crate::read::Read;
use crate::store::{NewAnnotation, NewEvidence, Store, create_annotation_in, insert_evidence_in};

/// The `turn` of evidence made in the Connecto window. A live capture always
/// names its terminal and the initial build's turns are `<conversation>#<n>`,
/// so no other evidence carries it with no terminal and no session.
pub const WINDOW_TURN: &str = "connecto-window";

/// Room the fixed sentence takes in front of the user's words, in bytes: a
/// text longer than [`connecto_core::memory::MAX_STATEMENT_BYTES`] is
/// refused by [`UserStatement::check`]. One constant, so the two cannot drift.
pub const SENTENCE_ROOM: usize = connecto_core::memory::STATEMENT_SENTENCE_BYTES;

/// Longest quote of the subject in the evidence's excerpt, in characters.
pub const QUOTE_CHARS: usize = 400;

/// Whether `e` is a statement the user made in the Connecto window (not a
/// terminal prompt credited to the user).
#[must_use]
pub fn is_window_statement(e: &Evidence) -> bool {
    e.kind == EvidenceKind::UserStatement
        && e.terminal.is_none()
        && e.tool_session.is_none()
        && e.log.is_none()
        && e.turn.as_deref() == Some(WINDOW_TURN)
}

/// The checks done at once, on the calling thread: the contract's rules, and
/// room for the fixed sentence.
pub fn check(statement: &UserStatement) -> std::result::Result<(), String> {
    statement.check().map_err(|e| match e {
        MemoryError::TextTooLong => format!(
            "the text is too long (at most {} bytes)",
            connecto_core::memory::MAX_STATEMENT_BYTES
        ),
        other => other.to_string(),
    })
}

/// Whether the statement may change a memory at once, so it needs the
/// project's publication gate.
#[must_use]
pub fn publishes(statement: &UserStatement) -> bool {
    matches!(
        statement.kind,
        UserStatementKind::Accept | UserStatementKind::Reject
    ) && matches!(statement.about, Some(Subject::Memory(_)))
}

/// A statement waiting for the project's control thread, with the id its
/// result carries in the view. The gate is taken there, never by the UI.
pub(crate) struct Job {
    pub id: u64,
    pub statement: UserStatement,
}

/// A statement with its words redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stated {
    pub kind: UserStatementKind,
    pub about: Option<Subject>,
    pub room: Option<Room>,
    /// Empty when the user gave none (Accept, Reject).
    pub text: Redacted<String>,
    /// What was on screen; required with `about`.
    pub seen: Option<Seen>,
}

impl Stated {
    /// `statement`'s words through redaction (the patterns and `secrets`).
    pub fn redact(
        statement: &UserStatement,
        secrets: &[String],
    ) -> std::result::Result<Self, String> {
        let text = if statement.text.trim().is_empty() {
            Redacted::assume(String::new())
        } else {
            connecto_redact::redact(&statement.text, secrets)
                .map_err(|_| "the text could not be checked for secrets".to_owned())?
        };
        Ok(Self {
            kind: statement.kind,
            about: statement.about.clone(),
            room: statement.room,
            text,
            seen: statement.seen.clone(),
        })
    }
}

/// What happened to the statement's effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Nothing to apply at once: the annotation is open for the next dream.
    Open,
    /// Applied at once in this memory version.
    Published(u64),
    /// The final judgement or the store did not allow it now: nothing of it
    /// was applied and the annotation is open for the next dream. Why.
    NotAllowed(String),
}

/// What [`Store::user_statement`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub evidence: EvidenceId,
    pub annotation: Annotation,
    pub applied: Applied,
}

/// What the statement is about, as found in the store.
enum Target {
    Memory(Memory),
    Note(Annotation),
    Room(Room),
}

/// The standing the user's acceptance backs for a memory of `kind` that
/// stands at `current`: never more than a user statement supports, and
/// never a claim the user did not make (done, run).
const fn accepted(kind: MemoryKind, current: Standing) -> Standing {
    match kind {
        MemoryKind::Decision | MemoryKind::Convention => Standing::Accepted,
        MemoryKind::Fact | MemoryKind::Problem => Standing::Established,
        MemoryKind::Work | MemoryKind::Procedure => current,
    }
}

/// What the annotation is, and what it changes at once.
struct Plan {
    kind: AnnotationKind,
    target: AnnotationTarget,
    scopes: Vec<Scope>,
    memory_kind: Option<MemoryKind>,
    standing: Option<Standing>,
    sentence: String,
    /// The memory changed at once, with its new status and standing.
    effect: Option<Effect>,
}

struct Effect {
    memory: MemoryId,
    status: Option<MemoryStatus>,
    standing: Option<Standing>,
    /// The annotation is absorbed (into no memory) with the change.
    absorb: bool,
}

/// The subject by its exact id (no merge followed), refused unless it is
/// still what the user saw: live, open, the same version and content.
fn resolve(
    read: &Read,
    about: Option<&Subject>,
    seen: Option<&Seen>,
    room: Option<Room>,
) -> Result<Target> {
    let Some(about) = about else {
        return Ok(Target::Room(room.unwrap_or(Room::Decisions)));
    };
    let seen = seen.ok_or(MemoryError::SeenRequired)?;
    if &seen.subject != about {
        return Err(MemoryError::SeenMismatch.into());
    }
    let changed = || StoreError::Changed(about.clone());
    match about {
        Subject::Memory(id) => {
            let Some(m) = read.memory(id)? else {
                // Merged away or withdrawn since: not what was seen.
                return Err(match read.memory_row(id)? {
                    Some(_) => changed(),
                    None => StoreError::UnknownMemory(id.clone()),
                });
            };
            if &m.seen() != seen {
                return Err(changed());
            }
            Ok(Target::Memory(m))
        }
        Subject::Annotation(id) => {
            let note = read
                .annotation(id)?
                .ok_or_else(|| StoreError::UnknownAnnotation(id.clone()))?;
            if !note.is_open() {
                return Err(changed());
            }
            let target = match note.target_memory() {
                Some(t) => Some(read.memory(t)?.ok_or_else(changed)?),
                None => None,
            };
            if &note.seen(target.as_ref()) != seen {
                return Err(changed());
            }
            Ok(Target::Note(note))
        }
    }
}

/// What the statement was about, as the user read it: one line, redacted,
/// at most [`QUOTE_CHARS`] characters. `None` for a free note.
fn quote(target: &Target, secrets: &[String]) -> Option<String> {
    let text = match target {
        Target::Memory(m) => &m.summary,
        Target::Note(a) => &a.text,
        Target::Room(_) => return None,
    };
    let line = crate::render::clean(text).trim().to_owned();
    let line = crate::ingest::redact(&line, secrets)
        .unwrap_or_else(|| "(text withheld: redaction could not run)".to_owned());
    if line.chars().count() > QUOTE_CHARS {
        let mut cut: String = line.chars().take(QUOTE_CHARS).collect();
        cut.push_str("...");
        Some(cut)
    } else {
        Some(line)
    }
}

/// An open statement of the user with the same target, kind and text.
fn already_stated(read: &Read, plan: &Plan, body: &str) -> Result<Option<AnnotationId>> {
    Ok(read
        .open_annotations()?
        .into_iter()
        .find(|a| {
            a.origin == AnnotationOrigin::User
                && a.target == plan.target
                && a.kind == plan.kind
                && a.text == body
        })
        .map(|a| a.id))
}

fn plan(kind: UserStatementKind, target: Target) -> Plan {
    use UserStatementKind as K;
    match target {
        Target::Memory(m) => {
            let scopes = m.scopes.iter().map(|s| s.scope.clone()).collect();
            let weakest = Standing::default_for(m.kind);
            let decision = matches!(m.kind, MemoryKind::Decision | MemoryKind::Convention);
            let base = |kind, standing, sentence: &str, effect| Plan {
                kind,
                target: AnnotationTarget::Memory(m.id.clone()),
                scopes,
                memory_kind: Some(m.kind),
                standing,
                sentence: format!("The user {sentence} {} in the Connecto window", m.id),
                effect,
            };
            match kind {
                K::Accept => {
                    let standing = accepted(m.kind, m.standing);
                    base(
                        AnnotationKind::Integration,
                        Some(standing),
                        "accepted",
                        Some(Effect {
                            memory: m.id.clone(),
                            status: Some(MemoryStatus::Verified),
                            standing: (standing != m.standing).then_some(standing),
                            absorb: true,
                        }),
                    )
                }
                K::Reject if decision => base(
                    AnnotationKind::Integration,
                    Some(Standing::Discarded),
                    "rejected",
                    Some(Effect {
                        memory: m.id.clone(),
                        status: None,
                        standing: (m.standing != Standing::Discarded)
                            .then_some(Standing::Discarded),
                        absorb: true,
                    }),
                ),
                K::Reject => base(
                    AnnotationKind::Contradiction,
                    None,
                    "rejected",
                    Some(Effect {
                        memory: m.id.clone(),
                        status: Some(MemoryStatus::Conflict),
                        standing: (m.standing != weakest).then_some(weakest),
                        absorb: false,
                    }),
                ),
                K::Correct => base(AnnotationKind::Correction, None, "corrected", None),
                K::Note => Plan {
                    sentence: format!(
                        "The user's note about {}, written in the Connecto window",
                        m.id
                    ),
                    ..base(AnnotationKind::Integration, None, "noted", None)
                },
            }
        }
        Target::Note(a) => {
            // On a note about a memory a standing would read as the memory's:
            // the text says what the user did; only a note for a new memory
            // carries the standing the user backs.
            let new = matches!(a.target, AnnotationTarget::New { .. });
            let standing = match kind {
                K::Accept if new => a
                    .memory_kind
                    .map(|k| accepted(k, a.standing.unwrap_or_else(|| Standing::default_for(k)))),
                K::Reject if new => a
                    .memory_kind
                    .filter(|k| matches!(k, MemoryKind::Decision | MemoryKind::Convention))
                    .map(|_| Standing::Discarded),
                _ => None,
            };
            let (note_kind, memory_kind) = match kind {
                K::Correct if !new => (AnnotationKind::Correction, None),
                K::Correct => (AnnotationKind::Integration, None),
                _ => (AnnotationKind::Integration, a.memory_kind),
            };
            let verb = match kind {
                K::Accept => "The user accepted note",
                K::Reject => "The user rejected note",
                K::Correct => "The user corrected note",
                K::Note => "The user's note about note",
            };
            Plan {
                kind: note_kind,
                target: a.target.clone(),
                scopes: a.scopes.clone(),
                memory_kind,
                standing: standing.filter(|_| memory_kind.is_some()),
                sentence: format!("{verb} {}, in the Connecto window", a.id),
                effect: None,
            }
        }
        Target::Room(room) => Plan {
            kind: AnnotationKind::Integration,
            target: AnnotationTarget::New { room },
            scopes: vec![Scope::Project],
            memory_kind: None,
            standing: None,
            sentence: "The user's note, written in the Connecto window".to_owned(),
            effect: None,
        },
    }
}

/// The staged items of an effect, in the order the judgement accepts them:
/// a status first (its evidence must be new to the memory to confirm it),
/// then the standing (which counts the evidence now among the sources).
fn staged(effect: &Effect, evidence: EvidenceId, annotation: &AnnotationId) -> Vec<Staged> {
    let mut out = Vec::new();
    if let Some(status) = effect.status {
        out.push(Staged::Op(MemoryOp::SetStatus {
            id: effect.memory.clone(),
            status,
            sources: vec![evidence],
        }));
    }
    if effect.standing.is_some() || effect.status.is_none() {
        let mut u = MemoryUpdate::new(effect.memory.clone());
        u.standing = effect.standing;
        if effect.status.is_none() {
            u.add_sources = vec![evidence];
        }
        out.push(Staged::Op(MemoryOp::Update(u)));
    }
    if effect.absorb {
        out.push(Staged::Absorb(Absorb {
            annotation: annotation.clone(),
            into: None,
        }));
    }
    out
}

impl Store {
    /// Record a statement the user made in the Connecto window, in one
    /// transaction (see the module): its evidence, its annotation, and its
    /// effect when it has one and the final judgement allows it. A target
    /// that is not a live memory or an open annotation of this project is
    /// refused and nothing is written.
    pub fn user_statement(
        &mut self,
        stated: &Stated,
        now_ms: u64,
        known_secrets: &[String],
    ) -> Result<Recorded> {
        let project_id = self.project_id().to_owned();
        let mut tx = self.begin()?;
        let target = resolve(
            &Read::new(&tx),
            stated.about.as_ref(),
            stated.seen.as_ref(),
            stated.room,
        )?;
        if let Target::Memory(m) = &target
            && matches!(
                stated.kind,
                UserStatementKind::Accept | UserStatementKind::Reject
            )
        {
            if m.status == MemoryStatus::Superseded {
                return Err(StoreError::Retired(
                    "this memory is superseded: act on the one that replaced it",
                ));
            }
            if m.room() == Room::Archive {
                return Err(StoreError::Retired(
                    "this memory is archived: it is kept for reference; add a note or a \
                     correction instead",
                ));
            }
        }
        let quoted = quote(&target, known_secrets);
        let plan = plan(stated.kind, target);
        let words = stated.text.get().trim();
        let body = if words.is_empty() {
            format!("{}.", plan.sentence)
        } else {
            format!("{}:\n{words}", plan.sentence)
        };
        check_note_text(&body)?;
        if let Some(id) = already_stated(&Read::new(&tx), &plan, &body)? {
            return Err(StoreError::AlreadyStated(id));
        }
        // The evidence also records what was accepted, as it was read.
        let excerpt = match &quoted {
            Some(q) if words.is_empty() => format!("{}: «{q}».", plan.sentence),
            Some(q) => format!("{}: «{q}»\n{words}", plan.sentence),
            None => body.clone(),
        };
        // The sentence holds ids only; the quote and the words were redacted.
        let body = Redacted::assume(body);
        let mut new = NewEvidence::new(
            EvidenceKind::UserStatement,
            now_ms,
            Redacted::assume(excerpt),
        );
        new.turn = Some(WINDOW_TURN.to_owned());
        let evidence = insert_evidence_in(&tx, &project_id, new)?;
        let annotation = create_annotation_in(
            &tx,
            &project_id,
            NewAnnotation {
                id: None,
                created_ms: now_ms,
                kind: plan.kind,
                target: plan.target,
                text: body,
                scopes: plan.scopes,
                sources: vec![evidence.id],
                verification: Verification::UserStated,
                origin: AnnotationOrigin::User,
                memory_kind: plan.memory_kind,
                standing: plan.standing,
            },
        )?;
        let applied = match &plan.effect {
            None => Applied::Open,
            Some(effect) => {
                let items = staged(effect, evidence.id, &annotation.id);
                let judged =
                    operations::recheck(&Read::new(&tx), &items, annotation.seq, known_secrets);
                let refused: Vec<String> = judged
                    .rejected
                    .iter()
                    .chain(judged.adjusted.iter())
                    .map(|r| r.reason.clone())
                    .collect();
                if refused.is_empty() && !judged.is_empty() {
                    let publication = judged.publication(
                        &project_id,
                        Cycle::Dream,
                        None,
                        annotation.seq,
                        None,
                        now_ms,
                    );
                    let sp = tx.savepoint()?;
                    match publish_in(&sp, &project_id, publication, &mut |_| false) {
                        Ok(published) => {
                            if stated.kind == UserStatementKind::Accept {
                                // The latest independent confirmation: now.
                                sp.execute(
                                    "UPDATE memory SET last_confirmed_ms = ?2 WHERE id = ?1 AND live = 1",
                                    rusqlite::params![
                                        effect.memory.as_str(),
                                        crate::codec::to_i64(now_ms)
                                    ],
                                )?;
                            }
                            sp.commit()?;
                            Applied::Published(published.version)
                        }
                        // Dropping the savepoint rolls the publication back;
                        // the evidence and the annotation stay.
                        Err(e) => Applied::NotAllowed(e.to_string()),
                    }
                } else if refused.is_empty() {
                    Applied::NotAllowed("there was nothing to change".to_owned())
                } else {
                    Applied::NotAllowed(refused.join("; "))
                }
            }
        };
        tx.commit()?;
        Ok(Recorded {
            evidence: evidence.id,
            annotation,
            applied,
        })
    }
}

/// The typed refusal for a store error.
#[must_use]
pub fn refusal(error: &StoreError) -> StatementRefusal {
    match error {
        StoreError::Changed(_) => StatementRefusal::Changed,
        StoreError::AlreadyStated(_) => StatementRefusal::AlreadyStated,
        StoreError::UnknownMemory(_) | StoreError::UnknownAnnotation(_) => {
            StatementRefusal::NotFound
        }
        StoreError::Invalid(e) => StatementRefusal::Invalid(e.to_string()),
        StoreError::Retired(why) => StatementRefusal::Invalid((*why).to_owned()),
        StoreError::WriterGone => StatementRefusal::Disabled,
        other => StatementRefusal::Other(crate::render::clean(&other.to_string())),
    }
}

/// The final state of a statement the store answered.
#[must_use]
pub fn final_state(result: &Result<Recorded>) -> StatementState {
    match result {
        Ok(Recorded {
            applied: Applied::Published(version),
            ..
        }) => StatementState::Recorded {
            applied: true,
            version: Some(*version),
            note: None,
        },
        Ok(Recorded {
            applied: Applied::Open,
            ..
        }) => StatementState::Recorded {
            applied: false,
            version: None,
            note: None,
        },
        Ok(Recorded {
            annotation,
            applied: Applied::NotAllowed(why),
            ..
        }) => StatementState::Recorded {
            applied: false,
            version: None,
            note: Some(format!(
                "Recorded as note {}, but it could not be applied now ({}); it stays open and \
                 the next dream will consider it.",
                annotation.id,
                crate::render::clean(why)
            )),
        },
        Err(e) => StatementState::Refused(refusal(e)),
    }
}
