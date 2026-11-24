//! The event queue, batches, runs, schedule state, staging and the initial
//! build's rows: durable primitives (spec §5.3-§5.5, §5.9, §7, §11.2
//! "Idempotenza", "Riavvio").

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Fixture, PROJECT, T0, new_note, user};
use connecto_core::memory::{
    AnnotationKind, AnnotationTarget, BootstrapOptions, BootstrapState, Cycle, LocalDate,
    ModelChoice, Provider, Room, RunOutcome, SourceClass,
};
use connecto_memory::{
    BatchState, BootstrapSource, Checkpoint, Enqueued, EventState, NewEvent, ScheduleState, Store,
};

fn event(key: &str, priority: bool) -> NewEvent {
    NewEvent {
        dedup_key: key.to_owned(),
        kind: "turn_end".to_owned(),
        created_ms: T0,
        payload: "{}".to_owned(),
        priority,
        internal: false,
    }
}

#[test]
fn the_same_event_twice_is_one_row() {
    let mut f = Fixture::new();
    let first = f.store.enqueue_event(event("k1", false)).unwrap();
    let again = f.store.enqueue_event(event("k1", false)).unwrap();
    assert_eq!(first, Enqueued::Inserted(1));
    assert_eq!(again, Enqueued::Duplicate(1));
    assert_eq!(f.store.ready_events(10).unwrap().len(), 1);
    // Internal events are kept but never ready.
    let mut internal = event("k2", false);
    internal.internal = true;
    let seq = f.store.enqueue_event(internal).unwrap().seq();
    assert_eq!(
        f.store.event(seq).unwrap().unwrap().state,
        EventState::Internal
    );
    assert_eq!(f.store.ready_events(10).unwrap().len(), 1);
    // Proposals come first.
    f.store.enqueue_event(event("k3", true)).unwrap();
    let ready: Vec<u64> = f
        .store
        .ready_events(10)
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(ready, [3, 1]);
    // Evidence ids share the counter: no clash with event seqs.
    assert_eq!(user(&mut f.store).seq(), 4);
}

#[test]
fn a_retried_batch_writes_its_notes_once_and_events_are_considered_once() {
    let mut f = Fixture::new();
    let a = f.store.enqueue_event(event("a", false)).unwrap().seq();
    let b = f.store.enqueue_event(event("b", false)).unwrap().seq();
    let e = user(&mut f.store);
    let batch = f.store.open_batch(Cycle::Development, &[a, b], T0).unwrap();
    assert!(f.store.ready_events(10).unwrap().is_empty());
    // An event already in a batch cannot join another.
    assert!(f.store.open_batch(Cycle::Development, &[a], T0).is_err());
    // First attempt fails: waits, then retried.
    assert_eq!(
        f.store.fail_batch(batch, "timeout", T0 + 100, 3).unwrap(),
        BatchState::Retry
    );
    assert!(f.store.retry_batches(T0 + 50).unwrap().is_empty());
    assert_eq!(f.store.retry_batches(T0 + 100).unwrap(), [batch]);
    f.store.start_batch(batch, None).unwrap();
    let notes = vec![new_note(
        AnnotationTarget::New {
            room: Room::Workshop,
        },
        AnnotationKind::Integration,
        &[e],
    )];
    let done = f.store.complete_batch(batch, notes.clone()).unwrap();
    assert_eq!(done.annotations.len(), 1);
    assert!(!done.already);
    // The same batch delivered again: nothing new.
    let again = f.store.complete_batch(batch, notes).unwrap();
    assert!(again.already);
    assert_eq!(f.store.read().open_annotations().unwrap().len(), 1);
    for seq in [a, b] {
        assert_eq!(
            f.store.event(seq).unwrap().unwrap().state,
            EventState::Considered
        );
    }
    let row = f.store.batch(batch).unwrap().unwrap();
    assert_eq!(
        (row.state, row.attempts, row.events),
        (BatchState::Done, 1, vec![a, b])
    );
}

#[test]
fn a_batch_fails_for_good_after_its_attempts_and_shows_in_the_backlog() {
    let mut f = Fixture::new();
    let a = f.store.enqueue_event(event("a", false)).unwrap().seq();
    f.store.enqueue_event(event("b", false)).unwrap();
    let batch = f.store.open_batch(Cycle::Development, &[a], T0).unwrap();
    f.store.fail_batch(batch, "bad json", T0 + 1, 2).unwrap();
    assert_eq!(
        f.store.fail_batch(batch, "bad json", T0 + 2, 2).unwrap(),
        BatchState::Failed
    );
    assert_eq!(f.store.event(a).unwrap().unwrap().state, EventState::Failed);
    assert_eq!(f.store.event(a).unwrap().unwrap().attempts, 2);
    let backlog = f.store.backlog(T0 + 1000).unwrap();
    assert_eq!(backlog.queued, 1);
    // An absolute Unix timestamp (the contract), not an age.
    assert_eq!(backlog.oldest_queued_ms, Some(T0));
    assert_eq!(backlog.failed_batches, 1);
    assert_eq!(backlog.last_failure.as_deref(), Some("bad json"));
}

