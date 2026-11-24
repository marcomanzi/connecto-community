//! The dream cycle against the fake host and a real store (spec §5.5,
//! §11.2): cutoff, packets per room, staging, atomic publication and crash
//! points, malformed replies, limits, secrets, pause, Stop Everything, held
//! runs, evidence loops, distinctions, branches, revert.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use connecto_core::Redacted;
use connecto_core::memory::{
    Address, AnnotationId, AnnotationKind, AnnotationOrigin, AnnotationTarget, Cycle,
    DreamSettings, EvidenceId, EvidenceKind, MemoryId, MemoryKind, MemoryStatus, Room, RunOutcome,
    Scope, Standing, Subject, Verb, Verification,
};
use connecto_memory::dream::{self, CrashAt, DreamReport, Trigger};
use connecto_memory::testing::FakeHost;
use connecto_memory::{
    Host, MemoryOp, NewAnnotation, NewEvidence, NewMemory, Publication, PublishStep, Reader,
    RunError, Store, Writer, scheduler,
};

const SECRET: &str = concat!("ghp_", "0123456789abcdefghijABCDEFGHIJ012345");

struct Bench {
    dir: tempfile::TempDir,
    writer: Writer,
    host: Arc<FakeHost>,
    project: PathBuf,
    settings: DreamSettings,
}

fn bench() -> Bench {
    bench_for("proj-a")
}

fn bench_for(id: &str) -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("repo");
    std::fs::create_dir_all(&project).unwrap();
    let writer = Writer::spawn(&dir.path().join("memory.db"), id, 64).unwrap();
    Bench {
        dir,
        writer,
        host: Arc::new(FakeHost::new()),
        project,
        settings: DreamSettings::default(),
    }
}

impl Bench {
    fn db(&self) -> PathBuf {
        self.dir.path().join("memory.db")
    }

    fn with<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Store) -> connecto_memory::Result<R> + Send + 'static,
    ) -> R {
        self.writer.call(f).unwrap().unwrap()
    }

    fn dream(&self) -> DreamReport {
        self.dream_with(None, &AtomicBool::new(false))
    }

    fn dream_with(&self, crash: Option<CrashAt>, cancel: &AtomicBool) -> DreamReport {
        self.dream_full(crash, cancel.load(Ordering::SeqCst), None, &Trigger::Now)
    }

    fn dream_full(
        &self,
        crash: Option<CrashAt>,
        cancel: bool,
        before_publish: Option<&(dyn Fn() + Sync)>,
        trigger: &Trigger,
    ) -> DreamReport {
        let handle = self.writer.handle();
        let cx = dream::Context {
            host: Arc::clone(&self.host) as Arc<dyn Host>,
            writer: &handle,
            project_id: "proj-a",
            project_path: &self.project,
            settings: &self.settings,
            cancel: Arc::new(AtomicBool::new(cancel)),
            crash,
            before_publish,
        };
        dream::run(&cx, trigger).unwrap()
    }

    /// A dream on a thread of its own.
    fn dream_in_thread(&self) -> std::thread::JoinHandle<DreamReport> {
        let host = Arc::clone(&self.host) as Arc<dyn Host>;
        let handle = self.writer.handle();
        let settings = self.settings.clone();
        let project = self.project.clone();
        std::thread::spawn(move || {
            let cx = dream::Context {
                host,
                writer: &handle,
                project_id: "proj-a",
                project_path: &project,
                settings: &settings,
                cancel: Arc::new(AtomicBool::new(false)),
                crash: None,
                before_publish: None,
            };
            dream::run(&cx, &Trigger::Now).unwrap()
        })
    }

    fn evidence(&self, kind: EvidenceKind, session: &str, text: &str) -> EvidenceId {
        let (session, text) = (session.to_owned(), text.to_owned());
        self.with(move |s| {
            let mut e = NewEvidence::new(kind, 1, Redacted::assume(text));
            e.tool_session = Some(session.clone());
            e.terminal = Some(format!("term-{session}"));
            Ok(s.insert_evidence(e)?.id)
        })
    }

    fn note(
        &self,
        room: Room,
        text: &str,
        sources: &[EvidenceId],
        kind: Option<(MemoryKind, Standing)>,
    ) -> AnnotationId {
        let (text, sources) = (text.to_owned(), sources.to_vec());
        self.with(move |s| {
            Ok(s.create_annotation(NewAnnotation {
                id: None,
                created_ms: 1,
                kind: AnnotationKind::Integration,
                target: AnnotationTarget::New { room },
                text: Redacted::assume(text),
                scopes: vec![Scope::Project],
                sources,
                verification: Verification::Unverified,
                origin: AnnotationOrigin::Agent {
                    terminal: "claude-1".to_owned(),
                },
                memory_kind: kind.map(|k| k.0),
                standing: kind.map(|k| k.1),
            })?
            .id)
        })
    }

    fn memory(
        &self,
        address: &str,
        kind: MemoryKind,
        summary: &str,
        sources: &[EvidenceId],
    ) -> MemoryId {
        let (address, summary, sources) =
            (address.to_owned(), summary.to_owned(), sources.to_vec());
        self.with(move |s| {
            let cutoff = s.read().annotation_seq()?;
            let p = s.publish(Publication {
                project_id: "proj-a".to_owned(),
                cycle: Cycle::Bootstrap,
                base_version: None,
                cutoff,
                ops: vec![MemoryOp::Create(NewMemory {
                    id: None,
                    address: Address::parse(&address).unwrap(),
                    kind,
                    standing: None,
                    summary: Redacted::assume(summary),
                    details: Redacted::assume(String::new()),
                    scopes: vec![Scope::Project],
                    status: MemoryStatus::Candidate,
                    anchors: Vec::new(),
                    sources,
                    relations: Vec::new(),
                })],
                absorb: Vec::new(),
                run_id: None,
                now_ms: 1,
            })?;
            Ok(p.created[0].clone())
        })
    }

    fn version(&self) -> u64 {
        self.with(|s| s.version())
    }

    fn staged_count(&self) -> usize {
        self.with(|s| {
            let n: i64 =
                s.connection_mut()
                    .query_row("SELECT count(*) FROM dream_staging", [], |r| r.get(0))?;
            Ok(usize::try_from(n).unwrap())
        })
    }

    fn staging_text(&self) -> String {
        self.with(|s| {
            let mut stmt = s
                .connection_mut()
                .prepare("SELECT payload FROM dream_staging")?;
            let rows: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows.join("\n"))
        })
    }

    fn open_notes(&self) -> Vec<AnnotationId> {
        self.with(|s| {
            Ok(s.read()
                .open_annotations()?
                .into_iter()
                .map(|a| a.id)
                .collect())
        })
    }

    fn run_entry(&self, id: u64) -> connecto_core::memory::RunEntry {
        self.with(move |s| Ok(s.run(id)?.unwrap()))
    }

    /// What a restarted app does before its first dream.
    fn restart(&self) {
        self.with(|s| {
            s.interrupt_running(5)?;
            scheduler::recover_at_start(s)?;
            Ok(())
        });
    }
}

