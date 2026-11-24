//! Durable primitives for the cycles: the event queue and its batches
//! (spec §5.3, §5.4), run rows, the scheduler's per-date state (§7), dream
//! staging (§5.5) and the initial build's manifest and checkpoints (§5.9).
//! The cycles themselves live elsewhere; these only keep their state safe on
//! disk.

use connecto_core::memory::{
    Annotation, Backlog, BootstrapOptions, BootstrapState, Cycle, LocalDate, ModelChoice, Provider,
    Room, RunEntry, RunOutcome, SourceClass,
};
use rusqlite::{OptionalExtension, Row, params};

use crate::codec::{strings_from_json, strings_to_json, to_i64, to_u64, word};
use crate::error::{Result, StoreError};
use crate::schema;
use crate::store::{NewAnnotation, Store, create_annotation_in};

/// Where an event is in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventState {
    Queued,
    Considered,
    Failed,
    /// Produced by memory itself (a `connecto memory` command, its output):
    /// kept for the record, never sent to a model.
    Internal,
}

impl EventState {
    pub const ALL: &'static [Self] =
        &[Self::Queued, Self::Considered, Self::Failed, Self::Internal];

    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Queued => "in_coda",
            Self::Considered => "considerati",
            Self::Failed => "fallito",
            Self::Internal => "interno",
        }
    }

    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.word() == word)
    }
}

/// An event to queue. `dedup_key` is the hash of (terminal, session, turn,
/// kind, redacted content) the caller computes (spec §5.3); `payload` is
/// already redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    pub dedup_key: String,
    pub kind: String,
    pub created_ms: u64,
    pub payload: String,
    /// An explicit proposal: closes the grouping window at once.
    pub priority: bool,
    pub internal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    Inserted(u64),
    /// The key was already queued; its `seq`.
    Duplicate(u64),
}

impl Enqueued {
    #[must_use]
    pub const fn seq(self) -> u64 {
        match self {
            Self::Inserted(seq) | Self::Duplicate(seq) => seq,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub seq: u64,
    pub dedup_key: String,
    pub kind: String,
    pub state: EventState,
    pub priority: bool,
    pub created_ms: u64,
    pub payload: String,
    pub batch_id: Option<u64>,
    pub attempts: u32,
}

/// Where a batch is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchState {
    Running,
    /// Failed, waiting for `next_attempt_ms` to run again.
    Retry,
    Done,
    /// Failed for good after the last attempt.
    Failed,
    /// Waiting on the user (the CLI does not trust the project folder): no
    /// attempt used, not retried until released.
    Held,
}

impl BatchState {
    const fn word(self) -> &'static str {
        match self {
            Self::Running => "in_corso",
            Self::Retry => "in_coda",
            Self::Done => "completato",
            Self::Failed => "fallito",
            Self::Held => "sospeso",
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        [
            Self::Running,
            Self::Retry,
            Self::Done,
            Self::Failed,
            Self::Held,
        ]
        .into_iter()
        .find(|s| s.word() == word)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRow {
    pub id: u64,
    pub cycle: Cycle,
    pub state: BatchState,
    pub created_ms: u64,
    pub attempts: u32,
    pub next_attempt_ms: Option<u64>,
    pub last_error: Option<String>,
    /// Why a held batch waits.
    pub hold_reason: Option<String>,
    pub run_id: Option<u64>,
    pub events: Vec<u64>,
}

/// What [`Store::complete_batch`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchDone {
    pub annotations: Vec<Annotation>,
    /// The batch had already been completed: nothing was written again.
    pub already: bool,
}

/// Where a scheduled date is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleState {
    Running,
    Done,
    /// Waiting on the user (folder not trusted): not done, claimable again.
    Held {
        reason: String,
    },
    /// Due while the app was closed, not yet recovered.
    Missed,
    /// Covered by the cumulative run in `recovered_by`.
    Recovered,
}

impl ScheduleState {
    fn word(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Running => ("running", None),
            Self::Done => ("done", None),
            Self::Held { reason } => ("held", Some(reason)),
            Self::Missed => ("missed", None),
            Self::Recovered => ("recovered", None),
        }
    }

    fn from_word(word: &str, reason: Option<String>) -> Option<Self> {
        Some(match word {
            "running" => Self::Running,
            "done" => Self::Done,
            "held" => Self::Held {
                reason: reason.unwrap_or_default(),
            },
            "missed" => Self::Missed,
            "recovered" => Self::Recovered,
            _ => return None,
        })
    }

    /// Whether the date needs no further run.
    #[must_use]
    pub const fn counts_as_done(&self) -> bool {
        matches!(self, Self::Done | Self::Recovered)
    }
}

