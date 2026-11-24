//! Regression tests for review round 1 of the dream cycle (T8/T9): each
//! test is a reviewer's reproduction, now refused.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use connecto_core::Redacted;
use connecto_core::memory::{
    Address, Anchor, AnnotationId, AnnotationKind, AnnotationOrigin, AnnotationTarget, Cycle,
    DreamSettings, EvidenceId, EvidenceKind, LocalDate, LocalTime, MemoryId, MemoryKind,
    MemoryStatus, Room, RunOutcome, Scope, Standing, Subject, Verb, Verification, Weekday,
};
use connecto_memory::dream::{self, CrashAt, DreamReport, Trigger};
use connecto_memory::operations::{self, Staged};
use connecto_memory::scheduler::{self, Check};
use connecto_memory::testing::FakeHost;
use connecto_memory::{
    ChangedFile, GitSnapshot, Host, MemoryOp, MemoryUpdate, NewAnnotation, NewEvidence, NewMemory,
    Publication, ScheduleState, Store, WallClock, Writer,
};

const NEEDLE: &str = "needle-secret-value";
const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";

struct Bench {
    _dir: tempfile::TempDir,
    writer: Writer,
    host: Arc<FakeHost>,
    project: PathBuf,
    settings: DreamSettings,
}

fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("repo");
    std::fs::create_dir_all(&project).unwrap();
    let writer = Writer::spawn(&dir.path().join("memory.db"), "proj-a", 64).unwrap();
    Bench {
        _dir: dir,
        writer,
        host: Arc::new(FakeHost::new()),
        project,
        settings: DreamSettings::default(),
    }
}

