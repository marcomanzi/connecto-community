//! The writer thread and the fake host.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::{Fixture, PROJECT};
use connecto_core::memory::{Cycle, ModelChoice, Provider};
use connecto_memory::testing::{FakeHost, START_MS};
use connecto_memory::{
    GitSnapshot, Host, ModelJob, NewEvent, PostError, RunError, StoreError, Writer,
};

#[test]
fn the_writer_runs_jobs_in_order_and_refuses_another_project() {
    let f = Fixture::new();
    let db = f.db();
    drop(f.store);
    assert!(matches!(
        Writer::spawn(&db, "proj-b", 4),
        Err(StoreError::WrongProject)
    ));
    let writer = Writer::spawn(&db, PROJECT, 4).unwrap();
    let handle = writer.handle();
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let handle = handle.clone();
            std::thread::spawn(move || {
                for i in 0..10 {
                    handle
                        .call(move |s| {
                            s.enqueue_event(NewEvent {
                                dedup_key: format!("{t}-{i}"),
                                kind: "k".to_owned(),
                                created_ms: 0,
                                payload: String::new(),
                                priority: false,
                                internal: false,
                            })
                        })
                        .unwrap()
                        .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(
        writer.call(|s| s.ready_events(100).unwrap().len()).unwrap(),
        40
    );
    drop(writer);
    assert!(matches!(handle.call(|_| ()), Err(StoreError::WriterGone)));
    assert_eq!(handle.post(|_| ()), Err(PostError::Gone));
}

#[test]
fn posting_never_blocks_when_the_queue_is_full() {
    let f = Fixture::new();
    let db = f.db();
    drop(f.store);
    let writer = Writer::spawn(&db, PROJECT, 1).unwrap();
    let (block_tx, block_rx) = crossbeam_channel::bounded::<()>(0);
    let (started_tx, started_rx) = crossbeam_channel::bounded::<()>(0);
    writer
        .handle()
        .post(move |_| {
            started_tx.send(()).unwrap();
            block_rx.recv().unwrap();
        })
        .unwrap();
    started_rx.recv().unwrap();
    writer.handle().post(|_| ()).unwrap();
    let start = Instant::now();
    assert_eq!(writer.handle().post(|_| ()), Err(PostError::Full));
    assert!(start.elapsed() < Duration::from_secs(1));
    block_tx.send(()).unwrap();
}

#[test]
fn the_fake_host_scripts_replies_clock_git_and_pause() {
    let host = Arc::new(FakeHost::new());
    let job = ModelJob {
        project_id: PROJECT.to_owned(),
        cycle: Some(Cycle::Development),
        choice: ModelChoice::new(Provider::Claude, "haiku", "low"),
        prompt: "abc".to_owned(),
    };
    let cancel = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(1);
    host.reply("[]");
    host.fail(RunError::HeldUntrusted {
        reason: "folder not trusted".to_owned(),
    });
    let reply = host.run_model(&job, deadline, &cancel).unwrap();
    assert_eq!((reply.text.as_str(), reply.chars_in), ("[]", 3));
    assert!(matches!(
        host.run_model(&job, deadline, &cancel),
        Err(RunError::HeldUntrusted { .. })
    ));
    assert!(matches!(
        host.run_model(&job, deadline, &cancel),
        Err(RunError::Failed(_))
    ));
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    host.reply("unused");
    assert_eq!(
        host.run_model(&job, deadline, &cancel),
        Err(RunError::Cancelled)
    );
    assert_eq!(host.jobs().len(), 4);
    assert!(host.capability(&job.choice).is_available());
    assert!(
        !host
            .capability(&ModelChoice::new(Provider::Codex, "gpt-5", "low"))
            .is_available()
    );
    assert_eq!(host.now().utc_ms, START_MS);
    host.advance(Duration::from_secs(60));
    assert_eq!(host.now().utc_ms, START_MS + 60_000);
    assert_eq!(host.git_snapshot(std::path::Path::new(".")), None);
    host.set_git(Some(GitSnapshot {
        branch: Some("main".to_owned()),
        ..GitSnapshot::default()
    }));
    assert!(host.git_snapshot(std::path::Path::new(".")).is_some());
    assert!(!host.paused());
    host.set_paused(true);
    host.set_paused(true);
    assert!(host.paused());
    assert_eq!(host.pause_epoch(), 1);
    host.set_paused(false);
    host.set_paused(true);
    assert_eq!(host.pause_epoch(), 2);
    host.wake();
    assert_eq!(host.wakes(), 1);
    host.set_secrets(vec!["s".to_owned()]);
    assert_eq!(host.known_secrets(), ["s"]);
}
