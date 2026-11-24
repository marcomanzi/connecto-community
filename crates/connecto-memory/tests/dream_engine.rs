//! The dream and its scheduler inside the engine, against the fake host
//! (spec §5.5, §6.1, §7, §11.2): "Dream now", the version notice, the
//! schedule with a fake clock, pause, Stop Everything, two projects at once,
//! a proposal while a dream publishes, revert, the publication gate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::engine::{World, db, view, world};
use common::wait_for;
use connecto_core::memory::{
    AnnotationId, AnnotationKind, Cycle, DreamSettings, EvidenceId, LocalDate, LocalTime,
    MemoryBridge, MemoryCaller, MemoryCommand, MemoryControl, MemorySettings, Room, RunEntry,
    RunOutcome, Weekday,
};
use connecto_memory::{Reader, WallClock, scheduler};

/// Memory on, the development cycle slow (one run an hour, long window),
/// the dream on every day at 02:30 with no minimum.
fn enable(w: &World, id: &str) {
    wait_for("the project", || view(w, id).is_some());
    let mut s = MemorySettings {
        enabled: true,
        ..MemorySettings::default()
    };
    s.development.group_seconds = 3600;
    s.development.max_runs_per_hour = 1;
    s.dream = DreamSettings {
        days: Weekday::ALL.iter().copied().collect(),
        min_new_items: 0,
        ..DreamSettings::default()
    };
    w.engine.set_settings(id, s).unwrap();
    wait_for("memory on", || {
        Reader::open(&db(w, id), id).is_ok() && view(w, id).is_some_and(|v| v.enabled)
    });
}

fn caller(terminal: &str) -> MemoryCaller {
    MemoryCaller {
        terminal: terminal.into(),
        tool_session: Some(format!("session-{terminal}")),
        branch: None,
    }
}

fn propose(w: &World, id: &str, text: &str) -> String {
    w.engine.command(
        id,
        &caller("t2"),
        &MemoryCommand::Propose {
            kind: AnnotationKind::Integration,
            about: None,
            room: Some(Room::Workshop),
            scope: None,
        },
        Some(text),
    )
}

fn last_run(w: &World, id: &str, cycle: Cycle) -> Option<RunEntry> {
    view(w, id)?
        .cycle(cycle)?
        .last
        .clone()
        .filter(|r| !r.outcome.is_running())
}

