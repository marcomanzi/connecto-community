//! The robustness suite's cross-component rows (spec §11.2, §11.2b) that the
//! per-component suites left partial; the traceability table is
//! `docs/specs/project-memory.work/robustness.md`.
//!
//! Real process deaths: a child copy of this test binary is stopped dead
//! (SIGKILL) while it holds a write transaction open. The stall comes from a
//! `TEMP` trigger the child puts on its own connection, which turns the
//! write into a very long computation, so the parent knows exactly which
//! write the process died in. Fake host, fake model, fake clock: no CLI.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::{PROJECT, T0, create, evidence_in};
use connecto_core::Redacted;
use connecto_core::memory::{
    AnnotationKind, AnnotationOrigin, AnnotationTarget, Cycle, DreamSettings, EvidenceKind,
    MemoryId, MemoryKind, MemoryStatus, Room, RunOutcome, Scope, Seen, Standing, Subject,
    UserStatementKind, Verification,
};
use connecto_memory::dream::{self, CrashAt, DreamReport, Trigger};
use connecto_memory::testing::FakeHost;
use connecto_memory::user_statement::{Applied, Stated};
use connecto_memory::{Host, NewAnnotation, PublishStep, Store, Writer, scheduler};

// ---- killing a child process inside a write transaction

const CHILD_DB: &str = "CONNECTO_ROBUSTNESS_DB";
const CHILD_TRIGGER: &str = "CONNECTO_ROBUSTNESS_TRIGGER";

/// SQL that makes the connection it runs on stall inside the write that
/// fires `trigger` (for example `AFTER INSERT ON publication`): about 10^14
/// steps of a cross join, far longer than any test waits.
fn stall_sql(trigger: &str) -> String {
    format!(
        "CREATE TEMP TABLE slow (x INTEGER);
         WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 100)
         INSERT INTO slow SELECT x FROM c;
         CREATE TEMP TRIGGER stall {trigger} BEGIN
           SELECT count(*) FROM slow a, slow b, slow c, slow d, slow e, slow f, slow g;
         END;"
    )
}

/// Run `child` (a test of this binary) with the database and the trigger in
/// its environment, wait until it has held the write lock for a while (it
/// is stalled inside the transaction), and kill it. Panics if the child
/// finished on its own: the trigger never fired.
fn kill_inside(child: &str, db: &Path, trigger: &str) {
    let exe = std::env::current_exe().unwrap();
    let mut process = std::process::Command::new(exe)
        .args([child, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_DB, db)
        .env(CHILD_TRIGGER, trigger)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut locked_since: Option<Instant> = None;
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            panic!("{child} at `{trigger}` ended on its own ({status}): the write never stalled");
        }
        assert!(Instant::now() < deadline, "{child} at `{trigger}`: no lock");
        let probe = rusqlite::Connection::open(db).unwrap();
        probe.busy_timeout(Duration::ZERO).unwrap();
        let locked = probe.execute_batch("BEGIN IMMEDIATE").is_err();
        if !locked {
            let _ = probe.execute_batch("ROLLBACK");
        }
        drop(probe);
        locked_since = match (locked, locked_since) {
            (false, _) => None,
            (true, None) => Some(Instant::now()),
            (true, Some(t)) => Some(t),
        };
        if locked_since.is_some_and(|t| t.elapsed() > Duration::from_millis(500)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    process.kill().unwrap();
    let status = process.wait().unwrap();
    assert!(!status.success(), "{child} was killed");
}

fn child_env() -> Option<(PathBuf, String)> {
    let db = std::env::var(CHILD_DB).ok()?;
    let trigger = std::env::var(CHILD_TRIGGER).ok()?;
    Some((PathBuf::from(db), trigger))
}

// ---- crash: a user statement (one transaction, killed at each write)

fn seen_memory(store: &Store, id: &MemoryId) -> Seen {
    store.read().memory(id).unwrap().unwrap().seen()
}

fn accept(store: &Store, id: &MemoryId) -> Stated {
    Stated {
        kind: UserStatementKind::Accept,
        about: Some(Subject::Memory(id.clone())),
        room: None,
        text: Redacted::assume("we agreed on it".to_owned()),
        seen: Some(seen_memory(store, id)),
    }
}

/// Everything a user statement can touch, as plain values.
fn statement_state(db: &Path) -> (usize, usize, u64, Vec<(Standing, MemoryStatus)>, i64) {
    let mut store = Store::open(db, PROJECT).unwrap();
    let revisions: i64 = store
        .connection_mut()
        .query_row("SELECT count(*) FROM revision", [], |r| r.get(0))
        .unwrap();
    let read = store.read();
    let memories = read
        .memories()
        .unwrap()
        .into_iter()
        .map(|m| (m.standing, m.status))
        .collect();
    (
        read.all_evidence().unwrap().len(),
        read.annotations().unwrap().len(),
        read.version().unwrap(),
        memories,
        revisions,
    )
}

fn proposed_decision(db: &Path) -> MemoryId {
    let mut store = Store::open(db, PROJECT).unwrap();
    let ev = evidence_in(&mut store, EvidenceKind::AgentProposal, Some("agent"), T0).id;
    create(
        &mut store,
        "decisioni/use-sqlite",
        MemoryKind::Decision,
        &[ev],
    )
}

/// Child: make the statement with a stall at the named write.
#[test]
fn user_statement_child() {
    let Some((db, trigger)) = child_env() else {
        return;
    };
    let mut store = Store::open(&db, PROJECT).unwrap();
    store
        .connection_mut()
        .execute_batch(&stall_sql(&trigger))
        .unwrap();
    let id = store.read().memories().unwrap()[0].id.clone();
    let stated = accept(&store, &id);
    let _ = store.user_statement(&stated, T0 + 60_000, &[]);
    std::process::exit(0);
}

#[test]
fn a_user_statement_killed_at_any_write_leaves_nothing_and_can_be_made_again() {
    for trigger in [
        "AFTER INSERT ON evidence",
        "AFTER INSERT ON annotation",
        "AFTER INSERT ON revision",
        "AFTER UPDATE ON memory",
        "AFTER INSERT ON publication",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let id = proposed_decision(&db);
        let before = statement_state(&db);
        assert_eq!(before.3, [(Standing::Proposed, MemoryStatus::Candidate)]);
        kill_inside("user_statement_child", &db, trigger);
        assert_eq!(statement_state(&db), before, "killed at `{trigger}`");
        // Reopened, the same statement goes through whole, once.
        let mut store = Store::open(&db, PROJECT).unwrap();
        let stated = accept(&store, &id);
        let r = store.user_statement(&stated, T0 + 60_000, &[]).unwrap();
        assert!(
            matches!(r.applied, Applied::Published(v) if v == before.2 + 1),
            "{r:?}"
        );
        drop(store);
        let after = statement_state(&db);
        assert_eq!(
            (after.0, after.1, after.2),
            (before.0 + 1, before.1 + 1, before.2 + 1)
        );
        assert_eq!(after.3, [(Standing::Accepted, MemoryStatus::Verified)]);
    }
}

// ---- crash: a dream (pending record and reconcile), in-process and killed

struct DreamBench {
    _dir: tempfile::TempDir,
    db: PathBuf,
    project: PathBuf,
    writer: Option<Writer>,
    host: Arc<FakeHost>,
    settings: DreamSettings,
}

impl DreamBench {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let db = dir.path().join("memory.db");
        let writer = Writer::spawn(&db, PROJECT, 64).unwrap();
        Self {
            _dir: dir,
            db,
            project,
            writer: Some(writer),
            host: Arc::new(FakeHost::new()),
            settings: DreamSettings::default(),
        }
    }

    fn with<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Store) -> connecto_memory::Result<R> + Send + 'static,
    ) -> R {
        self.writer.as_ref().unwrap().call(f).unwrap().unwrap()
    }

    fn dream(&self, crash: Option<CrashAt>) -> DreamReport {
        let handle = self.writer.as_ref().unwrap().handle();
        let cx = dream::Context {
            host: Arc::clone(&self.host) as Arc<dyn Host>,
            writer: &handle,
            project_id: PROJECT,
            project_path: &self.project,
            settings: &self.settings,
            cancel: Arc::new(AtomicBool::new(false)),
            crash,
            before_publish: None,
        };
        dream::run(&cx, &Trigger::Now).unwrap()
    }

    /// What a restarted app does before its first dream: runs left
    /// `running` become interrupted, staging goes, then the scheduler's
    /// check reconciles the pending records (no program: nothing is due).
    fn restart(&mut self) {
        drop(self.writer.take());
        self.writer = Some(Writer::spawn(&self.db, PROJECT, 64).unwrap());
        let host = Arc::clone(&self.host);
        self.with(move |s| {
            s.interrupt_running(host.now().utc_ms)?;
            scheduler::recover_at_start(s)?;
            scheduler::check(s, &DreamSettings::default(), &host.now())?;
            Ok(())
        });
    }

    fn version(&self) -> u64 {
        self.with(|s| s.version())
    }

    fn pending_records(&self) -> i64 {
        self.with(|s| {
            Ok(s.connection_mut().query_row(
                "SELECT count(*) FROM meta WHERE key LIKE 'dream.pending.%'",
                [],
                |r| r.get(0),
            )?)
        })
    }

    fn staged(&self) -> i64 {
        self.with(|s| {
            Ok(s.connection_mut()
                .query_row("SELECT count(*) FROM dream_staging", [], |r| r.get(0))?)
        })
    }

    fn open_notes(&self) -> usize {
        self.with(|s| Ok(s.read().open_annotations()?.len()))
    }

    fn new_items(&self) -> u64 {
        self.with(|s| scheduler::new_items(s))
    }
}