/// One row of the scheduler's state: at most one per (program, local date).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleEntry {
    pub program: String,
    pub date: LocalDate,
    pub state: ScheduleState,
    pub run_id: Option<u64>,
    pub recovered_by: Option<u64>,
    pub updated_ms: u64,
}

/// A staged dream operation; `payload` is the cycle's own encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedOp {
    pub id: u64,
    pub run_id: u64,
    pub room: Option<Room>,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapRow {
    pub id: String,
    pub state: BootstrapState,
    /// Stored clamped.
    pub options: BootstrapOptions,
    pub started_ms: u64,
    pub updated_ms: u64,
}

/// One source in an initial build's manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSource {
    pub path: String,
    pub class: SourceClass,
    pub hash: Option<String>,
    /// The bootstrap's own word (`selected`, `excluded`, `analysed`...).
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub packet: u32,
    pub state: String,
    pub run_id: Option<u64>,
    pub payload: String,
    pub updated_ms: u64,
}

fn bootstrap_word(state: &BootstrapState) -> (&'static str, Option<&str>) {
    match state {
        BootstrapState::NotStarted => ("not_started", None),
        BootstrapState::Queued => ("queued", None),
        BootstrapState::Running => ("running", None),
        BootstrapState::Interrupted => ("interrupted", None),
        BootstrapState::Failed { reason } => ("failed", Some(reason)),
        BootstrapState::Completed => ("completed", None),
    }
}

fn bootstrap_state(word: &str, reason: Option<String>) -> Result<BootstrapState> {
    Ok(match word {
        "not_started" => BootstrapState::NotStarted,
        "queued" => BootstrapState::Queued,
        "running" => BootstrapState::Running,
        "interrupted" => BootstrapState::Interrupted,
        "failed" => BootstrapState::Failed {
            reason: reason.unwrap_or_default(),
        },
        "completed" => BootstrapState::Completed,
        _ => return Err(StoreError::Corrupt("bootstrap state")),
    })
}

fn outcome_columns(outcome: &RunOutcome) -> (&'static str, u32, u32, Option<u64>, Option<&str>) {
    match outcome {
        RunOutcome::Running => ("running", 0, 0, None, None),
        RunOutcome::Annotated { written, rejected } => {
            ("annotated", *written, *rejected, None, None)
        }
        RunOutcome::Published {
            version,
            applied,
            rejected,
        } => ("published", *applied, *rejected, Some(*version), None),
        RunOutcome::NoChange => ("no_change", 0, 0, None, None),
        RunOutcome::Refused { proposed, reason } => ("refused", 0, *proposed, None, Some(reason)),
        RunOutcome::HeldByPause => ("held_by_pause", 0, 0, None, None),
        RunOutcome::Held { reason } => ("held", 0, 0, None, Some(reason)),
        RunOutcome::Cancelled => ("cancelled", 0, 0, None, None),
        RunOutcome::Interrupted => ("interrupted", 0, 0, None, None),
        RunOutcome::Failed { reason } => ("failed", 0, 0, None, Some(reason)),
    }
}

fn outcome_from(
    word: &str,
    written: u32,
    rejected: u32,
    version: Option<u64>,
    reason: Option<String>,
) -> Result<RunOutcome> {
    Ok(match word {
        "running" => RunOutcome::Running,
        "annotated" => RunOutcome::Annotated { written, rejected },
        "published" => RunOutcome::Published {
            version: version.unwrap_or_default(),
            applied: written,
            rejected,
        },
        "no_change" => RunOutcome::NoChange,
        "refused" => RunOutcome::Refused {
            proposed: rejected,
            reason: reason.unwrap_or_default(),
        },
        "held_by_pause" => RunOutcome::HeldByPause,
        "cancelled" => RunOutcome::Cancelled,
        "interrupted" => RunOutcome::Interrupted,
        "failed" => RunOutcome::Failed {
            reason: reason.unwrap_or_default(),
        },
        "held" => RunOutcome::Held {
            reason: reason.unwrap_or_default(),
        },
        _ => return Err(StoreError::Corrupt("run outcome")),
    })
}

fn options_to_json(o: &BootstrapOptions) -> String {
    serde_json::json!({
        "choice": o.choice.as_ref().map(|c| serde_json::json!({
            "provider": c.provider.word(), "model": c.model, "effort": c.effort,
        })),
        "sources": o.sources.iter().map(|s| s.word()).collect::<Vec<_>>(),
        "exclude": o.exclude,
        "history_days": o.history_days,
        "max_input_chars": o.max_input_chars,
        "max_minutes": o.max_minutes,
    })
    .to_string()
}