#[test]
fn a_held_batch_uses_no_attempt_and_waits_until_released() {
    let mut f = Fixture::new();
    let a = f.store.enqueue_event(event("a", false)).unwrap().seq();
    let batch = f.store.open_batch(Cycle::Development, &[a], T0).unwrap();
    f.store
        .hold_batch(batch, "the CLI does not trust the project folder")
        .unwrap();
    let row = f.store.batch(batch).unwrap().unwrap();
    assert_eq!((row.state, row.attempts), (BatchState::Held, 0));
    assert!(row.hold_reason.is_some());
    assert!(f.store.retry_batches(u64::MAX >> 2).unwrap().is_empty());
    assert_eq!(
        f.store.hold_reason().unwrap().as_deref(),
        Some("the CLI does not trust the project folder")
    );
    assert_eq!(f.store.event(a).unwrap().unwrap().state, EventState::Queued);
    assert_eq!(f.store.release_held_batches(T0 + 5).unwrap(), [batch]);
    assert_eq!(f.store.retry_batches(T0 + 5).unwrap(), [batch]);
    assert_eq!(f.store.hold_reason().unwrap(), None);
    assert_eq!(f.store.batch(batch).unwrap().unwrap().attempts, 0);
}

#[test]
fn runs_are_recorded_and_interrupted_at_restart() {
    let f = Fixture::new();
    let db = f.db();
    let mut store = f.store;
    let choice = ModelChoice::new(Provider::Claude, "haiku", "low");
    let done = store.start_run(Cycle::Development, &choice, T0, 0).unwrap();
    store
        .finish_run(
            done,
            &RunOutcome::Annotated {
                written: 2,
                rejected: 1,
            },
            T0 + 500,
            1000,
            200,
        )
        .unwrap();
    let held = store.start_run(Cycle::Dream, &choice, T0, 0).unwrap();
    store.hold_run(held, "folder not trusted", T0 + 1).unwrap();
    assert_eq!(
        store.run_hold(held).unwrap().as_deref(),
        Some("folder not trusted")
    );
    assert_eq!(
        store.run(held).unwrap().unwrap().outcome,
        RunOutcome::Held {
            reason: "folder not trusted".to_owned()
        }
    );
    // Through `finish_run` too, and `hold_reason` sees it.
    let held2 = store
        .start_run(Cycle::Development, &choice, T0 + 2, 0)
        .unwrap();
    let outcome = RunOutcome::Held {
        reason: "trust the folder".to_owned(),
    };
    store.finish_run(held2, &outcome, T0 + 3, 10, 0).unwrap();
    assert_eq!(store.run(held2).unwrap().unwrap().outcome, outcome);
    assert_eq!(
        store.run_hold(held2).unwrap().as_deref(),
        Some("trust the folder")
    );
    assert_eq!(
        store.hold_reason().unwrap().as_deref(),
        Some("trust the folder")
    );
    let a = store.enqueue_event(event("a", false)).unwrap().seq();
    let batch = store.open_batch(Cycle::Development, &[a], T0).unwrap();
    let cut = store.start_run(Cycle::Dream, &choice, T0, 1).unwrap();
    store
        .set_run_recovers(cut, &[LocalDate::parse("2026-11-12").unwrap()])
        .unwrap();
    drop(store);
    // Restart: the run found running is interrupted, its batch back in line.
    let mut store = Store::open(&db, PROJECT).unwrap();
    assert_eq!(store.interrupt_running(T0 + 9).unwrap(), [cut]);
    let run = store.run(cut).unwrap().unwrap();
    assert_eq!(run.outcome, RunOutcome::Interrupted);
    assert_eq!(run.recovers.len(), 1);
    assert_eq!(
        store.batch(batch).unwrap().unwrap().state,
        BatchState::Retry
    );
    let entry = store.run(done).unwrap().unwrap();
    assert_eq!(
        entry.outcome,
        RunOutcome::Annotated {
            written: 2,
            rejected: 1
        }
    );
    assert_eq!(
        (entry.duration_ms, entry.chars_in, entry.chars_out),
        (Some(500), 1000, 200)
    );
    assert_eq!(store.runs(Some(Cycle::Dream), 10).unwrap().len(), 2);
    assert_eq!(store.runs(None, 10).unwrap()[0].id, cut);
}

