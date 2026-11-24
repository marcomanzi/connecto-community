//! The development cycle (spec §5.4, §5.7, §5.8, §5.10): queued events, in
//! small batches, to annotations a cheap model writes and the host validates.
//!
//! One [`Develop::step`] does at most one batch, on the project's develop
//! thread: one run at a time per project and cycle.
//!
//! - **Grouping window.** A batch waits until the oldest ready event is
//!   `group_seconds` old (0: at once). An explicit proposal
//!   ([`Develop::close_window`]) or a priority event closes it at once.
//! - **Batch.** The oldest ready events that fit the input cap, with the
//!   relevant memories (FTS on the events' terms and touched paths) within a
//!   quarter of it. A batch that does not fit is not cut: the cap decides how
//!   many events go in, and the rest make the next batch. One event larger
//!   than the cap goes alone, in consecutive runs, each carrying one marked
//!   part; the batch completes only when every part has an answer.
//! - **Exactly once.** The annotations of every run of a batch are inserted
//!   with the batch's events marked considered in one transaction
//!   ([`crate::Store::complete_batch`]); a completed batch writes nothing
//!   again.
//! - **Part progress.** A batch in several runs stages each finished run's
//!   validated annotations in the store (`engine.devbatch.<id>`): a retry
//!   after a failed later part, a hold, a pause, the hourly limit or a
//!   restart resumes at the first unfinished part. The final insert is still
//!   one transaction, exactly once, and clears the staging.
//! - **Publication gate.** The insert checks [`RunGate`] inside the writer
//!   transaction, so a pause or a cancel that comes while it waits in the
//!   writer's queue still stops it.
//! - **Input cap.** Every rendered prompt is at most `max_input_chars`; a cap
//!   below the prompt's fixed part plus [`MIN_EVENT_CHARS`] cannot run and is
//!   reported ([`Step::Misconfigured`]) instead of being exceeded.
//! - **Failures** count an attempt and retry with a growing delay; at
//!   `max_attempts` the batch is `fallito`. **Held** (the CLI does not trust
//!   the folder) uses no attempt and is released after [`HOLD_RETRY_MS`], or
//!   [`HOLD_ACTIVITY_MS`] after a terminal of the project was used again.
//!   **Cancelled** (Stop Everything, closing) and **a pause** put the batch
//!   back with no attempt used; nothing is inserted when the pause epoch
//!   changed during the run.
//! - **Never during a pause**, and at most `max_runs_per_hour` runs started
//!   in the last hour.
//! - Each run records characters in and out, duration and outcome.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use connecto_core::Redacted;
use connecto_core::memory::{
    AnnotationId, AnnotationKind, AnnotationOrigin, AnnotationTarget, Cycle, DevelopmentSettings,
    EvidenceId, Memory, MemoryId, MemoryKind, Room, RunOutcome, Scope, Standing, Subject,
    Verification,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::RunEnd;
use crate::capture::Author;
use crate::error::{Result, StoreError};
use crate::gate::{Closed, RunGate};
use crate::host::{Host, ModelJob, RunError};
use crate::ingest::{EventPayload, EventWhat, clean_reason, safe_branch, safe_id};
use crate::prompt::{self, Block, pieces};
use crate::queue::{BatchState, EventRow};
use crate::read::SearchQuery;
use crate::store::NewAnnotation;
use crate::validate::{self, Packet};
use crate::writer::{WriterHandle, call};

/// How long one development run may take (ADR 0025).
pub const DEADLINE: Duration = Duration::from_secs(120);
/// First retry delay; it doubles with each attempt.
pub const RETRY_BASE_MS: u64 = 30_000;
pub const RETRY_MAX_MS: u64 = 30 * 60_000;
/// A held batch is tried again after this long.
pub const HOLD_RETRY_MS: u64 = 15 * 60_000;
/// ... or this long after the hold when a terminal of the project was used
/// (the user may have trusted the folder in it).
pub const HOLD_ACTIVITY_MS: u64 = 60_000;
/// Events in one batch, at most.
pub const MAX_BATCH_EVENTS: usize = 200;
/// Characters of relevant memories, at most (and at most a quarter of the
/// input cap).
pub const MAX_MEMORY_CHARS: usize = 15_000;
/// Room the events need beyond the prompt's fixed part: a cap smaller than
/// that plus the fixed part cannot run.
pub const MIN_EVENT_CHARS: usize = 1000;
const HOUR_MS: u64 = 3_600_000;

/// What one step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing queued.
    Idle,
    Paused,
    /// Events wait for the grouping window or a retry time.
    Waiting,
    RateLimited,
    /// `max_input_chars` is below what one prompt needs; nothing runs until
    /// it is raised to `needed`.
    Misconfigured {
        needed: usize,
    },
    /// The batch ran (one or more runs) and ended so.
    Ran {
        batch: u64,
        outcome: RunOutcome,
    },
}

/// What a step needs from its thread.
pub struct Context<'a> {
    pub host: Arc<dyn Host>,
    pub writer: &'a WriterHandle,
    pub project_id: &'a str,
    pub settings: &'a DevelopmentSettings,
    /// Set to cancel the run in flight (Stop Everything, closing, memory
    /// off, project closed). Also closes the publication gate.
    pub cancel: Arc<AtomicBool>,
}