fn options_from_json(text: &str) -> Result<BootstrapOptions> {
    use serde_json::Value;
    let bad = || StoreError::Corrupt("bootstrap options");
    let v: Value = serde_json::from_str(text).map_err(|_| bad())?;
    let words = |key: &str| -> Result<Vec<String>> {
        v.get(key)
            .and_then(Value::as_array)
            .ok_or_else(bad)?
            .iter()
            .map(|w| w.as_str().map(str::to_owned).ok_or_else(bad))
            .collect()
    };
    let number = |key: &str| v.get(key).and_then(Value::as_u64).ok_or_else(bad);
    let choice = match v.get("choice") {
        Some(Value::Object(c)) => {
            let text = |k: &str| c.get(k).and_then(Value::as_str).ok_or_else(bad);
            Some(ModelChoice::new(
                Provider::from_word(text("provider")?).ok_or_else(bad)?,
                text("model")?,
                text("effort")?,
            ))
        }
        _ => None,
    };
    Ok(BootstrapOptions {
        choice,
        sources: words("sources")?
            .iter()
            .map(|w| SourceClass::from_word(w).ok_or_else(bad))
            .collect::<Result<_>>()?,
        exclude: words("exclude")?,
        history_days: u32::try_from(number("history_days")?).map_err(|_| bad())?,
        max_input_chars: number("max_input_chars")?,
        max_minutes: u32::try_from(number("max_minutes")?).map_err(|_| bad())?,
    }
    .clamped())
}

const RUN_COLUMNS: &str = "id, cycle, provider, model, effort, started_ms, finished_ms, chars_in,
    chars_out, outcome, written, rejected, version, reason, recovers";

fn run_row(row: &Row<'_>) -> rusqlite::Result<Result<RunEntry>> {
    let id: i64 = row.get(0)?;
    let cycle: String = row.get(1)?;
    let provider: String = row.get(2)?;
    let model: String = row.get(3)?;
    let effort: String = row.get(4)?;
    let started: i64 = row.get(5)?;
    let finished: Option<i64> = row.get(6)?;
    let chars_in: i64 = row.get(7)?;
    let chars_out: i64 = row.get(8)?;
    let outcome: String = row.get(9)?;
    let written: u32 = row.get(10)?;
    let rejected: u32 = row.get(11)?;
    let version: Option<i64> = row.get(12)?;
    let reason: Option<String> = row.get(13)?;
    let recovers: String = row.get(14)?;
    Ok((|| {
        let recovers = if recovers.is_empty() {
            Vec::new()
        } else {
            strings_from_json(&recovers)?
                .iter()
                .map(|d| LocalDate::parse(d).ok_or(StoreError::Corrupt("date")))
                .collect::<Result<_>>()?
        };
        Ok(RunEntry {
            id: to_u64(id),
            cycle: word(Cycle::from_word(&cycle), "cycle")?,
            choice: ModelChoice::new(
                word(Provider::from_word(&provider), "provider")?,
                &model,
                &effort,
            ),
            started_ms: to_u64(started),
            duration_ms: finished.map(|f| to_u64(f).saturating_sub(to_u64(started))),
            chars_in: to_u64(chars_in),
            chars_out: to_u64(chars_out),
            outcome: outcome_from(&outcome, written, rejected, version.map(to_u64), reason)?,
            recovers,
        })
    })())
}

fn event_row(row: &Row<'_>) -> rusqlite::Result<Result<EventRow>> {
    let state: String = row.get(3)?;
    let batch: Option<i64> = row.get(7)?;
    let seq: i64 = row.get(0)?;
    let created: i64 = row.get(5)?;
    let attempts: u32 = row.get(8)?;
    let event = (row.get(1)?, row.get(2)?, row.get(4)?, row.get(6)?);
    Ok(
        word(EventState::from_word(&state), "event state").map(|state| EventRow {
            seq: to_u64(seq),
            dedup_key: event.0,
            kind: event.1,
            state,
            priority: event.2,
            created_ms: to_u64(created),
            payload: event.3,
            batch_id: batch.map(to_u64),
            attempts,
        }),
    )
}

const EVENT_COLUMNS: &str =
    "seq, dedup_key, kind, state, priority, created_ms, payload, batch_id, attempts";

fn collect<T, P: rusqlite::Params>(
    conn: &rusqlite::Connection,
    sql: &str,
    params: P,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<Result<T>>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, map)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// A date's fields are public: refuse one that would not read back.
fn check_date(date: LocalDate) -> Result<()> {
    if date.is_valid() && (0..=9999).contains(&date.year) {
        Ok(())
    } else {
        Err(StoreError::BadDate)
    }
}

fn valid_text_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 256 && !key.chars().any(char::is_control)
}