#[test]
fn one_run_per_program_and_date_and_a_held_date_is_not_done() {
    let mut f = Fixture::new();
    let day = LocalDate::parse("2026-11-14").unwrap();
    assert!(f.store.claim_schedule("dream", day, Some(1), T0).unwrap());
    assert!(!f.store.claim_schedule("dream", day, Some(2), T0).unwrap());
    // Held: claimable again, not done.
    f.store
        .set_schedule(
            "dream",
            day,
            &ScheduleState::Held {
                reason: "folder not trusted".to_owned(),
            },
            None,
            None,
            T0,
        )
        .unwrap();
    let entry = &f.store.schedule("dream").unwrap()[0];
    assert!(!entry.state.counts_as_done());
    assert!(
        f.store
            .claim_schedule("dream", day, Some(3), T0 + 1)
            .unwrap()
    );
    f.store
        .set_schedule("dream", day, &ScheduleState::Done, None, None, T0 + 2)
        .unwrap();
    assert!(
        !f.store
            .claim_schedule("dream", day, Some(4), T0 + 3)
            .unwrap()
    );
    let entry = &f.store.schedule("dream").unwrap()[0];
    assert_eq!(
        (entry.state.clone(), entry.run_id),
        (ScheduleState::Done, Some(3))
    );
    assert!(entry.state.counts_as_done());
    // Missed dates recovered by one run.
    for d in ["2026-11-11", "2026-11-12"] {
        f.store
            .set_schedule(
                "dream",
                LocalDate::parse(d).unwrap(),
                &ScheduleState::Recovered,
                None,
                Some(3),
                T0,
            )
            .unwrap();
    }
    let rows = f.store.schedule("dream").unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.state.counts_as_done()));
    assert!(f.store.schedule("other").unwrap().is_empty());
}

#[test]
fn staging_survives_a_restart_until_discarded() {
    let f = Fixture::new();
    let db = f.db();
    let mut store = f.store;
    store
        .stage(7, Some(Room::Workshop), "{\"op\":\"create\"}", T0)
        .unwrap();
    store.stage(7, None, "{\"op\":\"link\"}", T0).unwrap();
    store.stage(8, None, "{}", T0).unwrap();
    drop(store);
    let mut store = Store::open(&db, PROJECT).unwrap();
    let staged = store.staged(7).unwrap();
    assert_eq!(staged.len(), 2);
    assert_eq!(staged[0].room, Some(Room::Workshop));
    assert_eq!(store.discard_staging(Some(7)).unwrap(), 2);
    assert_eq!(store.discard_staging(None).unwrap(), 1);
    assert!(store.staged(8).unwrap().is_empty());
}

#[test]
fn an_initial_build_keeps_clamped_options_a_manifest_and_checkpoints() {
    let mut f = Fixture::new();
    let options = BootstrapOptions {
        choice: Some(ModelChoice::new(Provider::Claude, "sonnet", "medium")),
        history_days: 0,
        max_minutes: 100_000,
        exclude: vec!["vendor/**".to_owned()],
        ..BootstrapOptions::default()
    };
    f.store.create_bootstrap("b-1", &options, T0).unwrap();
    let row = f.store.bootstrap(None).unwrap().unwrap();
    assert_eq!(row.state, BootstrapState::Queued);
    assert_eq!(row.options, options.clone().clamped());
    assert_eq!(row.options.history_days, 1);
    f.store
        .set_bootstrap_state(
            "b-1",
            &BootstrapState::Failed {
                reason: "x".to_owned(),
            },
            T0 + 1,
        )
        .unwrap();
    assert_eq!(
        f.store.bootstrap(Some("b-1")).unwrap().unwrap().state,
        BootstrapState::Failed {
            reason: "x".to_owned()
        }
    );
    assert!(
        f.store
            .set_bootstrap_state("nope", &BootstrapState::Running, T0)
            .is_err()
    );
    let source = BootstrapSource {
        path: "README.md".to_owned(),
        class: SourceClass::Documentation,
        hash: Some("abc".to_owned()),
        state: "selected".to_owned(),
    };
    f.store.put_bootstrap_source("b-1", &source).unwrap();
    f.store
        .put_bootstrap_source(
            "b-1",
            &BootstrapSource {
                state: "analysed".to_owned(),
                ..source
            },
        )
        .unwrap();
    let sources = f.store.bootstrap_sources("b-1").unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].state, "analysed");
    let cp = Checkpoint {
        packet: 0,
        state: "done".to_owned(),
        run_id: Some(3),
        payload: "{}".to_owned(),
        updated_ms: T0,
    };
    f.store.put_checkpoint("b-1", &cp).unwrap();
    f.store.put_checkpoint("b-1", &cp).unwrap();
    assert_eq!(f.store.checkpoints("b-1").unwrap(), vec![cp]);
}