/// One note through `propose`, and the development run it wakes (which
/// uses the hour's only run). Returns the note and its evidence.
fn seed(w: &World, id: &str, text: &str) -> (AnnotationId, EvidenceId) {
    w.host.reply(r#"{"annotations":[]}"#);
    let out = propose(w, id, text);
    assert!(out.contains("Stored note"), "{out}");
    wait_for("the development run", || {
        last_run(w, id, Cycle::Development).is_some()
    });
    let reader = Reader::open(&db(w, id), id).unwrap();
    let read = reader.read();
    let note = read
        .open_annotations()
        .unwrap()
        .into_iter()
        .find(|a| a.text == text)
        .unwrap();
    (note.id.clone(), note.sources[0].evidence)
}

fn open_notes(w: &World, id: &str) -> Vec<AnnotationId> {
    let reader = Reader::open(&db(w, id), id).unwrap();
    reader
        .read()
        .open_annotations()
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect()
}

fn create_reply(note: &AnnotationId, evidence: EvidenceId, summary: &str) -> String {
    format!(
        r#"{{"operations":[{{"op":"create","ref":"n","room":"officina","kind":"procedura","summary":"{summary}","sources":["{evidence}"]}},{{"op":"absorb","annotation":"{note}","into":"n"}}]}}"#
    )
}

fn clock(day: &str, hour: u8, minute: u8) -> WallClock {
    let d = LocalDate::parse(day).unwrap();
    let t = LocalTime::new(hour, minute).unwrap();
    WallClock {
        utc_ms: u64::try_from(scheduler::stamp(d, t) * 60_000).unwrap(),
        local_date: d,
        local_time: t,
        weekday: scheduler::weekday(d),
        utc_offset_secs: 0,
    }
}

#[test]
fn dream_now_publishes_and_open_terminals_get_a_notice_at_their_next_prompt() {
    let w = world(&["p1"]);
    enable(&w, "p1");
    let (note, ev) = seed(&w, "p1", "Release with make release.");
    // A terminal open before the dream records the version it started from.
    assert_eq!(w.engine.take_version_notice("p1", "t9"), None);
    w.host
        .reply(&create_reply(&note, ev, "Release with make release."));
    let wakes = w.host.wakes();
    w.engine.dream_now("p1").unwrap();
    wait_for("the dream", || last_run(&w, "p1", Cycle::Dream).is_some());
    let run = last_run(&w, "p1", Cycle::Dream).unwrap();
    let RunOutcome::Published {
        version, applied, ..
    } = run.outcome
    else {
        panic!("{run:?}")
    };
    assert_eq!(applied, 2);
    assert!(run.chars_in > 0 && run.chars_out > 0 && run.duration_ms.is_some());
    wait_for("the view", || {
        view(&w, "p1").is_some_and(|v| v.version == version)
    });
    assert!(
        w.host.wakes() > wakes,
        "the view changed (the UI, not a terminal)"
    );
    // The notice waits for the terminal's next prompt; asked twice, once.
    assert_eq!(w.engine.take_version_notice("p1", "t9"), Some(version));
    assert_eq!(w.engine.take_version_notice("p1", "t9"), None);
    assert!(open_notes(&w, "p1").is_empty());
    let report = w.engine.dream_report("p1").unwrap();
    assert_eq!(report.sessions, 1);
    assert!(report.rejected.is_empty());
}

#[test]
fn the_scheduler_runs_once_per_date_and_catches_up_skipped_days_in_one_run() {
    let w = world(&["p1"]);
    w.host.set_clock(clock("2026-11-16", 1, 0));
    enable(&w, "p1");
    let (note, ev) = seed(&w, "p1", "Release with make release.");
    w.engine.check_schedule_now();
    wait_for("the next dream in the view", || {
        view(&w, "p1").is_some_and(|v| v.next_dream_ms == Some(clock("2026-11-16", 2, 30).utc_ms))
    });
    // Closed until Thursday morning: Monday to Wednesday were skipped.
    w.host
        .reply(&create_reply(&note, ev, "Release with make release."));
    w.host.set_clock(clock("2026-11-19", 9, 0));
    w.engine.check_schedule_now();
    wait_for("the catch-up dream", || {
        last_run(&w, "p1", Cycle::Dream).is_some()
    });
    let run = last_run(&w, "p1", Cycle::Dream).unwrap();
    assert!(
        matches!(run.outcome, RunOutcome::Published { .. }),
        "{run:?}"
    );
    assert_eq!(run.recovers.len(), 3);
    wait_for("skipped dates in the view", || {
        view(&w, "p1").is_some_and(|v| {
            v.skipped.len() == 3 && v.skipped.iter().all(|s| s.recovered_by == Some(run.id))
        })
    });
    // Looked at again and again: no second run for these dates.
    for _ in 0..3 {
        w.engine.check_schedule_now();
        std::thread::sleep(Duration::from_millis(250));
    }
    let dreams: Vec<_> = w
        .host
        .jobs()
        .into_iter()
        .filter(|j| j.cycle == Some(Cycle::Dream))
        .collect();
    assert_eq!(dreams.len(), 1);
    assert_eq!(
        view(&w, "p1").unwrap().next_dream_ms,
        Some(clock("2026-11-20", 2, 30).utc_ms)
    );
}

#[test]
fn no_dream_starts_while_paused_and_a_pause_during_one_blocks_its_publication() {
    let w = world(&["p1"]);
    w.host.set_clock(clock("2026-11-16", 1, 0));
    enable(&w, "p1");
    let (note, ev) = seed(&w, "p1", "Release with make release.");
    w.engine.check_schedule_now();
    std::thread::sleep(Duration::from_millis(300));
    w.host.set_paused(true);
    assert_eq!(w.engine.run_dream_now("p1"), Err("paused".to_owned()));
    // Due while paused: nothing starts.
    w.host.set_clock(clock("2026-11-16", 2, 30));
    w.engine.check_schedule_now();
    std::thread::sleep(Duration::from_millis(500));
    assert!(last_run(&w, "p1", Cycle::Dream).is_none());
    // Resume: it starts; a pause comes while its session runs.
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(4);
    w.host.hold_runs(entered_tx, release_rx);
    w.host
        .reply(&create_reply(&note, ev, "Release with make release."));
    w.host.set_paused(false);
    w.engine.check_schedule_now();
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    w.host.set_paused(true);
    release_tx.send(()).unwrap();
    wait_for("the dream held by the pause", || {
        last_run(&w, "p1", Cycle::Dream).is_some_and(|r| r.outcome == RunOutcome::HeldByPause)
    });
    assert_eq!(view(&w, "p1").unwrap().version, 0);
    assert_eq!(open_notes(&w, "p1"), std::slice::from_ref(&note));
    // After Resume the date is still due and the work runs again.
    w.host
        .reply(&create_reply(&note, ev, "Release with make release."));
    w.host.set_paused(false);
    w.engine.check_schedule_now();
    release_tx.send(()).unwrap();
    wait_for("the dream after resume", || {
        last_run(&w, "p1", Cycle::Dream)
            .is_some_and(|r| matches!(r.outcome, RunOutcome::Published { .. }))
    });
    assert!(open_notes(&w, "p1").is_empty());
}

#[test]
fn stop_everything_cancels_a_dream_and_keeps_the_queue() {
    let w = world(&["p1"]);
    enable(&w, "p1");
    let (note, ev) = seed(&w, "p1", "Release with make release.");
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (_release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    w.host.hold_runs(entered_tx, release_rx);
    w.host.reply(&create_reply(&note, ev, "x y z"));
    w.engine.dream_now("p1").unwrap();
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    // Stop Everything: pause, then cancel the runs.
    w.host.set_paused(true);
    w.engine.cancel_runs();
    wait_for("the cancelled dream", || {
        last_run(&w, "p1", Cycle::Dream).is_some_and(|r| r.outcome == RunOutcome::Cancelled)
    });
    assert_eq!(open_notes(&w, "p1"), [note]);
    assert_eq!(view(&w, "p1").unwrap().version, 0);
    // Nothing restarts until the user resumes.
    w.engine.check_schedule_now();
    std::thread::sleep(Duration::from_millis(500));
    let dreams = w
        .host
        .jobs()
        .iter()
        .filter(|j| j.cycle == Some(Cycle::Dream))
        .count();
    assert_eq!(dreams, 1);
}

#[test]
fn two_projects_dream_at_once_without_mixing() {
    let w = world(&["pa", "pb"]);
    enable(&w, "pa");
    enable(&w, "pb");
    seed(&w, "pa", "alpha procedure");
    seed(&w, "pb", "beta procedure");
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(4);
    w.host.hold_runs(entered_tx, release_rx);
    w.host.reply(r#"{"operations":[]}"#);
    w.host.reply(r#"{"operations":[]}"#);
    w.engine.dream_now("pa").unwrap();
    w.engine.dream_now("pb").unwrap();
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    drop(release_tx);
    wait_for("both dreams", || {
        last_run(&w, "pa", Cycle::Dream).is_some() && last_run(&w, "pb", Cycle::Dream).is_some()
    });
    for job in w
        .host
        .jobs()
        .iter()
        .filter(|j| j.cycle == Some(Cycle::Dream))
    {
        let (mine, other) = if job.project_id == "pa" {
            ("alpha", "beta")
        } else {
            ("beta", "alpha")
        };
        assert!(job.prompt.contains(mine) && !job.prompt.contains(other));
    }
    for id in ["pa", "pb"] {
        let reader = Reader::open(&db(&w, id), id).unwrap();
        let runs = reader.read();
        assert!(
            runs.all_evidence()
                .unwrap()
                .iter()
                .all(|e| e.project_id == id)
        );
    }
}

#[test]
fn a_proposal_while_a_dream_publishes_stays_open_and_intact() {
    let w = world(&["p1"]);
    enable(&w, "p1");
    let (note, ev) = seed(&w, "p1", "Release with make release.");
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(4);
    w.host.hold_runs(entered_tx, release_rx);
    w.host
        .reply(&create_reply(&note, ev, "Release with make release."));
    w.engine.dream_now("p1").unwrap();
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    // Proposals while the dream runs and publishes (the hour's development
    // run is used up: they stay notes).
    let proposer = {
        let engine = w.engine.clone();
        std::thread::spawn(move || {
            for i in 0..5 {
                let out = engine.command(
                    "p1",
                    &caller("t3"),
                    &MemoryCommand::Propose {
                        kind: AnnotationKind::Task,
                        about: None,
                        room: Some(Room::OpenWork),
                        scope: None,
                    },
                    Some(&format!("Follow-up number {i}.")),
                );
                assert!(out.contains("Stored note"), "{out}");
            }
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    release_tx.send(()).unwrap();
    proposer.join().unwrap();
    wait_for("the dream", || {
        last_run(&w, "p1", Cycle::Dream)
            .is_some_and(|r| matches!(r.outcome, RunOutcome::Published { .. }))
    });
    let reader = Reader::open(&db(&w, "p1"), "p1").unwrap();
    let open = reader.read().open_annotations().unwrap();
    assert_eq!(open.len(), 5);
    for a in &open {
        assert!(a.text.starts_with("Follow-up number"));
        assert!(a.absorbed_in.is_none());
    }
}

#[test]
fn revert_from_the_ui_restores_the_previous_version() {
    let w = world(&["p1"]);
    enable(&w, "p1");
    let (note, ev) = seed(&w, "p1", "Release with make release.");
    w.host
        .reply(&create_reply(&note, ev, "Release with make release."));
    w.engine.dream_now("p1").unwrap();
    wait_for("the dream", || last_run(&w, "p1", Cycle::Dream).is_some());
    wait_for("version 1", || {
        view(&w, "p1").is_some_and(|v| v.version == 1)
    });
    assert!(w.engine.revert("p1", 7).is_ok(), "checked on its thread");
    wait_for("the refusal", || {
        view(&w, "p1").is_some_and(|v| {
            v.last_error
                .as_deref()
                .is_some_and(|e| e.contains("not undone"))
        })
    });
    w.engine.revert("p1", 1).unwrap();
    wait_for("version 2", || {
        view(&w, "p1").is_some_and(|v| v.version == 2)
    });
    let report = w.engine.dream_report("p1").unwrap();
    assert_eq!(report.reverted, Some((1, 2)));
    assert_eq!(open_notes(&w, "p1"), [note]);
    let v = view(&w, "p1").unwrap();
    assert_eq!(v.versions[0].reverts, Some(1));
    assert_eq!(v.totals.total(), 0);
}

#[test]
fn the_publication_gate_keeps_a_dream_and_an_initial_build_apart() {
    let w = world(&["p1"]);
    enable(&w, "p1");
    let (note, ev) = seed(&w, "p1", "Release with make release.");
    let gate = w.engine.publication_gate("p1").unwrap();
    let held = gate.try_acquire(Cycle::Bootstrap).unwrap();
    assert!(w.engine.dream_now("p1").is_err());
    assert!(w.engine.revert("p1", 1).is_err());
    drop(held);
    w.host
        .reply(&create_reply(&note, ev, "Release with make release."));
    w.engine.dream_now("p1").unwrap();
    // While the dream runs, the build cannot take the gate.
    let start = Instant::now();
    while gate.holder().is_none() && start.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(1));
    }
    wait_for("the dream", || last_run(&w, "p1", Cycle::Dream).is_some());
    wait_for("the gate free", || gate.holder().is_none());
    assert!(gate.try_acquire(Cycle::Bootstrap).is_some());
}