fn reply(b: &Bench, text: &str) {
    b.host.reply(text);
}

/// A project with one memory, two annotations about it, evidence.
struct Seeded {
    user: EvidenceId,
    agent: EvidenceId,
    store: MemoryId,
    a: AnnotationId,
    b: AnnotationId,
}

fn seeded(b: &Bench) -> Seeded {
    let user = b.evidence(
        EvidenceKind::UserStatement,
        "u1",
        "Keep the memory store outside the repository.",
    );
    let agent = b.evidence(
        EvidenceKind::AgentClaim,
        "s1",
        "The store is SQLite in WAL mode.",
    );
    let store = b.memory(
        "architettura/store",
        MemoryKind::Fact,
        "The memory store is SQLite.",
        &[agent],
    );
    let a = b.note(
        Room::Architecture,
        "The store runs in WAL mode.",
        &[agent],
        None,
    );
    let bb = b.note(
        Room::Decisions,
        "Keep the store outside the repository.",
        &[user],
        None,
    );
    Seeded {
        user,
        agent,
        store,
        a,
        b: bb,
    }
}

#[test]
fn a_dream_consolidates_and_publishes_one_version() {
    let b = bench();
    let s = seeded(&b);
    let before = b.version();
    reply(
        &b,
        &format!(
            r#"{{"operations":[
            {{"op":"update","id":"{m}","summary":"The memory store is SQLite in WAL mode.","sources":["{agent}"]}},
            {{"op":"absorb","annotation":"{a}","into":"{m}"}},
            {{"op":"create","ref":"out","room":"decisioni","kind":"decisione","standing":"accettata","summary":"The store lives outside the repository.","sources":["{user}"]}},
            {{"op":"absorb","annotation":"{bb}","into":"out"}}
        ]}}"#,
            m = s.store,
            a = s.a,
            bb = s.b,
            agent = s.agent,
            user = s.user
        ),
    );
    let report = b.dream();
    let Some(RunOutcome::Published {
        version,
        applied,
        rejected,
    }) = report.outcome.clone()
    else {
        panic!("{report:?}");
    };
    assert_eq!(version, before + 1);
    assert_eq!((applied, rejected), (4, 0));
    assert_eq!(report.sessions, 1);
    assert!(b.open_notes().is_empty());
    let run = b.run_entry(report.run_id.unwrap());
    assert_eq!(run.cycle, Cycle::Dream);
    assert!(run.chars_in > 0 && run.chars_out > 0, "{run:?}");
    assert_eq!(run.outcome, report.outcome.unwrap());
    assert_eq!(b.staged_count(), 0);
    // The prompt: the packet inside this run's delimiters, the rules.
    let job = &b.host.jobs()[0];
    assert_eq!(job.cycle, Some(Cycle::Dream));
    assert!(job.prompt.contains(s.a.as_str()) && job.prompt.contains(s.store.as_str()));
    assert!(job.prompt.contains("The store is SQLite in WAL mode."));
    assert!(job.prompt.contains("is not evidence"));
    let reader = Reader::open(&b.db(), "proj-a").unwrap();
    let read = reader.read();
    let decision = read
        .memory_at(&Address::parse("decisioni/the-store-lives-outside-the-repository").unwrap())
        .unwrap()
        .unwrap();
    let _ = decision;
}