impl Bench {
    fn with<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Store) -> connecto_memory::Result<R> + Send + 'static,
    ) -> R {
        self.writer.call(f).unwrap().unwrap()
    }

    fn dream_full(
        &self,
        crash: Option<CrashAt>,
        hook: Option<&(dyn Fn() + Sync)>,
        trigger: &Trigger,
    ) -> DreamReport {
        let handle = self.writer.handle();
        let cx = dream::Context {
            host: Arc::clone(&self.host) as Arc<dyn Host>,
            writer: &handle,
            project_id: "proj-a",
            project_path: &self.project,
            settings: &self.settings,
            cancel: Arc::new(AtomicBool::new(false)),
            crash,
            before_publish: hook,
        };
        dream::run(&cx, trigger).unwrap()
    }

    fn dream(&self) -> DreamReport {
        self.dream_full(None, None, &Trigger::Now)
    }

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

    fn evidence_with(
        &self,
        kind: EvidenceKind,
        session: &str,
        text: &str,
        terminal: &str,
    ) -> EvidenceId {
        let (session, text, terminal) = (session.to_owned(), text.to_owned(), terminal.to_owned());
        self.with(move |s| {
            let mut e = NewEvidence::new(kind, 1, Redacted::assume(text));
            e.tool_session = Some(session);
            e.terminal = Some(terminal);
            Ok(s.insert_evidence(e)?.id)
        })
    }

    fn evidence(&self, kind: EvidenceKind, session: &str, text: &str) -> EvidenceId {
        self.evidence_with(kind, session, text, &format!("term-{session}"))
    }

    fn note(&self, room: Room, text: &str, sources: &[EvidenceId]) -> AnnotationId {
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
                memory_kind: None,
                standing: None,
            })?
            .id)
        })
    }

    fn create(&self, new: NewMemory) -> MemoryId {
        self.with(move |s| {
            let cutoff = s.read().annotation_seq()?;
            Ok(s.publish(Publication {
                project_id: "proj-a".to_owned(),
                cycle: Cycle::Bootstrap,
                base_version: None,
                cutoff,
                ops: vec![MemoryOp::Create(new)],
                absorb: Vec::new(),
                run_id: None,
                now_ms: 1,
            })?
            .created[0]
                .clone())
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

    fn all_text(&self) -> String {
        self.with(|s| {
            let conn = s.connection_mut();
            let mut out = String::new();
            for sql in [
                "SELECT payload FROM dream_staging",
                "SELECT room || '/' || slug || ' ' || summary || ' ' || details FROM memory",
                "SELECT scope FROM memory_scope",
                "SELECT path || ' ' || blob FROM memory_anchor",
                "SELECT address FROM address_redirect",
            ] {
                let mut stmt = conn.prepare(sql)?;
                let rows: Vec<String> = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                out.push_str(&rows.join("\n"));
                out.push('\n');
            }
            Ok(out)
        })
    }
}

fn memory(
    address: &str,
    kind: MemoryKind,
    standing: Option<Standing>,
    summary: &str,
    details: &str,
    sources: &[EvidenceId],
) -> NewMemory {
    NewMemory {
        id: None,
        address: Address::parse(address).unwrap(),
        kind,
        standing,
        summary: Redacted::assume(summary.to_owned()),
        details: Redacted::assume(details.to_owned()),
        scopes: vec![Scope::Project],
        status: MemoryStatus::Candidate,
        anchors: Vec::new(),
        sources: sources.to_vec(),
        relations: Vec::new(),
    }
}

// ---- 1. model-controlled metadata

#[test]
fn f1_a_secret_in_a_slug_scope_path_or_address_never_reaches_staging_or_memory() {
    let b = bench();
    b.host.set_secrets(vec![NEEDLE.to_owned()]);
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "Release with make.");
    let m = b.create(memory(
        "officina/release",
        MemoryKind::Procedure,
        None,
        "Release with make.",
        "",
        &[user],
    ));
    let n = b.note(Room::Workshop, "Release notes go in CHANGELOG.", &[user]);
    let ops = format!(
        r#"[{{"op":"create","room":"officina","slug":"{NEEDLE}","kind":"procedura","summary":"A","sources":["{user}"]}},
            {{"op":"create","room":"officina","kind":"procedura","summary":"B","scope":["branch:{NEEDLE}"],"sources":["{user}"]}},
            {{"op":"create","room":"officina","kind":"procedura","summary":"C","scope":["component:src/{NEEDLE}"],"sources":["{user}"]}},
            {{"op":"create","room":"officina","kind":"procedura","summary":"D","anchors":[{{"path":"{NEEDLE}/a.rs","blob":"{BLOB}"}}],"sources":["{user}"]}},
            {{"op":"update","id":"{m}","scope":["worktree:/tmp/{NEEDLE}"]}},
            {{"op":"move","id":"{m}","to":"officina/{NEEDLE}"}},
            {{"op":"create","room":"officina","slug":"token-sk-live-abcdefghijklmnop","kind":"procedura","summary":"E","sources":["{user}"]}},
            {{"op":"keep","annotation":"{n}","reason":"waiting"}}]"#
    );
    b.host.reply(&ops);
    let r = b.dream_full(Some(CrashAt::AfterSession(0)), None, &Trigger::Now);
    assert!(r.outcome.is_none());
    let staged = b.all_text();
    assert!(!staged.contains(NEEDLE), "{staged}");
    assert!(!staged.contains("sk-live-abcdefghijklmnop"), "{staged}");
    // Run it for real: the refusals are listed, nothing of it is stored.
    b.with(|s| {
        s.interrupt_running(5)?;
        scheduler::recover_at_start(s)?;
        Ok(())
    });
    b.host.reply(&ops);
    let r = b.dream();
    assert_eq!(r.rejected.len(), 7, "{r:?}");
    assert!(
        r.rejected.iter().all(|x| x.contains("withheld")),
        "{:?}",
        r.rejected
    );
    assert!(!b.all_text().contains(NEEDLE));
}