/// A note from a user statement, and the model's reply that turns it into
/// a memory and absorbs it.
fn seed_note(store: &mut Store) -> String {
    let user = evidence_in(store, EvidenceKind::UserStatement, Some("u1"), T0).id;
    let note = store
        .create_annotation(NewAnnotation {
            id: None,
            created_ms: T0,
            kind: AnnotationKind::Integration,
            target: AnnotationTarget::New {
                room: Room::Workshop,
            },
            text: Redacted::assume("Release with make release.".to_owned()),
            scopes: vec![Scope::Project],
            sources: vec![user],
            verification: Verification::Unverified,
            origin: AnnotationOrigin::Agent {
                terminal: "claude-1".to_owned(),
            },
            memory_kind: None,
            standing: None,
        })
        .unwrap();
    format!(
        r#"[{{"op":"create","ref":"r","room":"officina","kind":"procedura","summary":"Release with make release.","sources":["{user}"]}},
            {{"op":"absorb","annotation":"{n}","into":"r"}}]"#,
        n = note.id
    )
}

#[test]
fn a_dream_now_stopped_after_its_publication_committed_is_finished_by_reconcile() {
    let mut b = DreamBench::new();
    let reply = b.with(|s| Ok(seed_note(s)));
    b.host.reply(&reply);
    let r = b.dream(Some(CrashAt::AfterPublish));
    let run = r.run_id.unwrap();
    assert_eq!(b.version(), 1, "the publication committed");
    assert_eq!(b.pending_records(), 1, "its books are not written yet");
    b.restart();
    let entry = b.with(move |s| Ok(s.run(run)?.unwrap()));
    assert!(
        matches!(entry.outcome, RunOutcome::Published { version: 1, .. }),
        "finished, not interrupted: {entry:?}"
    );
    assert_eq!(b.pending_records(), 0);
    assert_eq!(b.staged(), 0);
    assert_eq!(b.open_notes(), 0);
    assert_eq!(b.new_items(), 0, "the cutoff advanced");
    // The next "Dream now" is not offered the absorbed note again.
    let note = reply
        .split('"')
        .find(|w| w.starts_with("p-"))
        .unwrap()
        .to_owned();
    b.host.reply("[]");
    let again = b.dream(None);
    assert_eq!(again.outcome, Some(RunOutcome::NoChange), "{again:?}");
    let last = b.host.jobs().last().unwrap().prompt.clone();
    assert!(!last.contains(&note), "the absorbed note came back");
    assert_eq!(b.version(), 1);
}

