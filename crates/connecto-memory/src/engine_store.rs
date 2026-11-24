//! Store primitives the engine added on top of the queue (T4-T6): an event
//! with its evidence in one transaction, a batch put back with no attempt
//! used, freshness of the whole memory, and small engine state in `meta`.
//! Kept apart from `queue.rs` so the two evolve without stepping on each
//! other.

use connecto_core::memory::Evidence;
use rusqlite::{OptionalExtension, params};

use crate::codec::{to_i64, to_u64};
use crate::error::{Result, StoreError};
use crate::gate::RunGate;
use crate::queue::{BatchDone, BatchState, Enqueued, EventState, NewEvent};
use crate::schema;
use crate::store::{NewAnnotation, NewEvidence, Store, create_annotation_in, insert_evidence_in};

/// How a run ended, for the gated writes that finish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunEnd {
    pub run: u64,
    pub finished_ms: u64,
    pub chars_in: u64,
    pub chars_out: u64,
    /// For a batch completion: annotations the batch's runs staged (written
    /// plus dropped), so rejected = staged - written.
    pub staged: u32,
}

/// Finish a run inside the caller's transaction (the columns
/// `Store::finish_run` writes).
fn finish_run_in(
    tx: &rusqlite::Transaction<'_>,
    run: &RunEnd,
    outcome: &str,
    written: u32,
    rejected: u32,
    reason: Option<&str>,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE run SET outcome = ?2, written = ?3, rejected = ?4, version = NULL, reason = ?5,
             finished_ms = ?6, chars_in = ?7, chars_out = ?8 WHERE id = ?1",
        params![
            to_i64(run.run),
            outcome,
            written,
            rejected,
            reason,
            to_i64(run.finished_ms),
            to_i64(run.chars_in),
            to_i64(run.chars_out)
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::UnknownRun(run.run));
    }
    Ok(())
}

fn valid_text_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 256 && !key.chars().any(char::is_control)
}

impl Store {
    /// [`Store::complete_batch`] behind a [`RunGate`]: the annotations are
    /// inserted and the events marked considered in one transaction, which
    /// commits only if the gate is still open right before the commit
    /// (otherwise [`StoreError::Aborted`] and nothing is written). The
    /// batch's staged part results (`progress_key`) are cleared in the same
    /// transaction. A batch already completed writes nothing again.
    ///
    /// `run`, when given, is finished as `annotated` (written: what was
    /// inserted; rejected: `run.staged` minus that) in the same transaction.
    pub fn complete_batch_gated(
        &mut self,
        id: u64,
        annotations: Vec<NewAnnotation>,
        progress_key: &str,
        run: Option<RunEnd>,
        gate: &RunGate,
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
        tx.execute(
            "DELETE FROM meta WHERE key = ?1",
            [format!("engine.{progress_key}")],
        )?;
        if let Some(run) = run {
            let n = u32::try_from(written.len()).unwrap_or(u32::MAX);
            finish_run_in(
                &tx,
                &run,
                "annotated",
                n,
                run.staged.saturating_sub(n),
                None,
            )?;
        }
        // The last moment: a pause or a cancel that came while this job
        // waited behind others, or while it ran, stops it here.
        gate.check()?;
        tx.commit()?;
        Ok(BatchDone {
            annotations: written,
            already: false,
        })
    }

