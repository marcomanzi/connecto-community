//! The dream schedule over a fake clock (spec §7, §11.2 "Scheduler"):
//! daylight-saving changes both ways, a restart right at the scheduled time,
//! three days skipped, held runs, the minimum condition. Always one run per
//! date and a single catch-up run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use connecto_core::Redacted;
use connecto_core::memory::{
    Cycle, DreamSettings, EvidenceKind, LocalDate, LocalTime, ModelChoice, Provider, RunOutcome,
    Weekday,
};
use connecto_memory::scheduler::{self, Check, Plan};
use connecto_memory::{NewEvidence, ScheduleState, Store, WallClock};

fn date(text: &str) -> LocalDate {
    LocalDate::parse(text).unwrap()
}

/// A wall clock at local `date hh:mm` with the given UTC offset in hours.
fn at(day: &str, hour: u8, minute: u8, offset_hours: i32) -> WallClock {
    let d = date(day);
    let t = LocalTime::new(hour, minute).unwrap();
    let local_secs = scheduler::stamp(d, t) * 60;
    let utc = local_secs - i64::from(offset_hours) * 3600;
    WallClock {
        utc_ms: u64::try_from(utc).unwrap() * 1000,
        local_date: d,
        local_time: t,
        weekday: scheduler::weekday(d),
        utc_offset_secs: offset_hours * 3600,
    }
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("memory.db"), "proj-a").unwrap();
    (dir, store)
}

fn something_new(store: &mut Store) {
    store
        .insert_evidence(NewEvidence::new(
            EvidenceKind::UserStatement,
            1,
            Redacted::assume("x".to_owned()),
        ))
        .unwrap();
}

fn every_day() -> DreamSettings {
    DreamSettings {
        days: Weekday::ALL.iter().copied().collect(),
        min_new_items: 0,
        ..DreamSettings::default()
    }
}

/// What the engine does with a plan: a run row, the claim, then the end.
fn run(store: &mut Store, plan: &Plan, now: &WallClock, outcome: &RunOutcome) -> u64 {
    let choice = ModelChoice::new(Provider::Claude, "sonnet", "medium");
    let id = store
        .start_run(Cycle::Dream, &choice, now.utc_ms, 0)
        .unwrap();
    assert!(scheduler::claim(store, plan, id, now).unwrap());
    if !outcome.is_running() {
        store.finish_run(id, outcome, now.utc_ms, 1, 1).unwrap();
        scheduler::settle(store, plan, id, outcome, now.utc_ms).unwrap();
    }
    id
}

/// Tick through `clocks`; run (to `NoChange`) whatever is due. Returns the
/// plans that ran.
fn tick_through(store: &mut Store, settings: &DreamSettings, clocks: &[WallClock]) -> Vec<Plan> {
    let mut ran = Vec::new();
    for now in clocks {
        if let Check::Run(plan) = scheduler::check(store, settings, now).unwrap() {
            run(store, &plan, now, &RunOutcome::NoChange);
            ran.push(plan);
        }
    }
    ran
}

#[test]
fn a_date_runs_once_when_its_time_comes() {
    let (_d, mut s) = store();
    let settings = every_day();
    let ran = tick_through(
        &mut s,
        &settings,
        &[
            at("2026-11-16", 1, 0, 1),
            at("2026-11-16", 2, 29, 1),
            at("2026-11-16", 2, 30, 1),
            at("2026-11-16", 2, 31, 1),
            at("2026-11-16", 23, 59, 1),
            at("2026-11-17", 2, 0, 1),
            at("2026-11-17", 2, 45, 1),
        ],
    );
    let dates: Vec<LocalDate> = ran.iter().map(|p| p.primary).collect();
    assert_eq!(dates, [date("2026-11-16"), date("2026-11-17")]);
    assert!(ran.iter().all(|p| p.recovers.is_empty()));
}

#[test]
fn spring_forward_skips_the_hour_and_still_runs_once() {
    // Europe, 2026-03-29: 02:00 local becomes 03:00; 02:30 never happens.
    let (_d, mut s) = store();
    let settings = every_day();
    let ran = tick_through(
        &mut s,
        &settings,
        &[
            at("2026-03-29", 1, 0, 1),
            at("2026-03-29", 1, 59, 1),
            at("2026-03-29", 3, 0, 2),
            at("2026-03-29", 3, 30, 2),
            at("2026-03-29", 4, 0, 2),
        ],
    );
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].primary, date("2026-03-29"));
}

#[test]
fn fall_back_repeats_the_hour_and_still_runs_once() {
    // Europe, 2026-10-25: 03:00 local becomes 02:00; 02:30 happens twice.
    let (_d, mut s) = store();
    let settings = every_day();
    let ran = tick_through(
        &mut s,
        &settings,
        &[
            at("2026-10-25", 1, 0, 2),
            at("2026-10-25", 2, 30, 2),
            at("2026-10-25", 2, 59, 2),
            at("2026-10-25", 2, 0, 1),
            at("2026-10-25", 2, 30, 1),
            at("2026-10-25", 2, 45, 1),
        ],
    );
    assert_eq!(ran.len(), 1);
    let rows = s.schedule(scheduler::PROGRAM).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, ScheduleState::Done);
}

