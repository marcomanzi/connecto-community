//! The dream schedule (spec §7, D11, ADR 0026): which local dates are due,
//! which run stands for which date, and what the view shows. Plain code over
//! the store and a [`WallClock`]; the engine's scheduler thread calls it at
//! start, every [`TICK`], and when the wall clock jumps (the machine woke
//! up). Nothing runs while the app is closed.
//!
//! - **Program.** One per project ([`PROGRAM`]): the weekdays and local
//!   `HH:MM` of [`DreamSettings`]. A date is *due* once its local date and
//!   time have come, counting only from when the schedule was first seen
//!   (memory turned on), and at most [`CATCH_UP_DAYS`] back.
//! - **Idempotence.** `(program, local date)` has at most one run. Local
//!   date and time are compared as calendar values, never as instants, so an
//!   hour a daylight-saving change repeats finds the date already claimed,
//!   and an hour it skips is passed by the first tick after it: one run for
//!   the date either way.
//! - **Catch-up.** Due dates with no run make **one** cumulative run: it
//!   stands for the latest date and records the earlier ones as recovered by
//!   it ([`crate::Store::set_run_recovers`]).
//! - **Restart.** A date whose run is no longer running (the app stopped mid
//!   run, so the run is `interrupted`) is due again, once, through the same
//!   catch-up.
//! - **Minimum condition.** Fewer than `min_new_items` events and
//!   annotations after the last dream's cutoff: the dates are settled with no
//!   run.
//! - **Held** (the CLI does not trust the folder): the date is not done; it
//!   is tried again [`HOLD_RETRY_MS`] later, as a catch-up. **Paused,
//!   cancelled** (Stop Everything) or **held by a pause**: the dates wait,
//!   and the next check after Resume starts them.

use std::time::Duration;

use connecto_core::memory::{DreamSettings, LocalDate, LocalTime, RunOutcome, SkippedRun, Weekday};

use rusqlite::OptionalExtension;

use crate::error::Result;
use crate::host::WallClock;
use crate::queue::{ScheduleEntry, ScheduleState};
use crate::schema;
use crate::store::Store;

/// The program name in `schedule_state`.
pub const PROGRAM: &str = "dream";
/// How often the scheduler looks.
pub const TICK: Duration = Duration::from_secs(30);
/// A wall clock that moved this much more (or less) than the monotonic one
/// between two looks means the machine slept: look again at once.
pub const JUMP: Duration = Duration::from_secs(60);
/// How far back missed dates are caught up.
pub const CATCH_UP_DAYS: i64 = 31;
/// A held date is tried again after this long.
pub const HOLD_RETRY_MS: u64 = 5 * 60_000;

const SINCE_KEY: &str = "dream_since";
const CUTOFF_EVENT_KEY: &str = "dream_cutoff_event";
const CUTOFF_ANNOTATION_KEY: &str = "dream_cutoff_annotation";

// ---- calendar arithmetic (proleptic Gregorian)