#[test]
fn the_cutoff_leaves_later_annotations_open_and_intact() {
    let b = bench();
    let s = seeded(&b);
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    b.host.hold_runs(entered_tx, release_rx);
    let late_ev = b.evidence(EvidenceKind::UserStatement, "u2", "Later fact.");
    // A reply that tries to absorb an annotation made during the run.
    let a = s.a.clone();
    let m = s.store.clone();
    let worker = b.dream_in_thread();
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let late = b.note(
        Room::Architecture,
        "Arrived while dreaming.",
        &[late_ev],
        None,
    );
    b.host.reply(&format!(
        r#"[{{"op":"absorb","annotation":"{a}","into":"{m}"}},{{"op":"absorb","annotation":"{late}","into":"{m}"}}]"#
    ));
    release_tx.send(()).unwrap();
    let report = worker.join().unwrap();
    assert!(
        matches!(
            report.outcome,
            Some(RunOutcome::Published {
                applied: 1,
                rejected: 1,
                ..
            })
        ),
        "{report:?}"
    );
    assert!(
        report.rejected[0].contains("cutoff"),
        "{:?}",
        report.rejected
    );
    let open = b.open_notes();
    assert!(open.contains(&late) && open.contains(&s.b) && !open.contains(&s.a));
    // The late annotation is exactly as written.
    let note = b.with(move |st| Ok(st.read().annotation(&late)?.unwrap()));
    assert_eq!(note.text, "Arrived while dreaming.");
    assert!(note.absorbed_in.is_none());
    // Nothing of it was in the packet.
    assert!(!b.host.jobs()[0].prompt.contains("Arrived while dreaming"));
}