impl Store {
    /// Queue an event once: a key already seen is not queued again.
    pub fn enqueue_event(&mut self, event: NewEvent) -> Result<Enqueued> {
        if !valid_text_key(&event.dedup_key) {
            return Err(StoreError::Corrupt("dedup key"));
        }
        let project_id = self.project_id().to_owned();
        let tx = self.begin()?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT seq FROM event WHERE dedup_key = ?1",
                [&event.dedup_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(seq) = existing {
            return Ok(Enqueued::Duplicate(to_u64(seq)));
        }
        let seq = schema::next_counter(&tx, "event_seq")?;
        let state = if event.internal {
            EventState::Internal
        } else {
            EventState::Queued
        };
        tx.execute(
            "INSERT INTO event (seq, project_id, dedup_key, kind, state, priority, created_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                to_i64(seq),
                project_id,
                event.dedup_key,
                event.kind,
                state.word(),
                event.priority,
                to_i64(event.created_ms),
                event.payload
            ],
        )?;
        tx.commit()?;
        Ok(Enqueued::Inserted(seq))
    }

    pub fn event(&self, seq: u64) -> Result<Option<EventRow>> {
        let sql = format!("SELECT {EVENT_COLUMNS} FROM event WHERE seq = ?1");
        Ok(collect(self.conn(), &sql, [to_i64(seq)], event_row)?
            .into_iter()
            .next())
    }

    /// Queued events in no batch, explicit proposals first, then oldest.
    pub fn ready_events(&self, limit: usize) -> Result<Vec<EventRow>> {
        let sql = format!(
            "SELECT {EVENT_COLUMNS} FROM event WHERE state = 'in_coda' AND batch_id IS NULL
             ORDER BY priority DESC, seq LIMIT ?1"
        );
        collect(self.conn(), &sql, [to_i64(limit as u64)], event_row)
    }

    /// Put queued events into a new running batch.
    pub fn open_batch(&mut self, cycle: Cycle, events: &[u64], now_ms: u64) -> Result<u64> {
        let project_id = self.project_id().to_owned();
        let tx = self.begin()?;
        tx.execute(
            "INSERT INTO batch (project_id, cycle, state, created_ms) VALUES (?1, ?2, 'in_corso', ?3)",
            params![project_id, cycle.word(), to_i64(now_ms)],
        )?;
        let batch = tx.last_insert_rowid();
        for seq in events {
            let changed = tx.execute(
                "UPDATE event SET batch_id = ?2
                 WHERE seq = ?1 AND state = 'in_coda' AND batch_id IS NULL",
                params![to_i64(*seq), batch],
            )?;
            if changed == 0 {
                return Err(StoreError::UnknownEvent(*seq));
            }
        }
        tx.commit()?;
        Ok(to_u64(batch))
    }

    pub fn batch(&self, id: u64) -> Result<Option<BatchRow>> {
        let head = self
            .conn()
            .query_row(
                "SELECT cycle, state, created_ms, attempts, next_attempt_ms, last_error, run_id,
                     hold_reason
                 FROM batch WHERE id = ?1",
                [to_i64(id)],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, u32>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<i64>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((cycle, state, created, attempts, next, error, run, hold)) = head else {
            return Ok(None);
        };
        let events: Vec<i64> = self
            .conn()
            .prepare("SELECT seq FROM event WHERE batch_id = ?1 ORDER BY seq")?
            .query_map([to_i64(id)], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Some(BatchRow {
            id,
            cycle: word(Cycle::from_word(&cycle), "cycle")?,
            state: word(BatchState::from_word(&state), "batch state")?,
            created_ms: to_u64(created),
            attempts,
            next_attempt_ms: next.map(to_u64),
            last_error: error,
            hold_reason: hold,
            run_id: run.map(to_u64),
            events: events.into_iter().map(to_u64).collect(),
        }))
    }

    /// Batches waiting to be tried again whose time has come.
    pub fn retry_batches(&self, now_ms: u64) -> Result<Vec<u64>> {
        let ids: Vec<i64> = self
            .conn()
            .prepare(
                "SELECT id FROM batch WHERE state = 'in_coda' AND coalesce(next_attempt_ms, 0) <= ?1
                 ORDER BY id",
            )?
            .query_map([to_i64(now_ms)], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids.into_iter().map(to_u64).collect())
    }

    /// Mark a batch running again (a retry), with the run trying it.
    pub fn start_batch(&mut self, id: u64, run_id: Option<u64>) -> Result<()> {
        let changed = self.conn().execute(
            "UPDATE batch SET state = 'in_corso', run_id = ?2 WHERE id = ?1 AND state IN ('in_corso', 'in_coda')",
            params![to_i64(id), run_id.map(to_i64)],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownBatch(id));
        }
        Ok(())
    }