/// Days since 1970-01-01.
#[must_use]
pub fn days_from_civil(date: LocalDate) -> i64 {
    let y = i64::from(date.year) - i64::from(date.month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(date.month);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(date.day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01.
#[must_use]
pub fn civil_from_days(days: i64) -> LocalDate {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    LocalDate {
        year: i32::try_from(year).unwrap_or(i32::MAX),
        month: u8::try_from(month).unwrap_or(1),
        day: u8::try_from(day).unwrap_or(1),
    }
}

/// The weekday of a date.
#[must_use]
pub fn weekday(date: LocalDate) -> Weekday {
    // 1970-01-01 was a Thursday.
    let index = (days_from_civil(date) + 3).rem_euclid(7);
    Weekday::ALL
        .get(usize::try_from(index).unwrap_or(0))
        .copied()
        .unwrap_or(Weekday::Mon)
}

/// A local date and time as minutes since 1970-01-01 00:00 local: an order,
/// not an instant.
#[must_use]
pub fn stamp(date: LocalDate, time: LocalTime) -> i64 {
    days_from_civil(date) * 1440 + i64::from(time.hour()) * 60 + i64::from(time.minute())
}

fn now_stamp(now: &WallClock) -> i64 {
    stamp(now.local_date, now.local_time)
}

// ---- what is due

/// The dream that should start now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The date the run is for: the latest due one.
    pub primary: LocalDate,
    /// Earlier due dates it also stands for (catch-up), oldest first.
    pub recovers: Vec<LocalDate>,
}

impl Plan {
    /// Every date of the plan.
    #[must_use]
    pub fn dates(&self) -> Vec<LocalDate> {
        let mut out = self.recovers.clone();
        out.push(self.primary);
        out
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// Nothing is due.
    Idle,
    /// Dates were due but too little arrived since the last dream: settled
    /// with no run.
    BelowMinimum(Vec<LocalDate>),
    /// Start a dream for this plan.
    Run(Plan),
}

/// The scheduled dates from `since` to `now`, both included, on the
/// program's weekdays, oldest first.
#[must_use]
pub fn due_dates(settings: &DreamSettings, since: i64, now: &WallClock) -> Vec<LocalDate> {
    if settings.days.is_empty() {
        return Vec::new();
    }
    let today = days_from_civil(now.local_date);
    let first = today - CATCH_UP_DAYS;
    let now_at = now_stamp(now);
    (first..=today)
        .map(civil_from_days)
        .filter(|d| settings.days.contains(&weekday(*d)))
        .filter(|d| {
            let at = stamp(*d, settings.time);
            at >= since && at <= now_at
        })
        .collect()
}

/// When the schedule was first seen (a local stamp); set to now the first
/// time.
pub fn since(store: &Store, now: &WallClock) -> Result<i64> {
    let stored = schema::meta_u64(store.conn(), SINCE_KEY)?;
    if stored == 0 {
        let at = now_stamp(now);
        schema::set_meta_u64(
            store.conn(),
            SINCE_KEY,
            u64::try_from(at).unwrap_or(1).max(1),
        )?;
        return Ok(at);
    }
    Ok(i64::try_from(stored).unwrap_or(i64::MAX))
}

/// Whether a date's row leaves it to run.
fn pending(row: Option<&ScheduleEntry>, store: &Store, now: &WallClock) -> Result<bool> {
    let Some(row) = row else {
        return Ok(true);
    };
    Ok(match &row.state {
        ScheduleState::Done | ScheduleState::Recovered => false,
        ScheduleState::Missed => true,
        ScheduleState::Held { .. } => row.updated_ms.saturating_add(HOLD_RETRY_MS) <= now.utc_ms,
        // Running in this process, or cut short by a stop: due again once
        // its run is no longer running.
        ScheduleState::Running => match row.run_id {
            Some(id) => store.run(id)?.is_none_or(|r| !r.outcome.is_running()),
            None => true,
        },
    })
}

/// Events and annotations after the last dream's cutoff.
pub fn new_items(store: &Store) -> Result<u64> {
    let read = store.read();
    let events = read.event_seq()?;
    let annotations = read.annotation_seq()?;
    let since_events = schema::meta_u64(store.conn(), CUTOFF_EVENT_KEY)?;
    let since_annotations = schema::meta_u64(store.conn(), CUTOFF_ANNOTATION_KEY)?;
    Ok(events.saturating_sub(since_events) + annotations.saturating_sub(since_annotations))
}

/// Record a finished dream's cutoff: what it has seen.
pub fn set_cutoff(store: &Store, events: u64, annotations: u64) -> Result<()> {
    let keep = |key: &str, value: u64| -> Result<()> {
        let old = schema::meta_u64(store.conn(), key)?;
        schema::set_meta_u64(store.conn(), key, old.max(value))
    };
    keep(CUTOFF_EVENT_KEY, events)?;
    keep(CUTOFF_ANNOTATION_KEY, annotations)
}

const PENDING_PREFIX: &str = "dream.pending.";

/// Run `f` as one transaction over the store's plain writes (staging, run
/// rows, schedule rows, `meta`): all or nothing. `f` must not call a store
/// method that opens its own transaction.
pub(crate) fn in_tx<R>(store: &mut Store, f: impl FnOnce(&mut Store) -> Result<R>) -> Result<R> {
    store.conn().execute_batch("BEGIN IMMEDIATE")?;
    match f(store) {
        Ok(value) => match store.conn().execute_batch("COMMIT") {
            Ok(()) => Ok(value),
            Err(e) => {
                let _ = store.conn().execute_batch("ROLLBACK");
                Err(e.into())
            }
        },
        Err(e) => {
            let _ = store.conn().execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Before a dream publishes: what its books must record once the
/// publication is in (its cutoff), kept until they are written, so a crash
/// between the two is finished by [`reconcile`].
pub fn mark_pending(store: &Store, run_id: u64, events: u64, annotations: u64) -> Result<()> {
    store.conn().execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        rusqlite::params![
            format!("{PENDING_PREFIX}{run_id}"),
            format!("{events},{annotations}")
        ],
    )?;
    Ok(())
}

/// The run's books are written: forget its pending record.
pub fn clear_pending(store: &Store, run_id: u64) -> Result<()> {
    store.conn().execute(
        "DELETE FROM meta WHERE key = ?1",
        [format!("{PENDING_PREFIX}{run_id}")],
    )?;
    Ok(())
}

/// Finish the books of dreams whose publication committed but whose process
/// stopped before writing the rest: the run becomes `Published` (not
/// interrupted), the cutoff advances, its dates are settled (so the date is
/// not dreamt again), its staging is dropped. A dream still running in this
/// process is left alone; one that stopped before its publication committed
/// only loses its record (its date is caught up as usual). Returns the runs
/// finished. One transaction.
pub fn reconcile(store: &mut Store, now_ms: u64) -> Result<Vec<u64>> {
    in_tx(store, |store| {
        let pending: Vec<(String, String)> = store
            .conn()
            .prepare("SELECT key, value FROM meta WHERE key LIKE 'dream.pending.%' ORDER BY key")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut finished = Vec::new();
        for (key, value) in pending {
            let Some(run_id) = key
                .strip_prefix(PENDING_PREFIX)
                .and_then(|n| n.parse::<u64>().ok())
            else {
                store
                    .conn()
                    .execute("DELETE FROM meta WHERE key = ?1", [&key])?;
                continue;
            };
            let run = store.run(run_id)?;
            if run.as_ref().is_some_and(|r| r.outcome.is_running()) {
                continue;
            }
            let published: Option<(i64, i64)> = store
                .conn()
                .query_row(
                    "SELECT version, operations FROM publication
                     WHERE run_id = ?1 AND reverts IS NULL",
                    [i64::try_from(run_id).unwrap_or(i64::MAX)],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let (Some(run), Some((version, operations))) = (run, published)
                && matches!(run.outcome, RunOutcome::Interrupted)
            {
                let outcome = RunOutcome::Published {
                    version: u64::try_from(version).unwrap_or(0),
                    applied: u32::try_from(operations).unwrap_or(u32::MAX),
                    rejected: 0,
                };
                store.finish_run(run_id, &outcome, now_ms, run.chars_in, run.chars_out)?;
                let mut cut = value.split(',').map(|n| n.parse::<u64>().unwrap_or(0));
                let (events, annotations) = (cut.next().unwrap_or(0), cut.next().unwrap_or(0));
                set_cutoff(store, events, annotations)?;
                let primary = store
                    .schedule(PROGRAM)?
                    .into_iter()
                    .find(|r| r.run_id == Some(run_id) && r.state == ScheduleState::Running);
                if let Some(row) = primary {
                    let plan = Plan {
                        primary: row.date,
                        recovers: run.recovers.clone(),
                    };
                    settle(store, &plan, run_id, &outcome, now_ms)?;
                }
                store.discard_staging(Some(run_id))?;
                finished.push(run_id);
            }
            clear_pending(store, run_id)?;
        }
        Ok(finished)
    })
}

/// Look at the schedule: what is due, and whether it should run. Earlier
/// due dates with no row are recorded as missed (they show as skipped);
/// dates below the minimum condition are settled with no run.
pub fn check(store: &mut Store, settings: &DreamSettings, now: &WallClock) -> Result<Check> {
    reconcile(store, now.utc_ms)?;
    let since = since(store, now)?;
    let rows = store.schedule(PROGRAM)?;
    let mut due = Vec::new();
    for date in due_dates(settings, since, now) {
        if pending(rows.iter().find(|r| r.date == date), store, now)? {
            due.push(date);
        }
    }
    let Some(primary) = due.last().copied() else {
        return Ok(Check::Idle);
    };
    if new_items(store)? < u64::from(settings.min_new_items) {
        for date in &due {
            store.set_schedule(PROGRAM, *date, &ScheduleState::Done, None, None, now.utc_ms)?;
        }
        return Ok(Check::BelowMinimum(due));
    }
    let recovers: Vec<LocalDate> = due.iter().copied().filter(|d| *d != primary).collect();
    for date in &recovers {
        let row = rows.iter().find(|r| r.date == *date);
        if row.is_none() {
            store.set_schedule(
                PROGRAM,
                *date,
                &ScheduleState::Missed,
                None,
                None,
                now.utc_ms,
            )?;
        }
    }
    Ok(Check::Run(Plan { primary, recovers }))
}

/// Whether the plan's date is still free to claim (nothing claimed it
/// meanwhile).
pub fn claimable(store: &Store, plan: &Plan, now: &WallClock) -> Result<bool> {
    let rows = store.schedule(PROGRAM)?;
    pending(rows.iter().find(|r| r.date == plan.primary), store, now)
}

/// Claim the plan's dates for run `run_id`: the primary as running, the
/// rest as recovered by it once it ends. False when the primary was claimed
/// meanwhile (nothing changed).
pub fn claim(store: &mut Store, plan: &Plan, run_id: u64, now: &WallClock) -> Result<bool> {
    let rows = store.schedule(PROGRAM)?;
    let row = rows.iter().find(|r| r.date == plan.primary);
    if !pending(row, store, now)? {
        return Ok(false);
    }
    store.set_schedule(
        PROGRAM,
        plan.primary,
        &ScheduleState::Running,
        Some(run_id),
        None,
        now.utc_ms,
    )?;
    if !plan.recovers.is_empty() {
        store.set_run_recovers(run_id, &plan.recovers)?;
    }
    Ok(true)
}

/// Settle the plan's dates once its run ended with `outcome`.
pub fn settle(
    store: &mut Store,
    plan: &Plan,
    run_id: u64,
    outcome: &RunOutcome,
    now_ms: u64,
) -> Result<()> {
    let (primary, others) = match outcome {
        RunOutcome::Published { .. }
        | RunOutcome::NoChange
        | RunOutcome::Refused { .. }
        | RunOutcome::Failed { .. } => (ScheduleState::Done, Some(ScheduleState::Recovered)),
        RunOutcome::Held { reason } => (
            ScheduleState::Held {
                reason: reason.clone(),
            },
            None,
        ),
        RunOutcome::HeldByPause
        | RunOutcome::Cancelled
        | RunOutcome::Interrupted
        | RunOutcome::Running
        | RunOutcome::Annotated { .. } => (ScheduleState::Missed, None),
    };
    store.set_schedule(PROGRAM, plan.primary, &primary, Some(run_id), None, now_ms)?;
    for date in &plan.recovers {
        match &others {
            Some(state) => {
                store.set_schedule(PROGRAM, *date, state, None, Some(run_id), now_ms)?;
            }
            None => store.set_schedule(PROGRAM, *date, &primary, None, None, now_ms)?,
        }
    }
    if others.is_none() && !plan.recovers.is_empty() {
        store.set_run_recovers(run_id, &[])?;
    }
    Ok(())
}

/// What the view shows: the next scheduled dream (Unix ms; now when a due
/// date waits) and the dates skipped, newest first.
pub fn view(
    store: &Store,
    settings: &DreamSettings,
    now: &WallClock,
) -> Result<(Option<u64>, Vec<SkippedRun>)> {
    let rows = store.schedule(PROGRAM)?;
    let today = days_from_civil(now.local_date);
    let mut skipped: Vec<SkippedRun> = rows
        .iter()
        .filter(|r| days_from_civil(r.date) >= today - CATCH_UP_DAYS)
        .filter_map(|r| match r.state {
            ScheduleState::Missed | ScheduleState::Held { .. } if r.date != now.local_date => {
                Some(SkippedRun {
                    date: r.date,
                    recovered_by: None,
                })
            }
            ScheduleState::Recovered => Some(SkippedRun {
                date: r.date,
                recovered_by: r.recovered_by,
            }),
            _ => None,
        })
        .collect();
    skipped.sort_by_key(|s| std::cmp::Reverse(s.date));
    if settings.days.is_empty() {
        return Ok((None, skipped));
    }
    let since = i64::try_from(schema::meta_u64(store.conn(), SINCE_KEY)?).unwrap_or(0);
    for date in due_dates(settings, since, now) {
        if pending(rows.iter().find(|r| r.date == date), store, now)? {
            return Ok((Some(now.utc_ms), skipped));
        }
    }
    let now_at = now_stamp(now);
    for offset in 0..=7 {
        let date = civil_from_days(today + offset);
        let at = stamp(date, settings.time);
        let done = rows
            .iter()
            .any(|r| r.date == date && !matches!(r.state, ScheduleState::Missed));
        if settings.days.contains(&weekday(date)) && at > now_at && !done {
            let ahead = u64::try_from(at - now_at).unwrap_or(0);
            return Ok((Some(now.utc_ms.saturating_add(ahead * 60_000)), skipped));
        }
    }
    Ok((None, skipped))
}

/// At start-up, before any dream of this process: the staging of runs cut
/// short is dropped (spec §5.5). Their schedule rows become due again
/// through [`check`] once their run is marked interrupted.
pub fn recover_at_start(store: &mut Store) -> Result<usize> {
    store.discard_staging(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn date(text: &str) -> LocalDate {
        LocalDate::parse(text).unwrap()
    }

    #[test]
    fn calendar_round_trips_and_knows_weekdays() {
        for d in [
            "1970-01-01",
            "2000-02-29",
            "2026-11-14",
            "2026-03-29",
            "2400-12-31",
            "1969-12-31",
        ] {
            let x = date(d);
            assert_eq!(civil_from_days(days_from_civil(x)), x, "{d}");
        }
        assert_eq!(days_from_civil(date("1970-01-01")), 0);
        assert_eq!(weekday(date("1970-01-01")), Weekday::Thu);
        assert_eq!(weekday(date("2026-11-14")), Weekday::Sat);
        assert_eq!(weekday(date("2026-11-16")), Weekday::Mon);
        assert_eq!(
            days_from_civil(date("2026-03-01")) - days_from_civil(date("2026-02-28")),
            1
        );
    }
}