#[test]
fn a_dream_stopped_inside_its_publication_publishes_once_after_restart() {
    let mut b = DreamBench::new();
    let reply = b.with(|s| Ok(seed_note(s)));
    b.host.reply(&reply);
    let r = b.dream(Some(CrashAt::Publish(PublishStep::Commit)));
    let run = r.run_id.unwrap();
    assert_eq!(b.version(), 0);
    assert_eq!(b.pending_records(), 1);
    assert!(b.staged() > 0);
    b.restart();
    let entry = b.with(move |s| Ok(s.run(run)?.unwrap()));
    assert_eq!(entry.outcome, RunOutcome::Interrupted);
    assert_eq!(b.pending_records(), 0, "a record with no publication goes");
    assert_eq!((b.version(), b.staged(), b.open_notes()), (0, 0, 1));
    b.host.reply(&reply);
    let again = b.dream(None);
    assert!(
        matches!(
            again.outcome,
            Some(RunOutcome::Published { version: 1, .. })
        ),
        "{again:?}"
    );
    assert_eq!(
        (b.version(), b.open_notes(), b.pending_records()),
        (1, 0, 0)
    );
}

/// Child: one dream on the database, with a stall at the named write.
#[test]
fn dream_child() {
    let Some((db, trigger)) = child_env() else {
        return;
    };
    let dir = db.parent().unwrap().to_owned();
    let writer = Writer::spawn(&db, PROJECT, 64).unwrap();
    let sql = stall_sql(&trigger);
    writer
        .call(move |s| -> connecto_memory::Result<()> {
            Ok(s.connection_mut().execute_batch(&sql)?)
        })
        .unwrap()
        .unwrap();
    let reply = std::fs::read_to_string(dir.join("reply.json")).unwrap();
    let host = Arc::new(FakeHost::new());
    host.reply(&reply);
    let handle = writer.handle();
    let settings = DreamSettings::default();
    let cx = dream::Context {
        host: host as Arc<dyn Host>,
        writer: &handle,
        project_id: PROJECT,
        project_path: &dir,
        settings: &settings,
        cancel: Arc::new(AtomicBool::new(false)),
        crash: None,
        before_publish: None,
    };
    let _ = dream::run(&cx, &Trigger::Now);
    std::process::exit(0);
}

#[test]
fn a_dream_killed_inside_or_after_its_publication_is_old_or_finished_never_twice() {
    // (trigger, published by the killed process)
    for (trigger, committed) in [
        ("AFTER INSERT ON dream_staging", false),
        ("AFTER INSERT ON revision", false),
        ("AFTER INSERT ON publication", false),
        // The run's books, in their own transaction after the commit.
        ("AFTER UPDATE ON run WHEN NEW.finished_ms IS NOT NULL", true),
    ] {
        let mut b = DreamBench::new();
        let reply = b.with(|s| Ok(seed_note(s)));
        std::fs::write(b.db.parent().unwrap().join("reply.json"), &reply).unwrap();
        // The child needs the database to itself.
        drop(b.writer.take());
        kill_inside("dream_child", &b.db, trigger);
        b.writer = Some(Writer::spawn(&b.db, PROJECT, 64).unwrap());
        assert_eq!(b.version(), u64::from(committed), "killed at `{trigger}`");
        b.restart();
        let runs = b.with(|s| s.runs(Some(Cycle::Dream), 10));
        assert_eq!(runs.len(), 1, "{trigger}");
        assert_eq!(b.pending_records(), 0, "{trigger}");
        assert_eq!(b.staged(), 0, "{trigger}");
        if committed {
            assert!(
                matches!(runs[0].outcome, RunOutcome::Published { version: 1, .. }),
                "{trigger}: {runs:?}"
            );
            assert_eq!((b.open_notes(), b.new_items()), (0, 0), "{trigger}");
        } else {
            assert_eq!(runs[0].outcome, RunOutcome::Interrupted, "{trigger}");
            assert_eq!(b.open_notes(), 1, "{trigger}");
            b.host.reply(&reply);
            let again = b.dream(None);
            assert!(
                matches!(again.outcome, Some(RunOutcome::Published { .. })),
                "{trigger}: {again:?}"
            );
        }
        // Either way, exactly one version holds the dream.
        assert_eq!(b.version(), 1, "{trigger}");
        let memories = b.with(|s| s.read().memories());
        assert_eq!(memories.len(), 1, "{trigger}");
    }
}

// ---- through the engine

mod world {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use connecto_core::memory::{
        Cycle, MemoryControl, MemorySettings, ProjectMemoryView, Provider, RunEntry,
    };
    use connecto_memory::capture::Author;
    use connecto_memory::testing::FakeHost;
    use connecto_memory::{
        Captured, CapturedReply, Engine, EngineConfig, Happened, OpenProject, Reader,
    };

    pub struct World {
        pub dir: tempfile::TempDir,
        pub host: Arc<FakeHost>,
        pub engine: Engine,
    }

    impl Drop for World {
        fn drop(&mut self) {
            self.engine.shutdown();
        }
    }