/// The cycle's state between steps (in memory only; everything that must
/// survive a restart is in the store).
#[derive(Debug, Default)]
pub struct Develop {
    close_window: bool,
    held_since: Option<u64>,
    activity_since_hold: bool,
}

/// The delay before attempt `attempt + 1`.
#[must_use]
pub fn retry_delay_ms(attempts: u32) -> u64 {
    RETRY_BASE_MS
        .saturating_mul(1u64 << attempts.saturating_sub(1).min(16))
        .min(RETRY_MAX_MS)
}

/// One event as the model sees it.
#[derive(Debug, Clone)]
struct Item {
    evidence: Option<EvidenceId>,
    header: String,
    text: String,
    /// Words and paths to look for relevant memories with.
    terms: Vec<String>,
}

impl Item {
    fn chars(&self) -> usize {
        self.header.chars().count() + self.text.chars().count() + 3
    }
}

/// One run's material: whole events, or consecutive parts of one event.
#[derive(Debug, Clone, Default)]
struct Run {
    blocks: Vec<Block>,
    sources: BTreeSet<EvidenceId>,
    terms: Vec<String>,
}

fn header(seq: u64, p: &EventPayload, exposed: &[Subject]) -> String {
    let mut parts = Vec::new();
    match p.evidence_kind {
        Some(kind) => {
            parts.push(format!("e-{seq}"));
            parts.push(kind.word().to_owned());
        }
        None => {
            parts.push(format!("event {seq} (not citable)"));
            parts.push(p.what.word().replace('_', " "));
        }
    }
    match p.author {
        Some(Author::User) => parts.push("typed by the user in the window".to_owned()),
        Some(Author::Agent) => parts
            .push("typed by Connecto for another agent or an automation, not the user".to_owned()),
        Some(Author::Unknown | Author::Connecto) => {
            parts.push("origin not known: not the user".to_owned());
        }
        None => {}
    }
    if let Some(t) = &p.terminal {
        let tool = p.provider.map_or("", |p| p.word());
        parts.push(format!("{tool} terminal {t}").trim().to_owned());
    }
    if let Some(s) = &p.tool_session {
        parts.push(format!(
            "session {}",
            s.chars().take(12).collect::<String>()
        ));
    }
    if let Some(t) = &p.turn {
        parts.push(format!("turn {t}"));
    }
    if let Some(b) = &p.branch {
        parts.push(format!("branch {b}"));
    }
    if !exposed.is_empty() {
        let ids: Vec<String> = exposed.iter().map(ToString::to_string).collect();
        parts.push(format!("this session had been shown {}", ids.join(" ")));
    }
    format!("[{}]", parts.join(" | "))
}

/// One queued event as the model sees it, from either payload form: the
/// capture's ([`EventPayload`]) or a `propose`/`flag` note's
/// ([`crate::commands::PROPOSAL_EVENT`], whose text is its evidence).
fn describe(read: &crate::Read<'_>, row: &EventRow, secrets: &[String]) -> Result<Item> {
    let evidence = read.evidence(EvidenceId::new(row.seq))?;
    let (p, note) = match EventPayload::from_json(&row.payload) {
        Ok(p) => (p, None),
        Err(_) => {
            let v: serde_json::Value = serde_json::from_str(&row.payload).unwrap_or_default();
            let s = |k: &str| {
                v.get(k)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            };
            let p = EventPayload {
                what: EventWhat::Reply,
                evidence_kind: evidence.as_ref().map(|e| e.kind),
                provider: None,
                // Another writer's payload: its identifiers and branch are
                // checked as the capture's are before they reach a prompt.
                terminal: s("terminal").and_then(|t| safe_id(&t, secrets)),
                tool_session: s("tool_session").and_then(|t| safe_id(&t, secrets)),
                turn: None,
                branch: s("branch").and_then(|b| safe_branch(&b, secrets)),
                head: None,
                text: evidence.as_ref().map(|e| e.excerpt.get().clone()),
                unavailable: false,
                withheld: false,
                author: None,
                log: None,
            };
            let note = s("annotation").and_then(|a| safe_id(&a, secrets)).map(|a| {
                let about = s("about")
                    .and_then(|m| safe_id(&m, secrets))
                    .map(|m| format!(" about {m}"))
                    .unwrap_or_default();
                format!("(already stored as note {a}{about}; add only what it lacks)")
            });
            (p, note)
        }
    };
    let exposed = evidence
        .as_ref()
        .filter(|e| e.kind.from_agent())
        .map(|e| e.exposed_to.clone())
        .unwrap_or_default();
    let mut text = body(&p);
    if let Some(note) = note {
        text = format!("{note}\n{text}");
    }
    // Captured text was redacted with the secrets known then; a secret known
    // only since is hidden here, on the whole item, before it is sized,
    // packed, split or signed, so a run's size is the size it is sent at.
    let text = crate::commands::hide_known(&text, secrets);
    let mut terms = terms_of(&text);
    if let Some(e) = &evidence {
        terms.extend(e.files.iter().map(|f| f.path.clone()));
    }
    Ok(Item {
        evidence: evidence.filter(|_| p.evidence_kind.is_some()).map(|e| e.id),
        header: crate::commands::hide_known(&header(row.seq, &p, &exposed), secrets),
        text,
        terms,
    })
}