    /// Stage one finished part of a batch (its validated annotations, in
    /// `value`) and finish its run as `annotated` (written 0: nothing is
    /// inserted before the last part), in one transaction behind the gate.
    pub fn stage_part_gated(
        &mut self,
        progress_key: &str,
        value: &str,
        run: RunEnd,
        rejected: u32,
        gate: &RunGate,
    ) -> Result<()> {
        let tx = self.begin()?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![format!("engine.{progress_key}"), value],
        )?;
        finish_run_in(&tx, &run, "annotated", 0, rejected, None)?;
        gate.check()?;
        tx.commit()?;
        Ok(())
    }

    /// Count a failed attempt (as [`Store::fail_batch`]) and finish the run as
    /// failed with `reason`, in one transaction behind the gate. On the last
    /// attempt the batch's staged parts (`progress_key`) are dropped too.
    #[allow(clippy::too_many_arguments)]
    pub fn fail_batch_gated(
        &mut self,
        id: u64,
        reason: &str,
        retry_at_ms: u64,
        max_attempts: u32,
        progress_key: &str,
        run: RunEnd,
        gate: &RunGate,
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
        let failed = attempts >= max_attempts;
        let state = if failed {
            BatchState::Failed
        } else {
            BatchState::Retry
        };
        let word = if failed { "fallito" } else { "in_coda" };
        tx.execute(
            "UPDATE batch SET state = ?2, attempts = ?3, next_attempt_ms = ?4, last_error = ?5 WHERE id = ?1",
            params![to_i64(id), word, attempts, to_i64(retry_at_ms), reason],
        )?;
        tx.execute(
            "UPDATE event SET attempts = ?2, next_attempt_ms = ?3, last_error = ?4,
                 state = CASE WHEN ?5 THEN 'fallito' ELSE state END
             WHERE batch_id = ?1",
            params![to_i64(id), attempts, to_i64(retry_at_ms), reason, failed],
        )?;
        if failed {
            tx.execute(
                "DELETE FROM meta WHERE key = ?1",
                [format!("engine.{progress_key}")],
            )?;
        }
        let shown = if failed {
            format!("{reason} (last attempt)")
        } else {
            reason.to_owned()
        };
        finish_run_in(&tx, &run, "failed", 0, 0, Some(&shown))?;
        gate.check()?;
        tx.commit()?;
        Ok(state)
    }

    /// Hold a batch and its run (the CLI does not trust the folder), in one
    /// transaction behind the gate. Uses no attempt.
    pub fn hold_gated(&mut self, id: u64, reason: &str, run: RunEnd, gate: &RunGate) -> Result<()> {
        let tx = self.begin()?;
        let changed = tx.execute(
            "UPDATE batch SET state = 'sospeso', hold_reason = ?2, next_attempt_ms = NULL
             WHERE id = ?1 AND state IN ('in_corso', 'in_coda', 'sospeso')",
            params![to_i64(id), reason],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownBatch(id));
        }
        finish_run_in(&tx, &run, "held", 0, 0, Some(reason))?;
        gate.check()?;
        tx.commit()?;
        Ok(())
    }

    /// Queue an event and store the evidence it gives (`e-<seq>`), in one
    /// transaction, so an event never exists without its evidence or the
    /// other way round. A key already seen queues nothing and stores no
    /// evidence. The evidence's `event_seq` is set here.
    pub fn enqueue_with_evidence(
        &mut self,
        event: NewEvent,
        evidence: Option<NewEvidence>,
    ) -> Result<(Enqueued, Option<Evidence>)> {
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
            return Ok((Enqueued::Duplicate(to_u64(seq)), None));
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
        let evidence = match evidence {
            Some(mut new) => {
                new.event_seq = Some(seq);
                Some(insert_evidence_in(&tx, &project_id, new)?)
            }
            None => None,
        };
        tx.commit()?;
        Ok((Enqueued::Inserted(seq), evidence))
    }

    /// Put a running batch back in the queue, due at `now_ms`, without
    /// counting an attempt: the run was cancelled or a pause came, which is
    /// not the batch's fault.
    pub fn requeue_batch(&mut self, id: u64, now_ms: u64) -> Result<()> {
        let changed = self.conn().execute(
            "UPDATE batch SET state = 'in_coda', next_attempt_ms = ?2
             WHERE id = ?1 AND state IN ('in_corso', 'in_coda')",
            params![to_i64(id), to_i64(now_ms)],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownBatch(id));
        }
        Ok(())
    }

    /// Unix milliseconds of the latest event the development cycle has
    /// considered: the freshness of the memory as a whole.
    pub fn considered_through_ms(&self) -> Result<Option<u64>> {
        let latest: Option<i64> = self.conn().query_row(
            "SELECT max(created_ms) FROM event WHERE state = 'considerati'",
            [],
            |r| r.get(0),
        )?;
        Ok(latest.map(to_u64))
    }

    /// A value the cycles keep between runs (the last git snapshot, ...),
    /// under `engine.<key>` in `meta`.
    pub fn engine_state(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [format!("engine.{key}")],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_engine_state(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![format!("engine.{key}"), value],
        )?;
        Ok(())
    }
}