#[test]
fn f1_the_validator_refuses_structural_values_redaction_would_change() {
    let b = bench();
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "x");
    b.with(move |s| {
        let packet = operations::Packet::new(vec![user], Vec::new(), Vec::new(), 0);
        let secrets = vec![NEEDLE.to_owned()];
        for op in [
            format!(r#"{{"op":"create","room":"officina","slug":"a-{NEEDLE}","kind":"procedura","summary":"x","sources":["{user}"]}}"#),
            format!(r#"{{"op":"create","room":"officina","kind":"procedura","summary":"x","scope":["branch:x/{NEEDLE}"],"sources":["{user}"]}}"#),
            format!(r#"{{"op":"create","room":"officina","kind":"procedura","summary":"x","anchors":[{{"path":"a/{NEEDLE}.rs","blob":"{BLOB}"}}],"sources":["{user}"]}}"#),
            // A pattern secret in a scope value.
            format!(r#"{{"op":"create","room":"officina","kind":"procedura","summary":"x","scope":["branch:ghp_0123456789abcdefghijABCDEFGHIJ"],"sources":["{user}"]}}"#),
            // A summary that is only the secret slugifies into nothing safe.
            format!(r#"{{"op":"create","room":"officina","kind":"procedura","summary":"{NEEDLE}","sources":["{user}"]}}"#),
        ] {
            let v = operations::validate(&s.read(), &packet, operations::parse(&format!("[{op}]")).unwrap(), &secrets);
            assert!(v.ops.is_empty(), "{op}: {v:?}");
        }
        Ok(())
    });
}

// ---- 2. pause, epoch, cancel at the publication boundary

#[test]
fn f2_a_pause_after_validation_blocks_the_publication() {
    let b = bench();
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "x");
    let n = b.note(Room::Workshop, "Release with make.", &[user]);
    b.host
        .reply(&format!(r#"[{{"op":"absorb","annotation":"{n}"}}]"#));
    let host = Arc::clone(&b.host);
    let pause = move || {
        host.set_paused(true);
        host.set_paused(false);
    };
    let r = b.dream_full(None, Some(&pause), &Trigger::Now);
    assert_eq!(r.outcome, Some(RunOutcome::HeldByPause));
    assert_eq!(b.version(), 0);
    assert_eq!(b.staged_count(), 0);
}

#[test]
fn f2_a_pause_while_staging_waits_in_the_writer_queue_stages_nothing() {
    let b = bench();
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "x");
    let n = b.note(Room::Workshop, "Release with make.", &[user]);
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    b.host.hold_runs(entered_tx, release_rx);
    b.host
        .reply(&format!(r#"[{{"op":"absorb","annotation":"{n}"}}]"#));
    let worker = b.dream_in_thread();
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    // The writer is busy when the reply comes back.
    let (block_tx, block_rx) = crossbeam_channel::bounded::<()>(1);
    b.writer
        .handle()
        .post(move |_| {
            let _ = block_rx.recv();
        })
        .unwrap();
    release_tx.send(()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    b.host.set_paused(true);
    b.host.set_paused(false);
    block_tx.send(()).unwrap();
    let r = worker.join().unwrap();
    assert_eq!(r.outcome, Some(RunOutcome::HeldByPause));
    assert_eq!((b.version(), b.staged_count()), (0, 0));
}

// ---- 3. a copy under a new id does not launder exposure

#[test]
fn f3_a_copy_of_x_from_a_session_that_read_x_is_neither_verified_nor_established() {
    let b = bench();
    let claim = b.evidence(EvidenceKind::AgentClaim, "s1", "WAL breaks on NFS.");
    let x = b.create(memory(
        "problemi/wal-nfs",
        MemoryKind::Problem,
        None,
        "WAL breaks on NFS.",
        "",
        &[claim],
    ));
    let x2 = x.clone();
    b.with(move |s| {
        s.record_served(
            Some("s9"),
            Some("term-s9"),
            Verb::Read,
            &[Subject::Memory(x2)],
            0,
        )
    });
    let echo = b.evidence(
        EvidenceKind::AgentClaim,
        "s9",
        "Confirmed: WAL breaks on NFS.",
    );
    let n = b.note(Room::Problems, "WAL on NFS is broken (confirmed).", &[echo]);
    b.host.reply(&format!(
        r#"[{{"op":"create","ref":"c","room":"problemi","slug":"wal-on-nfs","kind":"problema","standing":"accertato","status":"verificato","summary":"WAL breaks on NFS.","sources":["{echo}"]}},
            {{"op":"absorb","annotation":"{n}","into":"c"}},
            {{"op":"update","id":"{x}","standing":"accertato","sources":["{echo}"]}},
            {{"op":"set_status","id":"{x}","status":"verificato","sources":["{echo}"]}}]"#
    ));
    let r = b.dream();
    assert!(
        matches!(r.outcome, Some(RunOutcome::Published { .. })),
        "{r:?}"
    );
    let all = b.with(|s| s.read().memories());
    for m in &all {
        assert_eq!(m.standing, Standing::Hypothesis, "{}", m.address);
        assert_eq!(m.status, MemoryStatus::Candidate, "{}", m.address);
    }
    assert_eq!(all.len(), 2);
    assert_eq!(r.rejected.len(), 2, "{r:?}");
}

// ---- 4. later sessions see what earlier ones staged

#[test]
fn f4_a_merge_is_judged_on_the_standing_an_earlier_session_lowered() {
    let mut b = bench();
    b.settings.max_input_chars = 5000;
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "We decided both.");
    let long = "Reasoning. ".repeat(110);
    let a = b.create(memory(
        "decisioni/a",
        MemoryKind::Decision,
        Some(Standing::Accepted),
        "Use SQLite.",
        &long,
        &[user],
    ));
    let bb = b.create(memory(
        "decisioni/b",
        MemoryKind::Decision,
        Some(Standing::Accepted),
        "Use WAL.",
        &long,
        &[user],
    ));
    let reply = format!(
        r#"[{{"op":"update","id":"{bb}","standing":"proposta"}},{{"op":"merge","into":"{a}","from":["{bb}"]}}]"#
    );
    for _ in 0..10 {
        b.host.reply(&reply);
    }
    let r = b.dream();
    assert!(
        b.host.jobs().len() >= 3,
        "two room sessions and the final pass"
    );
    assert!(b.host.jobs().last().unwrap().prompt.contains("Final pass"));
    assert!(
        matches!(r.outcome, Some(RunOutcome::Published { .. })),
        "{r:?}"
    );
    let (ma, mb) = b.with(move |s| Ok((s.read().memory(&a)?, s.read().memory(&bb)?)));
    assert_eq!(ma.unwrap().standing, Standing::Accepted);
    let mb = mb.expect("B was not merged away");
    assert_eq!(mb.standing, Standing::Proposed);
    assert!(
        r.rejected.iter().any(|x| x.contains("keep them apart")),
        "{:?}",
        r.rejected
    );
}

#[test]
fn f4_the_final_recheck_refuses_an_ordered_list_that_breaks_a_rule() {
    let b = bench();
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "x");
    let a = b.create(memory(
        "decisioni/a",
        MemoryKind::Decision,
        Some(Standing::Accepted),
        "A.",
        "",
        &[user],
    ));
    let bb = b.create(memory(
        "decisioni/b",
        MemoryKind::Decision,
        Some(Standing::Accepted),
        "B.",
        "",
        &[user],
    ));
    b.with(move |s| {
        let mut lower = MemoryUpdate::new(bb.clone());
        lower.standing = Some(Standing::Proposed);
        let staged = vec![
            Staged::Op(MemoryOp::Update(lower)),
            Staged::Op(MemoryOp::Merge {
                into: a.clone(),
                from: vec![bb.clone()],
            }),
        ];
        let judged = operations::recheck(&s.read(), &staged, 0, &[]);
        assert_eq!(judged.ops().len(), 1);
        assert_eq!(judged.op_origin(0), Some(0));
        assert_eq!(judged.rejected.len(), 1);
        assert!(judged.rejected[0].reason.contains("keep them apart"));
        Ok(())
    });
}

// ---- 5. a fragment authorizes only what it delivered

#[test]
fn f5_a_fragment_cannot_cite_evidence_delivered_only_to_a_later_fragment() {
    let mut b = bench();
    b.settings.max_input_chars = 5000;
    let e1 = b.evidence(
        EvidenceKind::UserStatement,
        "u1",
        &"first part ".repeat(110),
    );
    let e2 = b.evidence(
        EvidenceKind::UserStatement,
        "u2",
        &format!("LATER-ONLY {}", "second part ".repeat(100)),
    );
    let _n = b.note(Room::Workshop, "Release procedure.", &[e1, e2]);
    // The first fragment's reply cites e2, which it was not given.
    b.host.reply(&format!(
        r#"[{{"op":"create","room":"officina","kind":"procedura","summary":"From later text.","sources":["{e2}"]}}]"#
    ));
    for _ in 0..10 {
        b.host.reply("[]");
    }
    let r = b.dream();
    let jobs = b.host.jobs();
    assert!(jobs.len() >= 2);
    assert!(!jobs[0].prompt.contains("LATER-ONLY"));
    assert!(jobs.iter().skip(1).any(|j| j.prompt.contains("LATER-ONLY")));
    // Its one proposal refused: "1 proposed, 0 kept", not "nothing to change".
    assert!(
        matches!(&r.outcome, Some(RunOutcome::Refused { proposed: 1, reason })
            if reason.starts_with("1 proposed, 0 kept: sources:")),
        "{r:?}"
    );
    assert!(
        r.rejected[0].contains("not in the material"),
        "{:?}",
        r.rejected
    );
    for j in &jobs {
        assert!(j.prompt.chars().count() <= 5000);
    }
}

#[test]
fn f5_a_final_pass_part_names_only_the_memories_it_listed() {
    let mut b = bench();
    b.settings.max_input_chars = 5000;
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "x");
    let long = |c: &str| format!("{c} {}", "summary words ".repeat(26));
    let m1 = b.create(memory(
        "architettura/m1",
        MemoryKind::Fact,
        None,
        &long("first"),
        "",
        &[user],
    ));
    for i in 0..5 {
        b.create(memory(
            &format!("officina/m{i}"),
            MemoryKind::Procedure,
            None,
            &long("middle"),
            "",
            &[user],
        ));
    }
    let m5 = b.create(memory(
        "problemi/m5",
        MemoryKind::Problem,
        None,
        &long("last"),
        "",
        &[user],
    ));
    let link = format!(r#"[{{"op":"link","from":"{m1}","relation":"vedi_anche","to":"{m5}"}}]"#);
    for _ in 0..20 {
        b.host.reply(&link);
    }
    let r = b.dream();
    let finals = b
        .host
        .jobs()
        .iter()
        .filter(|j| j.prompt.contains("Final pass"))
        .count();
    assert!(finals >= 2, "the index is in several parts");
    // Every link refused: the run says so.
    assert!(
        matches!(&r.outcome, Some(RunOutcome::Refused { proposed, .. }) if *proposed > 0),
        "{r:?}"
    );
    let rels = b.with(|s| s.read().relations());
    assert!(rels.is_empty());
}

// ---- 6. a committed publication is never dreamt again

fn at(day: &str, hour: u8, minute: u8) -> WallClock {
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
fn f6_a_crash_after_a_scheduled_publication_committed_is_finished_not_rerun() {
    let mut b = bench();
    b.settings.days = Weekday::ALL.iter().copied().collect();
    b.settings.min_new_items = 0;
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "x");
    let n = b.note(Room::Workshop, "Release with make.", &[user]);
    let settings = b.settings.clone();
    b.host.set_clock(at("2026-11-16", 1, 0));
    let c = at("2026-11-16", 1, 0);
    b.with(move |s| scheduler::check(s, &settings, &c).map(|_| ()));
    let now = at("2026-11-16", 2, 30);
    b.host.set_clock(now);
    let settings = b.settings.clone();
    let Check::Run(plan) = b.with(move |s| scheduler::check(s, &settings, &now)) else {
        panic!("due")
    };
    b.host
        .reply(&format!(r#"[{{"op":"absorb","annotation":"{n}"}}]"#));
    let r = b.dream_full(Some(CrashAt::AfterPublish), None, &Trigger::Scheduled(plan));
    let run = r.run_id.unwrap();
    assert_eq!(b.version(), 1, "the publication committed");
    // Restart.
    b.with(|s| {
        s.interrupt_running(10)?;
        scheduler::recover_at_start(s)?;
        Ok(())
    });
    let settings = b.settings.clone();
    let later = at("2026-11-16", 2, 31);
    assert_eq!(
        b.with(move |s| scheduler::check(s, &settings, &later)),
        Check::Idle
    );
    let (entry, rows, items) = b.with(move |s| {
        Ok((
            s.run(run)?.unwrap(),
            s.schedule(scheduler::PROGRAM)?,
            scheduler::new_items(s)?,
        ))
    });
    assert!(
        matches!(entry.outcome, RunOutcome::Published { version: 1, .. }),
        "{entry:?}"
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, ScheduleState::Done);
    assert_eq!(items, 0, "the cutoff advanced");
    assert_eq!(b.staged_count(), 0);
}

// ---- 7. the time limit holds until the commit

#[test]
fn f7_the_time_limit_is_checked_at_the_publication_boundary() {
    let mut b = bench();
    b.settings.max_minutes = 1;
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "x");
    let n = b.note(Room::Workshop, "Release with make.", &[user]);
    b.host
        .reply(&format!(r#"[{{"op":"absorb","annotation":"{n}"}}]"#));
    let host = Arc::clone(&b.host);
    let late = move || host.advance(Duration::from_secs(61));
    let r = b.dream_full(None, Some(&late), &Trigger::Now);
    assert!(
        matches!(&r.outcome, Some(RunOutcome::Failed { reason }) if reason.contains("time limit")),
        "{r:?}"
    );
    assert_eq!((b.version(), b.staged_count()), (0, 0));
}

#[test]
fn f7_a_writer_delay_after_the_last_reply_cannot_carry_a_dream_past_its_limit() {
    let mut b = bench();
    b.settings.max_minutes = 1;
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "x");
    let n = b.note(Room::Workshop, "Release with make.", &[user]);
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    b.host.hold_runs(entered_tx, release_rx);
    b.host
        .reply(&format!(r#"[{{"op":"absorb","annotation":"{n}"}}]"#));
    let worker = {
        let host = Arc::clone(&b.host) as Arc<dyn Host>;
        let handle = b.writer.handle();
        let settings = b.settings.clone();
        let project = b.project.clone();
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
    };
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (block_tx, block_rx) = crossbeam_channel::bounded::<()>(1);
    let host = Arc::clone(&b.host);
    b.writer
        .handle()
        .post(move |_| {
            let _ = block_rx.recv();
            host.advance(Duration::from_secs(61));
        })
        .unwrap();
    release_tx.send(()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    block_tx.send(()).unwrap();
    let r = worker.join().unwrap();
    assert!(
        matches!(&r.outcome, Some(RunOutcome::Failed { reason }) if reason.contains("time limit")),
        "{r:?}"
    );
    assert_eq!((b.version(), b.staged_count()), (0, 0));
}

// ---- siblings

#[test]
fn sa_an_unknown_blob_is_neither_a_change_nor_a_deletion() {
    let b = bench();
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "x");
    let mut new = memory(
        "officina/build",
        MemoryKind::Procedure,
        None,
        "Build with make.",
        "",
        &[user],
    );
    new.anchors = vec![
        Anchor::new("Makefile", BLOB).unwrap(),
        Anchor::new("gone.mk", BLOB).unwrap(),
        Anchor::new("big.bin", BLOB).unwrap(),
    ];
    b.create(new);
    let _ = b.note(Room::Workshop, "Build notes.", &[user]);
    b.host.set_blob("Makefile", BLOB);
    b.host.set_missing("gone.mk");
    // big.bin: not set, so unknown (blob None, not deleted).
    b.host.reply("[]");
    b.dream();
    let p = b.host.jobs()[0].prompt.clone();
    assert!(p.contains("anchor Makefile at 0123456789abcdef0123456789abcdef01234567: unchanged"));
    assert!(p.contains(
        "anchor gone.mk at 0123456789abcdef0123456789abcdef01234567: deleted since observed"
    ));
    assert!(p.contains(
        "anchor big.bin at 0123456789abcdef0123456789abcdef01234567: current state unknown"
    ));
}

#[test]
fn sb_a_limit_too_small_for_the_fixed_prompt_is_a_visible_misconfiguration() {
    let mut b = bench();
    // Below the contract minimum: only reachable without clamping.
    b.settings.max_input_chars = 2000;
    let user = b.evidence(EvidenceKind::UserStatement, "u1", "x");
    let _ = b.note(Room::Workshop, "Release with make.", &[user]);
    let r = b.dream();
    assert!(
        matches!(&r.outcome, Some(RunOutcome::Failed { reason }) if reason.contains("raise max_input_chars")),
        "{r:?}"
    );
    assert!(b.host.jobs().is_empty(), "nothing over the limit was sent");
    // At the contract minimum every request fits, fixed part included.
    let mut b = bench();
    b.settings.max_input_chars = connecto_core::memory::MIN_INPUT_CHARS;
    let user = b.evidence(EvidenceKind::UserStatement, "u1", &"context ".repeat(400));
    for i in 0..6 {
        b.note(
            Room::Workshop,
            &format!("Note {i}: {}", "words ".repeat(150)),
            &[user],
        );
    }
    for _ in 0..30 {
        b.host.reply("[]");
    }
    b.dream();
    let jobs = b.host.jobs();
    assert!(jobs.len() > 1);
    for j in jobs {
        assert!(
            j.prompt.chars().count() <= 5000,
            "{}",
            j.prompt.chars().count()
        );
    }
}

#[test]
fn sc_git_and_metadata_holding_a_secret_are_withheld_from_prompts() {
    let b = bench();
    b.host.set_secrets(vec![NEEDLE.to_owned()]);
    b.host.set_git(Some(GitSnapshot {
        branch: Some(format!("feature/{NEEDLE}")),
        head: Some("abcdef1234567".to_owned()),
        changed: vec![
            ChangedFile {
                path: format!("src/{NEEDLE}.rs"),
                blob: None,
                deleted: false,
            },
            ChangedFile {
                path: "src/lib.rs".to_owned(),
                blob: Some(BLOB.to_owned()),
                deleted: false,
            },
        ],
    }));
    let e = b.evidence_with(
        EvidenceKind::UserStatement,
        "s1",
        "Release with make.",
        &format!("t-{NEEDLE}"),
    );
    let _ = b.note(Room::Workshop, "Release with make.", &[e]);
    b.host.reply("[]");
    b.dream();
    let p = b.host.jobs()[0].prompt.clone();
    assert!(!p.contains(NEEDLE), "{p}");
    assert!(p.contains("src/lib.rs") && p.contains("and 1 more"));
    assert!(p.contains("(withheld)"));
}
