//! The engine with its threads, against the fake host (spec §5.2, §11.2):
//! settings, capture, reads, exports, the bridge, Stop Everything, two
//! projects at once, deletion.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::engine::{World, db, view, world};
use common::wait_for;
use connecto_core::memory::{
    AnnotationKind, Cycle, ExportFormat, MemoryBridge, MemoryCaller, MemoryCommand, MemoryControl,
    MemorySettings, NOT_ENABLED, Provider, ReadRequest, ReadState, Room, RunOutcome,
};
use connecto_memory::{Captured, CapturedReply, Happened, Reader, delete_memory_files};

fn enable(w: &World, id: &str, group_seconds: u32) {
    wait_for("the project", || view(w, id).is_some());
    let mut s = MemorySettings {
        enabled: true,
        ..MemorySettings::default()
    };
    s.development.group_seconds = group_seconds;
    w.engine.set_settings(id, s).unwrap();
    wait_for("memory on", || {
        Reader::open(&db(w, id), id).is_ok() && view(w, id).is_some_and(|v| v.enabled)
    });
}

fn captured(project: &str, turn: &str, what: Happened) -> Captured {
    Captured {
        project_id: project.to_owned(),
        terminal: "t1".to_owned(),
        provider: Provider::Claude,
        tool_session: Some("s1".to_owned()),
        session_start_ms: 1,
        turn: turn.to_owned(),
        log: None,
        what,
    }
}

fn turn(w: &World, project: &str, n: u32, prompt: &str, reply: &str) {
    let t = format!("#{n}");
    assert!(w.engine.capture().offer(captured(
        project,
        &t,
        Happened::Prompt {
            text: prompt.to_owned(),
            author: connecto_memory::capture::Author::User,
            truncated: false,
        }
    )));
    assert!(w.engine.capture().offer(captured(
        project,
        &t,
        Happened::TurnEnd {
            reply: CapturedReply::Text(reply.to_owned())
        }
    )));
}

fn last_dev_run(w: &World, id: &str) -> Option<connecto_core::memory::RunEntry> {
    view(w, id)?
        .cycle(Cycle::Development)?
        .last
        .clone()
        .filter(|r| !r.outcome.is_running())
}

#[test]
fn settings_capture_and_a_run_show_in_the_view_with_their_measures() {
    let w = world(&["p1"]);
    enable(&w, "p1", 0);
    let saved = w.host.saved_settings("p1").unwrap();
    assert!(saved.enabled);
    w.host.reply(
        r#"{"annotations":[{"type":"integrazione","memory_kind":"fatto","standing":"ipotesi","text":"Builds use make.","sources":["e-2"]}]}"#,
    );
    turn(&w, "p1", 1, "how do we build?", "We build with make.");
    wait_for("the run", || last_dev_run(&w, "p1").is_some());
    let run = last_dev_run(&w, "p1").unwrap();
    assert_eq!(
        run.outcome,
        RunOutcome::Annotated {
            written: 1,
            rejected: 0
        }
    );
    assert!(run.chars_in > 1000 && run.chars_out > 10);
    assert!(run.duration_ms.is_some());
    wait_for("the counts", || {
        view(&w, "p1").is_some_and(|v| v.open_annotations == 1 && v.considered_through_ms.is_some())
    });
    // A UI read answers in the view, never on the caller's thread.
    let id = w
        .engine
        .request_read(
            "p1",
            ReadRequest::Search {
                query: "make".into(),
                room: None,
                scope: None,
                all: false,
            },
        )
        .unwrap();
    wait_for("the read", || {
        view(&w, "p1")
            .and_then(|v| v.read(id).cloned())
            .is_some_and(|r| matches!(r.state, ReadState::Ready(_)))
    });
    // Export.
    w.engine.export("p1", ExportFormat::Both).unwrap();
    wait_for("the export", || {
        view(&w, "p1").is_some_and(|v| {
            v.last_export
                .as_ref()
                .is_some_and(|e| e.result.as_ref().is_ok_and(|f| f.len() == 2))
        })
    });
    // Off: capture stops, the data stays.
    let mut off = w.host.saved_settings("p1").unwrap();
    off.enabled = false;
    w.engine.set_settings("p1", off).unwrap();
    wait_for("memory off", || view(&w, "p1").is_some_and(|v| !v.enabled));
    let jobs = w.host.jobs().len();
    turn(&w, "p1", 2, "anything", "more");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(w.host.jobs().len(), jobs);
    assert!(db(&w, "p1").exists());
    assert!(
        w.engine
            .request_read("p1", ReadRequest::Room(Room::Workshop))
            .is_err()
    );
    // Bad model words are refused at once.
    let mut bad = MemorySettings::default();
    bad.development.choice.model = "--tools".into();
    assert!(w.engine.set_settings("p1", bad).is_err());
}