    pub fn world(projects: &[&str]) -> World {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeHost::new());
        host.set_projects(
            projects
                .iter()
                .map(|id| {
                    let path = dir.path().join("repos").join(id);
                    std::fs::create_dir_all(&path).unwrap();
                    OpenProject {
                        id: (*id).to_owned(),
                        path,
                    }
                })
                .collect(),
        );
        let engine = start(&dir, &host);
        World { dir, host, engine }
    }

    pub fn start(dir: &tempfile::TempDir, host: &Arc<FakeHost>) -> Engine {
        Engine::start(EngineConfig {
            data_root: dir.path().join("data"),
            host: Arc::clone(host) as Arc<dyn connecto_memory::Host>,
        })
        .unwrap()
    }

    pub fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ok() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn view(w: &World, id: &str) -> Option<ProjectMemoryView> {
        w.engine.view().project(id).cloned()
    }

    pub fn db(w: &World, id: &str) -> PathBuf {
        w.dir
            .path()
            .join("data/projects")
            .join(id)
            .join("memory/memory.db")
    }

    pub fn reader(w: &World, id: &str) -> Reader {
        Reader::open(&db(w, id), id).unwrap()
    }

    pub fn enable(w: &World, id: &str, group_seconds: u32) {
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

    pub fn captured(
        project: &str,
        terminal: &str,
        session: &str,
        turn: &str,
        what: Happened,
    ) -> Captured {
        Captured {
            project_id: project.to_owned(),
            terminal: terminal.to_owned(),
            provider: Provider::Claude,
            tool_session: Some(session.to_owned()),
            session_start_ms: 1,
            turn: turn.to_owned(),
            log: None,
            what,
        }
    }

    /// A user prompt and the agent's reply, in `terminal`/`session`.
    pub fn turn(w: &World, project: &str, who: (&str, &str), n: u32, prompt: &str, reply: &str) {
        let t = format!("#{n}");
        assert!(w.engine.capture().offer(captured(
            project,
            who.0,
            who.1,
            &t,
            Happened::Prompt {
                text: prompt.to_owned(),
                author: Author::User,
                truncated: false,
            }
        )));
        assert!(w.engine.capture().offer(captured(
            project,
            who.0,
            who.1,
            &t,
            Happened::TurnEnd {
                reply: CapturedReply::Text(reply.to_owned())
            }
        )));
    }

    pub fn runs(w: &World, id: &str, cycle: Cycle) -> Vec<RunEntry> {
        view(w, id)
            .map(|v| {
                v.runs
                    .iter()
                    .filter(|r| r.cycle == cycle && !r.outcome.is_running())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

use connecto_core::memory::{
    Lookup, MemoryBridge, MemoryCaller, MemoryCommand, MemoryControl, UserStatement,
};
use world::{World, db, enable, reader, runs, turn, view, wait_for};

fn caller(terminal: &str, session: &str) -> MemoryCaller {
    MemoryCaller {
        terminal: terminal.into(),
        tool_session: Some(session.into()),
        branch: None,
    }
}

fn propose(w: &World, id: &str, who: &MemoryCaller, room: Room, text: &str) -> String {
    w.engine.command(
        id,
        who,
        &MemoryCommand::Propose {
            kind: AnnotationKind::Integration,
            about: None,
            room: Some(room),
            scope: None,
        },
        Some(text),
    )
}

fn evidence_of(w: &World, id: &str, session: &str) -> Vec<connecto_core::memory::Evidence> {
    reader(w, id)
        .read()
        .all_evidence()
        .unwrap()
        .into_iter()
        .filter(|e| e.tool_session.as_deref() == Some(session))
        .collect()
}

#[test]
fn development_through_the_engine_waits_out_a_pause_and_resumes_after_stop_everything() {
    let w = world::world(&["p1"]);
    enable(&w, "p1", 0);
    // Paused before the turn: captured and queued, no run.
    w.host.set_paused(true);
    turn(&w, "p1", ("t1", "s1"), 1, "how do we build?", "With make.");
    wait_for("queued", || {
        view(&w, "p1").is_some_and(|v| v.backlog.queued == 2)
    });
    std::thread::sleep(Duration::from_millis(1200));
    assert!(w.host.jobs().is_empty(), "no run starts in a pause");
    // Resume: the queue runs.
    w.host.reply(r#"{"annotations":[]}"#);
    w.host.set_paused(false);
    wait_for("the run after resume", || {
        runs(&w, "p1", Cycle::Development).len() == 1
    });
    wait_for("the queue drained", || {
        view(&w, "p1").is_some_and(|v| v.backlog.queued == 0)
    });
    // Stop Everything during a run: cancelled, the queue kept, nothing
    // until resumed; then the same events run again.
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(4);
    w.host.hold_runs(entered_tx, release_rx);
    w.host.reply(r#"{"annotations":[]}"#);
    turn(&w, "p1", ("t1", "s1"), 2, "and the tests?", "cargo test.");
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    w.host.set_paused(true);
    w.engine.cancel_runs();
    wait_for("cancelled", || {
        runs(&w, "p1", Cycle::Development)
            .first()
            .is_some_and(|r| r.outcome == RunOutcome::Cancelled)
    });
    wait_for("kept", || {
        view(&w, "p1").is_some_and(|v| v.backlog.queued == 2)
    });
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(w.host.jobs().len(), 2, "nothing restarts while stopped");
    drop(release_tx);
    w.host.reply(r#"{"annotations":[]}"#);
    w.host.set_paused(false);
    wait_for("the events run again", || {
        view(&w, "p1").is_some_and(|v| v.backlog.queued == 0)
            && runs(&w, "p1", Cycle::Development)
                .first()
                .is_some_and(|r| matches!(r.outcome, RunOutcome::Annotated { .. }))
    });
    assert_eq!(w.host.jobs().len(), 3);
}

#[test]
fn an_engine_restart_keeps_the_queue_and_reruns_a_batch_cut_in_flight() {
    let mut w = world::world(&["p1"]);
    enable(&w, "p1", 0);
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(4);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    w.host.hold_runs(entered_tx, release_rx);
    w.host.reply(r#"{"annotations":[]}"#);
    turn(&w, "p1", ("t1", "s1"), 1, "first prompt", "first reply");
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    // More events arrive while the batch runs; then the app closes.
    turn(&w, "p1", ("t1", "s1"), 2, "second prompt", "second reply");
    wait_for("all four events stored", || {
        reader(&w, "p1").read().event_seq().unwrap() >= 4
    });
    w.engine.shutdown();
    // The next start: nothing lost, the cut batch runs again.
    drop(release_tx);
    w.host.reply(r#"{"annotations":[]}"#);
    w.host.reply(r#"{"annotations":[]}"#);
    let engine = world::start(&w.dir, &w.host);
    w.engine = engine;
    wait_for("everything considered", || {
        view(&w, "p1").is_some_and(|v| v.enabled && v.backlog.queued == 0)
            && runs(&w, "p1", Cycle::Development)
                .iter()
                .any(|r| matches!(r.outcome, RunOutcome::Annotated { .. }))
    });
    let all = runs(&w, "p1", Cycle::Development);
    assert!(
        all.iter()
            .any(|r| matches!(r.outcome, RunOutcome::Interrupted | RunOutcome::Cancelled)),
        "{all:?}"
    );
    let prompts: String = w.host.jobs().iter().map(|j| j.prompt.clone()).collect();
    for text in [
        "first prompt",
        "first reply",
        "second prompt",
        "second reply",
    ] {
        assert!(prompts.contains(text), "{text} reached a run");
    }
    let _ = db(&w, "p1");
}

/// A proposal from `t2`, made into memory X by a dream. Returns X.
fn memory_from_a_dream(w: &World, id: &str, text: &str) -> MemoryId {
    w.host.reply(r#"{"annotations":[]}"#);
    let out = propose(w, id, &caller("t2", "s2"), Room::Workshop, text);
    assert!(out.contains("Stored note"), "{out}");
    wait_for("the proposal's run", || {
        !runs(w, id, Cycle::Development).is_empty()
    });
    let note = reader(w, id).read().open_annotations().unwrap()[0].clone();
    w.host.reply(&format!(
        r#"{{"operations":[{{"op":"create","ref":"x","room":"officina","kind":"procedura","summary":"{text}","sources":["{e}"]}},{{"op":"absorb","annotation":"{n}","into":"x"}}]}}"#,
        e = note.sources[0].evidence,
        n = note.id
    ));
    w.engine.dream_now(id).unwrap();
    wait_for("the dream", || {
        runs(w, id, Cycle::Dream)
            .iter()
            .any(|r| matches!(r.outcome, RunOutcome::Published { .. }))
    });
    reader(w, id).read().memories().unwrap()[0].id.clone()
}

#[test]
fn an_agent_that_read_x_through_the_command_and_repeats_it_never_verifies_x_end_to_end() {
    let w = world::world(&["p1"]);
    enable(&w, "p1", 60);
    let x = memory_from_a_dream(&w, "p1", "Release with make release.");
    let fresh = reader(&w, "p1").read().memory(&x).unwrap().unwrap();
    assert_eq!(
        (fresh.status, fresh.standing),
        (MemoryStatus::Candidate, Standing::Documented)
    );
    // Session s5 reads X with `connecto memory read`...
    let page = w.engine.command(
        "p1",
        &caller("t5", "s5"),
        &MemoryCommand::Read {
            target: Lookup::Memory(x.clone()),
            budget: None,
        },
        None,
    );
    assert!(page.contains("make release"), "{page}");
    // ...then, in a later turn, repeats it as done and tested.
    turn(
        &w,
        "p1",
        ("t5", "s5"),
        1,
        "ship it",
        "Released with make release, as documented; it works and cargo test: 12 passed.",
    );
    wait_for("the turn stored", || evidence_of(&w, "p1", "s5").len() >= 2);
    let echoes: Vec<_> = evidence_of(&w, "p1", "s5")
        .into_iter()
        .filter(|e| e.kind != EvidenceKind::UserStatement)
        .collect();
    assert!(!echoes.is_empty());
    for e in &echoes {
        assert!(
            e.exposed_to.contains(&Subject::Memory(x.clone())),
            "the reply carries what its session was shown: {e:?}"
        );
    }
    let sources: Vec<String> = echoes.iter().map(|e| format!("\"{}\"", e.id)).collect();
    let sources = sources.join(",");
    // The development cycle is asked to make it executed and verified.
    w.host.reply(&format!(
        r#"{{"annotations":[{{"type":"integrazione","about":"{x}","memory_kind":"procedura","standing":"eseguita","text":"make release was run and works.","scope":["project"],"sources":[{sources}]}}]}}"#
    ));
    w.host.advance(Duration::from_secs(61));
    wait_for("the development run", || {
        runs(&w, "p1", Cycle::Development)
            .iter()
            .any(|r| matches!(r.outcome, RunOutcome::Annotated { written: 1, .. }))
    });
    let note = reader(&w, "p1")
        .read()
        .open_annotations()
        .unwrap()
        .into_iter()
        .find(|a| a.text.contains("was run and works"))
        .unwrap();
    assert_eq!(note.verification, Verification::Unverified);
    assert_eq!(note.standing, Some(Standing::Documented));
    // The dream that follows is asked to verify X on those words.
    w.host.reply(&format!(
        r#"{{"operations":[{{"op":"set_status","id":"{x}","status":"verificato","sources":[{sources}]}},{{"op":"update","id":"{x}","standing":"eseguita","sources":[{sources}]}},{{"op":"absorb","annotation":"{n}","into":"{x}"}}]}}"#,
        n = note.id
    ));
    w.engine.dream_now("p1").unwrap();
    wait_for("the second dream", || {
        runs(&w, "p1", Cycle::Dream).len() == 2
    });
    let after = reader(&w, "p1").read().memory(&x).unwrap().unwrap();
    assert_ne!(after.status, MemoryStatus::Verified, "{after:?}");
    assert_eq!(after.standing, Standing::Documented, "{after:?}");
    let page = w.engine.command(
        "p1",
        &caller("t6", "s6"),
        &MemoryCommand::Read {
            target: Lookup::Memory(x),
            budget: None,
        },
        None,
    );
    assert!(!page.contains("verificato"), "{page}");
}

const KNOWN: &str = "zebra-Known-Value-77310";
const PATTERN: &str = concat!("ghp_", "0123456789abcdefghijABCDEFGHIJ012345");

fn has_secret(text: &str) -> Option<&'static str> {
    [KNOWN, PATTERN].into_iter().find(|s| text.contains(s))
}

#[test]
fn seeded_secrets_reach_no_host_surface_including_the_view_and_exports() {
    let w = world::world(&["p1"]);
    w.host.set_secrets(vec![KNOWN.to_owned()]);
    w.host.set_git(Some(connecto_memory::GitSnapshot {
        branch: Some(format!("feature/{KNOWN}")),
        head: Some("1111111111111111111111111111111111111111".to_owned()),
        changed: Vec::new(),
    }));
    enable(&w, "p1", 0);
    // A turn, and a model that copies the secrets into its note.
    w.host.reply(&format!(
        r#"{{"annotations":[{{"type":"integrazione","memory_kind":"fatto","standing":"ipotesi","text":"Deploy uses {KNOWN} and {PATTERN}.","scope":["project"],"sources":["e-2"]}}]}}"#
    ));
    turn(
        &w,
        "p1",
        ("t1", "s1"),
        1,
        &format!("the deploy key is {KNOWN}, the token {PATTERN}"),
        &format!("Logged in with {KNOWN} and pushed with {PATTERN}."),
    );
    wait_for("the first run", || {
        !runs(&w, "p1", Cycle::Development).is_empty()
    });
    // A proposal and a flag with the secrets.
    w.host.reply(r#"{"annotations":[]}"#);
    let out = propose(
        &w,
        "p1",
        &caller("t2", "s2"),
        Room::Workshop,
        &format!("Deploy with {KNOWN} then {PATTERN}."),
    );
    assert!(out.contains("Stored note"), "{out}");
    wait_for("the proposal's run", || {
        runs(&w, "p1", Cycle::Development).len() == 2
    });
    // A run that fails with the secret in the CLI's error text.
    w.host.fail(connecto_memory::RunError::Failed(format!(
        "API Error: key {KNOWN} / {PATTERN} refused"
    )));
    turn(&w, "p1", ("t1", "s1"), 2, "again", "ok");
    wait_for("the failed run", || {
        runs(&w, "p1", Cycle::Development)
            .iter()
            .any(|r| matches!(r.outcome, RunOutcome::Failed { .. }))
    });
    // A statement from the window.
    w.engine
        .user_statement(
            "p1",
            UserStatement {
                kind: UserStatementKind::Note,
                about: None,
                room: Some(Room::Workshop),
                text: format!("Use {KNOWN} for staging, {PATTERN} for prod."),
                seen: None,
            },
        )
        .unwrap();
    wait_for("the statement", || {
        reader(&w, "p1")
            .read()
            .all_evidence()
            .unwrap()
            .iter()
            .filter(|e| e.kind == EvidenceKind::UserStatement)
            .count()
            >= 3
    });
    // A dream whose model writes the secrets into a memory.
    let user = reader(&w, "p1")
        .read()
        .all_evidence()
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == EvidenceKind::UserStatement)
        .unwrap()
        .id;
    w.host.reply(&format!(
        r#"{{"operations":[{{"op":"create","room":"officina","kind":"procedura","summary":"Deploy with {KNOWN}.","details":"Token {PATTERN}.","sources":["{user}"]}}]}}"#
    ));
    w.engine.dream_now("p1").unwrap();
    wait_for("the dream", || !runs(&w, "p1", Cycle::Dream).is_empty());
    let report = w.engine.dream_report("p1");
    let memory = reader(&w, "p1")
        .read()
        .memories()
        .unwrap()
        .first()
        .unwrap_or_else(|| panic!("{:?} {report:?}", runs(&w, "p1", Cycle::Dream)))
        .id
        .clone();
    // Every surface the host controls.
    let mut surfaces: Vec<(String, String)> = Vec::new();
    for job in w.host.jobs() {
        surfaces.push(("a model prompt".into(), job.prompt));
    }
    let me = caller("t9", "s9");
    for (name, command) in [
        ("brief", MemoryCommand::Brief { budget: None }),
        ("status", MemoryCommand::Status { budget: None }),
        (
            "search",
            MemoryCommand::Search {
                query: "deploy".into(),
                room: None,
                scope: None,
                all: true,
                budget: None,
            },
        ),
        (
            "read",
            MemoryCommand::Read {
                target: Lookup::Memory(memory.clone()),
                budget: None,
            },
        ),
        (
            "room",
            MemoryCommand::Room {
                room: Room::Workshop,
                budget: None,
            },
        ),
    ] {
        surfaces.push((name.into(), w.engine.command("p1", &me, &command, None)));
    }
    for request in [
        connecto_core::memory::ReadRequest::Search {
            query: "deploy".into(),
            room: None,
            scope: None,
            all: true,
        },
        connecto_core::memory::ReadRequest::Room(Room::Workshop),
        connecto_core::memory::ReadRequest::Open(Lookup::Memory(memory)),
    ] {
        let id = w.engine.request_read("p1", request).unwrap();
        wait_for("the read", || {
            view(&w, "p1")
                .and_then(|v| v.read(id).cloned())
                .is_some_and(|r| !matches!(r.state, connecto_core::memory::ReadState::Pending))
        });
    }
    w.engine
        .export("p1", connecto_core::memory::ExportFormat::Both)
        .unwrap();
    wait_for("the export", || {
        view(&w, "p1").is_some_and(|v| v.last_export.is_some())
    });
    let export = view(&w, "p1").unwrap().last_export.unwrap().result.unwrap();
    for path in export {
        if path.is_dir() {
            for entry in std::fs::read_dir(&path).unwrap() {
                let file = entry.unwrap().path();
                surfaces.push((
                    format!("export {}", file.display()),
                    std::fs::read_to_string(&file).unwrap(),
                ));
            }
        } else {
            surfaces.push((
                format!("export {}", path.display()),
                std::fs::read_to_string(&path).unwrap(),
            ));
        }
    }
    surfaces.push(("the UI view".into(), format!("{:?}", w.engine.view())));
    surfaces.push((
        "the dream report".into(),
        format!("{:?}", w.engine.dream_report("p1")),
    ));
    for ext in ["", "-wal"] {
        let bytes = std::fs::read(format!("{}{ext}", db(&w, "p1").display())).unwrap_or_default();
        surfaces.push((
            format!("memory.db{ext}"),
            String::from_utf8_lossy(&bytes).into_owned(),
        ));
    }
    // The secrets did go in: the redaction marker shows where.
    assert!(
        surfaces.iter().any(|(_, text)| text.contains("[REDACTED]")),
        "nothing was redacted: the seeding did not reach the host"
    );
    let leaks: Vec<String> = surfaces
        .iter()
        .filter_map(|(name, text)| has_secret(text).map(|s| format!("{s} in {name}")))
        .collect();
    assert!(leaks.is_empty(), "{leaks:#?}");
}

#[test]
fn no_tests_passed_or_decision_exists_without_a_reply_or_proposal_that_says_it() {
    let w = world::world(&["p1"]);
    enable(&w, "p1", 60);
    w.host.set_git(Some(connecto_memory::GitSnapshot {
        branch: Some("main".into()),
        head: Some("1111111111111111111111111111111111111111".into()),
        changed: Vec::new(),
    }));
    // A turn that changes code and says nothing about tests or decisions.
    turn(
        &w,
        "p1",
        ("t1", "s1"),
        1,
        "refactor the parser",
        "Split the parser.",
    );
    wait_for("the first turn", || evidence_of(&w, "p1", "s1").len() >= 2);
    // Its turn-end snapshot follows the reply on the capture thread.
    std::thread::sleep(Duration::from_millis(300));
    w.host.set_git(Some(connecto_memory::GitSnapshot {
        branch: Some("main".into()),
        head: Some("2222222222222222222222222222222222222222".into()),
        changed: vec![connecto_memory::ChangedFile {
            path: "src/parser.rs".into(),
            blob: Some("3333333333333333333333333333333333333333".into()),
            deleted: false,
        }],
    }));
    w.host
        .set_blob("src/parser.rs", "3333333333333333333333333333333333333333");
    turn(&w, "p1", ("t1", "s1"), 2, "go on", "Moved the lexer out.");
    wait_for("the code observation", || {
        reader(&w, "p1")
            .read()
            .all_evidence()
            .unwrap()
            .iter()
            .any(|e| e.kind == EvidenceKind::CodeObservation)
    });
    let kinds: Vec<EvidenceKind> = reader(&w, "p1")
        .read()
        .all_evidence()
        .unwrap()
        .iter()
        .map(|e| e.kind)
        .collect();
    assert!(
        kinds.contains(&EvidenceKind::CodeObservation),
        "the change was observed: {kinds:?}"
    );
    assert!(
        !kinds.contains(&EvidenceKind::ReportedOutcome),
        "no outcome without a reply that reports one: {kinds:?}"
    );
    // The model claims tests passed and a decision was taken, citing
    // everything there is.
    let all: Vec<String> = reader(&w, "p1")
        .read()
        .all_evidence()
        .unwrap()
        .iter()
        .filter(|e| e.kind != EvidenceKind::UserStatement)
        .map(|e| format!("\"{}\"", e.id))
        .collect();
    let all = all.join(",");
    w.host.reply(&format!(
        r#"{{"annotations":[
            {{"type":"integrazione","memory_kind":"procedura","standing":"eseguita","text":"cargo test passes after the split.","scope":["project"],"sources":[{all}]}},
            {{"type":"integrazione","memory_kind":"decisione","standing":"accettata","text":"The parser is split in two.","scope":["project"],"sources":[{all}]}}]}}"#
    ));
    w.host.advance(Duration::from_secs(61));
    wait_for("the run", || {
        runs(&w, "p1", Cycle::Development)
            .iter()
            .any(|r| matches!(r.outcome, RunOutcome::Annotated { .. }))
    });
    let notes = reader(&w, "p1").read().annotations().unwrap();
    let standing = |t: &str| notes.iter().find(|a| a.text.contains(t)).unwrap().standing;
    assert_eq!(standing("cargo test passes"), Some(Standing::Documented));
    assert_eq!(standing("split in two"), Some(Standing::Proposed));
    // A proposal saying tests passed is a proposal, not a reported outcome.
    w.host.reply(r#"{"annotations":[]}"#);
    propose(
        &w,
        "p1",
        &caller("t2", "s2"),
        Room::Workshop,
        "All tests passed; we decided to keep the split.",
    );
    let proposal = evidence_of(&w, "p1", "s2");
    assert_eq!(proposal.len(), 1);
    assert_eq!(proposal[0].kind, EvidenceKind::AgentProposal);
    // Only a reply that reports a result makes a reported outcome.
    turn(
        &w,
        "p1",
        ("t1", "s1"),
        3,
        "run the tests",
        "cargo test: 41 passed.",
    );
    wait_for("the outcome", || {
        evidence_of(&w, "p1", "s1")
            .iter()
            .any(|e| e.kind == EvidenceKind::ReportedOutcome)
    });
    let outcomes: Vec<_> = reader(&w, "p1")
        .read()
        .all_evidence()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EvidenceKind::ReportedOutcome)
        .collect();
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].excerpt.get().contains("41 passed"));
}

#[test]
fn planning_and_investigation_without_a_diff_are_consulted_noted_and_found_later() {
    let w = world::world(&["p1"]);
    enable(&w, "p1", 60);
    // Something to consult: a note from an earlier session.
    w.host.reply(r#"{"annotations":[]}"#);
    propose(
        &w,
        "p1",
        &caller("t2", "s2"),
        Room::Decisions,
        "Exports must stay outside the repository.",
    );
    wait_for("the proposal's run", || {
        !runs(&w, "p1", Cycle::Development).is_empty()
    });
    let earlier = reader(&w, "p1").read().open_annotations().unwrap()[0]
        .id
        .clone();
    // A planning session consults the memory first, then plans; no diff.
    let brief = w.engine.command(
        "p1",
        &caller("t3", "s3"),
        &MemoryCommand::Brief { budget: None },
        None,
    );
    assert!(brief.contains("outside the repository"), "{brief}");
    turn(
        &w,
        "p1",
        ("t3", "s3"),
        1,
        "Plan the export feature. No code yet.",
        "Plan: JSON first, then Markdown, written outside the repository. I suggest JSON first; your call.",
    );
    // An investigation, no change either.
    turn(
        &w,
        "p1",
        ("t3", "s3"),
        2,
        "Why is startup slow?",
        "It may be the index rebuild at open; not verified.",
    );
    wait_for("the turns", || evidence_of(&w, "p1", "s3").len() >= 4);
    let replies: Vec<_> = evidence_of(&w, "p1", "s3")
        .into_iter()
        .filter(|e| e.kind != EvidenceKind::UserStatement)
        .collect();
    assert!(
        replies[0]
            .exposed_to
            .contains(&Subject::Annotation(earlier)),
        "the planning reply records what the brief showed"
    );
    w.host.reply(&format!(
        r#"{{"annotations":[
            {{"type":"integrazione","memory_kind":"decisione","standing":"accettata","text":"Export JSON first, then Markdown.","scope":["project"],"sources":["{p}"]}},
            {{"type":"integrazione","memory_kind":"lavoro","standing":"implementato","text":"Export feature: JSON then Markdown.","scope":["project"],"sources":["{p}"]}},
            {{"type":"integrazione","memory_kind":"problema","standing":"ipotesi","text":"Slow startup may come from the index rebuild at open.","scope":["project"],"sources":["{i}"]}}]}}"#,
        p = replies[0].id,
        i = replies[1].id
    ));
    w.host.advance(Duration::from_secs(61));
    wait_for("the run", || {
        runs(&w, "p1", Cycle::Development)
            .iter()
            .any(|r| matches!(r.outcome, RunOutcome::Annotated { written: 3, .. }))
    });
    let notes = reader(&w, "p1").read().open_annotations().unwrap();
    let standing = |t: &str| notes.iter().find(|a| a.text.contains(t)).unwrap().standing;
    assert_eq!(standing("JSON first, then"), Some(Standing::Proposed));
    assert_eq!(standing("Export feature"), Some(Standing::Planned));
    assert_eq!(standing("index rebuild"), Some(Standing::Hypothesis));
    // A later agent finds the investigation, as a hypothesis.
    let found = w.engine.command(
        "p1",
        &caller("t4", "s4"),
        &MemoryCommand::Search {
            query: "startup index rebuild".into(),
            room: None,
            scope: None,
            all: false,
            budget: None,
        },
        None,
    );
    assert!(found.contains("index rebuild at open"), "{found}");
    assert!(found.contains("ipotesi"), "{found}");
}

// ---- branches: a rebase rewrites history but the branch is still there

#[test]
fn a_rebased_branch_keeps_its_memory_scope_and_status_and_only_marks_anchors() {
    use connecto_core::memory::Anchor;
    use connecto_memory::capture::Author;
    use connecto_memory::ingest::Ingest;
    use connecto_memory::{
        Captured, CapturedReply, ChangedFile, GitSnapshot, Happened, MemoryOp, NewMemory,
        Publication, WRITER_QUEUE,
    };
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let writer = Writer::spawn(&dir.path().join("memory.db"), PROJECT, WRITER_QUEUE).unwrap();
    let host = Arc::new(FakeHost::new());
    let w = writer.handle();
    let x = w
        .call(|s| -> connecto_memory::Result<MemoryId> {
            let e = evidence_in(s, EvidenceKind::AgentClaim, Some("s0"), T0).id;
            let cutoff = s.read().annotation_seq()?;
            let p = s.publish(Publication {
                project_id: PROJECT.to_owned(),
                cycle: Cycle::Dream,
                base_version: None,
                cutoff,
                ops: vec![MemoryOp::Create(NewMemory {
                    id: None,
                    address: connecto_core::memory::Address::parse("lavori-aperti/parser").unwrap(),
                    kind: MemoryKind::Work,
                    standing: Some(Standing::InProgress),
                    summary: Redacted::assume("The parser rewrite is in progress.".to_owned()),
                    details: Redacted::assume(String::new()),
                    scopes: vec![Scope::Branch("feature-x".to_owned())],
                    status: MemoryStatus::Candidate,
                    anchors: vec![Anchor::new("src/parser.rs", A).unwrap()],
                    sources: vec![e],
                    relations: Vec::new(),
                })],
                absorb: Vec::new(),
                run_id: None,
                now_ms: T0,
            })?;
            Ok(p.created[0].clone())
        })
        .unwrap()
        .unwrap();
    let snap = |head: &str, blob: &str| GitSnapshot {
        branch: Some("feature-x".into()),
        head: Some(head.into()),
        changed: vec![ChangedFile {
            path: "src/parser.rs".into(),
            blob: Some(blob.into()),
            deleted: false,
        }],
    };
    host.set_git(Some(snap("1111111111111111111111111111111111111111", A)));
    host.set_blob("src/parser.rs", A);
    host.set_branches(Some(vec!["main".into(), "feature-x".into()]));
    let mut ingest = Ingest::new(project);
    ingest.baseline(&w, host.as_ref()).unwrap();
    // Rebased onto main: new HEAD, the file's content changed, same branch.
    host.set_git(Some(snap("2222222222222222222222222222222222222222", B)));
    host.set_blob("src/parser.rs", B);
    let c = |what| Captured {
        project_id: PROJECT.to_owned(),
        terminal: "t1".to_owned(),
        provider: connecto_core::memory::Provider::Claude,
        tool_session: Some("s1".to_owned()),
        session_start_ms: 1,
        turn: "#1".to_owned(),
        log: None,
        what,
    };
    ingest
        .ingest(
            &w,
            host.as_ref(),
            c(Happened::Prompt {
                text: "rebase feature-x on main".into(),
                author: Author::User,
                truncated: false,
            }),
        )
        .unwrap();
    let out = ingest
        .ingest(
            &w,
            host.as_ref(),
            c(Happened::TurnEnd {
                reply: CapturedReply::Text("Rebased.".into()),
            }),
        )
        .unwrap();
    assert_eq!(out.queued.len(), 2, "the reply and the code observation");
    let m = w
        .call(move |s| s.read().memory(&x))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(m.scopes.len(), 1);
    assert!(!m.scopes[0].gone, "the branch is still there");
    assert_eq!(m.scopes[0].scope, Scope::Branch("feature-x".into()));
    assert!(m.freshness.anchors_changed, "the anchored file changed");
    assert_eq!(
        (m.room(), m.status, m.standing),
        (
            Room::OpenWork,
            MemoryStatus::Candidate,
            Standing::InProgress
        ),
        "a rebase decides nothing"
    );
}