#[test]
fn malformed_replies() {
    let b = bench();
    let s = seeded(&b);
    let version = b.version();
    // Wholly unreadable: the run fails with no effect.
    for bad in ["{\"operations\": [", "", "   ", "I cannot help with that."] {
        reply(&b, bad);
        let r = b.dream();
        assert!(
            matches!(r.outcome, Some(RunOutcome::Failed { .. })),
            "{bad:?}: {r:?}"
        );
        assert_eq!(b.version(), version);
        assert_eq!(b.staged_count(), 0);
        assert_eq!(b.open_notes().len(), 2);
    }
    // Too large to read.
    reply(
        &b,
        &"x".repeat(connecto_memory::operations::MAX_REPLY_CHARS + 1),
    );
    assert!(matches!(b.dream().outcome, Some(RunOutcome::Failed { .. })));
    // A reply too large for the hook and not in the log (`Dropped`).
    b.host.fail(RunError::Failed(
        "the reply was dropped and the log could not be read".to_owned(),
    ));
    assert!(matches!(b.dream().outcome, Some(RunOutcome::Failed { .. })));
    b.host.fail(RunError::ModelError);
    assert!(matches!(b.dream().outcome, Some(RunOutcome::Failed { .. })));
    assert_eq!(b.version(), version);
    // Readable, every operation bad: rejected one by one, nothing published,
    // and the run says "5 proposed, 0 kept" (not "nothing to change").
    reply(
        &b,
        &format!(
            r#"[{{"op":"teleport","id":"{m}"}},
                {{"op":"archive","id":"m-zzzzzzzz","sources":["{u}"]}},
                {{"op":"archive","id":"{m}","sources":["e-4040"]}},
                {{"op":"absorb","annotation":"p-zzzzzzzz"}},
                {{"op":"delete","id":"{m}"}}]"#,
            m = s.store,
            u = s.user
        ),
    );
    let r = b.dream();
    match &r.outcome {
        Some(RunOutcome::Refused { proposed, reason }) => {
            assert_eq!(*proposed, 5);
            assert!(reason.starts_with("5 proposed, 0 kept: "), "{reason}");
            assert!(reason.contains("op: unknown operation"), "{reason}");
            assert!(reason.ends_with("and 2 other reasons"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.rejected.len(), 5);
    assert_eq!(b.version(), version);
    // The stored run says the same, and the view's report lists each one.
    let run = r.run_id.unwrap();
    let stored = b.with(move |st| st.run(run)).unwrap();
    assert_eq!(stored.outcome, r.outcome.clone().unwrap());
    let view = r.to_view(0);
    assert_eq!(view.rejected.len(), 5);
    assert!(view.rejected[0].starts_with("session 1, operation 1: "));
    // A dream with nothing to propose stays "nothing to change".
    reply(&b, "[]");
    assert_eq!(b.dream().outcome, Some(RunOutcome::NoChange));
    // Some good, some bad: the good ones are published.
    reply(
        &b,
        &format!(
            r#"[{{"op":"absorb","annotation":"{a}","into":"{m}"}},{{"op":"archive","id":"m-zzzzzzzz","sources":["{u}"]}}]"#,
            a = s.a,
            m = s.store,
            u = s.user
        ),
    );
    let r = b.dream();
    assert!(
        matches!(
            r.outcome,
            Some(RunOutcome::Published {
                applied: 1,
                rejected: 1,
                ..
            })
        ),
        "{r:?}"
    );
}

#[test]
fn a_large_packet_is_split_by_room_with_a_final_pass_and_published_together() {
    let mut b = bench();
    b.settings.max_input_chars = 9000;
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "Short context.");
    // With memories, a final pass across rooms follows the room sessions.
    b.memory(
        "architettura/store",
        MemoryKind::Fact,
        "The store is SQLite.",
        &[user],
    );
    let mut ids = Vec::new();
    for (room, n) in [
        (Room::Architecture, 3),
        (Room::Workshop, 3),
        (Room::Decisions, 2),
    ] {
        for i in 0..n {
            ids.push(b.note(
                room,
                &format!("Note {i} for {}: {}", room.word(), "detail ".repeat(200)),
                &[user],
                None,
            ));
        }
    }
    // The same reply to every session: absorb all. Each session may only
    // absorb what its own packet holds; the rest is refused there.
    let all: Vec<String> = ids
        .iter()
        .map(|id| format!(r#"{{"op":"absorb","annotation":"{id}"}}"#))
        .collect();
    for _ in 0..20 {
        reply(&b, &format!("[{}]", all.join(",")));
    }
    let r = b.dream();
    let jobs = b.host.jobs();
    assert!(jobs.len() >= 4, "{}", jobs.len());
    assert!(
        jobs.iter().all(|j| j.prompt.chars().count() <= 9000),
        "a prompt over the cap"
    );
    assert!(jobs.iter().any(|j| j.prompt.contains("room architettura")));
    assert!(jobs.iter().any(|j| j.prompt.contains("room officina")));
    assert!(jobs.last().unwrap().prompt.contains("final pass"));
    // Every note went whole to exactly one session.
    for id in &ids {
        let n = jobs
            .iter()
            .filter(|j| j.prompt.contains(&format!("[{id} |")))
            .count();
        assert_eq!(n, 1, "{id}");
    }
    // Absorbed once each, all in one version.
    let Some(RunOutcome::Published {
        version, applied, ..
    }) = r.outcome
    else {
        panic!("{r:?}")
    };
    assert_eq!((version, applied), (2, 8));
    assert!(b.open_notes().is_empty());
    let run = b.run_entry(r.run_id.unwrap());
    let total: u64 = jobs.iter().map(|j| j.prompt.chars().count() as u64).sum();
    assert_eq!(run.chars_in, total);
    assert_eq!(r.sessions as usize, jobs.len());
}

#[test]
fn a_crash_at_any_point_leaves_the_old_or_the_new_version_and_no_staging() {
    let setup = || {
        let mut b = bench();
        b.settings.max_input_chars = 5000;
        let s = seeded(&b);
        let n1 = b.note(
            Room::Workshop,
            &"Run make release. ".repeat(50),
            &[s.user],
            None,
        );
        let n2 = b.note(
            Room::Workshop,
            &"Sign with the Developer ID. ".repeat(30),
            &[s.user],
            None,
        );
        let full = format!(
            r#"[{{"op":"absorb","annotation":"{a}","into":"{m}"}},
                {{"op":"update","id":"{m}","summary":"SQLite store, WAL."}},
                {{"op":"keep","annotation":"{bb}","reason":"not settled"}},
                {{"op":"create","room":"officina","slug":"release","kind":"procedura","summary":"Release with make release.","sources":["{u}"]}},
                {{"op":"absorb","annotation":"{n1}"}},{{"op":"absorb","annotation":"{n2}"}}]"#,
            a = s.a,
            m = s.store,
            bb = s.b,
            u = s.user
        );
        for _ in 0..20 {
            reply(&b, &full);
        }
        b
    };
    for step in PublishStep::all(2) {
        let b = setup();
        let before = b.version();
        let r = b.dream_with(Some(CrashAt::Publish(step)), &AtomicBool::new(false));
        assert!(b.host.jobs().len() > 1, "split into several sessions");
        let run = r.run_id.unwrap();
        assert!(r.outcome.is_none(), "{step:?}: {r:?}");
        assert!(
            b.staged_count() > 0,
            "{step:?}: staging accumulates across sessions"
        );
        b.restart();
        assert_eq!(b.version(), before, "{step:?}");
        assert_eq!(b.staged_count(), 0, "{step:?}");
        assert_eq!(b.run_entry(run).outcome, RunOutcome::Interrupted);
        assert_eq!(b.open_notes().len(), 4, "{step:?}");
        // The next dream publishes it all, once.
        let r = b.dream();
        assert!(
            matches!(r.outcome, Some(RunOutcome::Published { .. })),
            "{step:?}: {r:?}"
        );
        assert_eq!(b.version(), before + 1);
        assert_eq!(b.open_notes().len(), 1);
    }
    // A crash between sessions: what the first sessions staged is dropped.
    for after in 0..2 {
        let b = setup();
        let before = b.version();
        let r = b.dream_with(Some(CrashAt::AfterSession(after)), &AtomicBool::new(false));
        assert!(r.outcome.is_none());
        assert!(b.staged_count() > 0);
        b.restart();
        assert_eq!((b.version(), b.staged_count()), (before, 0));
    }
}

#[test]
fn secrets_never_reach_prompts_staging_or_memory() {
    let mut b = bench();
    b.host.set_secrets(vec![SECRET.to_owned()]);
    // Seeded past the ingest redaction, as if it had slipped through.
    let e = b.evidence(
        EvidenceKind::UserStatement,
        "u1",
        &format!("deploy with token {SECRET} now"),
    );
    let n = b.note(
        Room::Workshop,
        &format!("The deploy token is {SECRET}."),
        &[e],
        None,
    );
    b.settings.max_input_chars = 200_000;
    reply(
        &b,
        &format!(
            r#"[{{"op":"create","ref":"d","room":"officina","kind":"procedura","summary":"Deploy uses token {SECRET}.","details":"Set {SECRET} first.","sources":["{e}"]}},
               {{"op":"absorb","annotation":"{n}","into":"d"}},
               {{"op":"keep","annotation":"{n}","reason":"{SECRET}"}}]"#
        ),
    );
    let r = b.dream_with(Some(CrashAt::AfterSession(0)), &AtomicBool::new(false));
    assert!(r.outcome.is_none());
    let staged = b.staging_text();
    assert!(!staged.is_empty() && !staged.contains(SECRET), "{staged}");
    for job in b.host.jobs() {
        assert!(!job.prompt.contains(SECRET));
    }
    b.restart();
    reply(
        &b,
        &format!(
            r#"[{{"op":"create","ref":"d","room":"officina","kind":"procedura","summary":"Deploy uses token {SECRET}.","details":"Set {SECRET} first.","sources":["{e}"]}},
               {{"op":"absorb","annotation":"{n}","into":"d"}}]"#
        ),
    );
    let r = b.dream();
    assert!(
        matches!(r.outcome, Some(RunOutcome::Published { .. })),
        "{r:?}"
    );
    let all = b.with(|s| {
        let read = s.read();
        Ok(read
            .memories()?
            .into_iter()
            .map(|m| format!("{} {}", m.summary, m.details))
            .collect::<Vec<_>>()
            .join("\n"))
    });
    assert!(all.contains("[REDACTED]") && !all.contains(SECRET), "{all}");
    for job in b.host.jobs() {
        assert!(!job.prompt.contains(SECRET));
    }
}