    /// Write the batch's annotations and mark its events considered, in one
    /// transaction. A batch already completed writes nothing again.
    pub fn complete_batch(
        &mut self,
        id: u64,
        annotations: Vec<NewAnnotation>,
    ) -> Result<BatchDone> {
        let project_id = self.project_id().to_owned();
        let tx = self.begin()?;
        let state: Option<String> = tx
            .query_row("SELECT state FROM batch WHERE id = ?1", [to_i64(id)], |r| {
                r.get(0)
            })
            .optional()?;
        match state.as_deref() {
            None => return Err(StoreError::UnknownBatch(id)),
            Some("completato") => {
                return Ok(BatchDone {
                    annotations: Vec::new(),
                    already: true,
                });
            }
            Some(_) => {}
        }
        let mut written = Vec::new();
        for annotation in annotations {
            written.push(create_annotation_in(&tx, &project_id, annotation)?);
        }
        tx.execute(
            "UPDATE event SET state = 'considerati' WHERE batch_id = ?1",
            [to_i64(id)],
        )?;
        tx.execute(
            "UPDATE batch SET state = 'completato', last_error = NULL WHERE id = ?1",
            [to_i64(id)],
        )?;
        tx.commit()?;
        Ok(BatchDone {
            annotations: written,
            already: false,
        })
    }

    /// Count a failed attempt. Below `max_attempts` the batch waits until
    /// `retry_at_ms`; at it, the batch and its events are `fallito`.
    pub fn fail_batch(
        &mut self,
        id: u64,
        reason: &str,
        retry_at_ms: u64,
        max_attempts: u32,
    ) -> Result<BatchState> {
        let tx = self.begin()?;
        let attempts: Option<u32> = tx
            .query_row(
                "SELECT attempts FROM batch WHERE id = ?1 AND state IN ('in_corso', 'in_coda')",
                [to_i64(id)],
                |r| r.get(0),
            )
            .optional()?;
        let attempts = attempts
            .ok_or(StoreError::UnknownBatch(id))?
            .saturating_add(1);
        let state = if attempts >= max_attempts {
            BatchState::Failed
        } else {
            BatchState::Retry
        };
        tx.execute(
            "UPDATE batch SET state = ?2, attempts = ?3, next_attempt_ms = ?4, last_error = ?5 WHERE id = ?1",
            params![to_i64(id), state.word(), attempts, to_i64(retry_at_ms), reason],
        )?;
        tx.execute(
            "UPDATE event SET attempts = ?2, next_attempt_ms = ?3, last_error = ?4,
                 state = CASE WHEN ?5 THEN 'fallito' ELSE state END
             WHERE batch_id = ?1",
            params![
                to_i64(id),
                attempts,
                to_i64(retry_at_ms),
                reason,
                state == BatchState::Failed
            ],
        )?;
        tx.commit()?;
        Ok(state)
    }