#[test]
fn a_restart_right_at_the_scheduled_time_relaunches_once() {
    let (_d, mut s) = store();
    let settings = every_day();
    assert_eq!(
        scheduler::check(&mut s, &settings, &at("2026-11-16", 2, 0, 1)).unwrap(),
        Check::Idle
    );
    let now = at("2026-11-16", 2, 30, 1);
    let Check::Run(plan) = scheduler::check(&mut s, &settings, &now).unwrap() else {
        panic!("due")
    };
    // The run starts, and the app stops in the middle of it.
    let first = run(&mut s, &plan, &now, &RunOutcome::Running);
    // While it runs, nothing else is due for that date.
    assert_eq!(
        scheduler::check(&mut s, &settings, &now).unwrap(),
        Check::Idle
    );
    // Restart.
    assert_eq!(s.interrupt_running(now.utc_ms).unwrap(), [first]);
    scheduler::recover_at_start(&mut s).unwrap();
    let later = at("2026-11-16", 2, 31, 1);
    let Check::Run(again) = scheduler::check(&mut s, &settings, &later).unwrap() else {
        panic!("relaunched")
    };
    assert_eq!(again.primary, date("2026-11-16"));
    assert!(again.recovers.is_empty());
    run(&mut s, &again, &later, &RunOutcome::NoChange);
    assert_eq!(
        scheduler::check(&mut s, &settings, &at("2026-11-16", 3, 0, 1)).unwrap(),
        Check::Idle
    );
    // Two run rows for the date: the interrupted one and its relaunch.
    let runs = s.runs(Some(Cycle::Dream), 10).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].outcome, RunOutcome::Interrupted);
}

#[test]
fn three_skipped_days_make_one_catch_up_run() {
    let (_d, mut s) = store();
    let settings = DreamSettings {
        min_new_items: 0,
        ..DreamSettings::default()
    };
    // Seen first on Monday 2026-11-16 at 01:00; the app closes.
    assert_eq!(
        scheduler::check(&mut s, &settings, &at("2026-11-16", 1, 0, 1)).unwrap(),
        Check::Idle
    );
    // It opens again on Thursday at 10:00.
    let now = at("2026-11-19", 10, 0, 1);
    let Check::Run(plan) = scheduler::check(&mut s, &settings, &now).unwrap() else {
        panic!("due")
    };
    assert_eq!(plan.primary, date("2026-11-19"));
    assert_eq!(
        plan.recovers,
        [date("2026-11-16"), date("2026-11-17"), date("2026-11-18")]
    );
    // Before the run ends, the skipped dates show as skipped.
    let (next, skipped) = scheduler::view(&s, &settings, &now).unwrap();
    assert_eq!(next, Some(now.utc_ms));
    assert_eq!(skipped.len(), 3);
    assert!(skipped.iter().all(|r| r.recovered_by.is_none()));
    let id = run(
        &mut s,
        &plan,
        &now,
        &RunOutcome::Published {
            version: 1,
            applied: 1,
            rejected: 0,
        },
    );
    let entry = s.run(id).unwrap().unwrap();
    assert_eq!(entry.recovers, plan.recovers);
    // Nothing else is due; one run for four dates.
    assert_eq!(
        scheduler::check(&mut s, &settings, &now).unwrap(),
        Check::Idle
    );
    assert_eq!(s.runs(Some(Cycle::Dream), 10).unwrap().len(), 1);
    let rows = s.schedule(scheduler::PROGRAM).unwrap();
    let states: Vec<(LocalDate, ScheduleState, Option<u64>)> = rows
        .into_iter()
        .map(|r| (r.date, r.state, r.recovered_by))
        .collect();
    assert_eq!(
        states,
        [
            (date("2026-11-16"), ScheduleState::Recovered, Some(id)),
            (date("2026-11-17"), ScheduleState::Recovered, Some(id)),
            (date("2026-11-18"), ScheduleState::Recovered, Some(id)),
            (date("2026-11-19"), ScheduleState::Done, None),
        ]
    );
    let (next, skipped) = scheduler::view(&s, &settings, &now).unwrap();
    assert_eq!(skipped.len(), 3);
    assert!(skipped.iter().all(|r| r.recovered_by == Some(id)));
    // The next one is Friday 02:30 local.
    let friday = at("2026-11-20", 2, 30, 1);
    assert_eq!(next, Some(friday.utc_ms));
    // Weekends are not on the default program.
    let ran = tick_through(
        &mut s,
        &settings,
        &[
            at("2026-11-20", 2, 30, 1),
            at("2026-11-21", 2, 30, 1),
            at("2026-11-22", 2, 30, 1),
            at("2026-11-23", 2, 30, 1),
        ],
    );
    let dates: BTreeSet<LocalDate> = ran.iter().map(|p| p.primary).collect();
    assert_eq!(dates, [date("2026-11-20"), date("2026-11-23")].into());
}