#[test]
fn held_cancelled_paused_and_timed_out_runs_publish_nothing() {
    let b = bench();
    let s = seeded(&b);
    let version = b.version();
    let ok = format!(
        r#"[{{"op":"absorb","annotation":"{}","into":"{}"}}]"#,
        s.a, s.store
    );
    // Held: the CLI does not trust the folder. Not a failure.
    b.host.fail(RunError::HeldUntrusted {
        reason: "the CLI has not been trusted for this project folder yet".to_owned(),
    });
    let r = b.dream();
    assert!(
        matches!(&r.outcome, Some(RunOutcome::Held { reason }) if reason.contains("trusted")),
        "{r:?}"
    );
    assert_eq!(
        b.with(move |st| st.run_hold(r.run_id.unwrap())).as_deref(),
        Some("the CLI has not been trusted for this project folder yet")
    );
    // Cancelled (Stop Everything) before it starts: nothing at all.
    let r = b.dream_with(None, &AtomicBool::new(true));
    assert_eq!((r.outcome, r.run_id), (None, None));
    // A timeout.
    b.host.fail(RunError::Timeout);
    assert!(matches!(b.dream().outcome, Some(RunOutcome::Failed { .. })));
    // Paused before: nothing starts, no run row.
    b.host.set_paused(true);
    let r = b.dream();
    assert_eq!((r.outcome, r.run_id), (None, None));
    b.host.set_paused(false);
    // A pause during the session: nothing is published.
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    b.host.hold_runs(entered_tx, release_rx);
    b.host.reply(&ok);
    let worker = b.dream_in_thread();
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    b.host.set_paused(true);
    b.host.set_paused(false);
    release_tx.send(()).unwrap();
    let r = worker.join().unwrap();
    assert_eq!(r.outcome, Some(RunOutcome::HeldByPause));
    assert_eq!(b.version(), version);
    assert_eq!(b.staged_count(), 0);
    assert_eq!(b.open_notes().len(), 2);
    // After the pause the work runs again.
    drop(release_tx);
    b.host.reply(&ok);
    assert!(matches!(
        b.dream().outcome,
        Some(RunOutcome::Published { .. })
    ));
}