#[test]
fn a_proposal_through_the_bridge_closes_the_window_and_is_readable_at_once() {
    let w = world(&["p1"]);
    // A long window: only the proposal makes the cycle run now.
    enable(&w, "p1", 3600);
    assert!(w.engine.enabled("p1"));
    let caller = MemoryCaller {
        terminal: "t2".into(),
        tool_session: Some("s2".into()),
        branch: None,
    };
    w.host.reply(r#"{"annotations":[]}"#);
    let out = w.engine.command(
        "p1",
        &caller,
        &MemoryCommand::Propose {
            kind: AnnotationKind::Integration,
            about: None,
            room: Some(Room::Decisions),
            scope: None,
        },
        Some("We should keep exports outside the repository."),
    );
    assert!(out.contains("Stored note"), "{out}");
    let brief = w
        .engine
        .command("p1", &caller, &MemoryCommand::Brief { budget: None }, None);
    assert!(brief.starts_with("[connecto memory]"));
    wait_for("the proposal's run", || last_dev_run(&w, "p1").is_some());
    let prompt = w.host.jobs()[0].prompt.clone();
    assert!(prompt.contains("keep exports outside the repository"));
    assert!(prompt.contains("already stored as note p-"));
    // Off for an unknown project, and the version notice.
    assert_eq!(
        w.engine.command(
            "nope",
            &caller,
            &MemoryCommand::Brief { budget: None },
            None
        ),
        NOT_ENABLED
    );
    assert_eq!(w.engine.take_version_notice("p1", "t2"), None);
    assert_eq!(w.engine.take_version_notice("p1", "t2"), None);
}

#[test]
fn stop_everything_cancels_runs_in_flight_and_keeps_the_queue() {
    let w = world(&["p1"]);
    enable(&w, "p1", 0);
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (_release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    w.host.hold_runs(entered_tx, release_rx);
    w.host.reply(r#"{"annotations":[]}"#);
    turn(&w, "p1", 1, "q", "a");
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    w.host.set_paused(true);
    w.engine.cancel_runs();
    assert_eq!(w.host.cancels(), 1);
    wait_for("the cancelled run", || {
        last_dev_run(&w, "p1").is_some_and(|r| r.outcome == RunOutcome::Cancelled)
    });
    wait_for("the queue", || {
        view(&w, "p1").is_some_and(|v| v.backlog.queued == 2)
    });
    // Paused: nothing starts again.
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(w.host.jobs().len(), 1);
}

#[test]
fn two_projects_run_at_once_without_mixing() {
    let w = world(&["pa", "pb"]);
    enable(&w, "pa", 0);
    enable(&w, "pb", 0);
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(4);
    w.host.hold_runs(entered_tx, release_rx);
    w.host.reply(r#"{"annotations":[]}"#);
    w.host.reply(r#"{"annotations":[]}"#);
    turn(&w, "pa", 1, "alpha prompt", "alpha reply");
    turn(&w, "pb", 1, "beta prompt", "beta reply");
    // Both runs are in flight at the same time.
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    drop(release_tx);
    wait_for("both runs", || {
        last_dev_run(&w, "pa").is_some() && last_dev_run(&w, "pb").is_some()
    });
    for job in w.host.jobs() {
        let (mine, other) = if job.project_id == "pa" {
            ("alpha", "beta")
        } else {
            ("beta", "alpha")
        };
        assert!(job.prompt.contains(mine) && !job.prompt.contains(other));
    }
    for (id, other) in [("pa", "beta"), ("pb", "alpha")] {
        let reader = Reader::open(&db(&w, id), id).unwrap();
        let read = reader.read();
        for e in read.all_evidence().unwrap() {
            assert_eq!(e.project_id, id);
            assert!(!e.excerpt.get().contains(other));
        }
    }
    // Another project's database is refused.
    assert!(Reader::open(&db(&w, "pa"), "pb").is_err());
}

/// Where deletion is supported (Unix); elsewhere it is refused (see
/// `deletion_is_refused_where_unsupported_and_memory_is_turned_off`).
#[cfg(unix)]
#[test]
fn deleting_removes_only_the_known_files_of_that_memory() {
    let w = world(&["p1", "p2"]);
    enable(&w, "p1", 0);
    enable(&w, "p2", 0);
    w.engine.export("p1", ExportFormat::Both).unwrap();
    wait_for("the export", || {
        view(&w, "p1").is_some_and(|v| v.last_export.is_some())
    });
    let mem = db(&w, "p1").parent().unwrap().to_owned();
    std::fs::write(mem.join("notes-of-mine.txt"), "keep").unwrap();
    std::fs::write(mem.join("exports/other.json"), "keep").unwrap();
    let outside = w.dir.path().join("outside.txt");
    std::fs::write(&outside, "keep").unwrap();
    w.engine.delete_memory("p1").unwrap();
    wait_for("the deletion", || {
        !db(&w, "p1").exists() && view(&w, "p1").is_some_and(|v| !v.enabled)
    });
    assert!(!w.host.saved_settings("p1").unwrap().enabled);
    assert!(mem.join("notes-of-mine.txt").exists());
    assert!(mem.join("exports/other.json").exists());
    assert!(outside.exists());
    assert!(db(&w, "p2").exists(), "another project is untouched");
    assert!(
        w.dir.path().join("repos/p1").exists(),
        "the repository is untouched"
    );
}

#[cfg(unix)]
#[test]
fn file_deletion_follows_no_link_and_refuses_odd_ids() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mem = root.join("projects/p/memory");
    std::fs::create_dir_all(mem.join("exports/memory-12")).unwrap();
    std::fs::write(mem.join("memory.db"), "db").unwrap();
    std::fs::write(mem.join("memory.db-wal"), "wal").unwrap();
    std::fs::write(mem.join("exports/memory-12.json"), "j").unwrap();
    std::fs::write(mem.join("exports/memory-12/officina.md"), "m").unwrap();
    std::fs::write(mem.join("exports/memory-12/extra.txt"), "keep").unwrap();
    let target = root.join("precious");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("officina.md"), "keep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, mem.join("exports/memory-13")).unwrap();
    let removed = delete_memory_files(root, "p").unwrap();
    assert!(!mem.join("memory.db").exists() && !mem.join("memory.db-wal").exists());
    assert!(!mem.join("exports/memory-12.json").exists());
    assert!(!mem.join("exports/memory-12/officina.md").exists());
    assert!(
        mem.join("exports/memory-12/extra.txt").exists(),
        "unknown files stay"
    );
    assert!(
        target.join("officina.md").exists(),
        "a link is not followed"
    );
    assert!(removed.iter().all(|p| p.starts_with(&mem)));
    for bad in ["..", ".", "a/b", "", "../p"] {
        assert!(delete_memory_files(root, bad).is_err(), "{bad}");
    }
    assert!(Path::new(&root.join("precious")).exists());
}

/// Review finding 1: a link anywhere below the data folder (the project
/// folder, `memory`, `exports`) is never followed by a deletion.
#[cfg(unix)]
#[test]
fn deletion_refuses_linked_folders() {
    use std::os::unix::fs::symlink;
    for linked in [
        "projects/p",
        "projects/p/memory",
        "projects/p/memory/exports",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("memory.db"), "precious").unwrap();
        std::fs::write(outside.join("memory-1.json"), "precious").unwrap();
        std::fs::create_dir_all(root.join("projects/p/memory/exports")).unwrap();
        // Replace the level with a link to the outside folder.
        let at = root.join(linked);
        let parent = at.parent().unwrap().to_owned();
        let name = at.file_name().unwrap().to_owned();
        let moved = parent.join(format!("{}-real", name.to_string_lossy()));
        std::fs::rename(&at, &moved).unwrap();
        symlink(&outside, &at).unwrap();
        let result = delete_memory_files(&root, "p");
        assert!(
            outside.join("memory.db").exists() && outside.join("memory-1.json").exists(),
            "{linked}: {result:?}"
        );
        assert!(result.is_err() || result.unwrap().iter().all(|p| !p.starts_with(&outside)));
    }
}

/// Review finding 3: closing a project while its model run is in flight
/// neither hangs the supervisor nor a later shutdown.
#[test]
fn closing_a_project_during_a_run_does_not_hang() {
    let w = world(&["p1"]);
    enable(&w, "p1", 0);
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (_release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    w.host.hold_runs(entered_tx, release_rx);
    w.host.reply(r#"{"annotations":[]}"#);
    turn(&w, "p1", 1, "q", "a");
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    // The project closes while the run waits on the model.
    w.host.set_projects(Vec::new());
    wait_for("the project gone", || view(&w, "p1").is_none());
    let engine = w.engine.clone();
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        engine.shutdown();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("shutdown finished");
}

/// Round 2, finding 3: where link-proof deletion is not available the
/// deletion is refused, naming the folder; memory is off and closed.
#[test]
fn the_unsupported_message_names_the_folder() {
    let dir = std::path::Path::new("/data/projects/p/memory");
    let m = connecto_memory::unsupported_message(dir);
    assert!(m.contains("/data/projects/p/memory"));
    assert!(m.contains("turned off"));
    assert!(m.contains("remove this folder yourself"));
}

/// The refusal itself, compiled and checked on every platform.
#[test]
fn the_refusal_is_unsupported_and_names_the_projects_folder() {
    let e = connecto_memory::deletion_refusal(std::path::Path::new("/data"), "p1");
    assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
    assert!(e.to_string().contains("p1"));
    assert!(e.to_string().contains("remove this folder yourself"));
    assert_eq!(connecto_memory::DELETION_SUPPORTED, cfg!(unix));
}

/// Compiled on every platform; runs its checks where deletion is refused.
#[test]
fn deletion_is_refused_where_unsupported_and_memory_is_turned_off() {
    if connecto_memory::DELETION_SUPPORTED {
        return;
    }
    let w = world(&["p1"]);
    enable(&w, "p1", 0);
    let err = w.engine.delete_memory("p1").unwrap_err();
    assert!(err.contains("remove this folder yourself"));
    wait_for("memory off", || view(&w, "p1").is_some_and(|v| !v.enabled));
    assert!(db(&w, "p1").exists(), "nothing deleted");
    assert!(
        delete_memory_files(&w.dir.path().join("data"), "p1")
            .is_err_and(|e| e.kind() == std::io::ErrorKind::Unsupported)
    );
}