fn body(p: &EventPayload) -> String {
    match (&p.text, p.unavailable, p.withheld) {
        (Some(t), _, _) => t.clone(),
        (None, true, _) => match p.what {
            EventWhat::Prompt => "(content not available: the prompt was too large for the hook \
                                  and could not be read whole)"
                .to_owned(),
            _ => "(content not available: the reply was too large for the hook and the \
                  session log held no finished reply)"
                .to_owned(),
        },
        (None, _, true) => "(text withheld: redaction could not run)".to_owned(),
        (None, false, false) => match p.what {
            EventWhat::SessionEnd => "(the session ended)".to_owned(),
            _ => "(no text)".to_owned(),
        },
    }
}

const STOP: &[&str] = &[
    "this", "that", "with", "from", "have", "were", "will", "would", "there", "their", "what",
    "when", "which", "about", "into", "then", "than", "them", "they", "your", "also", "just",
    "should", "could", "been", "being", "does", "done", "here", "only", "some", "more", "like",
    "make", "need", "want", "please", "connecto",
];

fn terms_of(text: &str) -> Vec<String> {
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for w in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if w.chars().count() >= 4 && !w.chars().all(|c| c.is_ascii_digit()) {
            let w = w.to_lowercase();
            if !STOP.contains(&w.as_str()) {
                *count.entry(w).or_default() += 1;
            }
        }
    }
    let mut by: Vec<(usize, String)> = count.into_iter().map(|(w, n)| (n, w)).collect();
    by.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    by.into_iter().take(16).map(|(_, w)| w).collect()
}

/// Pack items into runs of at most `budget` characters; an item larger than
/// the budget alone, as consecutive marked parts.
fn pack(items: &[Item], budget: usize) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let mut current = Run::default();
    let mut used = 0usize;
    for item in items {
        let size = item.chars();
        if size > budget {
            if !current.blocks.is_empty() {
                runs.push(std::mem::take(&mut current));
                used = 0;
            }
            // The part header adds " (part i of n)"; 32 covers any count.
            let room = budget
                .saturating_sub(item.header.chars().count() + 32 + 3)
                .max(1);
            let parts = pieces(&item.text, room);
            let n = parts.len();
            for (i, text) in parts.into_iter().enumerate() {
                let mut run = Run::default();
                run.blocks.push(Block {
                    header: format!("{} (part {} of {n})", item.header, i + 1),
                    text,
                });
                run.sources.extend(item.evidence);
                run.terms.clone_from(&item.terms);
                runs.push(run);
            }
            continue;
        }
        if used + size > budget && !current.blocks.is_empty() {
            runs.push(std::mem::take(&mut current));
            used = 0;
        }
        current.blocks.push(Block {
            header: item.header.clone(),
            text: item.text.clone(),
        });
        current.sources.extend(item.evidence);
        current.terms.extend(item.terms.iter().cloned());
        used += size;
    }
    if !current.blocks.is_empty() {
        runs.push(current);
    }
    runs
}

fn memory_block(m: &Memory) -> Block {
    let mut flags = vec![
        m.id.to_string(),
        m.address.to_string(),
        m.kind.word().to_owned(),
        m.standing.word().to_owned(),
        m.status.word().to_owned(),
    ];
    if m.freshness.anchors_changed {
        flags.push("anchored files changed since".to_owned());
    }
    Block {
        header: format!("[{}]", flags.join(" | ")),
        text: m.summary.clone(),
    }
}

/// Memories relevant to a run: FTS hits on its terms, and memories anchored
/// to (or scoped on) the paths it touches; whole memories within `cap`.
fn relevant(
    writer: &WriterHandle,
    terms: &[String],
    paths: &[String],
    cap: usize,
) -> Result<Vec<Block>> {
    let text = terms.join(" ");
    let paths = paths.to_vec();
    let found = call(writer, move |s| {
        let read = s.read();
        let mut out: Vec<Memory> = Vec::new();
        if !text.trim().is_empty() {
            let mut q = SearchQuery::new(&text);
            q.limit = 30;
            for hit in read.search(&q)? {
                if let connecto_core::memory::SearchHit::Memory(card) = hit
                    && let Some(m) = read.memory(&card.id)?
                {
                    out.push(m);
                }
            }
        }
        if !paths.is_empty() {
            for m in read.memories()? {
                let touches = m.anchors.iter().any(|a| paths.contains(&a.path))
                    || m.scopes.iter().any(|s| match &s.scope {
                        Scope::Component(c) => paths.iter().any(|p| p.starts_with(c.as_str())),
                        _ => false,
                    });
                if touches && !out.iter().any(|o| o.id == m.id) {
                    out.push(m);
                }
            }
        }
        Ok(out)
    })?;
    let mut blocks = Vec::new();
    let mut used = 0;
    let mut seen = BTreeSet::new();
    for m in &found {
        if !seen.insert(m.id.clone()) {
            continue;
        }
        let b = memory_block(m);
        if used + b.chars() > cap {
            continue;
        }
        used += b.chars();
        blocks.push(b);
    }
    Ok(blocks)
}

/// The staging key of a batch's finished parts.
fn progress_key(batch: u64) -> String {
    format!("devbatch.{batch}")
}