#[test]
fn an_agent_that_read_x_and_repeats_it_does_not_verify_x() {
    let b = bench();
    let claim = b.evidence(EvidenceKind::AgentClaim, "s1", "Builds use make.");
    let x = b.memory(
        "officina/build",
        MemoryKind::Procedure,
        "Builds use make.",
        &[claim],
    );
    // Session s7 read X, then said it again three times.
    let x2 = x.clone();
    b.with(move |s| {
        s.record_served(
            Some("s7"),
            Some("term-s7"),
            Verb::Read,
            &[Subject::Memory(x2)],
            0,
        )
    });
    let mut echoes = Vec::new();
    for text in [
        "Builds use make.",
        "Yes, make builds it.",
        "make: all tests passed.",
    ] {
        echoes.push(b.evidence(EvidenceKind::AgentClaim, "s7", text));
    }
    let n = b.note(
        Room::Workshop,
        "Builds use make (confirmed).",
        &echoes,
        None,
    );
    let list: Vec<String> = echoes.iter().map(|e| format!("\"{e}\"")).collect();
    reply(
        &b,
        &format!(
            r#"[{{"op":"set_status","id":"{x}","status":"verificato","sources":[{}]}},
               {{"op":"update","id":"{x}","standing":"eseguita","sources":[{}]}},
               {{"op":"absorb","annotation":"{n}","into":"{x}"}}]"#,
            list.join(","),
            list.join(",")
        ),
    );
    let r = b.dream();
    assert!(
        matches!(
            r.outcome,
            Some(RunOutcome::Published {
                applied: 1,
                rejected: 2,
                ..
            })
        ),
        "{r:?}"
    );
    let m = b.with(move |s| Ok(s.read().memory(&x)?.unwrap()));
    assert_eq!(m.status, MemoryStatus::Candidate);
    assert_eq!(m.standing, Standing::Documented);
}