    /// Hold a batch until [`Store::release_held_batches`]: the CLI does not
    /// trust the project folder yet. Uses no attempt; its events stay queued
    /// in it.
    pub fn hold_batch(&mut self, id: u64, reason: &str) -> Result<()> {
        let changed = self.conn().execute(
            "UPDATE batch SET state = 'sospeso', hold_reason = ?2, next_attempt_ms = NULL
             WHERE id = ?1 AND state IN ('in_corso', 'in_coda', 'sospeso')",
            params![to_i64(id), reason],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownBatch(id));
        }
        Ok(())
    }

    /// Put every held batch back in the queue, due now. Returns their ids.
    pub fn release_held_batches(&mut self, now_ms: u64) -> Result<Vec<u64>> {
        let tx = self.begin()?;
        let ids: Vec<i64> = tx
            .prepare("SELECT id FROM batch WHERE state = 'sospeso' ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        tx.execute(
            "UPDATE batch SET state = 'in_coda', hold_reason = NULL, next_attempt_ms = ?1
             WHERE state = 'sospeso'",
            [to_i64(now_ms)],
        )?;
        tx.commit()?;
        Ok(ids.into_iter().map(to_u64).collect())
    }

    /// The reason the latest held batch or run waits, if any is held: what
    /// `status` shows.
    pub fn hold_reason(&self) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT reason FROM (
                     SELECT hold_reason AS reason, created_ms AS at FROM batch WHERE state = 'sospeso'
                     UNION ALL
                     SELECT reason, started_ms AS at FROM run WHERE outcome = 'held')
                 ORDER BY at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Queue length, when the oldest queued event was created (Unix ms, not
    /// an age), failed batches. `dropped_on_full` is the caller's in-memory
    /// counter. `_now_ms` is kept for callers; the value no longer depends on
    /// it.
    pub fn backlog(&self, _now_ms: u64) -> Result<Backlog> {
        let (queued, oldest): (i64, Option<i64>) = self.conn().query_row(
            "SELECT count(*), min(created_ms) FROM event WHERE state = 'in_coda'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let failed: i64 = self.conn().query_row(
            "SELECT count(*) FROM batch WHERE state = 'fallito'",
            [],
            |r| r.get(0),
        )?;
        let last_failure: Option<String> = self
            .conn()
            .query_row(
                "SELECT last_error FROM batch WHERE last_error IS NOT NULL ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(Backlog {
            queued: u32::try_from(queued).unwrap_or(u32::MAX),
            oldest_queued_ms: oldest.map(to_u64),
            failed_batches: u32::try_from(failed).unwrap_or(u32::MAX),
            dropped_on_full: 0,
            last_failure,
        })
    }

    /// A new run row, `running`.
    pub fn start_run(
        &mut self,
        cycle: Cycle,
        choice: &ModelChoice,
        started_ms: u64,
        pause_epoch: u64,
    ) -> Result<u64> {
        self.conn().execute(
            "INSERT INTO run (project_id, cycle, provider, model, effort, started_ms, outcome, pause_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7)",
            params![
                self.project_id(),
                cycle.word(),
                choice.provider.word(),
                choice.model,
                choice.effort,
                to_i64(started_ms),
                to_i64(pause_epoch)
            ],
        )?;
        Ok(to_u64(self.conn().last_insert_rowid()))
    }

    pub fn finish_run(
        &mut self,
        id: u64,
        outcome: &RunOutcome,
        finished_ms: u64,
        chars_in: u64,
        chars_out: u64,
    ) -> Result<()> {
        let (word, written, rejected, version, reason) = outcome_columns(outcome);
        let changed = self.conn().execute(
            "UPDATE run SET outcome = ?2, written = ?3, rejected = ?4, version = ?5, reason = ?6,
                 finished_ms = ?7, chars_in = ?8, chars_out = ?9 WHERE id = ?1",
            params![
                to_i64(id),
                word,
                written,
                rejected,
                version.map(to_i64),
                reason,
                to_i64(finished_ms),
                to_i64(chars_in),
                to_i64(chars_out)
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownRun(id));
        }
        Ok(())
    }

    /// End a run as [`RunOutcome::Held`]: the CLI does not trust the project
    /// folder yet. Not a failure and not an attempt; the work it was for
    /// stays queued. Same as [`Store::finish_run`] with that outcome, keeping
    /// the run's character counts.
    pub fn hold_run(&mut self, id: u64, reason: &str, now_ms: u64) -> Result<()> {
        let changed = self.conn().execute(
            "UPDATE run SET outcome = 'held', written = 0, rejected = 0, version = NULL,
                 reason = ?2, finished_ms = ?3 WHERE id = ?1",
            params![to_i64(id), reason, to_i64(now_ms)],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownRun(id));
        }
        Ok(())
    }

    /// Whether a run ended held, and why.
    pub fn run_hold(&self, id: u64) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT reason FROM run WHERE id = ?1 AND outcome = 'held'",
                [to_i64(id)],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// The dates a run recovers (spec §7).
    pub fn set_run_recovers(&mut self, id: u64, dates: &[LocalDate]) -> Result<()> {
        for date in dates {
            check_date(*date)?;
        }
        let dates: Vec<String> = dates.iter().map(ToString::to_string).collect();
        let changed = self.conn().execute(
            "UPDATE run SET recovers = ?2 WHERE id = ?1",
            params![to_i64(id), strings_to_json(&dates)],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownRun(id));
        }
        Ok(())
    }

    pub fn run(&self, id: u64) -> Result<Option<RunEntry>> {
        let sql = format!("SELECT {RUN_COLUMNS} FROM run WHERE id = ?1");
        Ok(collect(self.conn(), &sql, [to_i64(id)], run_row)?
            .into_iter()
            .next())
    }

    /// Runs, newest first; of one cycle or all.
    pub fn runs(&self, cycle: Option<Cycle>, limit: usize) -> Result<Vec<RunEntry>> {
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM run WHERE ?1 IS NULL OR cycle = ?1 ORDER BY id DESC LIMIT ?2"
        );
        collect(
            self.conn(),
            &sql,
            params![cycle.map(Cycle::word), to_i64(limit as u64)],
            run_row,
        )
    }

    /// At start-up: every run still `running` was cut short. Marks them
    /// `interrupted` and returns their ids.
    pub fn interrupt_running(&mut self, now_ms: u64) -> Result<Vec<u64>> {
        let tx = self.begin()?;
        let ids: Vec<i64> = tx
            .prepare("SELECT id FROM run WHERE outcome = 'running' ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        tx.execute(
            "UPDATE run SET outcome = 'interrupted', finished_ms = ?1 WHERE outcome = 'running'",
            [to_i64(now_ms)],
        )?;
        tx.execute(
            "UPDATE batch SET state = 'in_coda', next_attempt_ms = ?1 WHERE state = 'in_corso'",
            [to_i64(now_ms)],
        )?;
        tx.commit()?;
        Ok(ids.into_iter().map(to_u64).collect())
    }

    /// Claim (program, date) for a run, as `running`. False when the date
    /// already has a row that is not held: at most one run per program and
    /// local date, and a held date can be claimed again.
    pub fn claim_schedule(
        &mut self,
        program: &str,
        date: LocalDate,
        run_id: Option<u64>,
        now_ms: u64,
    ) -> Result<bool> {
        check_date(date)?;
        let changed = self.conn().execute(
            "INSERT INTO schedule_state (project_id, program, local_date, state, run_id, updated_ms)
             VALUES (?1, ?2, ?3, 'running', ?4, ?5)
             ON CONFLICT (project_id, program, local_date) DO UPDATE SET state = 'running',
                 reason = NULL, run_id = excluded.run_id, updated_ms = excluded.updated_ms
             WHERE schedule_state.state = 'held'",
            params![
                self.project_id(),
                program,
                date.to_string(),
                run_id.map(to_i64),
                to_i64(now_ms)
            ],
        )?;
        Ok(changed > 0)
    }

    /// Set the state of (program, date), creating it if needed.
    pub fn set_schedule(
        &mut self,
        program: &str,
        date: LocalDate,
        state: &ScheduleState,
        run_id: Option<u64>,
        recovered_by: Option<u64>,
        now_ms: u64,
    ) -> Result<()> {
        check_date(date)?;
        let (word, reason) = state.word();
        self.conn().execute(
            "INSERT INTO schedule_state (project_id, program, local_date, state, reason, run_id,
                 recovered_by, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (project_id, program, local_date) DO UPDATE SET state = excluded.state,
                 reason = excluded.reason,
                 run_id = coalesce(excluded.run_id, run_id),
                 recovered_by = coalesce(excluded.recovered_by, recovered_by),
                 updated_ms = excluded.updated_ms",
            params![
                self.project_id(),
                program,
                date.to_string(),
                word,
                reason,
                run_id.map(to_i64),
                recovered_by.map(to_i64),
                to_i64(now_ms)
            ],
        )?;
        Ok(())
    }

    /// A program's rows, by date.
    pub fn schedule(&self, program: &str) -> Result<Vec<ScheduleEntry>> {
        collect(
            self.conn(),
            "SELECT program, local_date, state, reason, run_id, recovered_by, updated_ms
             FROM schedule_state WHERE program = ?1 ORDER BY local_date",
            [program],
            |r| {
                let program: String = r.get(0)?;
                let date: String = r.get(1)?;
                let state: String = r.get(2)?;
                let reason: Option<String> = r.get(3)?;
                let run: Option<i64> = r.get(4)?;
                let recovered: Option<i64> = r.get(5)?;
                let updated: i64 = r.get(6)?;
                Ok((|| {
                    Ok(ScheduleEntry {
                        program,
                        date: LocalDate::parse(&date).ok_or(StoreError::Corrupt("date"))?,
                        state: word(ScheduleState::from_word(&state, reason), "schedule state")?,
                        run_id: run.map(to_u64),
                        recovered_by: recovered.map(to_u64),
                        updated_ms: to_u64(updated),
                    })
                })())
            },
        )
    }

    /// Stage one dream operation of a run.
    pub fn stage(
        &mut self,
        run_id: u64,
        room: Option<Room>,
        payload: &str,
        now_ms: u64,
    ) -> Result<u64> {
        self.conn().execute(
            "INSERT INTO dream_staging (project_id, run_id, room, payload, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![self.project_id(), to_i64(run_id), room.map(Room::word), payload, to_i64(now_ms)],
        )?;
        Ok(to_u64(self.conn().last_insert_rowid()))
    }

    /// A run's staged operations, in order.
    pub fn staged(&self, run_id: u64) -> Result<Vec<StagedOp>> {
        collect(
            self.conn(),
            "SELECT id, run_id, room, payload FROM dream_staging WHERE run_id = ?1 ORDER BY id",
            [to_i64(run_id)],
            |r| {
                let id: i64 = r.get(0)?;
                let run: i64 = r.get(1)?;
                let room: Option<String> = r.get(2)?;
                let payload: String = r.get(3)?;
                Ok((|| {
                    Ok(StagedOp {
                        id: to_u64(id),
                        run_id: to_u64(run),
                        room: room
                            .map(|r| word(Room::from_word(&r), "room"))
                            .transpose()?,
                        payload,
                    })
                })())
            },
        )
    }

    /// Drop the staging of one run, or of every run (at start-up, spec §5.5).
    /// Returns how many operations were dropped.
    pub fn discard_staging(&mut self, run_id: Option<u64>) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM dream_staging WHERE ?1 IS NULL OR run_id = ?1",
            [run_id.map(to_i64)],
        )?)
    }

    /// Create an initial build with a persistent id, `queued`. The options
    /// are stored clamped to their ranges.
    pub fn create_bootstrap(
        &mut self,
        id: &str,
        options: &BootstrapOptions,
        now_ms: u64,
    ) -> Result<()> {
        if !valid_text_key(id) {
            return Err(StoreError::Corrupt("bootstrap id"));
        }
        let options = options_to_json(&options.clone().clamped());
        self.conn().execute(
            "INSERT INTO bootstrap (id, project_id, state, options, started_ms, updated_ms)
             VALUES (?1, ?2, 'queued', ?3, ?4, ?4)",
            params![id, self.project_id(), options, to_i64(now_ms)],
        )?;
        Ok(())
    }

    pub fn set_bootstrap_state(
        &mut self,
        id: &str,
        state: &BootstrapState,
        now_ms: u64,
    ) -> Result<()> {
        let (word, reason) = bootstrap_word(state);
        let changed = self.conn().execute(
            "UPDATE bootstrap SET state = ?2, reason = ?3, updated_ms = ?4 WHERE id = ?1",
            params![id, word, reason, to_i64(now_ms)],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownBootstrap(id.to_owned()));
        }
        Ok(())
    }

    /// One initial build, or the latest when `id` is `None`.
    pub fn bootstrap(&self, id: Option<&str>) -> Result<Option<BootstrapRow>> {
        let row = self
            .conn()
            .query_row(
                "SELECT id, state, reason, options, started_ms, updated_ms FROM bootstrap
                 WHERE ?1 IS NULL OR id = ?1 ORDER BY started_ms DESC, rowid DESC LIMIT 1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(id, state, reason, options, started, updated)| {
            Ok(BootstrapRow {
                id,
                state: bootstrap_state(&state, reason)?,
                options: options_from_json(&options)?,
                started_ms: to_u64(started),
                updated_ms: to_u64(updated),
            })
        })
        .transpose()
    }

    /// Add or update a source in an initial build's manifest.
    pub fn put_bootstrap_source(&mut self, id: &str, source: &BootstrapSource) -> Result<()> {
        self.conn().execute(
            "INSERT INTO bootstrap_source (project_id, bootstrap_id, path, class, hash, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (bootstrap_id, path) DO UPDATE SET class = excluded.class,
                 hash = excluded.hash, state = excluded.state",
            params![
                self.project_id(),
                id,
                source.path,
                source.class.word(),
                source.hash,
                source.state
            ],
        )?;
        Ok(())
    }

    pub fn bootstrap_sources(&self, id: &str) -> Result<Vec<BootstrapSource>> {
        collect(
            self.conn(),
            "SELECT path, class, hash, state FROM bootstrap_source WHERE bootstrap_id = ?1 ORDER BY path",
            [id],
            |r| {
                let path: String = r.get(0)?;
                let class: String = r.get(1)?;
                let hash: Option<String> = r.get(2)?;
                let state: String = r.get(3)?;
                Ok(
                    word(SourceClass::from_word(&class), "source class").map(|class| {
                        BootstrapSource {
                            path,
                            class,
                            hash,
                            state,
                        }
                    }),
                )
            },
        )
    }

    /// Record a packet's checkpoint (replacing the previous one).
    pub fn put_checkpoint(&mut self, id: &str, checkpoint: &Checkpoint) -> Result<()> {
        let project_id = self.project_id().to_owned();
        put_checkpoint_in(self.conn(), &project_id, id, checkpoint)
    }

    pub fn checkpoints(&self, id: &str) -> Result<Vec<Checkpoint>> {
        collect(
            self.conn(),
            "SELECT packet, state, run_id, payload, updated_ms FROM bootstrap_checkpoint
             WHERE bootstrap_id = ?1 ORDER BY packet",
            [id],
            |r| {
                let run: Option<i64> = r.get(2)?;
                let updated: i64 = r.get(4)?;
                Ok(Ok(Checkpoint {
                    packet: r.get(0)?,
                    state: r.get(1)?,
                    run_id: run.map(to_u64),
                    payload: r.get(3)?,
                    updated_ms: to_u64(updated),
                }))
            },
        )
    }
}

/// [`Store::put_checkpoint`] on a connection or inside a caller's
/// transaction (a `Transaction` derefs to its connection).
pub(crate) fn put_checkpoint_in(
    conn: &rusqlite::Connection,
    project_id: &str,
    id: &str,
    checkpoint: &Checkpoint,
) -> Result<()> {
    conn.execute(
        "INSERT INTO bootstrap_checkpoint (project_id, bootstrap_id, packet, state, run_id, payload, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (bootstrap_id, packet) DO UPDATE SET state = excluded.state,
             run_id = excluded.run_id, payload = excluded.payload, updated_ms = excluded.updated_ms",
        params![
            project_id,
            id,
            checkpoint.packet,
            checkpoint.state,
            checkpoint.run_id.map(to_i64),
            checkpoint.payload,
            to_i64(checkpoint.updated_ms)
        ],
    )?;
    Ok(())
}