/// What identifies a batch's runs: if the material or the cap changed, the
/// staged parts do not apply.
fn signature(runs: &[Run]) -> String {
    let mut h = Sha256::new();
    for run in runs {
        for b in &run.blocks {
            h.update(b.header.as_bytes());
            h.update([0]);
            h.update(b.text.as_bytes());
            h.update([1]);
        }
        h.update([2]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn annotation_to_json(a: &NewAnnotation) -> Value {
    json!({
        "id": a.id.as_ref().map(AnnotationId::as_str),
        "created_ms": a.created_ms,
        "kind": a.kind.word(),
        "about": match &a.target { AnnotationTarget::Memory(m) => Some(m.as_str()), AnnotationTarget::New { .. } => None },
        "room": match &a.target { AnnotationTarget::New { room } => Some(room.word()), AnnotationTarget::Memory(_) => None },
        "text": a.text.get(),
        "scopes": a.scopes.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "sources": a.sources.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "memory_kind": a.memory_kind.map(MemoryKind::word),
        "standing": a.standing.map(Standing::word),
    })
}

fn annotation_from_json(v: &Value) -> Option<NewAnnotation> {
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    let target = match s("about") {
        Some(m) => AnnotationTarget::Memory(MemoryId::parse(m).ok()?),
        None => AnnotationTarget::New {
            room: Room::for_memories(s("room")?).ok()?,
        },
    };
    let list = |k: &str| -> Option<Vec<&str>> {
        v.get(k)?.as_array()?.iter().map(Value::as_str).collect()
    };
    Some(NewAnnotation {
        id: Some(AnnotationId::parse(s("id")?).ok()?),
        created_ms: v.get("created_ms")?.as_u64()?,
        kind: AnnotationKind::from_word(s("kind")?)?,
        target,
        text: Redacted::assume(s("text")?.to_owned()),
        scopes: list("scopes")?
            .into_iter()
            .map(|x| Scope::parse(x).ok())
            .collect::<Option<_>>()?,
        sources: list("sources")?
            .into_iter()
            .map(|x| EvidenceId::parse(x).ok())
            .collect::<Option<_>>()?,
        verification: Verification::Unverified,
        origin: AnnotationOrigin::DevelopmentCycle,
        memory_kind: s("memory_kind").and_then(MemoryKind::from_word),
        standing: s("standing").and_then(Standing::from_word),
    })
}

/// A batch's finished parts, kept in the store between attempts.
#[derive(Debug, Clone, Default)]
struct Progress {
    signature: String,
    /// One per finished part, in order: (run id, staged annotations,
    /// rejected count).
    parts: Vec<(u64, Vec<NewAnnotation>, u32)>,
}

impl Progress {
    fn to_json(&self) -> String {
        json!({
            "signature": self.signature,
            "parts": self.parts.iter().map(|(run, anns, rejected)| json!({
                "run": run,
                "rejected": rejected,
                "annotations": anns.iter().map(annotation_to_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn from_json(text: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(text).ok()?;
        let parts = v
            .get("parts")?
            .as_array()?
            .iter()
            .map(|p| {
                Some((
                    p.get("run")?.as_u64()?,
                    p.get("annotations")?
                        .as_array()?
                        .iter()
                        .map(annotation_from_json)
                        .collect::<Option<Vec<_>>>()?,
                    u32::try_from(p.get("rejected")?.as_u64()?).ok()?,
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            signature: v.get("signature")?.as_str()?.to_owned(),
            parts,
        })
    }
}

/// How a batch's runs ended, before the step reports it.
enum Ended {
    Ran(RunOutcome),
    /// The hourly limit came between two parts: the batch waits, its
    /// finished parts kept.
    RateLimited,
}

/// The run in flight when a batch stops: (run id, started, finished, chars
/// in, chars out).
type Current = (u64, u64, u64, u64, u64);

/// The prompt's size with no material: what every run needs.
fn fixed_chars() -> usize {
    let id = "0".repeat(32);
    let none = prompt::development(&id, &[], &[]).chars().count();
    let memory_header = prompt::development(
        &id,
        &[Block {
            header: String::new(),
            text: String::new(),
        }],
        &[],
    )
    .chars()
    .count()
        - none
        - 3;
    none + memory_header
}

/// The smallest `max_input_chars` the development cycle can run with.
#[must_use]
pub fn min_input_chars() -> usize {
    fixed_chars() + MIN_EVENT_CHARS
}

impl Develop {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An explicit proposal arrived: the next step does not wait.
    pub fn close_window(&mut self) {
        self.close_window = true;
    }

    /// A terminal of the project was used: a held batch may be tried sooner.
    pub fn activity(&mut self) {
        if self.held_since.is_some() {
            self.activity_since_hold = true;
        }
    }

    /// When the held batches (if any) were held, as far as this run knows.
    #[must_use]
    pub const fn held_since(&self) -> Option<u64> {
        self.held_since
    }

    fn recent_runs(cx: &Context<'_>, now: u64) -> Result<usize> {
        let max = cx.settings.max_runs_per_hour as usize;
        Ok(call(cx.writer, move |s| {
            s.runs(Some(Cycle::Development), max + 1)
        })?
        .iter()
        .filter(|r| r.started_ms.saturating_add(HOUR_MS) > now)
        .count())
    }

    /// Do at most one batch.
    pub fn step(&mut self, cx: &Context<'_>) -> Result<Step> {
        if cx.host.paused() {
            return Ok(Step::Paused);
        }
        let needed = min_input_chars();
        if (cx.settings.max_input_chars as usize) < needed {
            return Ok(Step::Misconfigured { needed });
        }
        let now = cx.host.now().utc_ms;
        self.release_held(cx, now)?;
        if Self::recent_runs(cx, now)? >= cx.settings.max_runs_per_hour as usize {
            return Ok(Step::RateLimited);
        }
        let retry = call(cx.writer, move |s| s.retry_batches(now))?;
        let batch = if let Some(id) = retry.first() {
            *id
        } else {
            let ready = call(cx.writer, |s| s.ready_events(MAX_BATCH_EVENTS))?;
            let Some(oldest) = ready.iter().map(|e| e.created_ms).min() else {
                // Retries not yet due, or nothing at all.
                let waiting = call(cx.writer, |s| s.retry_batches(u64::MAX))?;
                return Ok(if waiting.is_empty() {
                    Step::Idle
                } else {
                    Step::Waiting
                });
            };
            let window = u64::from(cx.settings.group_seconds) * 1000;
            let due = self.close_window
                || window == 0
                || ready.iter().any(|e| e.priority)
                || oldest.saturating_add(window) <= now;
            if !due {
                return Ok(Step::Waiting);
            }
            self.close_window = false;
            let picked = Self::pick(cx, &ready)?;
            call(cx.writer, move |s| {
                s.open_batch(Cycle::Development, &picked, now)
            })?
        };
        match self.run_batch(cx, batch) {
            Ok(Ended::Ran(outcome)) => Ok(Step::Ran { batch, outcome }),
            Ok(Ended::RateLimited) => Ok(Step::RateLimited),
            Err(e) => {
                // Leave nothing running: the batch goes back, no attempt used.
                let now = cx.host.now().utc_ms;
                let _ = call(cx.writer, move |s| s.requeue_batch(batch, now));
                Err(e)
            }
        }
    }

    fn release_held(&mut self, cx: &Context<'_>, now: u64) -> Result<()> {
        let held = call(cx.writer, |s| s.hold_reason())?.is_some();
        if !held {
            self.held_since = None;
            self.activity_since_hold = false;
            return Ok(());
        }
        let since = *self.held_since.get_or_insert(now);
        let waited = now.saturating_sub(since);
        if waited >= HOLD_RETRY_MS || (self.activity_since_hold && waited >= HOLD_ACTIVITY_MS) {
            call(cx.writer, move |s| s.release_held_batches(now))?;
            self.held_since = None;
            self.activity_since_hold = false;
        }
        Ok(())
    }

    /// (room for events, room for memories). Their sum plus the fixed part
    /// is at most `max_input_chars` (checked by [`Self::step`]).
    fn budgets(settings: &DevelopmentSettings) -> (usize, usize) {
        let max = settings.max_input_chars as usize;
        let room = max.saturating_sub(fixed_chars());
        let memories = (max / 4).min(MAX_MEMORY_CHARS);
        if room >= memories + MIN_EVENT_CHARS {
            (room - memories, memories)
        } else {
            (room, 0)
        }
    }

    /// The oldest ready events (proposals first) that fit one run; at least
    /// one.
    fn pick(cx: &Context<'_>, ready: &[EventRow]) -> Result<Vec<u64>> {
        let (budget, _) = Self::budgets(cx.settings);
        let rows = ready.to_vec();
        let secrets = cx.host.known_secrets();
        let items = call(cx.writer, move |s| {
            let read = s.read();
            rows.iter()
                .map(|row| describe(&read, row, &secrets).map(|item| (row.seq, item.chars())))
                .collect::<Result<Vec<_>>>()
        })?;
        let mut picked = Vec::new();
        let mut used = 0;
        for (seq, size) in items {
            if !picked.is_empty() && used + size > budget {
                break;
            }
            used += size;
            picked.push(seq);
            if size > budget {
                break;
            }
        }
        Ok(picked)
    }

    fn items(cx: &Context<'_>, events: Vec<u64>) -> Result<Vec<Item>> {
        let secrets = cx.host.known_secrets();
        call(cx.writer, move |s| {
            let mut out = Vec::new();
            for seq in events {
                let row = s.event(seq)?.ok_or(StoreError::UnknownEvent(seq))?;
                out.push(describe(&s.read(), &row, &secrets)?);
            }
            Ok(out)
        })
    }

    fn load_progress(cx: &Context<'_>, batch: u64, signature: &str) -> Result<Progress> {
        let key = progress_key(batch);
        let stored = call(cx.writer, move |s| s.engine_state(&key))?;
        Ok(stored
            .as_deref()
            .and_then(Progress::from_json)
            .filter(|p| p.signature == signature)
            .unwrap_or_else(|| Progress {
                signature: signature.to_owned(),
                parts: Vec::new(),
            }))
    }

    #[allow(clippy::too_many_lines)]
    fn run_batch(&mut self, cx: &Context<'_>, batch: u64) -> Result<Ended> {
        let row =
            call(cx.writer, move |s| s.batch(batch))?.ok_or(StoreError::UnknownBatch(batch))?;
        let items = Self::items(cx, row.events.clone())?;
        let (budget, memory_cap) = Self::budgets(cx.settings);
        let max_input = cx.settings.max_input_chars as usize;
        let runs = pack(&items, budget);
        let mut progress = Self::load_progress(cx, batch, &signature(&runs))?;
        let secrets = cx.host.known_secrets();
        let gate = RunGate::new(Arc::clone(&cx.host), Arc::clone(&cx.cancel));
        let attempts = row.attempts;

        for run in runs.iter().skip(progress.parts.len()) {
            let now = cx.host.now().utc_ms;
            if let Some(closed) = gate.closed() {
                return self.back(cx, batch, None, closed_outcome(closed));
            }
            if Self::recent_runs(cx, now)? >= cx.settings.max_runs_per_hour as usize {
                return self
                    .back(cx, batch, None, RunOutcome::NoChange)
                    .map(|_| Ended::RateLimited);
            }
            let paths: Vec<String> = run
                .terms
                .iter()
                .filter(|t| t.contains('/') || t.contains('.'))
                .cloned()
                .collect();
            let words: Vec<String> = run
                .terms
                .iter()
                .filter(|t| !t.contains('/'))
                .take(24)
                .cloned()
                .collect();
            let memories = if memory_cap > 0 {
                relevant(cx.writer, &words, &paths, memory_cap)?
            } else {
                Vec::new()
            };
            // The events were redacted whole, before packing ([`describe`]);
            // memories, stored as they were, are redacted here (the material
            // only, never the instructions). Redaction is applied once: a
            // second pass could grow what the first wrote. The delimiter id
            // is chosen after, so no secret can match it.
            let mut memories = prompt::hidden(&memories, &secrets);
            let events = run.blocks.clone();
            let id = prompt::fresh_id(&memories, &events);
            let mut text = prompt::development(&id, &memories, &events);
            // The cap holds for the whole prompt: memories give way first.
            while text.chars().count() > max_input && memories.pop().is_some() {
                text = prompt::development(&id, &memories, &events);
            }
            if text.chars().count() > max_input {
                // Never requeued unchanged: a failed attempt, so the batch
                // ends as failed after the last one instead of looping.
                return self.fail_unrun(
                    cx,
                    batch,
                    attempts,
                    "a development prompt over the input limit",
                );
            }
            let choice = cx.settings.choice.clone();
            let epoch = gate.epoch();
            let run_id = call(cx.writer, move |s| {
                let id = s.start_run(Cycle::Development, &choice, now, epoch)?;
                s.start_batch(batch, Some(id))?;
                Ok(id)
            })?;
            let job = ModelJob {
                project_id: cx.project_id.to_owned(),
                cycle: Some(Cycle::Development),
                choice: cx.settings.choice.clone(),
                prompt: text.clone(),
            };
            let chars_in = text.chars().count() as u64;
            let reply = cx
                .host
                .run_model(&job, Instant::now() + DEADLINE, gate.cancel_flag());
            let finished = cx.host.now().utc_ms;
            let reply = match reply {
                Ok(r) => r,
                Err(RunError::Cancelled) => {
                    let current = (run_id, now, finished, chars_in, 0);
                    return self.back(cx, batch, Some(current), RunOutcome::Cancelled);
                }
                Err(RunError::HeldUntrusted { reason }) => {
                    let reason = clean_reason(&reason, &secrets);
                    self.held_since = Some(finished);
                    self.activity_since_hold = false;
                    let (r, g) = (reason.clone(), gate.clone());
                    let end = run_end((run_id, now, finished, chars_in, 0), 0);
                    match call(cx.writer, move |s| s.hold_gated(batch, &r, end, &g)) {
                        Ok(()) => return Ok(Ended::Ran(RunOutcome::Held { reason })),
                        Err(StoreError::Aborted) => {
                            let current = (run_id, now, finished, chars_in, 0);
                            return self.back_closed(cx, batch, Some(current), &gate);
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => {
                    let current = (run_id, now, finished, chars_in, 0);
                    let reason = clean_reason(&e.to_string(), &secrets);
                    return self.fail(cx, batch, attempts, current, &reason, &gate);
                }
            };
            let finished =
                now.saturating_add(u64::try_from(reply.duration.as_millis()).unwrap_or(0));
            let chars_out = reply.text.chars().count() as u64;
            let chars_in = if reply.chars_in > 0 {
                reply.chars_in
            } else {
                chars_in
            };
            let current = (run_id, now, finished, chars_in, chars_out);
            if let Some(closed) = gate.closed() {
                return self.back(cx, batch, Some(current), closed_outcome(closed));
            }
            let parsed = match validate::parse_reply(&reply.text) {
                Ok(items) => items,
                Err(e) => {
                    return self.fail(cx, batch, attempts, current, &e.to_string(), &gate);
                }
            };
            let sources = run.sources.clone();
            let secrets = secrets.clone();
            let verdict = call(cx.writer, move |s| {
                let packet = Packet {
                    sources: &sources,
                    secrets: &secrets,
                    now_ms: finished,
                };
                Ok(validate::validate(&parsed, &packet, &s.read()))
            })?;
            let rejected = u32::try_from(verdict.rejected.len()).unwrap_or(u32::MAX);
            let kept_here = verdict.accepted.len();
            let why_here = verdict.rejected.clone();
            progress.parts.push((run_id, verdict.accepted, rejected));
            let last = progress.parts.len() == runs.len();
            if !last {
                // Staged: this part is never run again for this batch.
                // Staged and finished in one transaction behind the gate: a
                // pause or cancel since the reply stages nothing, and the
                // part runs again after it.
                let (key, value, g) = (progress_key(batch), progress.to_json(), gate.clone());
                let end = run_end(current, 0);
                match call(cx.writer, move |s| {
                    s.stage_part_gated(&key, &value, end, rejected, &g)
                }) {
                    Ok(()) => {
                        // Proposed and kept none: say so on its run.
                        if kept_here == 0 && rejected > 0 {
                            let reasons: Vec<&str> = why_here.iter().map(String::as_str).collect();
                            let o = RunOutcome::Refused {
                                proposed: rejected,
                                reason: crate::bootstrap::refusal_summary(rejected, 0, &reasons),
                            };
                            call(cx.writer, move |s| {
                                s.finish_run(
                                    end.run,
                                    &o,
                                    end.finished_ms,
                                    end.chars_in,
                                    end.chars_out,
                                )
                            })?;
                        }
                    }
                    Err(StoreError::Aborted) => {
                        return self.back_closed(cx, batch, Some(current), &gate);
                    }
                    Err(e) => return Err(e),
                }
            } else {
                // The last part: everything goes in at once, behind the gate.
                let annotations: Vec<NewAnnotation> = progress
                    .parts
                    .iter()
                    .flat_map(|(_, a, _)| a.iter().cloned())
                    .collect();
                let staged: u32 = progress
                    .parts
                    .iter()
                    .map(|(_, a, r)| r + u32::try_from(a.len()).unwrap_or(u32::MAX))
                    .sum();
                // The insert and the run's finish: one transaction, behind
                // the gate.
                let end = run_end(current, staged);
                let written = match Self::complete(cx, batch, annotations, Some(end), &gate) {
                    Ok(written) => written,
                    Err(StoreError::Aborted) => {
                        return self.back_closed(cx, batch, Some(current), &gate);
                    }
                    Err(e) => return Err(e),
                };
                let written = u32::try_from(written.len()).unwrap_or(u32::MAX);
                if written == 0 && staged > 0 {
                    // The batch proposed and kept nothing: "N proposed, 0
                    // kept" on its last run, with why.
                    let earlier: u32 = progress
                        .parts
                        .iter()
                        .rev()
                        .skip(1)
                        .map(|(_, _, r)| *r)
                        .sum();
                    let at_write = staged.saturating_sub(rejected).saturating_sub(earlier);
                    let mut reasons: Vec<&str> = why_here.iter().map(String::as_str).collect();
                    reasons.extend(std::iter::repeat_n(
                        "refused in an earlier part of this batch",
                        earlier as usize,
                    ));
                    reasons.extend(std::iter::repeat_n(
                        "refused by the store when written",
                        at_write as usize,
                    ));
                    let o = RunOutcome::Refused {
                        proposed: staged,
                        reason: crate::bootstrap::refusal_summary(staged, 0, &reasons),
                    };
                    let o2 = o.clone();
                    call(cx.writer, move |s| {
                        s.finish_run(end.run, &o2, end.finished_ms, end.chars_in, end.chars_out)
                    })?;
                    return Ok(Ended::Ran(o));
                }
                return Ok(Ended::Ran(RunOutcome::Annotated {
                    written,
                    rejected: staged.saturating_sub(written),
                }));
            }
        }
        // Every part was already staged (a resumed batch whose last insert
        // did not happen): insert now.
        let annotations: Vec<NewAnnotation> = progress
            .parts
            .iter()
            .flat_map(|(_, a, _)| a.iter().cloned())
            .collect();
        match Self::complete(cx, batch, annotations, None, &gate) {
            Ok(written) => Ok(Ended::Ran(RunOutcome::Annotated {
                written: u32::try_from(written.len()).unwrap_or(u32::MAX),
                rejected: 0,
            })),
            Err(StoreError::Aborted) => self.back_closed(cx, batch, None, &gate),
            Err(e) => Err(e),
        }
    }

    /// Insert the batch behind the gate, dropping (one by one) annotations
    /// the store refuses for a reason that names them. Returns the ids
    /// written. [`StoreError::Aborted`] when the gate closed.
    fn complete(
        cx: &Context<'_>,
        batch: u64,
        mut annotations: Vec<NewAnnotation>,
        run: Option<RunEnd>,
        gate: &RunGate,
    ) -> Result<BTreeSet<AnnotationId>> {
        for _ in 0..=annotations.len() {
            let attempt = annotations.clone();
            let (gate, key) = (gate.clone(), progress_key(batch));
            match call(cx.writer, move |s| {
                s.complete_batch_gated(batch, attempt, &key, run, &gate)
            }) {
                Ok(done) => return Ok(done.annotations.into_iter().map(|a| a.id).collect()),
                Err(e) => {
                    let named: Option<usize> = match &e {
                        StoreError::StandingUnsupported(id)
                        | StoreError::NotVerifiable(id)
                        | StoreError::IdTaken(id) => annotations
                            .iter()
                            .position(|a| a.id.as_ref().is_some_and(|x| x.as_str() == id)),
                        StoreError::NotUserStated(id) => {
                            annotations.iter().position(|a| a.id.as_ref() == Some(id))
                        }
                        StoreError::UnknownMemory(id) => annotations.iter().position(
                            |a| matches!(&a.target, AnnotationTarget::Memory(m) if m == id),
                        ),
                        StoreError::Invalid(_) | StoreError::UnknownEvidence(_) => {
                            (!annotations.is_empty()).then_some(0)
                        }
                        _ => None,
                    };
                    match named {
                        Some(i) => {
                            annotations.remove(i);
                        }
                        None => return Err(e),
                    }
                }
            }
        }
        Err(StoreError::Corrupt("the batch could not be completed"))
    }

    /// The batch goes back to the queue with no attempt used; staged parts
    /// are kept.
    fn back(
        &self,
        cx: &Context<'_>,
        batch: u64,
        current: Option<Current>,
        outcome: RunOutcome,
    ) -> Result<Ended> {
        let now = cx.host.now().utc_ms;
        call(cx.writer, move |s| s.requeue_batch(batch, now))?;
        if let Some((id, _, fin, ci, co)) = current {
            let o = outcome.clone();
            call(cx.writer, move |s| s.finish_run(id, &o, fin, ci, co))?;
        }
        Ok(Ended::Ran(outcome))
    }

    /// A failed attempt. Staged parts are kept for the retry; on the last
    /// attempt they are dropped with the batch.
    /// A failed attempt, counted and recorded behind the gate (a pause or
    /// cancel during the run uses no attempt). Staged parts are kept for the
    /// retry; on the last attempt they are dropped with the batch.
    fn fail(
        &self,
        cx: &Context<'_>,
        batch: u64,
        attempts: u32,
        current: Current,
        reason: &str,
        gate: &RunGate,
    ) -> Result<Ended> {
        let now = cx.host.now().utc_ms;
        let at = now.saturating_add(retry_delay_ms(attempts.saturating_add(1)));
        let max = cx.settings.max_attempts;
        let (r, key, g) = (reason.to_owned(), progress_key(batch), gate.clone());
        let end = run_end(current, 0);
        let state = match call(cx.writer, move |s| {
            s.fail_batch_gated(batch, &r, at, max, &key, end, &g)
        }) {
            Ok(state) => state,
            Err(StoreError::Aborted) => return self.back_closed(cx, batch, Some(current), gate),
            Err(e) => return Err(e),
        };
        Ok(Ended::Ran(RunOutcome::Failed {
            reason: if state == BatchState::Failed {
                format!("{reason} (last attempt)")
            } else {
                reason.to_owned()
            },
        }))
    }

    /// A failed attempt before any run started (no run row): counted like
    /// [`Self::fail`], so the batch is retried later and ends as failed
    /// after the last attempt, never requeued as it was.
    fn fail_unrun(
        &self,
        cx: &Context<'_>,
        batch: u64,
        attempts: u32,
        reason: &str,
    ) -> Result<Ended> {
        let now = cx.host.now().utc_ms;
        let at = now.saturating_add(retry_delay_ms(attempts.saturating_add(1)));
        let max = cx.settings.max_attempts;
        let r = reason.to_owned();
        let state = call(cx.writer, move |s| s.fail_batch(batch, &r, at, max))?;
        Ok(Ended::Ran(RunOutcome::Failed {
            reason: if state == BatchState::Failed {
                format!("{reason} (last attempt)")
            } else {
                reason.to_owned()
            },
        }))
    }

    /// The gate closed: back to the queue, the run recorded as paused or
    /// cancelled (the only writes allowed after the gate closes).
    fn back_closed(
        &self,
        cx: &Context<'_>,
        batch: u64,
        current: Option<Current>,
        gate: &RunGate,
    ) -> Result<Ended> {
        let closed = gate.closed().unwrap_or(Closed::Cancelled);
        self.back(cx, batch, current, closed_outcome(closed))
    }
}

const fn run_end(current: Current, staged: u32) -> RunEnd {
    let (run, _, finished_ms, chars_in, chars_out) = current;
    RunEnd {
        run,
        finished_ms,
        chars_in,
        chars_out,
        staged,
    }
}

const fn closed_outcome(closed: Closed) -> RunOutcome {
    match closed {
        Closed::Paused => RunOutcome::HeldByPause,
        Closed::Cancelled => RunOutcome::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(seq: u64, len: usize) -> Item {
        Item {
            evidence: Some(EvidenceId::new(seq)),
            header: format!("[e-{seq}]"),
            text: "x".repeat(len),
            terms: Vec::new(),
        }
    }

    #[test]
    fn packing_never_cuts_and_marks_parts() {
        let items = [item(1, 100), item(2, 100), item(3, 2500), item(4, 10)];
        let runs = pack(&items, 1000);
        // [1,2], three parts of 3, [4]
        assert_eq!(runs.len(), 5);
        assert_eq!(runs[0].blocks.len(), 2);
        let parts: String = runs[1..4]
            .iter()
            .map(|r| r.blocks[0].text.clone())
            .collect();
        assert_eq!(parts, "x".repeat(2500));
        assert!(runs[1].blocks[0].header.ends_with("(part 1 of 3)"));
        assert!(runs[3].blocks[0].header.ends_with("(part 3 of 3)"));
        assert!(runs.iter().all(|r| !r.sources.is_empty()));
        let total: usize = runs
            .iter()
            .flat_map(|r| &r.blocks)
            .map(|b| b.text.len())
            .sum();
        assert_eq!(total, 2710);
    }

    #[test]
    fn retry_delay_grows_and_stops_growing() {
        assert_eq!(retry_delay_ms(1), 30_000);
        assert_eq!(retry_delay_ms(2), 60_000);
        assert_eq!(retry_delay_ms(3), 120_000);
        assert_eq!(retry_delay_ms(40), RETRY_MAX_MS);
    }
}