#[test]
fn contradictions_stay_visible_as_conflict_with_both_sources() {
    let b = bench();
    let doc = b.evidence(
        EvidenceKind::UserStatement,
        "u1",
        "Exports must go to the data folder.",
    );
    let code = b.evidence(
        EvidenceKind::CodeObservation,
        "c1",
        "export.rs writes into the repository root.",
    );
    let m = b.memory(
        "architettura/exports",
        MemoryKind::Fact,
        "Exports go to the data folder.",
        &[doc],
    );
    let n = b.note(
        Room::Architecture,
        "The code writes exports into the repository.",
        &[code],
        None,
    );
    reply(
        &b,
        &format!(
            r#"[{{"op":"set_status","id":"{m}","status":"conflitto","sources":["{code}"]}},
               {{"op":"keep","annotation":"{n}","reason":"code and user disagree"}}]"#
        ),
    );
    let r = b.dream();
    assert!(
        matches!(r.outcome, Some(RunOutcome::Published { .. })),
        "{r:?}"
    );
    let mem = b.with(move |s| Ok(s.read().memory(&m)?.unwrap()));
    assert_eq!(mem.status, MemoryStatus::Conflict);
    let sources: Vec<EvidenceId> = mem.sources.iter().map(|s| s.evidence).collect();
    assert!(sources.contains(&doc) && sources.contains(&code));
    assert!(b.open_notes().contains(&n));
}

#[test]
fn a_gone_branch_is_annotated_not_promoted_or_archived() {
    let b = bench();
    let claim = b.evidence(
        EvidenceKind::AgentClaim,
        "s1",
        "Feature X lives on its branch.",
    );
    let m = b.memory(
        "lavori-aperti/feature-x",
        MemoryKind::Work,
        "Feature X in progress.",
        &[claim],
    );
    let m2 = m.clone();
    b.with(move |s| {
        let mut u = connecto_memory::MemoryUpdate::new(m2.clone());
        u.scopes = Some(vec![Scope::Branch("feature-x".to_owned())]);
        let cutoff = s.read().annotation_seq()?;
        s.publish(Publication {
            project_id: "proj-a".to_owned(),
            cycle: Cycle::Bootstrap,
            base_version: None,
            cutoff,
            ops: vec![MemoryOp::Update(u)],
            absorb: Vec::new(),
            run_id: None,
            now_ms: 1,
        })?;
        s.set_scope_gone(&m2, &Scope::Branch("feature-x".to_owned()), true)?;
        Ok(())
    });
    let n = b.note(
        Room::OpenWork,
        "Branch feature-x was squashed and deleted.",
        &[claim],
        None,
    );
    reply(
        &b,
        &format!(
            r#"[{{"op":"archive","id":"{m}","sources":["{claim}"]}},
               {{"op":"update","id":"{m}","scope":["project"],"sources":["{claim}"]}},
               {{"op":"update","id":"{m}","standing":"implementato","sources":["{claim}"]}},
               {{"op":"keep","annotation":"{n}","reason":"needs the user"}}]"#
        ),
    );
    let r = b.dream();
    assert_eq!(r.rejected.len(), 3, "{r:?}");
    let mem = b.with(move |s| Ok(s.read().memory(&m)?.unwrap()));
    assert_eq!(mem.address.room(), Room::OpenWork);
    assert_eq!(mem.scopes.len(), 1);
    assert!(mem.scopes[0].gone);
    assert_eq!(mem.standing, Standing::Planned);
}

#[test]
fn revert_restores_and_reopens_absorbed_annotations_later_ones_intact() {
    let b = bench();
    let s = seeded(&b);
    reply(
        &b,
        &format!(
            r#"[{{"op":"update","id":"{m}","summary":"Rewritten summary."}},{{"op":"absorb","annotation":"{a}","into":"{m}"}}]"#,
            m = s.store,
            a = s.a
        ),
    );
    let Some(RunOutcome::Published { version, .. }) = b.dream().outcome else {
        panic!()
    };
    let later_ev = b.evidence(EvidenceKind::UserStatement, "u9", "Later.");
    let later = b.note(
        Room::Architecture,
        "Arrived after the dream.",
        &[later_ev],
        None,
    );
    let reverted = b.with(move |st| st.revert(version, 99));
    assert_eq!(reverted.reopened, std::slice::from_ref(&s.a));
    let m = s.store.clone();
    let mem = b.with(move |st| Ok(st.read().memory(&m)?.unwrap()));
    assert_eq!(mem.summary, "The memory store is SQLite.");
    let open = b.open_notes();
    assert!(open.contains(&s.a) && open.contains(&later) && open.contains(&s.b));
}