#[test]
fn dates_before_the_schedule_was_first_seen_are_not_caught_up() {
    let (_d, mut s) = store();
    let settings = every_day();
    // First seen at 10:00: today's 02:30 had passed already.
    assert_eq!(
        scheduler::check(&mut s, &settings, &at("2026-11-16", 10, 0, 1)).unwrap(),
        Check::Idle
    );
}

#[test]
fn a_held_date_is_not_done_and_starts_again_later_as_catch_up() {
    let (_d, mut s) = store();
    let settings = every_day();
    scheduler::check(&mut s, &settings, &at("2026-11-16", 1, 0, 1)).unwrap();
    let now = at("2026-11-16", 2, 30, 1);
    let Check::Run(plan) = scheduler::check(&mut s, &settings, &now).unwrap() else {
        panic!()
    };
    let held = RunOutcome::Held {
        reason: "the CLI has not been trusted for this project folder yet".to_owned(),
    };
    run(&mut s, &plan, &now, &held);
    let rows = s.schedule(scheduler::PROGRAM).unwrap();
    assert!(matches!(rows[0].state, ScheduleState::Held { .. }));
    assert!(!rows[0].state.counts_as_done());
    // Not retried in a loop...
    assert_eq!(
        scheduler::check(&mut s, &settings, &at("2026-11-16", 2, 31, 1)).unwrap(),
        Check::Idle
    );
    // ...but at the first look after the hold delay.
    let later = at("2026-11-16", 2, 36, 1);
    let Check::Run(again) = scheduler::check(&mut s, &settings, &later).unwrap() else {
        panic!("held date starts again")
    };
    assert_eq!(again.primary, date("2026-11-16"));
    run(&mut s, &again, &later, &RunOutcome::NoChange);
    assert_eq!(
        scheduler::check(&mut s, &settings, &later).unwrap(),
        Check::Idle
    );
}

#[test]
fn a_pause_or_a_stop_leaves_the_date_due() {
    let (_d, mut s) = store();
    let settings = every_day();
    scheduler::check(&mut s, &settings, &at("2026-11-16", 1, 0, 1)).unwrap();
    let now = at("2026-11-16", 2, 30, 1);
    for outcome in [RunOutcome::HeldByPause, RunOutcome::Cancelled] {
        let Check::Run(plan) = scheduler::check(&mut s, &settings, &now).unwrap() else {
            panic!("{outcome:?}")
        };
        run(&mut s, &plan, &now, &outcome);
    }
    let Check::Run(plan) = scheduler::check(&mut s, &settings, &now).unwrap() else {
        panic!()
    };
    run(&mut s, &plan, &now, &RunOutcome::NoChange);
    assert_eq!(
        scheduler::check(&mut s, &settings, &now).unwrap(),
        Check::Idle
    );
}

#[test]
fn the_minimum_condition_settles_dates_without_a_run() {
    let (_d, mut s) = store();
    let settings = DreamSettings {
        min_new_items: 1,
        ..every_day()
    };
    scheduler::check(&mut s, &settings, &at("2026-11-16", 1, 0, 1)).unwrap();
    let now = at("2026-11-16", 2, 30, 1);
    assert_eq!(
        scheduler::check(&mut s, &settings, &now).unwrap(),
        Check::BelowMinimum(vec![date("2026-11-16")])
    );
    assert_eq!(
        scheduler::check(&mut s, &settings, &now).unwrap(),
        Check::Idle
    );
    // Something new, and the next date runs.
    something_new(&mut s);
    let tomorrow = at("2026-11-17", 2, 30, 1);
    assert!(matches!(
        scheduler::check(&mut s, &settings, &tomorrow).unwrap(),
        Check::Run(_)
    ));
    // A dream's cutoff makes what it saw old.
    scheduler::set_cutoff(&s, 1, 0).unwrap();
    assert_eq!(scheduler::new_items(&s).unwrap(), 0);
}

#[test]
fn no_days_means_no_schedule() {
    let (_d, mut s) = store();
    let settings = DreamSettings {
        days: BTreeSet::new(),
        ..every_day()
    };
    scheduler::check(&mut s, &settings, &at("2026-11-16", 1, 0, 1)).unwrap();
    assert_eq!(
        scheduler::check(&mut s, &settings, &at("2026-11-18", 9, 0, 1)).unwrap(),
        Check::Idle
    );
    assert_eq!(
        scheduler::view(&s, &settings, &at("2026-11-18", 9, 0, 1))
            .unwrap()
            .0,
        None
    );
}