#[test]
fn a_proposal_absorbed_by_a_dream_stays_a_proposal() {
    let b = bench();
    let proposal = b.evidence(EvidenceKind::AgentProposal, "s3", "Let's use Postgres.");
    let n = b.note(
        Room::Decisions,
        "Proposed: use Postgres.",
        &[proposal],
        Some((MemoryKind::Decision, Standing::Proposed)),
    );
    let plan = b.evidence(EvidenceKind::AgentClaim, "s3", "I will add export next.");
    let n2 = b.note(
        Room::OpenWork,
        "Plan: add export.",
        &[plan],
        Some((MemoryKind::Work, Standing::Planned)),
    );
    reply(
        &b,
        &format!(
            r#"[{{"op":"create","ref":"pg","room":"decisioni","kind":"decisione","standing":"accettata","status":"verificato","summary":"We use Postgres.","sources":["{proposal}"]}},
               {{"op":"absorb","annotation":"{n}","into":"pg"}},
               {{"op":"create","ref":"ex","room":"lavori-aperti","kind":"lavoro","standing":"implementato","summary":"Export is implemented.","sources":["{plan}"]}},
               {{"op":"absorb","annotation":"{n2}","into":"ex"}}]"#
        ),
    );
    let r = b.dream();
    assert!(
        matches!(r.outcome, Some(RunOutcome::Published { applied: 4, .. })),
        "{r:?}"
    );
    assert!(r.adjusted.len() >= 2, "{r:?}");
    let all = b.with(|s| s.read().memories());
    let pg = all
        .iter()
        .find(|m| m.summary == "We use Postgres.")
        .unwrap();
    assert_eq!(pg.standing, Standing::Proposed);
    assert_eq!(pg.status, MemoryStatus::Candidate);
    let ex = all
        .iter()
        .find(|m| m.summary == "Export is implemented.")
        .unwrap();
    assert_eq!(ex.standing, Standing::Planned);
}

#[test]
fn nothing_to_dream_on_is_no_change_and_records_the_cutoff() {
    let b = bench();
    let r = b.dream();
    assert_eq!(r.outcome, Some(RunOutcome::NoChange));
    assert!(b.host.jobs().is_empty());
    let _ = b.with(|s| scheduler::new_items(s));
    assert_eq!(b.with(|s| scheduler::new_items(s)), 0);
    let _ = b.evidence(EvidenceKind::UserStatement, "u", "x");
    assert_eq!(b.with(|s| scheduler::new_items(s)), 1);
}

#[test]
fn the_host_is_a_host() {
    // Compile-time: the fake host is usable behind the trait.
    let host: Arc<dyn Host> = Arc::new(FakeHost::new());
    assert!(!host.paused());
}

/// The instructions and the material of a prompt, the delimiter id
/// replaced by `ID`: everything before the line that opens the material,
/// and what is between it and the line that closes it.
fn split_prompt(prompt: &str, word: &str) -> (String, String) {
    // The delimiter starts a line; the instructions quote it in prose.
    let open = format!("\n<<<{word} ");
    let at = prompt.find(&open).unwrap();
    let id: String = prompt[at + open.len()..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    assert!(id.len() >= 16, "{id}");
    let close = format!("{word} {id}>>>");
    let end = prompt.rfind(&close).unwrap();
    (
        prompt[..at].replace(&id, "ID"),
        prompt[at + open.len() + id.len()..end].to_owned(),
    )
}

/// Review finding: secrets are hidden in the material only. A known secret
/// equal to a word of the protocol (an operation, a field, a room, the
/// delimiter word) leaves the instructions byte-identical, a reply in the
/// emitted shape still parses, and the secret is gone from the material.
#[test]
fn protocol_words_as_secrets_never_change_the_dream_instructions() {
    let secrets: Vec<String> = [
        "operations",
        "create",
        "sources",
        "scope",
        "kind",
        "architettura",
        "MATERIAL",
        "SQLite",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let prompt_with = |secrets: &[String]| {
        let b = bench();
        seeded(&b);
        b.host.set_secrets(secrets.to_vec());
        reply(&b, "[]");
        b.dream();
        b.host.jobs()[0].prompt.clone()
    };
    let (plain, _) = split_prompt(&prompt_with(&[]), "MATERIAL");
    let with = prompt_with(&secrets);
    let (instructions, material) = split_prompt(&with, "MATERIAL");
    assert_eq!(instructions, plain, "the instructions are untouched");
    assert!(instructions.contains(connecto_memory::operations::operations_prompt_section()));
    for s in &secrets {
        assert!(!material.contains(s.as_str()), "{s} in the material");
    }
    assert!(material.contains("[REDACTED]"));
    // The emitted example parses.
    let example = instructions
        .lines()
        .find_map(|l| l.strip_prefix("Example: "))
        .unwrap();
    let parsed = connecto_memory::operations::parse(example).unwrap();
    assert!(parsed.iter().all(|op| !matches!(
        op,
        connecto_memory::operations::ProposedOp::Malformed { .. }
    )));
}
