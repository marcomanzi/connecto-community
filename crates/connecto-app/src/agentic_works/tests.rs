#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use serde_json::json;
use std::process::Command;
use tokio_postgres::Client;

/// Test points in a job's dispatch: a pause right before its agent starts,
/// and when each job's agent started.
#[derive(Default)]
pub(super) struct Hooks {
    /// Called once, right before the next agent starts.
    pub pause: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Each job's token, and when its agent started.
    pub launches: Mutex<Vec<(String, Instant)>>,
    /// Called once, in the next claim for the queue name given, after it
    /// connected and before it is admitted.
    pub admit: Mutex<Option<db::Pause>>,
    /// Called once, when a queue (named by its identity) is about to have
    /// its change for several jobs at once started.
    pub migration: Mutex<Vec<db::Pause>>,
    /// How many of the next terminal closes fail.
    pub failing_closes: std::sync::atomic::AtomicUsize,
    /// Once, how long the next job's terminal goes unjournalled after its
    /// agent started.
    pub attach_delay: Mutex<Option<Duration>>,
}
impl Hooks {
    pub fn admission(&self, name: &str) {
        let pause = {
            let mut admit = self.admit.lock().unwrap();
            match admit.take() {
                Some((n, pause)) if n == name => Some(pause),
                other => {
                    *admit = other;
                    None
                }
            }
        };
        if let Some(pause) = pause {
            pause();
        }
    }
    pub fn before_migration(&self, queue: &str) {
        let pause = {
            let mut hooks = self.migration.lock().unwrap();
            hooks
                .iter()
                .position(|(q, _)| q == queue)
                .map(|at| hooks.remove(at).1)
        };
        if let Some(pause) = pause {
            pause();
        }
    }
    pub fn close_fails(&self) -> bool {
        self.failing_closes
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
    }
    pub fn before_dispatch(&self) {
        let pause = self.pause.lock().unwrap().take();
        if let Some(pause) = pause {
            pause();
        }
    }
    pub fn launched(&self, token: &str) {
        self.launches
            .lock()
            .unwrap()
            .push((token.to_owned(), Instant::now()));
    }
}

pub(super) struct Postgres(String);
impl Drop for Postgres {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}
impl Postgres {
    pub(super) async fn start() -> (Self, String, Client) {
        let password = uuid::Uuid::new_v4().to_string();
        let out = Command::new("docker")
            .args([
                "run",
                "--rm",
                "-d",
                "-p",
                "127.0.0.1::5432",
                "-e",
                &format!("POSTGRES_PASSWORD={password}"),
                "postgres:16",
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "Docker could not start PostgreSQL");
        let pg = Self(String::from_utf8(out.stdout).unwrap().trim().into());
        let port = Command::new("docker")
            .args(["port", &pg.0, "5432/tcp"])
            .output()
            .unwrap();
        let port = String::from_utf8(port.stdout).unwrap();
        let dsn = format!("postgresql://postgres:{password}@{}/postgres", port.trim());
        for _ in 0..100 {
            if let Ok(db) = db::open(&dsn).await {
                return (pg, dsn, db);
            }
            sleep(Duration::from_millis(150)).await;
        }
        panic!("PostgreSQL startup timed out");
    }
}

#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container"]
async fn postgres_claim_race_acl_routing_expiry_and_stale_outcome() {
    let (_pg, dsn, mut a) = Postgres::start().await;
    let queue = db::setup(&mut a, "p", true).await.unwrap();
    assert_eq!(db::setup(&mut a, "p", false).await.unwrap(), queue);
    assert!(db::setup(&mut a, "other", false).await.is_err());
    let seq: String = a
        .query_one(
            "SELECT pg_get_serial_sequence('connecto_works.connecto_jobs','id')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(seq, "connecto_works.connecto_jobs_id_seq");
    a.batch_execute("GRANT USAGE ON SEQUENCE connecto_works.connecto_jobs_id_seq TO PUBLIC")
        .await
        .unwrap();
    assert!(db::setup(&mut a, "p", false).await.is_err());
    a.batch_execute("REVOKE USAGE ON SEQUENCE connecto_works.connecto_jobs_id_seq FROM PUBLIC")
        .await
        .unwrap();
    a.batch_execute("GRANT INSERT(prompt) ON connecto_works.connecto_jobs TO PUBLIC")
        .await
        .unwrap();
    assert!(db::setup(&mut a, "p", false).await.is_err());
    a.batch_execute(
        "REVOKE INSERT(prompt) ON connecto_works.connecto_jobs FROM PUBLIC;
        INSERT INTO connecto_works.connecto_jobs (project_name,work_type,prompt,created_at) VALUES
        ('p','review','older','2000-01-01'),('p','review','tie','2020-01-01'),
        ('p','review','newest','2020-01-01'),('other','review','foreign',now())",
    )
    .await
    .unwrap();
    let mut b = db::open(&dsn).await.unwrap();
    let (x, y) = tokio::join!(
        db::claim(&mut a, "p", &queue, "p", 1, &|| true),
        db::claim(&mut b, "p", &queue, "p", 1, &|| true)
    );
    let jobs: Vec<_> = [x.unwrap(), y.unwrap()].into_iter().flatten().collect();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(
        (job.id, job.prompt.as_str(), job.cli.as_str()),
        (3, "newest", "agy")
    );
    db::finish(
        &mut a,
        "p",
        &queue,
        job.id,
        "stale",
        "failed",
        None,
        Some("wrong"),
    )
    .await
    .unwrap();
    assert!(
        db::claim(&mut b, "p", &queue, "p", 1, &|| true)
            .await
            .unwrap()
            .is_none()
    );
    a.batch_execute(
        "UPDATE connecto_works.connecto_jobs SET claimed_at=now()-interval '31 minutes' WHERE id=3",
    )
    .await
    .unwrap();
    let next = db::claim(&mut b, "p", &queue, "p", 1, &|| true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.id, 2);
    db::finish(
        &mut a,
        "p",
        &queue,
        job.id,
        &job.token,
        "done_from_agent",
        Some(&json!(123)),
        None,
    )
    .await
    .unwrap();
    db::finish(
        &mut b,
        "p",
        &queue,
        next.id,
        &next.token,
        "done_from_agent",
        Some(&json!({"count":2})),
        None,
    )
    .await
    .unwrap();
    let rows = b
        .query(
            "SELECT status,result FROM connecto_works.connecto_jobs ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>(),
        ["to_do", "done_from_agent", "timeout", "to_do"]
    );
    assert_eq!(rows[1].get::<_, Value>(1), json!({"count":2}));
}
/// Claims see each other whatever isolation the role defaults to: a claim
/// that read before another committed still counts that one's job once it
/// has the lock. A higher cap replaces the index queues were made with by
/// one that allows several jobs doing, and leaves alone an index of that
/// name on another table.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a disposable PostgreSQL 16 Docker container"]
async fn claims_count_each_other_and_only_the_old_index_is_replaced() {
    let (_pg, dsn, mut a) = Postgres::start().await;
    let queue = db::setup(&mut a, "rc", true).await.unwrap();
    let exists = |name: &'static str| {
        let dsn = dsn.clone();
        async move {
            db::open(&dsn)
                .await
                .unwrap()
                .query_one(
                    &format!("SELECT to_regclass('connecto_works.{name}') IS NOT NULL"),
                    &[],
                )
                .await
                .unwrap()
                .get::<_, bool>(0)
        }
    };
    assert!(
        exists("doing_work").await,
        "a new queue finds its jobs doing"
    );
    assert!(!exists("one_active_work").await);
    a.batch_execute(
        "ALTER ROLE postgres SET default_transaction_isolation TO 'repeatable read';
        INSERT INTO connecto_works.connecto_jobs (project_name,work_type,prompt) VALUES
        ('rc','review','one'),('rc','review','two'),('rc','review','three')",
    )
    .await
    .unwrap();
    let (mut b, mut c) = (db::open(&dsn).await.unwrap(), db::open(&dsn).await.unwrap());
    let isolation: String = b
        .query_one("SHOW default_transaction_isolation", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(isolation, "repeatable read");
    // b reads, then waits before the lock while c claims and commits.
    let (reached, at_lock) = std::sync::mpsc::channel();
    let (go, release) = std::sync::mpsc::channel::<()>();
    db::BEFORE_LOCK.lock().unwrap().push((
        queue.clone(),
        Box::new(move || {
            let _ = reached.send(());
            let _ = release.recv_timeout(Duration::from_secs(20));
        }),
    ));
    let q = queue.clone();
    let waiting = tokio::spawn(async move {
        db::claim(&mut b, "rc", &q, "rc", 1, &|| true)
            .await
            .map(|job| job.map(|j| j.id))
    });
    tokio::task::spawn_blocking(move || at_lock.recv_timeout(Duration::from_secs(20)))
        .await
        .unwrap()
        .unwrap();
    let first = db::claim(&mut c, "rc", &queue, "rc", 1, &|| true)
        .await
        .unwrap();
    assert!(first.is_some());
    go.send(()).unwrap();
    let second = waiting.await.unwrap();
    assert_eq!(second, Ok(None), "one job doing already: the cap is 1");
    // A queue made before: its one-job index is replaced once several jobs
    // are allowed, and a second job is taken.
    a.batch_execute(
        "DROP INDEX connecto_works.doing_work;
        CREATE UNIQUE INDEX one_active_work ON connecto_works.connecto_jobs (status) WHERE status = 'doing'",
    )
    .await
    .unwrap();
    assert_eq!(
        db::allow_several(&mut c, "rc", &queue, &|| true).await,
        Ok(true)
    );
    assert!(
        db::claim(&mut c, "rc", &queue, "rc", 2, &|| true)
            .await
            .unwrap()
            .is_some()
    );
    assert!(!exists("one_active_work").await && exists("doing_work").await);
    // An index of that name on another table is not the queue's.
    a.batch_execute(
        "CREATE TABLE connecto_works.notes(x int);
        CREATE INDEX one_active_work ON connecto_works.notes (x)",
    )
    .await
    .unwrap();
    assert_eq!(
        db::allow_several(&mut c, "rc", &queue, &|| true).await,
        Ok(true)
    );
    assert!(
        db::claim(&mut c, "rc", &queue, "rc", 3, &|| true)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        exists("one_active_work").await,
        "the owner's own index stays"
    );
    let doing: i64 = a
        .query_one(
            "SELECT count(*) FROM connecto_works.connecto_jobs WHERE status='doing'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(doing, 3);
}

/// A job that needs setup while that setup runs for another job takes that
/// run's own result, not what the store said before it: success after an
/// earlier failure, failure after an earlier success. A setup with another
/// command (a revision being published) is not its result: the job runs its
/// own afterwards.
#[test]
fn a_job_waiting_for_setup_takes_that_runs_own_result() {
    let (temp, service) = light();
    let s = service.state.clone();
    let root = temp.path().to_path_buf();
    let def = store::Definition {
        instructions: "{{prompt}}".into(),
        schema: "true".into(),
        setup: Some(r#"{"argv": ["tool/setup.sh"]}"#.into()),
        prepare: None,
    };
    s.store.publish("p", "video", &def, None, None).unwrap();
    let cmd = commands::parse(r#"{"argv": ["tool/setup.sh"]}"#).unwrap();
    let revised = commands::parse(r#"{"argv": ["tool/setup-v2.sh"]}"#).unwrap();
    // The job waits on the runtime while the test finishes the setup.
    let wait = |cmd: commands::Command| {
        let (s, root) = (s.clone(), root.clone());
        service.handle.spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(30);
            s.recorded_setup("p", "video", &root, &cmd, Some(deadline), None)
                .await
        })
    };
    let outcome = |job: tokio::task::JoinHandle<Result<()>>| {
        service
            .handle
            .block_on(async { timeout(Duration::from_secs(20), job).await })
            .expect("the job stopped waiting")
            .unwrap()
    };
    let settle = || std::thread::sleep(Duration::from_millis(600));
    // Failed before; the run being waited for succeeds.
    s.store.setup_ok("p", "video", None).unwrap();
    let claim = s.claim_setup("p", "video", &cmd).ok().unwrap();
    let job = wait(cmd.clone());
    settle();
    assert!(!job.is_finished(), "it waits for the running setup");
    claim.finish(&Ok(()));
    drop(claim);
    assert_eq!(outcome(job), Ok(()));
    // Succeeded before; the run being waited for fails.
    s.store
        .setup_ok("p", "video", Some("2026-11-23T10:00:00+01:00"))
        .unwrap();
    let claim = s.claim_setup("p", "video", &cmd).ok().unwrap();
    let job = wait(cmd.clone());
    settle();
    claim.finish(&Err("the download failed".into()));
    drop(claim);
    let failed = outcome(job).unwrap_err();
    assert!(failed.contains("the download failed"), "{failed}");
    // The run's own job stopped it (its database removed, Pause): the
    // claim ends unfinished, and the job runs setup itself instead of
    // taking a failure.
    let claim = s.claim_setup("p", "video", &cmd).ok().unwrap();
    let job = wait(cmd.clone());
    settle();
    drop(claim);
    let own = outcome(job).unwrap_err();
    assert!(own.contains("Connecto Home"), "{own}");
    // Another command's run: once it ends, the job runs its own setup
    // (which here cannot start: no Connecto Home), not taking that Ok.
    let claim = s.claim_setup("p", "video", &revised).ok().unwrap();
    let job = wait(cmd);
    settle();
    claim.finish(&Ok(()));
    drop(claim);
    let own = outcome(job).unwrap_err();
    assert!(own.contains("Connecto Home"), "{own}");
}

/// A queue made before is changed for several jobs while a producer's
/// transaction is still writing to it, longer than any statement of a
/// claim may wait: the new index is built concurrently, waiting for that
/// transaction, and the old one is dropped only once the new one is valid.
/// An index left invalid by an interrupted build is built again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a disposable PostgreSQL 16 Docker container"]
async fn an_old_queue_is_changed_while_a_producer_writes() {
    let (_pg, dsn, mut a) = Postgres::start().await;
    let queue = db::setup(&mut a, "mig", true).await.unwrap();
    a.batch_execute(
        "DROP INDEX connecto_works.doing_work;
        CREATE UNIQUE INDEX one_active_work ON connecto_works.connecto_jobs (status) WHERE status = 'doing';
        INSERT INTO connecto_works.connecto_jobs (project_name,work_type,prompt) VALUES
        ('mig','review','one'),('mig','review','two')",
    )
    .await
    .unwrap();
    assert!(
        db::claim(&mut a, "mig", &queue, "mig", 1, &|| true)
            .await
            .unwrap()
            .is_some()
    );
    // A producer's transaction, open for 8 s.
    let mut producer = db::open(&dsn).await.unwrap();
    let writing = producer.transaction().await.unwrap();
    writing
        .execute(
            "INSERT INTO connecto_works.connecto_jobs (project_name,work_type,prompt) VALUES ('mig','review','three')",
            &[],
        )
        .await
        .unwrap();
    let mut c = db::open(&dsn).await.unwrap();
    let q = queue.clone();
    let changing = tokio::spawn(async move {
        let changed = db::allow_several(&mut c, "mig", &q, &|| true).await;
        (changed, c)
    });
    sleep(Duration::from_secs(8)).await;
    if changing.is_finished() {
        let (changed, _) = changing.await.unwrap();
        panic!("it waits for the producer: {changed:?}");
    }
    writing.commit().await.unwrap();
    let (changed, mut c) = timeout(Duration::from_secs(30), changing)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(changed, Ok(true));
    let index = |name: &'static str| {
        let dsn = dsn.clone();
        async move {
            db::open(&dsn)
                .await
                .unwrap()
                .query_opt(
                    "SELECT indisvalid FROM pg_index WHERE indexrelid=to_regclass('connecto_works.'||$1)",
                    &[&name],
                )
                .await
                .unwrap()
                .map(|r| r.get::<_, bool>(0))
        }
    };
    assert_eq!(index("doing_work").await, Some(true));
    assert_eq!(index("one_active_work").await, None);
    // The statements of the queue are back to their own limit.
    let limit: String = c
        .query_one("SHOW statement_timeout", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(limit, "4s");
    let limit: String = c.query_one("SHOW lock_timeout", &[]).await.unwrap().get(0);
    assert_eq!(limit, "3s");
    assert!(
        db::claim(&mut c, "mig", &queue, "mig", 2, &|| true)
            .await
            .unwrap()
            .is_some()
    );
    // An interrupted build leaves the index invalid: it is built again.
    a.batch_execute(
        "UPDATE pg_index SET indisvalid=false WHERE indexrelid='connecto_works.doing_work'::regclass;
        UPDATE connecto_works.connecto_jobs SET status='done_from_agent' WHERE status='doing';
        CREATE UNIQUE INDEX one_active_work ON connecto_works.connecto_jobs (status) WHERE status = 'doing'",
    )
    .await
    .unwrap();
    assert_eq!(
        db::allow_several(&mut c, "mig", &queue, &|| true).await,
        Ok(true)
    );
    assert_eq!(index("doing_work").await, Some(true));
    assert_eq!(index("one_active_work").await, None);
}

/// Two Connectos (two Macs) sharing a queue made before: while one builds
/// the new index (waiting for a producer), the other leaves the queue alone
/// for that look instead of taking the half-built index for an interrupted
/// one and dropping it. The first one's change completes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a disposable PostgreSQL 16 Docker container"]
async fn two_connectos_change_a_queue_one_at_a_time() {
    let (_pg, dsn, mut a) = Postgres::start().await;
    let queue = db::setup(&mut a, "two", true).await.unwrap();
    a.batch_execute(
        "DROP INDEX connecto_works.doing_work;
        CREATE UNIQUE INDEX one_active_work ON connecto_works.connecto_jobs (status) WHERE status = 'doing'",
    )
    .await
    .unwrap();
    let mut producer = db::open(&dsn).await.unwrap();
    let writing = producer.transaction().await.unwrap();
    writing
        .execute(
            "INSERT INTO connecto_works.connecto_jobs (project_name,work_type,prompt) VALUES ('two','review','one')",
            &[],
        )
        .await
        .unwrap();
    let mut first = db::open(&dsn).await.unwrap();
    let q = queue.clone();
    let building = tokio::spawn(async move {
        let changed = db::allow_several(&mut first, "two", &q, &|| true).await;
        (changed, first)
    });
    // The first one's build has started, and waits for the producer.
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let started: bool = a
            .query_one(
                "SELECT to_regclass('connecto_works.doing_work') IS NOT NULL",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if started {
            break;
        }
        assert!(Instant::now() < until, "the build did not start");
        sleep(Duration::from_millis(50)).await;
    }
    let mut second = db::open(&dsn).await.unwrap();
    let other = timeout(
        Duration::from_secs(3),
        db::allow_several(&mut second, "two", &queue, &|| true),
    )
    .await
    .expect("the second Connecto leaves the queue at once");
    assert_eq!(other, Ok(false));
    writing.commit().await.unwrap();
    let (changed, _first) = timeout(Duration::from_secs(30), building)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(changed, Ok(true));
    let valid: Option<bool> = a
        .query_opt(
            "SELECT indisvalid FROM pg_index WHERE indexrelid=to_regclass('connecto_works.doing_work')",
            &[],
        )
        .await
        .unwrap()
        .map(|r| r.get(0));
    assert_eq!(valid, Some(true));
    // Now the second one finds it done.
    assert_eq!(
        db::allow_several(&mut second, "two", &queue, &|| true).await,
        Ok(true)
    );
}

#[test]
fn journal_survives_restart_without_a_second_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let job = db::Job {
        id: 17,
        token: "attempt".into(),
        kind: "review".into(),
        prompt: "one".into(),
        cli: "pi".into(),
        model: None,
        effort: None,
    };
    let open = || store::Store::open(root.path()).unwrap();
    open()
        .link(
            "project",
            "default",
            &connecto_core::agentic_works::Database::ConnectionString("DB".into()),
            "queue",
        )
        .unwrap();
    assert!(
        open()
            .link(
                "project",
                "default",
                &connecto_core::agentic_works::Database::ConnectionString("NEW".into()),
                "different"
            )
            .is_err()
    );
    open().begin("project", "default", &job, "name").unwrap();
    assert!(
        open()
            .begin("project", "default", &job, "duplicate")
            .is_err()
    );
    let uncertain = open().pending("project", "default").unwrap().remove(0);
    assert_eq!(uncertain.status, "failed");
    open().attach("project", "attempt", "pi-17").unwrap();
    open()
        .record(
            "project",
            "attempt",
            "done_from_agent",
            Some(&json!(false)),
            None,
        )
        .unwrap();
    open().clear("project", "wrong").unwrap();
    let outcome = open().pending("project", "default").unwrap().remove(0);
    assert_eq!(outcome.value, Some(json!(false)));
    assert_eq!(outcome.terminal.as_deref(), Some("pi-17"));
    open().clear("project", &outcome.token).unwrap();
    assert!(open().pending("project", "default").unwrap().is_empty());
}
#[test]
fn publication_and_reply_require_valid_json_and_schema() {
    assert!(definition("hello", "true").is_err());
    assert!(definition("{{prompt}}", r#"{"type":"imaginary"}"#).is_err());
    let validator = definition(
        "Review {{prompt}}",
        r#"{"type":"object","required":["answer"],"properties":{"answer":{"type":"integer"}}}"#,
    )
    .unwrap();
    assert!(validator.is_valid(&parse_json(r#"{"answer":3}"#).unwrap()));
    assert!(!validator.is_valid(&json!({"answer":"three"})));
    assert!(parse_json("prefix {\"answer\":3}").is_err());
    for cli in ["agy", "antigravity", "codex", "claude", "pi", "opencode"] {
        assert!(provider(cli).is_ok());
    }
    assert_eq!(provider("opencode"), Ok(ToolKind::OpenCode));
    assert!(provider("shell").is_err());
}

/// Review finding P3 (ADR 0041): a stand-in that never started (its
/// prepare failed) is only "Selected" in the job's reason; "Ran on" once it
/// started. An agent the job named adds nothing.
#[test]
fn a_failed_jobs_reason_says_how_far_its_stand_in_came() {
    let agent = agents::Agent {
        tool: ToolKind::Claude,
        model: Some("haiku".into()),
        effort: None,
        stand_in: true,
        for_project: false,
    };
    let chosen = |stage| Chosen {
        agent: agent.clone(),
        stage,
        stopped_at_start: false,
    };
    assert_eq!(
        failure_reason(
            "Prepare exited with code 4.".into(),
            Some(&chosen(AgentStage::Selected))
        ),
        "Prepare exited with code 4. (Selected Claude, model haiku, standing in for Antigravity; it had not started.)"
    );
    // Its terminal made, but its checks or login failed before the program
    // ran: dispatched, not ran.
    let stopped = Chosen {
        stopped_at_start: true,
        ..chosen(AgentStage::Dispatched)
    };
    assert_eq!(
        failure_reason("Provider launch failed.".into(), Some(&stopped)),
        "Provider launch failed. (Dispatched to Claude, model haiku, standing in for Antigravity; it had not started.)"
    );
    // Third review, P3: seen only as exited with no turn ended, it may have
    // run between two looks: dispatched, and nothing more claimed.
    assert_eq!(
        failure_reason(
            "Work session ended: Exited".into(),
            Some(&chosen(AgentStage::Dispatched))
        ),
        "Work session ended: Exited (Dispatched to Claude, model haiku, standing in for Antigravity.)"
    );
    assert_eq!(
        failure_reason(
            "Provider returned no final assistant text.".into(),
            Some(&chosen(AgentStage::Running))
        ),
        "Provider returned no final assistant text. (Ran on Claude, model haiku, standing in for Antigravity.)"
    );
    // Fourth review, P3: the database's status says the same as the reason.
    assert_eq!(
        ended_status(7, Some(&chosen(AgentStage::Dispatched))),
        "Job 7 ended (Dispatched to Claude, model haiku, standing in for Antigravity)."
    );
    assert_eq!(
        ended_status(7, Some(&stopped)),
        "Job 7 ended before its agent started (Dispatched to Claude, model haiku, standing in for Antigravity)."
    );
    assert_eq!(
        ended_status(7, Some(&chosen(AgentStage::Selected))),
        "Job 7 ended before its agent started (Selected Claude, model haiku, standing in for Antigravity)."
    );
    assert_eq!(
        ended_status(7, Some(&chosen(AgentStage::Running))),
        "Ran job 7 on Claude, model haiku, standing in for Antigravity."
    );
    assert_eq!(ended_status(7, None), "Ran job 7.");
    let named = Chosen {
        agent: agents::Agent {
            stand_in: false,
            ..agent.clone()
        },
        stage: AgentStage::Running,
        stopped_at_start: false,
    };
    assert_eq!(failure_reason("x".into(), Some(&named)), "x");
    assert_eq!(failure_reason("x".into(), None), "x");
    // The project's agent for jobs: the job named none, so it is said too.
    let project = Chosen {
        agent: agents::Agent {
            tool: ToolKind::Claude,
            model: Some("sonnet".into()),
            effort: None,
            stand_in: false,
            for_project: true,
        },
        stage: AgentStage::Running,
        stopped_at_start: false,
    };
    assert_eq!(
        failure_reason("x".into(), Some(&project)),
        "x (Ran on Claude, model sonnet, the project's agent for jobs.)"
    );
}

/// "Agent for jobs" is kept per project, Automatic until set, and read back
/// after a restart; a CLI this build does not know reads as Automatic.
#[test]
fn the_agent_for_jobs_is_saved_per_project() {
    use connecto_core::agentic_works::JobAgent;
    let dir = tempfile::tempdir().unwrap();
    let store = store::Store::open(dir.path()).unwrap();
    assert_eq!(store.job_agent("p").unwrap(), JobAgent::Automatic);
    store.set_parallel("p", 4).unwrap();
    let sonnet = JobAgent::Cli {
        tool: ToolKind::Claude,
        model: Some("sonnet".into()),
    };
    store.set_job_agent("p", &sonnet).unwrap();
    store
        .set_job_agent(
            "q",
            &JobAgent::Cli {
                tool: ToolKind::Pi,
                model: None,
            },
        )
        .unwrap();
    drop(store);
    let store = store::Store::open(dir.path()).unwrap();
    assert_eq!(store.job_agent("p").unwrap(), sonnet);
    assert_eq!(store.parallel("p").unwrap(), 4, "the other setting is kept");
    assert_eq!(
        store.job_agent("q").unwrap(),
        JobAgent::Cli {
            tool: ToolKind::Pi,
            model: None
        }
    );
    assert_eq!(store.job_agent("other").unwrap(), JobAgent::Automatic);
    store.set_parallel("p", 2).unwrap();
    assert_eq!(
        store.job_agent("p").unwrap(),
        sonnet,
        "kept by a change of parallel"
    );
    store.set_job_agent("p", &JobAgent::Automatic).unwrap();
    assert_eq!(store.job_agent("p").unwrap(), JobAgent::Automatic);
    assert_eq!(store.parallel("p").unwrap(), 2);
}

/// An empty project folder opened in [`light`]'s workspace (no terminals):
/// its id once its relay runs.
#[cfg(unix)]
fn open_light_project(temp: &tempfile::TempDir, service: &Works) -> String {
    let folder = temp.path().canonicalize().unwrap().join("project");
    std::fs::create_dir_all(&folder).unwrap();
    let workspace = &service.state.workspace;
    workspace.open_project(&folder).unwrap();
    let id = connecto_session::launch::project_id(&folder);
    let until = Instant::now() + Duration::from_secs(20);
    while workspace.relay(&id).is_none() {
        assert!(Instant::now() < until, "the project never opened");
        thread::sleep(Duration::from_millis(20));
    }
    id
}

/// A job that names an agent the project's Tools setting leaves out fails
/// with the project's refusal, naming the producer's choice; it never runs on
/// another agent.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_naming_an_agent_the_project_leaves_out_fails_with_its_refusal() {
    let agents = Arc::new(TestAgents::default());
    agents.denied.lock().unwrap().extend([
        ToolKind::Claude,
        ToolKind::Antigravity,
        ToolKind::OpenCode,
    ]);
    let (temp, service) = light_with(Arc::clone(&agents));
    let id = open_light_project(&temp, &service);
    let project = id.as_str();
    let s = Arc::clone(&service.state);
    let job = db::Job {
        id: 4,
        token: "tok-named".into(),
        kind: "review".into(),
        prompt: String::new(),
        cli: "claude".into(),
        model: None,
        effort: None,
    };
    s.running
        .lock()
        .unwrap()
        .entry(project.into())
        .or_default()
        .push(Running {
            db: "dev".into(),
            token: job.token.clone(),
            id: job.id,
            kind: job.kind.clone(),
            since: String::new(),
            deadline: Instant::now() + JOB_LIMIT,
            alone: false,
            agent: None,
            job_agent: JobAgent::Automatic,
        });
    let refused = s
        .choose_agent(project, "dev", &job, Instant::now() + JOB_LIMIT, &|| false)
        .await
        .unwrap_err();
    assert!(
        refused.contains("asks for claude (session_cli)"),
        "{refused}"
    );
    assert!(refused.contains("allows only codex, pi"), "{refused}");
    assert!(
        agents.asked.lock().unwrap().is_empty(),
        "no agent was looked at"
    );
    // One the project allows is chosen as named.
    let codex = db::Job {
        cli: "codex".into(),
        ..job
    };
    let chosen = s
        .choose_agent(project, "dev", &codex, Instant::now() + JOB_LIMIT, &|| {
            false
        })
        .await
        .unwrap();
    assert_eq!(chosen.tool, ToolKind::Codex);
}

/// Review finding 2: a job's agent is chosen with the agent for jobs read
/// at its admission, not one set after it; and a change of the setting
/// waits for an admission under way (it takes the claims' lock).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_keeps_the_agent_for_jobs_it_was_admitted_with() {
    let (temp, service) = light_with(Arc::default());
    let id = open_light_project(&temp, &service);
    let project = id.as_str();
    let s = Arc::clone(&service.state);
    let sonnet = JobAgent::Cli {
        tool: ToolKind::Claude,
        model: Some("sonnet".into()),
    };
    let job = db::Job {
        id: 3,
        token: "tok-admitted".into(),
        kind: "review".into(),
        prompt: String::new(),
        cli: "agy".into(),
        model: None,
        effort: None,
    };
    s.running
        .lock()
        .unwrap()
        .entry(project.into())
        .or_default()
        .push(Running {
            db: "dev".into(),
            token: job.token.clone(),
            id: job.id,
            kind: job.kind.clone(),
            since: String::new(),
            deadline: Instant::now() + JOB_LIMIT,
            alone: false,
            agent: None,
            job_agent: sonnet.clone(),
        });
    // Set back to Automatic after the admission, before the choice.
    s.action(
        project,
        WorksAction::JobAgent {
            agent: JobAgent::Automatic,
        },
    )
    .await
    .unwrap();
    assert_eq!(s.store.job_agent(project).unwrap(), JobAgent::Automatic);
    let chosen = s
        .choose_agent(project, "dev", &job, Instant::now() + JOB_LIMIT, &|| false)
        .await
        .unwrap();
    assert_eq!(
        (chosen.tool, chosen.model.as_deref(), chosen.for_project),
        (ToolKind::Claude, Some("sonnet"), true),
        "{chosen:?}"
    );
    // A change while an admission holds the claims' lock waits for it.
    let admission = s.claims.read().await;
    let change = {
        let s = Arc::clone(&s);
        let project = project.to_owned();
        let sonnet = sonnet.clone();
        tokio::spawn(async move {
            s.action(&project, WorksAction::JobAgent { agent: sonnet })
                .await
        })
    };
    sleep(Duration::from_millis(300)).await;
    assert!(
        !change.is_finished(),
        "the change did not wait for the admission"
    );
    assert_eq!(s.store.job_agent(project).unwrap(), JobAgent::Automatic);
    drop(admission);
    change.await.unwrap().unwrap();
    assert_eq!(s.store.job_agent(project).unwrap(), sonnet);
}

/// Review P3: only models the setting takes are offered (sheet and CLI
/// read the same list), so every one shown can be saved: a Pi model named
/// with its provider over 64 bytes, or one starting with `-`, is left out.
#[cfg(unix)]
#[test]
fn only_models_that_can_be_saved_are_offered() {
    use connecto_core::agentic_works::{WorksBridge, WorksCaller};
    let agents = Arc::new(TestAgents::default());
    let long = format!("openrouter/{}", "x".repeat(60));
    *agents.pi_models.lock().unwrap() = Some(vec![
        "lmstudio/google/gemma-4-e2b".into(),
        long.clone(),
        "-bad".into(),
    ]);
    let (temp, service) = light_with(agents);
    let id = open_light_project(&temp, &service);
    let pi = service
        .view(&id)
        .job_agent_choices
        .into_iter()
        .find(|c| c.tool == ToolKind::Pi)
        .unwrap();
    assert_eq!(pi.models, ["lmstudio/google/gemma-4-e2b"]);
    let caller = WorksCaller {
        terminal: "claude-1".into(),
        project_id: id.clone(),
        launched: Some(connecto_core::AgentMode::Bypass),
        job: false,
        attempt: 1,
    };
    let shown = service
        .command(&caller, "agent", &[], None, Box::new(|_| {}))
        .unwrap();
    assert!(!shown.contains(&long) && !shown.contains("-bad"), "{shown}");
    // What is offered can be saved.
    let args = ["pi", "--model", "lmstudio/google/gemma-4-e2b"].map(String::from);
    service
        .command(&caller, "agent", &args, None, Box::new(|_| {}))
        .unwrap();
}

/// `connecto works agent`: shown alone, set by a Bypass agent to a CLI the
/// project's Tools allow (with a model or its default) or back to auto,
/// refused for a CLI the Tools leave out, a bad model, and an agent not in
/// Bypass.
#[cfg(unix)]
#[test]
fn works_agent_sets_and_shows_the_agent_for_jobs() {
    use connecto_core::AgentMode;
    use connecto_core::agentic_works::{JobAgent, WorksBridge, WorksCaller};
    let agents = Arc::new(TestAgents::default());
    agents.denied.lock().unwrap().push(ToolKind::OpenCode);
    let (temp, service) = light_with(Arc::clone(&agents));
    let id = open_light_project(&temp, &service);
    let project = id.as_str();
    let bypass = WorksCaller {
        terminal: "claude-1".into(),
        project_id: project.into(),
        launched: Some(AgentMode::Bypass),
        job: false,
        attempt: 1,
    };
    let run = |who: &WorksCaller, args: &[&str]| {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        service.command(who, "agent", &args, None, Box::new(|_| {}))
    };
    let shown = run(&bypass, &[]).unwrap();
    assert!(shown.contains("Agent for jobs: Automatic"), "{shown}");
    assert!(
        shown.contains("  claude  its default model, opus, sonnet"),
        "{shown}"
    );
    assert!(!shown.contains("opencode"), "{shown}");
    let set = run(&bypass, &["claude", "--model", "sonnet"]).unwrap();
    assert!(set.contains("run on Claude, model sonnet"), "{set}");
    let sonnet = JobAgent::Cli {
        tool: ToolKind::Claude,
        model: Some("sonnet".into()),
    };
    assert_eq!(service.view(project).job_agent, sonnet);
    assert!(
        run(&bypass, &[])
            .unwrap()
            .contains("Agent for jobs: Claude, model sonnet.")
    );
    // Refused, and nothing changed.
    let left_out = run(&bypass, &["opencode"]).unwrap_err();
    assert!(
        left_out.contains("not one of this project's Tools"),
        "{left_out}"
    );
    assert!(run(&bypass, &["claude", "--model", "-x"]).is_err());
    // Review finding 1: a model of another CLI, or not in this one's list.
    let foreign = run(&bypass, &["claude", "--model", "gemini-4-argon-high"]).unwrap_err();
    assert!(
        foreign.contains("not one of Claude's models") && foreign.contains("sonnet"),
        "{foreign}"
    );
    assert!(
        run(&bypass, &["pi", "--model", "lmstudio/x"]).is_err(),
        "Pi lists none here"
    );
    assert!(run(&bypass, &["shell"]).is_err());
    assert!(run(&bypass, &["auto", "--model", "x"]).is_err());
    let asking = WorksCaller {
        launched: Some(AgentMode::Prompt),
        ..bypass.clone()
    };
    let refused = run(&asking, &["codex"]).unwrap_err();
    assert!(refused.starts_with("Not changed."), "{refused}");
    assert_eq!(service.view(project).job_agent, sonnet);
    // Pi lists no model here: its default model is still a choice.
    run(&bypass, &["pi"]).unwrap();
    assert_eq!(
        service.view(project).job_agent,
        JobAgent::Cli {
            tool: ToolKind::Pi,
            model: None
        }
    );
    // `agy` names Antigravity; no model is its default one.
    run(&bypass, &["agy"]).unwrap();
    assert_eq!(
        service.view(project).job_agent,
        JobAgent::Cli {
            tool: ToolKind::Antigravity,
            model: None
        }
    );
    let auto = run(&bypass, &["auto"]).unwrap();
    assert!(auto.contains("Automatic"), "{auto}");
    assert_eq!(service.view(project).job_agent, JobAgent::Automatic);
}

/// The program runs once its terminal is past its start: not while it is
/// starting (its checks, its login), failed or never started.
#[test]
fn a_jobs_agent_runs_only_past_its_terminals_start() {
    use TermActivity::*;
    for state in [NotStarted, Starting, NeedsLogin, Failed, Exited] {
        assert!(!program_runs(state, 0), "{state:?}");
    }
    for state in [Working, NeedsUser, Idle, Interrupted, Done] {
        assert!(program_runs(state, 0), "{state:?}");
    }
    assert!(program_runs(Exited, 1));
}

#[cfg(unix)]
/// Agentic Works on an empty workspace with an in-memory vault: no
/// database, no terminal. For what is decided before connecting.
pub(super) fn light() -> (tempfile::TempDir, Arc<Works>) {
    light_with(Arc::default())
}

#[cfg(unix)]
/// [`light`], its jobs' agents looked at in `agents`.
pub(super) fn light_with(agents: Arc<TestAgents>) -> (tempfile::TempDir, Arc<Works>) {
    let temp = tempfile::Builder::new()
        .prefix("awl")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap();
    let layout = connecto_session::launch::Layout {
        home: root.join("home"),
        data_root: root.join("data"),
    };
    std::fs::create_dir_all(&layout.home).unwrap();
    std::fs::create_dir_all(&layout.data_root).unwrap();
    let workspace = Arc::new(
        Workspace::start(connecto_workspace::WorkspaceConfig {
            layout: layout.clone(),
            extras: connecto_session::launch::LaunchExtras::default(),
            audit: connecto_audit::Audit::open(&root.join("audit.jsonl")).unwrap(),
            wake: Arc::new(|| {}),
            default_tools: vec![],
            after_update: false,
        })
        .unwrap(),
    );
    let secrets =
        connecto_secrets::Secrets::new(Arc::new(connecto_secrets::MemoryItems::default()), None);
    let vault = Arc::new(connecto_vault::Vault::open(
        Arc::new(secrets),
        Arc::new(connecto_vault::FixedKey([7; 32])),
        None,
    ));
    let audit = connecto_audit::Audit::open(&root.join("works-audit.jsonl")).unwrap();
    let service = Works::start(&layout.data_root, workspace, vault, audit, agents).unwrap();
    (temp, service)
}

/// The project's view read over and over while its running job asks over
/// and over (ADR 0040): the two take the questions' and the running jobs'
/// locks, and never wait for each other for ever.
#[cfg(unix)]
#[test]
fn reading_the_view_while_a_job_asks_never_deadlocks() {
    let (_temp, service) = light();
    let project = "00000000000000ab";
    let job = db::Job {
        id: 5,
        token: "tok-view".into(),
        kind: "review".into(),
        prompt: String::new(),
        cli: "claude".into(),
        model: None,
        effort: None,
    };
    let s = &service.state;
    s.store
        .begin(project, "dev", &job, "agentic_works_5")
        .unwrap();
    s.store.attach(project, &job.token, "claude-4").unwrap();
    s.running
        .lock()
        .unwrap()
        .entry(project.into())
        .or_default()
        .push(Running {
            db: "dev".into(),
            token: job.token.clone(),
            id: job.id,
            kind: job.kind.clone(),
            since: String::new(),
            deadline: Instant::now() + JOB_LIMIT,
            alone: false,
            agent: None,
            job_agent: JobAgent::Automatic,
        });
    let caller = connecto_core::agentic_works::WorksCaller {
        terminal: "claude-4".into(),
        project_id: project.into(),
        launched: None,
        job: true,
        attempt: 1,
    };
    let (done, finished) = std::sync::mpsc::channel();
    let until = Instant::now() + Duration::from_secs(3);
    let viewer = {
        let (service, done) = (service.clone(), done.clone());
        thread::spawn(move || {
            while Instant::now() < until {
                let _ = service.view(project);
            }
            let _ = done.send("view");
        })
    };
    let asker = {
        let service = service.clone();
        thread::spawn(move || {
            let mut asked = 0;
            while Instant::now() < until {
                if service
                    .state
                    .ask(&caller, "Which season?", Box::new(|_| {}))
                    .is_ok()
                {
                    asked += 1;
                }
            }
            let _ = done.send("ask");
            asked
        })
    };
    for _ in 0..2 {
        finished
            .recv_timeout(Duration::from_secs(30))
            .expect("reading the view and asking deadlocked");
    }
    viewer.join().unwrap();
    assert_eq!(asker.join().unwrap(), 1, "one question at a time");
    assert_eq!(service.view(project).questions.len(), 1);
}

/// A real project in a PTY workspace, its claude the fixture CLI, a
/// disposable PostgreSQL behind the project secret QUEUE (agentic_works), and
/// Agentic Works running.
pub(super) struct Rig {
    pub _pg: Postgres,
    pub dsn: String,
    pub db: Client,
    pub _temp: tempfile::TempDir,
    pub root: PathBuf,
    pub folder: PathBuf,
    pub project: String,
    pub layout: connecto_session::launch::Layout,
    pub workspace: Arc<Workspace>,
    pub audit: connecto_audit::Audit,
    pub service: Arc<Works>,
    /// What its jobs' agents are chosen from: every agent usable unless a
    /// test says otherwise.
    pub agents: Arc<TestAgents>,
}

/// Agents for tests: each agent's availability as set (usable when not
/// set), the agents asked about in order, and agents whose look is held open
/// until released (a slow sign-in check, Pi's keys slow to read).
#[derive(Default)]
pub(super) struct TestAgents {
    pub set: Mutex<Vec<(ToolKind, agents::Availability)>>,
    pub asked: Mutex<Vec<ToolKind>>,
    pub held: Mutex<Vec<ToolKind>>,
    /// The agents the project's Tools leave out.
    pub denied: Mutex<Vec<ToolKind>>,
    /// Pi's models, when set.
    pub pi_models: Mutex<Option<Vec<String>>>,
}

impl TestAgents {
    /// Usable: exactly `tools`; the others not installed.
    pub fn only(&self, tools: &[ToolKind]) {
        *self.set.lock().unwrap() = [
            ToolKind::Antigravity,
            ToolKind::Codex,
            ToolKind::Claude,
            ToolKind::Pi,
            ToolKind::OpenCode,
        ]
        .into_iter()
        .map(|t| {
            (
                t,
                if tools.contains(&t) {
                    agents::Availability::Usable
                } else {
                    agents::Availability::Unusable("not installed".into())
                },
            )
        })
        .collect();
    }

    /// Set what is said of `tool`.
    pub fn set(&self, tool: ToolKind, availability: agents::Availability) {
        let mut set = self.set.lock().unwrap();
        set.retain(|(t, _)| *t != tool);
        set.push((tool, availability));
    }
}

impl agents::AgentCheck for TestAgents {
    fn settings(&self, _: &str, _: ToolKind) -> agents::Availability {
        agents::Availability::Usable
    }

    fn allows(&self, _: &str, tool: ToolKind) -> bool {
        !self.denied.lock().unwrap().contains(&tool)
    }

    fn models(&self, _: &str, tool: ToolKind) -> Option<Vec<String>> {
        (tool == ToolKind::Pi)
            .then(|| self.pi_models.lock().unwrap().clone())
            .flatten()
    }

    fn look(&self, _: &str, _: &Path, tool: ToolKind, _: &agents::Profile) -> agents::Availability {
        self.asked.lock().unwrap().push(tool);
        while self.held.lock().unwrap().contains(&tool) {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.set
            .lock()
            .unwrap()
            .iter()
            .find(|(t, _)| *t == tool)
            .map_or(agents::Availability::Usable, |(_, a)| a.clone())
    }
}

pub(super) async fn rig() -> Rig {
    use connecto_core::{Binding, ConfigMode, ProjectSettings, ProviderScope, VaultControl};
    use connecto_session::launch::{LaunchExtras, Layout, project_id};
    use connecto_store::{Signature, Store, Tool};
    use std::{fs, os::unix::fs::PermissionsExt};
    struct Fixture;
    impl Signature for Fixture {
        fn verify(&self, _: &Path, _: Option<&str>) -> Result<()> {
            Ok(())
        }
    }
    let (pg, dsn, db) = Postgres::start().await;
    let temp = tempfile::Builder::new()
        .prefix("aw")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap();
    let home = root.join("home");
    let folder = home.join("project");
    fs::create_dir_all(&folder).unwrap();
    let project = project_id(&folder);
    let layout = Layout {
        home,
        data_root: root.join("data"),
    };
    fs::create_dir_all(&layout.data_root).unwrap();
    let entry = root.join("2.1.318");
    fs::write(&entry, include_str!("../../tests/fixtures/works_claude.py")).unwrap();
    fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
    let tools = Store::new(&layout.data_root);
    tools.provision(Tool::Claude, &entry, &Fixture).unwrap();
    let build = Command::new("cargo")
        .args(["build", "-q", "-p", "connecto-hook"])
        .status()
        .unwrap();
    assert!(build.success());
    let exe = std::env::current_exe().unwrap();
    let hook = exe
        .ancestors()
        .map(|dir| dir.join("connecto-hook"))
        .find(|p| p.is_file())
        .unwrap();
    tools.refresh_hook(&hook).unwrap();
    connecto_session::project_settings::save(
        &layout.data_root,
        &project,
        &ProjectSettings {
            claude_config: ConfigMode::Isolated,
            ..Default::default()
        },
    )
    .unwrap();
    let workspace = Arc::new(
        Workspace::start(connecto_workspace::WorkspaceConfig {
            layout: layout.clone(),
            extras: LaunchExtras::default(),
            audit: connecto_audit::Audit::open(&root.join("audit.jsonl")).unwrap(),
            wake: Arc::new(|| {}),
            default_tools: vec![],
            after_update: false,
        })
        .unwrap(),
    );
    workspace.open_project(&folder).unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    while workspace.relay(&project).is_none() {
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(20)).await;
    }
    let secrets =
        connecto_secrets::Secrets::new(Arc::new(connecto_secrets::MemoryItems::default()), None);
    let vault = Arc::new(connecto_vault::Vault::open(
        Arc::new(secrets),
        Arc::new(connecto_vault::FixedKey([7; 32])),
        None,
    ));
    let scope = ProviderScope::Project(project.clone());
    vault.put(&scope, "QUEUE", "test", &dsn).unwrap();
    vault
        .set_bindings(&scope, "QUEUE", vec![Binding::AgenticWorks])
        .unwrap();
    let audit = connecto_audit::Audit::open(&root.join("works-audit.jsonl")).unwrap();
    let agents = Arc::new(TestAgents::default());
    let service = Works::start(
        &layout.data_root,
        workspace.clone(),
        vault,
        audit.clone(),
        agents.clone(),
    )
    .unwrap();
    // Its tests change what is usable between jobs: nothing is kept.
    service.state.agents.keep_for(Duration::ZERO);
    Rig {
        _pg: pg,
        dsn,
        db,
        _temp: temp,
        root,
        folder,
        project,
        layout,
        workspace,
        audit,
        service,
        agents,
    }
}

/// One project, three databases (dev, staging, and one that stops
/// answering): jobs from dev and staging both run, each outcome goes back to
/// the database its job came from (both are job 1), a type published for
/// dev only fails in staging, and the broken database is marked failing
/// without stopping the others. It can then be removed.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn every_linked_database_is_served_and_one_failing_stops_none() {
    use connecto_core::{
        AgentMode, Binding, ProviderScope, VaultControl,
        agentic_works::{Database, WorksBridge},
    };
    let Rig {
        _pg,
        dsn,
        db: dev,
        _temp,
        project,
        service,
        workspace,
        folder,
        ..
    } = rig().await;
    for name in ["staging", "broken"] {
        dev.batch_execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
    }
    let (server, _) = dsn.rsplit_once('/').unwrap();
    let vault = service.state.vault.clone();
    let scope = ProviderScope::Project(project.clone());
    for (secret, value) in [
        ("STAGING", format!("{server}/staging")),
        ("BROKEN", format!("{server}/broken")),
    ] {
        vault.put(&scope, secret, "test", &value).unwrap();
        vault
            .set_bindings(&scope, secret, vec![Binding::AgenticWorks])
            .unwrap();
    }
    let s = &service.state;
    for (name, secret) in [
        ("dev", "QUEUE"),
        ("staging", "STAGING"),
        ("broken", "BROKEN"),
    ] {
        s.action(
            &project,
            WorksAction::Link {
                name: name.into(),
                database: Database::ConnectionString(secret.into()),
                create: true,
                password: None,
            },
        )
        .await
        .unwrap();
    }
    // A queue is linked under one name only.
    let twice = s
        .action(
            &project,
            WorksAction::Link {
                name: "again".into(),
                database: Database::ConnectionString("STAGING".into()),
                create: false,
                password: None,
            },
        )
        .await
        .unwrap_err();
    assert!(twice.contains("already linked"), "{twice}");
    let publish = |kind: &str, databases: Option<Vec<String>>| WorksAction::Publish {
        kind: kind.into(),
        instructions: "Review {{prompt}}".into(),
        schema: r#"{"type":"object","required":["answer"]}"#.into(),
        setup: None,
        prepare: None,
        databases,
    };
    s.action(&project, publish("review", None)).await.unwrap();
    s.action(&project, publish("dev-only", Some(vec!["dev".into()])))
        .await
        .unwrap();
    // A revision authored in the sheet keeps the type's databases: the
    // imported draft carries them, and the sheet publishes with them.
    s.action(
        &project,
        WorksAction::Author {
            kind: "dev-only".into(),
            cli: ToolKind::Claude,
        },
    )
    .await
    .unwrap();
    let until = Instant::now() + Duration::from_secs(45);
    let draft = loop {
        if let Ok(draft) = service.draft(&project, "dev-only") {
            break draft;
        }
        assert!(Instant::now() < until, "{}", service.view(&project).notice);
        sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(draft.databases, Some(vec!["dev".to_owned()]));
    let unknown = s
        .action(&project, publish("x", Some(vec!["qa".into()])))
        .await
        .unwrap_err();
    assert!(unknown.contains("no database named qa"), "{unknown}");
    // broken stops answering.
    vault
        .put(
            &scope,
            "BROKEN",
            "test",
            "postgresql://postgres:nothing@127.0.0.1:1/postgres",
        )
        .unwrap();
    let staging = db::open(&format!("{server}/staging")).await.unwrap();
    let insert = "INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt,session_cli) VALUES('project',$1,'README','claude')";
    dev.execute(insert, &[&"review"]).await.unwrap();
    staging.execute(insert, &[&"review"]).await.unwrap();
    staging.execute(insert, &[&"dev-only"]).await.unwrap();
    type Row = (i64, String, String, Option<Value>, Option<String>);
    async fn rows(client: &Client) -> Vec<Row> {
        client
            .query(
                "SELECT id,work_type,status,result,reason FROM connecto_works.connecto_jobs ORDER BY id",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
            .collect()
    }
    let until = Instant::now() + Duration::from_secs(120);
    loop {
        let (d, st) = (rows(&dev).await, rows(&staging).await);
        let finished = |list: &[Row]| list.iter().all(|r| r.2 != "to_do" && r.2 != "doing");
        if finished(&d) && finished(&st) {
            assert_eq!(d.len(), 1, "{d:?}");
            assert_eq!(
                (d[0].0, d[0].2.as_str(), d[0].3.clone()),
                (1, "done_from_agent", Some(json!({"answer": 42})))
            );
            assert_eq!(st.len(), 2, "{st:?}");
            assert_eq!(
                (st[0].0, st[0].2.as_str(), st[0].3.clone()),
                (1, "done_from_agent", Some(json!({"answer": 42})))
            );
            assert_eq!(st[1].2, "failed", "{st:?}");
            assert!(
                st[1]
                    .4
                    .as_deref()
                    .is_some_and(|r| r.contains("not published for the database staging")),
                "{st:?}"
            );
            break;
        }
        assert!(
            Instant::now() < until,
            "dev {d:?} staging {st:?} notice {}",
            service.view(&project).notice
        );
        sleep(Duration::from_millis(200)).await;
    }
    // Each database's status, as the sheet and `works databases` show it.
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let view = service.view(&project);
        let ok = |name: &str| view.database(name).unwrap().status.ok;
        if ok("broken") == Some(false) && ok("dev") == Some(true) && ok("staging") == Some(true) {
            break;
        }
        assert!(
            Instant::now() < until,
            "{:?}",
            view.databases
                .iter()
                .map(|d| (&d.name, &d.status))
                .collect::<Vec<_>>()
        );
        sleep(Duration::from_millis(200)).await;
    }
    let caller = connecto_core::agentic_works::WorksCaller {
        terminal: "claude-1".into(),
        project_id: project.clone(),
        launched: Some(AgentMode::Bypass),
        job: false,
        attempt: 1,
    };
    let (svc, who) = (service.clone(), caller.clone());
    let listed = tokio::task::spawn_blocking(move || {
        svc.command(&who, "databases", &[], None, Box::new(|_| {}))
    })
    .await
    .unwrap()
    .unwrap();
    assert!(
        listed.contains("broken") && listed.contains("FAILING") && listed.contains("staging"),
        "{listed}"
    );
    // The job of the one database is read from it, named.
    let (svc, who) = (service.clone(), caller.clone());
    let job = tokio::task::spawn_blocking(move || {
        let args = ["2", "--db", "staging"].map(String::from);
        let named = svc.command(&who, "job", &args, None, Box::new(|_| {}));
        let unnamed = svc.command(&who, "job", &["2".to_owned()], None, Box::new(|_| {}));
        (named, unnamed)
    })
    .await
    .unwrap();
    assert!(job.0.as_ref().unwrap().contains("\"failed\""), "{job:?}");
    assert!(job.1.unwrap_err().contains("--db"));
    // Removing the broken database leaves the others serving.
    let (svc, who) = (service.clone(), caller);
    let removed = tokio::task::spawn_blocking(move || {
        let args = ["--db", "broken"].map(String::from);
        svc.command(&who, "unlink", &args, None, Box::new(|_| {}))
    })
    .await
    .unwrap()
    .unwrap();
    assert!(removed.contains("Removed the database broken"), "{removed}");
    assert_eq!(
        service
            .view(&project)
            .databases
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["dev", "staging"]
    );
    dev.execute(insert, &[&"review"]).await.unwrap();
    let until = Instant::now() + Duration::from_secs(60);
    while rows(&dev).await[1].2 != "done_from_agent" {
        assert!(Instant::now() < until, "{:?}", rows(&dev).await);
        sleep(Duration::from_millis(200)).await;
    }

    // A database removed after a cycle listed the project's databases gives
    // that cycle nothing.
    let listed = s
        .store
        .links(&project)
        .unwrap()
        .into_iter()
        .find(|l| l.name == "staging")
        .unwrap();
    s.action(
        &project,
        WorksAction::Unlink {
            name: "staging".into(),
            force: false,
        },
    )
    .await
    .unwrap();
    staging.execute(insert, &[&"review"]).await.unwrap();
    let epoch = workspace.pause_switch().epoch();
    let served = s
        .take(&project, "project", &listed, epoch, &mut HashMap::new())
        .await
        .unwrap();
    assert!(matches!(served, Taken::Gone));
    assert_eq!(rows(&staging).await[2].2, "to_do");

    // Removing a database by force while its job runs: the job's agent is
    // stopped, its terminal closed, its outcome not written back.
    dev.execute(
        "INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt,session_cli) VALUES('project','review','wait-for-cancel','claude')",
        &[],
    )
    .await
    .unwrap();
    let until = Instant::now() + Duration::from_secs(60);
    let terminal = loop {
        if let Some(t) = s
            .store
            .pending(&project, "dev")
            .unwrap()
            .into_iter()
            .find_map(|p| p.terminal)
        {
            break t;
        }
        assert!(Instant::now() < until, "{:?}", rows(&dev).await);
        sleep(Duration::from_millis(100)).await;
    };
    let unlink = |force| WorksAction::Unlink {
        name: "dev".into(),
        force,
    };
    let refused = s.action(&project, unlink(false)).await.unwrap_err();
    assert!(refused.contains("is running"), "{refused}");
    s.action(&project, unlink(true)).await.unwrap();
    let open = || {
        workspace
            .relay(&project)
            .unwrap()
            .terminals()
            .view()
            .terminals
            .iter()
            .any(|t| t.id == terminal)
    };
    let until = Instant::now() + Duration::from_secs(30);
    while open() || !s.store.pendings(&project).unwrap().is_empty() {
        assert!(
            Instant::now() < until,
            "terminal open: {}, journal: {}",
            open(),
            s.store.pendings(&project).unwrap().len()
        );
        sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(rows(&dev).await[2].2, "doing");

    // Removed by force while its prepare command runs: the command is
    // stopped and no agent starts.
    dev.batch_execute(
        "UPDATE connecto_works.connecto_jobs SET status='failed' WHERE status='doing'",
    )
    .await
    .unwrap();
    s.action(
        &project,
        WorksAction::Link {
            name: "dev".into(),
            database: Database::ConnectionString("QUEUE".into()),
            create: false,
            password: None,
        },
    )
    .await
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let tool = folder.join("tool");
        std::fs::create_dir_all(&tool).unwrap();
        let slow = tool.join("slow.sh");
        std::fs::write(
            &slow,
            "#!/bin/sh
echo started > slow.started
sleep 20
echo finished > slow.finished
echo '{}'
",
        )
        .unwrap();
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    s.action(
        &project,
        WorksAction::Publish {
            kind: "slow".into(),
            instructions: "Review {{prompt}} with {{prepared}}".into(),
            schema: r#"{"type":"object"}"#.into(),
            setup: None,
            prepare: Some(r#"{"argv": ["tool/slow.sh"], "timeout_s": 60}"#.into()),
            databases: None,
        },
    )
    .await
    .unwrap();
    let agents = || {
        workspace
            .relay(&project)
            .unwrap()
            .terminals()
            .view()
            .terminals
            .iter()
            .filter(|t| {
                t.name
                    .as_deref()
                    .is_some_and(|n| n.starts_with("agentic_works_"))
            })
            .count()
    };
    dev.execute(insert, &[&"slow"]).await.unwrap();
    let until = Instant::now() + Duration::from_secs(60);
    while !folder.join("slow.started").exists() {
        assert!(Instant::now() < until, "{:?}", rows(&dev).await);
        sleep(Duration::from_millis(100)).await;
    }
    s.action(&project, unlink(true)).await.unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    while !s.store.pendings(&project).unwrap().is_empty() {
        assert!(Instant::now() < until, "the job still runs");
        sleep(Duration::from_millis(100)).await;
    }
    sleep(Duration::from_secs(1)).await;
    assert_eq!(agents(), 0);
    assert_eq!(rows(&dev).await[3].2, "doing");
    // The command was killed, not left to finish.
    sleep(Duration::from_secs(20)).await;
    assert!(!folder.join("slow.finished").exists());
    assert_eq!(agents(), 0);

    // Removed by force at the last moment before its agent starts: the
    // removal returns only once the agent has started and been journalled
    // (it then stops it), never before an agent starts.
    dev.batch_execute(
        "UPDATE connecto_works.connecto_jobs SET status='failed' WHERE status='doing'",
    )
    .await
    .unwrap();
    s.action(
        &project,
        WorksAction::Link {
            name: "dev".into(),
            database: Database::ConnectionString("QUEUE".into()),
            create: false,
            password: None,
        },
    )
    .await
    .unwrap();
    let (at_boundary, reached) = std::sync::mpsc::channel::<()>();
    let (removed, release) = std::sync::mpsc::channel::<()>();
    *s.hooks.pause.lock().unwrap() = Some(Box::new(move || {
        let _ = at_boundary.send(());
        // Long enough for an unguarded removal to finish meanwhile.
        let _ = release.recv_timeout(Duration::from_secs(3));
    }));
    dev.execute(insert, &[&"review"]).await.unwrap();
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(60)))
        .await
        .unwrap()
        .expect("the job reached its dispatch");
    let token = s.store.pending(&project, "dev").unwrap().remove(0).token;
    s.action(&project, unlink(true)).await.unwrap();
    let acknowledged = Instant::now();
    let _ = removed.send(());
    let until = Instant::now() + Duration::from_secs(30);
    while !s.store.pendings(&project).unwrap().is_empty() || agents() > 0 {
        assert!(Instant::now() < until, "the job still runs");
        sleep(Duration::from_millis(100)).await;
    }
    let launches = s.hooks.launches.lock().unwrap().clone();
    let launched: Vec<Instant> = launches
        .iter()
        .filter(|(t, _)| *t == token)
        .map(|(_, at)| *at)
        .collect();
    assert!(
        launched.iter().all(|at| *at < acknowledged),
        "an agent started after the removal returned"
    );
    assert_eq!(rows(&dev).await[4].2, "doing");
}

/// The project's job terminals open now.
fn job_terminals(workspace: &Workspace, project: &str) -> Vec<String> {
    workspace
        .relay(project)
        .unwrap()
        .terminals()
        .view()
        .terminals
        .iter()
        .filter_map(|t| t.name.clone())
        .filter(|n| n.starts_with("agentic_works_"))
        .collect()
}

/// Queue a job in `client`'s queue; its id.
async fn add_job(client: &Client, prompt: &str) -> i64 {
    client
        .query_one(
            "INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt,session_cli) VALUES('project','review',$1,'claude') RETURNING id",
            &[&prompt],
        )
        .await
        .unwrap()
        .get(0)
}

/// A job's status, result and reason.
async fn job_row(client: &Client, id: i64) -> (String, Option<Value>, Option<String>) {
    let r = client
        .query_one(
            "SELECT status,result,reason FROM connecto_works.connecto_jobs WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap();
    (r.get(0), r.get(1), r.get(2))
}

/// Link the rig's database as dev and a second one as staging, publish
/// `review`, and run up to `jobs` at once. The staging client.
async fn two_databases(rig: &Rig, jobs: u32) -> Client {
    use connecto_core::{
        Binding, ProviderScope, VaultControl,
        agentic_works::{Database, WorksAction},
    };
    rig.db
        .batch_execute("CREATE DATABASE staging")
        .await
        .unwrap();
    let (server, _) = rig.dsn.rsplit_once('/').unwrap();
    let scope = ProviderScope::Project(rig.project.clone());
    let s = &rig.service.state;
    s.vault
        .put(&scope, "STAGING", "test", &format!("{server}/staging"))
        .unwrap();
    s.vault
        .set_bindings(&scope, "STAGING", vec![Binding::AgenticWorks])
        .unwrap();
    for (name, secret) in [("dev", "QUEUE"), ("staging", "STAGING")] {
        s.action(
            &rig.project,
            WorksAction::Link {
                name: name.into(),
                database: Database::ConnectionString(secret.into()),
                create: true,
                password: None,
            },
        )
        .await
        .unwrap();
    }
    s.action(
        &rig.project,
        WorksAction::Publish {
            kind: "review".into(),
            instructions: "Review {{prompt}}".into(),
            schema: r#"{"type":"object","required":["answer"]}"#.into(),
            setup: None,
            prepare: None,
            databases: None,
        },
    )
    .await
    .unwrap();
    s.action(&rig.project, WorksAction::Parallel { jobs })
        .await
        .unwrap();
    db::open(&format!("{server}/staging")).await.unwrap()
}

/// Wait until `done` holds, up to `secs`.
async fn until(secs: u64, what: &str, mut done: impl AsyncFnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !done().await {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(100)).await;
    }
}

/// A claim that connected while the project had two places, one taken, is
/// admitted only after the number is lowered to one and acknowledged: it
/// takes nothing.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_lower_number_of_jobs_holds_for_a_claim_under_way() {
    let rig = rig().await;
    let _staging = two_databases(&rig, 2).await;
    let (s, project) = (&rig.service.state, &rig.project);
    // Jobs of a queue name only this claim looks for.
    rig.db
        .batch_execute(
            "INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt) VALUES ('held','review','one'),('held','review','two')",
        )
        .await
        .unwrap();
    s.running
        .lock()
        .unwrap()
        .entry(project.clone())
        .or_default()
        .push(Running {
            db: "dev".into(),
            token: "taken-elsewhere".into(),
            id: 0,
            kind: "review".into(),
            since: String::new(),
            deadline: Instant::now() + JOB_LIMIT,
            alone: false,
            agent: None,
            job_agent: JobAgent::Automatic,
        });
    let (reached, at_admission) = std::sync::mpsc::channel();
    let (go, release) = std::sync::mpsc::channel::<()>();
    *s.hooks.admit.lock().unwrap() = Some((
        "held".into(),
        Box::new(move || {
            let _ = reached.send(());
            let _ = release.recv_timeout(Duration::from_secs(20));
        }),
    ));
    let link = s
        .store
        .links(project)
        .unwrap()
        .into_iter()
        .find(|l| l.name == "dev")
        .unwrap();
    let epoch = rig.workspace.pause_switch().epoch();
    let (st, p) = (s.clone(), project.clone());
    let taking = rig.service.handle.spawn(async move {
        st.take(&p, "held", &link, epoch, &mut HashMap::new())
            .await
            .map(|t| match t {
                Taken::Full => "full",
                Taken::Job(..) => "job",
                _ => "other",
            })
    });
    tokio::task::spawn_blocking(move || at_admission.recv_timeout(Duration::from_secs(20)))
        .await
        .unwrap()
        .unwrap();
    timeout(
        Duration::from_secs(5),
        s.action(project, WorksAction::Parallel { jobs: 1 }),
    )
    .await
    .expect("lowering is acknowledged while the claim waits")
    .unwrap();
    go.send(()).unwrap();
    assert_eq!(taking.await.unwrap(), Ok("full"));
    let waiting: i64 = rig
        .db
        .query_one(
            "SELECT count(*) FROM connecto_works.connecto_jobs WHERE project_name='held' AND status='to_do'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(waiting, 2);
    s.running.lock().unwrap().remove(project);
}

/// A job whose terminal does not close at the first try keeps its place
/// and its journal row until it does: no moment has its agent running
/// with neither.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_job_lets_go_only_once_its_terminal_closed() {
    let rig = rig().await;
    let _staging = two_databases(&rig, 2).await;
    let (s, project) = (&rig.service.state, &rig.project);
    s.hooks
        .failing_closes
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let id = add_job(&rig.db, "README").await;
    until(90, "the job's end", async || {
        let row = job_row(&rig.db, id).await;
        let view = rig.service.view(project);
        let terminals = job_terminals(&rig.workspace, project);
        if row.0 != "to_do" && view.running.is_empty() {
            assert!(
                terminals.is_empty() && s.store.pendings(project).unwrap().is_empty(),
                "the job let go with its terminal open: {terminals:?} ({row:?})"
            );
            return row.0 == "done_from_agent";
        }
        false
    })
    .await;
    assert_eq!(
        s.hooks
            .failing_closes
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the first close failed"
    );
    assert_eq!(job_row(&rig.db, id).await.1, Some(json!({"answer": 42})));
}

/// A project without Antigravity (ADR 0041): a job that asks for it
/// (`agy`, the queue's default) runs on the first stand-in the project can
/// use, here Claude with Haiku, and the running job says so. With no agent at all, the next job
/// fails naming the order tried.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn without_antigravity_a_job_runs_on_the_first_stand_in() {
    let rig = rig().await;
    let _staging = two_databases(&rig, 1).await;
    let project = &rig.project;
    // Antigravity still being checked: the job is claimed and waits, with
    // no agent chosen and no terminal, rather than guess.
    rig.agents.only(&[ToolKind::Claude]);
    rig.agents.set(
        ToolKind::Antigravity,
        agents::Availability::Pending("Connecto is still checking it".into()),
    );
    async fn queue(db: &Client, prompt: &str) -> i64 {
        db.query_one(
            "INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt,model,effort) VALUES('project','review',$1,'gemini-pro','high') RETURNING id",
            &[&prompt],
        )
        .await
        .unwrap()
        .get(0)
    }
    let id = queue(&rig.db, "README").await;
    until(30, "the job's claim", async || {
        job_row(&rig.db, id).await.0 == "doing"
    })
    .await;
    sleep(Duration::from_secs(2)).await;
    let waiting = rig.service.view(project).running;
    assert!(
        waiting.len() == 1 && waiting[0].agent.is_empty(),
        "{waiting:?}"
    );
    assert!(job_terminals(&rig.workspace, project).is_empty());
    // The check ends: Antigravity is not installed.
    rig.agents.only(&[ToolKind::Claude]);
    let mut seen = String::new();
    until(90, "the job's end", async || {
        if let Some(r) = rig.service.view(project).running.first()
            && r.agent_stage == AgentStage::Running
        {
            seen = r.agent.clone();
        }
        job_row(&rig.db, id).await.0 == "done_from_agent"
    })
    .await;
    assert_eq!(seen, "Claude, model haiku, standing in for Antigravity");
    assert_eq!(job_row(&rig.db, id).await.1, Some(json!({"answer": 42})));
    let dispatched = std::fs::read_to_string(rig.folder.join("dispatch.jsonl")).unwrap();
    let args: Vec<String> = serde_json::from_str::<Value>(dispatched.lines().last().unwrap())
        .unwrap()["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_owned())
        .collect();
    assert!(
        args.windows(2).any(|w| w == ["--model", "haiku"]),
        "{args:?}"
    );
    assert!(
        !args.iter().any(|a| a == "gemini-pro" || a == "high"),
        "{args:?}"
    );
    // No agent at all: the job fails at once, naming the order tried.
    rig.agents.only(&[]);
    let id = queue(&rig.db, "README again").await;
    until(60, "the job's failure", async || {
        job_row(&rig.db, id).await.0 == "failed"
    })
    .await;
    let reason = job_row(&rig.db, id).await.2.unwrap();
    assert!(
        reason.starts_with("No agent can run this job. Tried, in order: Antigravity (not installed); Codex, model gpt-6.1-terra"),
        "{reason}"
    );
}

/// Second review, finding 1 (ADR 0041): a look at an agent that does not
/// end (Pi's keys slow to read, a sign-in check that hangs) holds no job
/// whose choice is made before it: with Claude usable, Pi and those after it
/// are never looked at and the job runs. A job whose choice does reach the
/// held look waits, and Pause ends it at once, the look still held.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_held_agent_look_holds_neither_an_earlier_agent_nor_a_cancel() {
    let rig = rig().await;
    let _staging = two_databases(&rig, 1).await;
    let project = &rig.project;
    rig.agents.only(&[ToolKind::Claude, ToolKind::Pi]);
    rig.agents.held.lock().unwrap().push(ToolKind::Pi);
    let queue = async |prompt: &str| -> i64 {
        rig.db
            .query_one(
                "INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt) VALUES('project','review',$1) RETURNING id",
                &[&prompt],
            )
            .await
            .unwrap()
            .get(0)
    };
    let id = queue("README").await;
    until(90, "the job's end", async || {
        job_row(&rig.db, id).await.0 == "done_from_agent"
    })
    .await;
    assert!(
        !rig.agents.asked.lock().unwrap().contains(&ToolKind::Pi),
        "Pi was looked at: {:?}",
        rig.agents.asked.lock().unwrap()
    );
    // Only Pi now, its look held: the job waits on it, and is cancelled.
    rig.agents.only(&[ToolKind::Pi]);
    rig.agents.asked.lock().unwrap().clear();
    let id = queue("README again").await;
    until(30, "the look at Pi", async || {
        rig.agents.asked.lock().unwrap().contains(&ToolKind::Pi)
    })
    .await;
    assert_eq!(job_row(&rig.db, id).await.0, "doing");
    assert!(job_terminals(&rig.workspace, project).is_empty());
    rig.workspace.pause_switch().pause();
    until(5, "the waiting job to end on Pause", async || {
        rig.service.view(project).running.is_empty()
    })
    .await;
    assert!(rig.agents.held.lock().unwrap().contains(&ToolKind::Pi));
    rig.agents.held.lock().unwrap().clear();
}

/// Held by a test while the process's one `connecto works` bridge is its
/// own service: the tests that set it run one at a time.
static BRIDGE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Four jobs at once (ADR 0040): one misses a piece of data and asks the
/// user, who sees the question (project, type, database, job) and answers;
/// its asking turn ends with valid JSON anyway (a guess), which must not
/// end the job: only the answered value reaches the database. One asks and
/// the user gives no answer; one asks, ends its asking turn with valid JSON
/// and its turn after the answer with no reply at all: it fails, the guess
/// is never taken for that turn's. One needs nothing. The plain job ends as
/// before while the others wait.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_job_missing_data_asks_the_user_and_ends_with_the_answer() {
    let rig = rig().await;
    let _bridge = BRIDGE.lock().await;
    connecto_session::set_works_bridge(
        rig.service.clone() as Arc<dyn connecto_core::agentic_works::WorksBridge>
    );
    let _staging = two_databases(&rig, 4).await;
    let project = &rig.project;
    let answered = add_job(
        &rig.db,
        "WRONG-FIRST ASK-USER:Which season should be reviewed?",
    )
    .await;
    let declined = add_job(&rig.db, "ASK-USER:Which team?").await;
    let silent = add_job(&rig.db, "WRONG-FIRST SILENT-AFTER ASK-USER:Which league?").await;
    let plain = add_job(&rig.db, "README").await;
    let mut asked = Vec::new();
    until(90, "both questions", async || {
        // An ask refused says why at once.
        for entry in std::fs::read_dir(&rig.folder).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("asked-") {
                let run: Value =
                    serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap();
                assert_eq!(run["code"], 0, "{name}: {run}");
            }
        }
        asked = rig.service.questions();
        asked.len() == 3
    })
    .await;
    let question = |job: i64| asked.iter().find(|q| q.job == job).unwrap().clone();
    let (first, second, third) = (question(answered), question(declined), question(silent));
    assert_eq!(
        (
            first.kind.as_str(),
            first.database.as_str(),
            first.text.as_str()
        ),
        ("review", "dev", "Which season should be reviewed?")
    );
    assert_eq!(first.project, *project);
    assert_ne!(first.id, second.id);
    assert_eq!(rig.service.view(project).questions.len(), 3);
    // The job that needs nothing ends as before, while the others wait,
    // even those whose asking turn ended with valid JSON.
    until(90, "the plain job", async || {
        job_row(&rig.db, plain).await.0 == "done_from_agent"
    })
    .await;
    assert_eq!(job_row(&rig.db, plain).await.1, Some(json!({"answer": 42})));
    sleep(Duration::from_secs(2)).await;
    for id in [answered, declined, silent] {
        assert_eq!(
            job_row(&rig.db, id).await.0,
            "doing",
            "job {id} ended unanswered"
        );
    }
    rig.service
        .answer(first.id, Some("2024/25".into()))
        .unwrap();
    rig.service.answer(second.id, None).unwrap();
    rig.service
        .answer(third.id, Some("Serie A".into()))
        .unwrap();
    assert!(rig.service.questions().is_empty());
    // Answered once only.
    assert!(rig.service.answer(first.id, Some("again".into())).is_err());
    for id in [answered, declined, silent] {
        until(90, "the asking job's end", async || {
            job_row(&rig.db, id).await.0 != "doing"
        })
        .await;
    }
    assert_eq!(
        job_row(&rig.db, answered).await.1,
        Some(json!({"answer": "2024/25"}))
    );
    let (status, value, _) = job_row(&rig.db, declined).await;
    assert_eq!(status, "done_from_agent");
    let value = value.unwrap();
    assert_eq!(value["answer"], "missing");
    assert!(
        value["why"]
            .as_str()
            .unwrap()
            .contains("chose not to answer"),
        "{value}"
    );
    // The turn after the answer kept no reply: the asking turn's guess is
    // not taken for it.
    let (status, value, reason) = job_row(&rig.db, silent).await;
    assert_eq!(
        (status.as_str(), value, reason.as_deref()),
        (
            "failed",
            None,
            Some("Provider returned no final assistant text.")
        )
    );
}

/// The user answers at once, while the turn that asked is still on (and
/// Connecto has not yet heard it began, so its list shows the terminal
/// idle); that turn then ends with a valid but wrong guess (ADR 0040). The
/// guess never ends the job: only the JSON of the turn that read the answer
/// with `connecto works answer` reaches the database.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn an_answer_given_while_the_asking_turn_is_on_counts_only_once_read() {
    let rig = rig().await;
    let _bridge = BRIDGE.lock().await;
    connecto_session::set_works_bridge(
        rig.service.clone() as Arc<dyn connecto_core::agentic_works::WorksBridge>
    );
    let _staging = two_databases(&rig, 1).await;
    let id = add_job(&rig.db, "EARLY-ASK WRONG-FIRST ASK-USER:Which match?").await;
    let mut asked = Vec::new();
    until(90, "the question", async || {
        asked = rig.service.questions();
        !asked.is_empty()
    })
    .await;
    rig.service
        .answer(asked[0].id, Some("2024/25".into()))
        .unwrap();
    until(90, "the job's end", async || {
        !matches!(job_row(&rig.db, id).await.0.as_str(), "to_do" | "doing")
    })
    .await;
    assert_eq!(
        job_row(&rig.db, id).await,
        (
            "done_from_agent".into(),
            Some(json!({"answer": "2024/25"})),
            None
        )
    );
}

/// The agent asks once, at once, while its terminal is not yet journalled
/// (held back 3 seconds here): the ask waits for the journal and is heard,
/// and the job ends with the answer (ADR 0040).
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_single_ask_before_the_terminal_is_journalled_is_heard() {
    let rig = rig().await;
    let _bridge = BRIDGE.lock().await;
    connecto_session::set_works_bridge(
        rig.service.clone() as Arc<dyn connecto_core::agentic_works::WorksBridge>
    );
    let _staging = two_databases(&rig, 1).await;
    *rig.service.state.hooks.attach_delay.lock().unwrap() = Some(Duration::from_secs(3));
    let id = add_job(&rig.db, "ASK-USER:Which match?").await;
    let mut asked = Vec::new();
    until(90, "the question", async || {
        for entry in std::fs::read_dir(&rig.folder).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("asked-") {
                let run: Value =
                    serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap();
                assert_eq!(run["code"], 0, "the one ask was refused: {run}");
            }
        }
        asked = rig.service.questions();
        !asked.is_empty()
    })
    .await;
    assert!(
        rig.service
            .state
            .hooks
            .attach_delay
            .lock()
            .unwrap()
            .is_none(),
        "the delay was used"
    );
    rig.service
        .answer(asked[0].id, Some("2024/25".into()))
        .unwrap();
    until(90, "the job's end", async || {
        !matches!(job_row(&rig.db, id).await.0.as_str(), "to_do" | "doing")
    })
    .await;
    assert_eq!(
        job_row(&rig.db, id).await,
        (
            "done_from_agent".into(),
            Some(json!({"answer": "2024/25"})),
            None
        )
    );
}

/// After the agent read the answer, its turn stops with valid interim JSON
/// while a background task still runs; the task reports back and that turn
/// ends with the final JSON (ADR 0040). Only the final JSON reaches the
/// database: a reply kept while background work runs ends no turn.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_reply_kept_while_background_work_runs_does_not_end_the_job() {
    let rig = rig().await;
    let _bridge = BRIDGE.lock().await;
    connecto_session::set_works_bridge(
        rig.service.clone() as Arc<dyn connecto_core::agentic_works::WorksBridge>
    );
    let _staging = two_databases(&rig, 1).await;
    let id = add_job(&rig.db, "BACKGROUND-FIRST ASK-USER:Which match?").await;
    let mut asked = Vec::new();
    until(90, "the question", async || {
        asked = rig.service.questions();
        !asked.is_empty()
    })
    .await;
    rig.service
        .answer(asked[0].id, Some("2024/25".into()))
        .unwrap();
    until(90, "the job's end", async || {
        !matches!(job_row(&rig.db, id).await.0.as_str(), "to_do" | "doing")
    })
    .await;
    assert_eq!(
        job_row(&rig.db, id).await,
        (
            "done_from_agent".into(),
            Some(json!({"answer": "2024/25"})),
            None
        )
    );
}

/// After the agent read the answer, its turn stops with valid interim JSON
/// while a background task runs; the task reports back and that turn ends
/// with no reply at all (ADR 0040). The interim JSON is not that turn's
/// reply: the job fails, and no result reaches the database.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn interim_json_then_a_silent_final_stop_fails_the_job() {
    let rig = rig().await;
    let _bridge = BRIDGE.lock().await;
    connecto_session::set_works_bridge(
        rig.service.clone() as Arc<dyn connecto_core::agentic_works::WorksBridge>
    );
    let _staging = two_databases(&rig, 1).await;
    let id = add_job(
        &rig.db,
        "BACKGROUND-FIRST SILENT-AFTER ASK-USER:Which match?",
    )
    .await;
    let mut asked = Vec::new();
    until(90, "the question", async || {
        asked = rig.service.questions();
        !asked.is_empty()
    })
    .await;
    rig.service
        .answer(asked[0].id, Some("2024/25".into()))
        .unwrap();
    until(90, "the job's end", async || {
        !matches!(job_row(&rig.db, id).await.0.as_str(), "to_do" | "doing")
    })
    .await;
    assert_eq!(
        job_row(&rig.db, id).await,
        (
            "failed".into(),
            None,
            Some("Provider returned no final assistant text.".into())
        )
    );
}

/// Several jobs from two databases running: removing one database by
/// force stops its jobs only, while the other's runs on to its own answer;
/// Pause stops every job, and their outcomes reach their own databases
/// after Resume; Connecto stopping with jobs running closes their
/// terminals, and the next start writes their outcomes back.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn several_running_jobs_through_removal_pause_and_restart() {
    use connecto_core::agentic_works::{Database, WorksAction};
    let rig = rig().await;
    let staging = two_databases(&rig, 4).await;
    let (s, project, workspace) = (&rig.service.state, &rig.project, &rig.workspace);
    let running = || rig.service.view(project).running;
    let hold = r#"CONNECTO-RUN:{"hold": 15, "reply": {"answer": "staging-a"}}"#;
    // Removal by force: dev's two jobs stop, staging's runs on.
    workspace.pause_switch().pause();
    let dev_jobs = [
        add_job(&rig.db, "wait-for-cancel").await,
        add_job(&rig.db, "wait-for-cancel").await,
    ];
    let kept = add_job(&staging, hold).await;
    workspace.pause_switch().resume();
    until(60, "three jobs running", async || running().len() == 3).await;
    s.action(
        project,
        WorksAction::Unlink {
            name: "dev".into(),
            force: true,
        },
    )
    .await
    .unwrap();
    until(30, "dev's jobs stopped", async || {
        let r = running();
        r.len() == 1
            && r[0].database == "staging"
            && job_terminals(workspace, project).len() == 1
            && s.store
                .pendings(project)
                .unwrap()
                .iter()
                .all(|p| p.db == "staging")
    })
    .await;
    until(60, "staging's job answered", async || {
        job_row(&staging, kept).await.0 == "done_from_agent"
    })
    .await;
    assert_eq!(
        job_row(&staging, kept).await.1,
        Some(json!({"answer": "staging-a"}))
    );
    for id in dev_jobs {
        assert_eq!(
            job_row(&rig.db, id).await.0,
            "doing",
            "dropped, not written"
        );
    }
    // Pause with jobs of both databases running.
    s.action(
        project,
        WorksAction::Link {
            name: "dev".into(),
            database: Database::ConnectionString("QUEUE".into()),
            create: false,
            password: None,
        },
    )
    .await
    .unwrap();
    workspace.pause_switch().pause();
    let paused = [
        (true, add_job(&rig.db, "wait-for-cancel").await),
        (false, add_job(&staging, "wait-for-cancel").await),
        (false, add_job(&staging, "wait-for-cancel").await),
    ];
    workspace.pause_switch().resume();
    // Said in full when it fails: what the journal, the terminals and each
    // database showed.
    let end = Instant::now() + Duration::from_secs(60);
    while running().len() < 3 {
        let v = rig.service.view(project);
        assert!(
            Instant::now() < end,
            "three jobs running again: journal {:?}, terminals {:?}, running {:?}, notice {:?}, databases {:?}",
            s.store
                .pendings(project)
                .unwrap()
                .iter()
                .map(|p| (p.db.clone(), p.id, p.dropped, p.terminal.clone()))
                .collect::<Vec<_>>(),
            job_terminals(workspace, project),
            v.running
                .iter()
                .map(|r| (&r.database, r.id))
                .collect::<Vec<_>>(),
            v.notice,
            v.databases
                .iter()
                .map(|d| (&d.name, &d.status.message))
                .collect::<Vec<_>>()
        );
        sleep(Duration::from_millis(100)).await;
    }
    workspace.pause_switch().pause();
    until(30, "every job stopped", async || {
        running().is_empty() && job_terminals(workspace, project).is_empty()
    })
    .await;
    workspace.pause_switch().resume();
    until(60, "the outcomes written back", async || {
        let mut all = true;
        for (dev, id) in paused {
            let client = if dev { &rig.db } else { &staging };
            all &= job_row(client, id).await.0 == "failed";
        }
        all
    })
    .await;
    // Connecto stops with jobs of both databases running.
    workspace.pause_switch().pause();
    let stopped = [
        (true, add_job(&rig.db, "wait-for-cancel").await),
        (false, add_job(&staging, "wait-for-cancel").await),
    ];
    workspace.pause_switch().resume();
    until(60, "two jobs running", async || running().len() == 2).await;
    rig.service.stop();
    assert!(!s.store.pendings(project).unwrap().is_empty());
    let restarted = Works::start(
        &rig.layout.data_root,
        workspace.clone(),
        s.vault.clone(),
        rig.audit.clone(),
        rig.agents.clone(),
    )
    .unwrap();
    until(
        60,
        "the outcomes written back after the start",
        async || {
            let mut all = true;
            for (dev, id) in stopped {
                let client = if dev { &rig.db } else { &staging };
                all &= job_row(client, id).await.0 == "failed";
            }
            all && restarted.state.store.pendings(project).unwrap().is_empty()
        },
    )
    .await;
    assert!(job_terminals(workspace, project).is_empty());
    restarted.stop();
}

/// A job admitted while the project ran one job at a time keeps the
/// project to itself: its agent was told of no other job and has no folder
/// of its own. Raising the number while it runs starts no other job until it
/// ends.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_job_admitted_alone_keeps_the_project_until_it_ends() {
    use connecto_core::agentic_works::WorksAction;
    let rig = rig().await;
    let staging = two_databases(&rig, 1).await;
    let (s, project) = (&rig.service.state, &rig.project);
    let first = add_job(
        &rig.db,
        r#"CONNECTO-RUN:{"hold": 15, "reply": {"answer": "first"}}"#,
    )
    .await;
    until(60, "the first job running", async || {
        rig.service.view(project).running.len() == 1
    })
    .await;
    s.action(project, WorksAction::Parallel { jobs: 10 })
        .await
        .unwrap();
    let second = add_job(
        &staging,
        r#"CONNECTO-RUN:{"hold": 1, "reply": {"answer": "second"}}"#,
    )
    .await;
    until(60, "the first job's answer", async || {
        let row = job_row(&rig.db, first).await;
        if row.0 != "done_from_agent" {
            assert_eq!(
                job_row(&staging, second).await.0,
                "to_do",
                "a job started beside one admitted alone"
            );
        }
        row.0 == "done_from_agent"
    })
    .await;
    until(60, "the second job's answer", async || {
        job_row(&staging, second).await.0 == "done_from_agent"
    })
    .await;
    assert_eq!(
        job_row(&staging, second).await.1,
        Some(json!({"answer": "second"}))
    );
}

/// Two jobs of one type, from two databases, both needing setup again
/// (prepare exits 75): the dev job runs setup, the staging job waits for
/// it. Removing dev by force stops dev's job and its setup; the staging job,
/// whose database is still linked, runs setup itself and answers.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_waiting_job_takes_over_the_setup_of_a_removed_database() {
    use connecto_core::agentic_works::WorksAction;
    use std::{fs, os::unix::fs::PermissionsExt};
    let rig = rig().await;
    let staging = two_databases(&rig, 2).await;
    let (s, project, folder) = (&rig.service.state, &rig.project, &rig.folder);
    let home = rig.root.join("connecto-home");
    fs::create_dir_all(&home).unwrap();
    rig.workspace.set_connecto_home(Some(home)).unwrap();
    fs::create_dir_all(folder.join("tool")).unwrap();
    for (name, body) in [
        // At publish it succeeds at once; later runs wait for `release`.
        (
            "setup.sh",
            "if [ -f published ]; then echo started >> setup.started; i=0; while [ ! -f release ] && [ $i -lt 900 ]; do sleep 0.1; i=$((i+1)); done; else touch published; fi; touch ready",
        ),
        (
            "prepare.sh",
            r#"[ -f ready ] || exit 75; echo '{"ok":true}'"#,
        ),
    ] {
        let path = folder.join("tool").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    s.action(
        project,
        WorksAction::Publish {
            kind: "vid".into(),
            instructions: "Review {{prompt}} with {{prepared}}".into(),
            schema: r#"{"type":"object","required":["answer"]}"#.into(),
            setup: Some(r#"{"argv": ["tool/setup.sh"], "timeout_s": 120}"#.into()),
            prepare: Some(r#"{"argv": ["tool/prepare.sh"], "timeout_s": 30}"#.into()),
            databases: None,
        },
    )
    .await
    .unwrap();
    fs::remove_file(folder.join("ready")).unwrap();
    let insert = "INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt,session_cli) VALUES('project','vid','clip','claude') RETURNING id";
    let started = || {
        fs::read_to_string(folder.join("setup.started"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    let owner: i64 = rig.db.query_one(insert, &[]).await.unwrap().get(0);
    until(60, "dev's job running setup", async || started() == 1).await;
    let waiter: i64 = staging.query_one(insert, &[]).await.unwrap().get(0);
    until(60, "both jobs running", async || {
        rig.service.view(project).running.len() == 2
    })
    .await;
    sleep(Duration::from_secs(2)).await;
    assert_eq!(started(), 1, "the staging job waits for dev's setup");
    s.action(
        project,
        WorksAction::Unlink {
            name: "dev".into(),
            force: true,
        },
    )
    .await
    .unwrap();
    until(30, "the staging job's own setup", async || {
        let row = job_row(&staging, waiter).await;
        assert!(row.0 == "doing", "the staging job ended: {row:?}");
        started() == 2
    })
    .await;
    fs::write(folder.join("release"), "").unwrap();
    until(60, "the staging job's answer", async || {
        job_row(&staging, waiter).await.0 != "doing"
    })
    .await;
    assert_eq!(
        job_row(&staging, waiter).await,
        ("done_from_agent".into(), Some(json!({"answer": 42})), None)
    );
    assert_eq!(job_row(&rig.db, owner).await.0, "doing", "dropped with dev");
}

/// A claim under way when the user pauses and resumes takes nothing: the
/// job it chose is left waiting, and the next look runs it, once. (Taken
/// with the old pause's epoch, it would fail before dispatch.)
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_claim_across_pause_and_resume_leaves_the_job_waiting() {
    let rig = rig().await;
    let _staging = two_databases(&rig, 2).await;
    let (s, project, workspace) = (&rig.service.state, &rig.project, &rig.workspace);
    let (reached, in_claim) = std::sync::mpsc::channel();
    let (go, release) = std::sync::mpsc::channel::<()>();
    // dev's queue alone: a claim in staging passes it by.
    let dev_queue = s
        .store
        .links(project)
        .unwrap()
        .into_iter()
        .find(|l| l.name == "dev")
        .unwrap()
        .queue;
    db::BEFORE_LOCK.lock().unwrap().push((
        dev_queue,
        Box::new(move || {
            let _ = reached.send(());
            let _ = release.recv_timeout(Duration::from_secs(30));
        }),
    ));
    let job = add_job(
        &rig.db,
        r#"CONNECTO-RUN:{"hold": 1, "reply": {"answer": "once"}}"#,
    )
    .await;
    tokio::task::spawn_blocking(move || in_claim.recv_timeout(Duration::from_secs(30)))
        .await
        .unwrap()
        .expect("a claim began");
    // Pause and Resume while that claim is under way.
    workspace.pause_switch().pause();
    workspace.pause_switch().resume();
    go.send(()).unwrap();
    until(60, "the job's answer", async || {
        let row = job_row(&rig.db, job).await;
        assert!(
            row.0 == "to_do" || row.0 == "doing" || row.0 == "done_from_agent",
            "the job was lost: {row:?}"
        );
        row.0 == "done_from_agent"
    })
    .await;
    assert_eq!(
        job_row(&rig.db, job).await.1,
        Some(json!({"answer": "once"}))
    );
    assert_eq!(s.hooks.launches.lock().unwrap().len(), 1, "run once");
}

/// A queue made before, being changed for several jobs while a producer
/// keeps a transaction open on it: the other database's jobs run meanwhile,
/// the changed one gives one job at a time, and Connecto closes at once,
/// the server's build stopped with it.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_queue_being_changed_holds_back_neither_the_others_nor_closing() {
    use connecto_core::agentic_works::WorksAction;
    let rig = rig().await;
    let staging = two_databases(&rig, 1).await;
    // dev, the rig's database, is a queue made before.
    rig.db
        .batch_execute(
            "DROP INDEX connecto_works.doing_work;
            CREATE UNIQUE INDEX one_active_work ON connecto_works.connecto_jobs (status) WHERE status = 'doing'",
        )
        .await
        .unwrap();
    let mut producer = db::open(&rig.dsn).await.unwrap();
    let writing = producer.transaction().await.unwrap();
    writing
        .execute(
            "INSERT INTO connecto_works.connecto_jobs (project_name,work_type,prompt) VALUES ('elsewhere','review','held')",
            &[],
        )
        .await
        .unwrap();
    let project = &rig.project;
    rig.service
        .state
        .action(project, WorksAction::Parallel { jobs: 2 })
        .await
        .unwrap();
    let jobs = [
        add_job(
            &staging,
            r#"CONNECTO-RUN:{"hold": 3, "reply": {"answer": "s1"}}"#,
        )
        .await,
        add_job(
            &staging,
            r#"CONNECTO-RUN:{"hold": 3, "reply": {"answer": "s2"}}"#,
        )
        .await,
    ];
    let dev_job = add_job(
        &rig.db,
        r#"CONNECTO-RUN:{"hold": 1, "reply": {"answer": "d1"}}"#,
    )
    .await;
    until(90, "staging's jobs and dev's one job", async || {
        let mut done = job_row(&rig.db, dev_job).await.0 == "done_from_agent";
        for id in jobs {
            done &= job_row(&staging, id).await.0 == "done_from_agent";
        }
        done
    })
    .await;
    let building: i64 = rig
        .db
        .query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE query ILIKE 'CREATE INDEX CONCURRENTLY%'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(building, 1, "dev's change still waits for the producer");
    let service = rig.service.clone();
    let closed = Instant::now();
    tokio::task::spawn_blocking(move || service.stop())
        .await
        .unwrap();
    assert!(
        closed.elapsed() < Duration::from_secs(10),
        "Connecto took {:?} to close",
        closed.elapsed()
    );
    let until_gone = Instant::now() + Duration::from_secs(10);
    loop {
        let building: i64 = rig
            .db
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE query ILIKE 'CREATE INDEX CONCURRENTLY%' AND state='active'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if building == 0 {
            break;
        }
        assert!(
            Instant::now() < until_gone,
            "the build still runs on the server"
        );
        sleep(Duration::from_millis(100)).await;
    }
    writing.rollback().await.unwrap();
}

/// A database removed while Connecto changes its queue for several jobs at
/// once (the build waiting for a producer): the removal returns only once
/// the change is called off, and when the producer then lets go, the queue
/// keeps the index it was made with: Connecto changes nothing more in it.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn removing_a_database_calls_off_its_queue_change() {
    use connecto_core::agentic_works::WorksAction;
    let rig = rig().await;
    let _staging = two_databases(&rig, 1).await;
    rig.db
        .batch_execute(
            "DROP INDEX connecto_works.doing_work;
            CREATE UNIQUE INDEX one_active_work ON connecto_works.connecto_jobs (status) WHERE status = 'doing'",
        )
        .await
        .unwrap();
    let mut producer = db::open(&rig.dsn).await.unwrap();
    let writing = producer.transaction().await.unwrap();
    writing
        .execute(
            "INSERT INTO connecto_works.connecto_jobs (project_name,work_type,prompt) VALUES ('elsewhere','review','held')",
            &[],
        )
        .await
        .unwrap();
    let (s, project) = (&rig.service.state, &rig.project);
    s.action(project, WorksAction::Parallel { jobs: 2 })
        .await
        .unwrap();
    let building = || async {
        rig.db
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE query ILIKE 'CREATE INDEX CONCURRENTLY%' AND state='active'",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0)
    };
    until(60, "dev's change under way", async || building().await == 1).await;
    s.action(
        project,
        WorksAction::Unlink {
            name: "dev".into(),
            force: true,
        },
    )
    .await
    .unwrap();
    until(10, "the build called off", async || building().await == 0).await;
    writing.commit().await.unwrap();
    sleep(Duration::from_secs(5)).await;
    let legacy: bool = rig
        .db
        .query_one(
            "SELECT to_regclass('connecto_works.one_active_work') IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(legacy, "the removed database's queue was changed");
}

/// dev, the rig's database, made a queue from before several jobs at once,
/// with a producer's transaction open on it (the producer's client: COMMIT
/// there lets go), and the setting raised to 2.
async fn dev_change_held(rig: &Rig) -> Client {
    use connecto_core::agentic_works::WorksAction;
    rig.db
        .batch_execute(
            "DROP INDEX connecto_works.doing_work;
            CREATE UNIQUE INDEX one_active_work ON connecto_works.connecto_jobs (status) WHERE status = 'doing'",
        )
        .await
        .unwrap();
    let producer = db::open(&rig.dsn).await.unwrap();
    producer
        .batch_execute(
            "BEGIN; INSERT INTO connecto_works.connecto_jobs (project_name,work_type,prompt) VALUES ('elsewhere','review','held')",
        )
        .await
        .unwrap();
    rig.service
        .state
        .action(&rig.project, WorksAction::Parallel { jobs: 2 })
        .await
        .unwrap();
    producer
}

/// How many index builds the server runs now.
async fn builds(client: &Client) -> i64 {
    client
        .query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE query ILIKE 'CREATE INDEX CONCURRENTLY%' AND state='active'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// Whether the queue still has the index it was made with.
async fn legacy_index(client: &Client) -> bool {
    client
        .query_one(
            "SELECT to_regclass('connecto_works.one_active_work') IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// The change is stopped where it runs, with its own connection's key:
/// though the link's credential now leads elsewhere (a server that does
/// not answer), removing the database stops the build on the server it
/// runs on.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_called_off_change_is_stopped_on_its_own_server() {
    use connecto_core::{ProviderScope, VaultControl, agentic_works::WorksAction};
    let rig = rig().await;
    let _staging = two_databases(&rig, 1).await;
    let producer = dev_change_held(&rig).await;
    until(60, "dev's change under way", async || {
        builds(&rig.db).await == 1
    })
    .await;
    let scope = ProviderScope::Project(rig.project.clone());
    rig.service
        .state
        .vault
        .put(
            &scope,
            "QUEUE",
            "test",
            "postgresql://postgres:nothing@127.0.0.1:1/postgres",
        )
        .unwrap();
    rig.service
        .state
        .action(
            &rig.project,
            WorksAction::Unlink {
                name: "dev".into(),
                force: true,
            },
        )
        .await
        .unwrap();
    until(10, "the build stopped", async || builds(&rig.db).await == 0).await;
    producer.batch_execute("COMMIT").await.unwrap();
    sleep(Duration::from_secs(3)).await;
    assert!(legacy_index(&rig.db).await);
}

/// When the change cannot be stopped (its server does not answer the
/// cancellation), removing the database fails and leaves it linked; once
/// the server answers again, the change is stopped and the removal works.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn removal_fails_while_the_change_cannot_be_stopped() {
    use connecto_core::agentic_works::WorksAction;
    let rig = rig().await;
    let _staging = two_databases(&rig, 1).await;
    let producer = dev_change_held(&rig).await;
    until(60, "dev's change under way", async || {
        builds(&rig.db).await == 1
    })
    .await;
    let s = &rig.service.state;
    let unlink = || {
        s.action(
            &rig.project,
            WorksAction::Unlink {
                name: "dev".into(),
                force: true,
            },
        )
    };
    let docker = |verb: &str| {
        let out = Command::new("docker")
            .args([verb, &rig._pg.0])
            .output()
            .unwrap();
        assert!(out.status.success(), "docker {verb}");
    };
    docker("pause");
    let refused = unlink().await;
    docker("unpause");
    let refused = refused.unwrap_err();
    assert!(refused.contains("still linked"), "{refused}");
    assert!(s.store.linked(&rig.project, "dev").unwrap().is_some());
    until(
        30,
        "the change stopped once the server answers",
        async || builds(&rig.db).await == 0,
    )
    .await;
    unlink().await.unwrap();
    producer.batch_execute("COMMIT").await.unwrap();
    sleep(Duration::from_secs(3)).await;
    assert!(legacy_index(&rig.db).await);
}

/// A look that was about to start the change when the database was removed
/// starts nothing: no index is built in a queue no longer the project's.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_change_about_to_start_when_its_database_goes_starts_nothing() {
    use connecto_core::agentic_works::WorksAction;
    let rig = rig().await;
    let _staging = two_databases(&rig, 1).await;
    let s = &rig.service.state;
    let dev_queue = s
        .store
        .links(&rig.project)
        .unwrap()
        .into_iter()
        .find(|l| l.name == "dev")
        .unwrap()
        .queue;
    let (reached, at_start) = std::sync::mpsc::channel();
    let (go, release) = std::sync::mpsc::channel::<()>();
    s.hooks.migration.lock().unwrap().push((
        dev_queue,
        Box::new(move || {
            let _ = reached.send(());
            let _ = release.recv_timeout(Duration::from_secs(30));
        }),
    ));
    let producer = dev_change_held(&rig).await;
    producer.batch_execute("COMMIT").await.unwrap();
    tokio::task::spawn_blocking(move || at_start.recv_timeout(Duration::from_secs(60)))
        .await
        .unwrap()
        .expect("a look is about to start dev's change");
    s.action(
        &rig.project,
        WorksAction::Unlink {
            name: "dev".into(),
            force: true,
        },
    )
    .await
    .unwrap();
    go.send(()).unwrap();
    sleep(Duration::from_secs(5)).await;
    let built: bool = rig
        .db
        .query_one(
            "SELECT to_regclass('connecto_works.doing_work') IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!built, "an index was built in a removed database's queue");
    assert!(legacy_index(&rig.db).await);
}

/// What a test makes [`cancel_dropping_proxy`] do.
#[derive(Default)]
struct ProxyControl {
    /// Take in and drop every cancellation request while set.
    drop_cancels: AtomicBool,
    /// Cut the client side of every session open now, keeping its
    /// connection to PostgreSQL open (the server sees nothing).
    cut_clients: tokio::sync::Notify,
    /// Close the connections to PostgreSQL kept that way.
    let_go: tokio::sync::Notify,
}

/// A TCP proxy to the PostgreSQL at `upstream` that passes connections
/// through, and misbehaves as `control` says (as a proxy on the way may).
async fn cancel_dropping_proxy(
    upstream: std::net::SocketAddr,
    control: Arc<ProxyControl>,
) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const CANCEL: u32 = 80_877_102;
    const SSL: u32 = 80_877_103;
    const GSS: u32 = 80_877_104;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let control = Arc::clone(&control);
            tokio::spawn(async move {
                let Ok(mut server) = tokio::net::TcpStream::connect(upstream).await else {
                    return;
                };
                // The startup packets, in the clear: SSL or GSS asked for
                // (and refused) first, then the startup or a cancellation.
                loop {
                    let mut head = [0u8; 8];
                    if client.read_exact(&mut head).await.is_err() {
                        return;
                    }
                    let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
                    let code = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
                    let mut rest = vec![0u8; len.saturating_sub(8)];
                    if client.read_exact(&mut rest).await.is_err() {
                        return;
                    }
                    if code == CANCEL && control.drop_cancels.load(Ordering::SeqCst) {
                        return;
                    }
                    if server.write_all(&head).await.is_err()
                        || server.write_all(&rest).await.is_err()
                    {
                        return;
                    }
                    if code == SSL || code == GSS {
                        let mut answer = [0u8; 1];
                        if server.read_exact(&mut answer).await.is_err()
                            || client.write_all(&answer).await.is_err()
                        {
                            return;
                        }
                        if answer[0] == b'N' {
                            continue;
                        }
                    }
                    break;
                }
                let cut = control.cut_clients.notified();
                let cut = tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut client, &mut server) => false,
                    () = cut => true,
                };
                if cut {
                    // The client's connection goes; PostgreSQL's stays,
                    // until the test lets it go.
                    drop(client);
                    control.let_go.notified().await;
                    drop(server);
                }
            });
        }
    });
    addr
}

/// The change is stopped only once its statement has ended on the server:
/// a cancellation taken in by a proxy and dropped does not count. While it
/// still runs, removing the database fails and leaves it linked; once the
/// cancellations get through, the change stops and the removal works.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_dropped_cancellation_leaves_the_database_linked() {
    use connecto_core::{ProviderScope, VaultControl, agentic_works::WorksAction};
    let rig = rig().await;
    // dev is reached through the proxy.
    let parsed: tokio_postgres::Config = rig.dsn.parse().unwrap();
    let port = parsed.get_ports()[0];
    let upstream = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let control = Arc::new(ProxyControl::default());
    control.drop_cancels.store(true, Ordering::SeqCst);
    let proxy = cancel_dropping_proxy(upstream, Arc::clone(&control)).await;
    let through = rig
        .dsn
        .replace(&format!("127.0.0.1:{port}"), &proxy.to_string());
    assert_ne!(through, rig.dsn);
    let scope = ProviderScope::Project(rig.project.clone());
    rig.service
        .state
        .vault
        .put(&scope, "QUEUE", "test", &through)
        .unwrap();
    let _staging = two_databases(&rig, 1).await;
    let producer = dev_change_held(&rig).await;
    until(60, "dev's change under way", async || {
        builds(&rig.db).await == 1
    })
    .await;
    let s = &rig.service.state;
    let unlink = || {
        s.action(
            &rig.project,
            WorksAction::Unlink {
                name: "dev".into(),
                force: true,
            },
        )
    };
    let refused = unlink().await.unwrap_err();
    assert!(refused.contains("still linked"), "{refused}");
    assert!(s.store.linked(&rig.project, "dev").unwrap().is_some());
    assert_eq!(builds(&rig.db).await, 1, "the build still runs");
    // The cancellations get through again: the change stops.
    control.drop_cancels.store(false, Ordering::SeqCst);
    until(30, "the change stopped", async || {
        builds(&rig.db).await == 0
    })
    .await;
    unlink().await.unwrap();
    producer.batch_execute("COMMIT").await.unwrap();
    sleep(Duration::from_secs(3)).await;
    assert!(legacy_index(&rig.db).await);
}

/// The change's connection is lost mid-build (a proxy cuts Connecto's side
/// and keeps the server's open): no answer from the server is not an end.
/// While the server's session still holds the change, removing the database
/// fails and leaves it linked; once that session is gone, the removal works.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn a_lost_change_connection_leaves_the_database_linked() {
    use connecto_core::{ProviderScope, VaultControl, agentic_works::WorksAction};
    let rig = rig().await;
    let parsed: tokio_postgres::Config = rig.dsn.parse().unwrap();
    let port = parsed.get_ports()[0];
    let upstream = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let control = Arc::new(ProxyControl::default());
    control.drop_cancels.store(true, Ordering::SeqCst);
    let proxy = cancel_dropping_proxy(upstream, Arc::clone(&control)).await;
    let through = rig
        .dsn
        .replace(&format!("127.0.0.1:{port}"), &proxy.to_string());
    let scope = ProviderScope::Project(rig.project.clone());
    rig.service
        .state
        .vault
        .put(&scope, "QUEUE", "test", &through)
        .unwrap();
    let _staging = two_databases(&rig, 1).await;
    let producer = dev_change_held(&rig).await;
    until(60, "dev's change under way", async || {
        builds(&rig.db).await == 1
    })
    .await;
    // Connecto's side of the change's connection is cut; the build goes on.
    control.cut_clients.notify_waiters();
    sleep(Duration::from_secs(2)).await;
    assert_eq!(builds(&rig.db).await, 1, "the build still runs");
    let s = &rig.service.state;
    let unlink = || {
        s.action(
            &rig.project,
            WorksAction::Unlink {
                name: "dev".into(),
                force: true,
            },
        )
    };
    let refused = unlink().await.unwrap_err();
    assert!(refused.contains("still linked"), "{refused}");
    assert!(s.store.linked(&rig.project, "dev").unwrap().is_some());
    assert_eq!(builds(&rig.db).await, 1, "the build still runs");
    // The cancellation gets through and the server's session ends: the
    // change is over, and the removal works.
    control.drop_cancels.store(false, Ordering::SeqCst);
    until(30, "the build stopped", async || builds(&rig.db).await == 0).await;
    control.let_go.notify_waiters();
    until(30, "the removal", async || unlink().await.is_ok()).await;
    producer.batch_execute("COMMIT").await.unwrap();
    sleep(Duration::from_secs(3)).await;
    assert!(legacy_index(&rig.db).await);
}

/// Ten jobs at once from two databases: with the setting at 10, sixteen
/// waiting jobs (ten in dev, six in staging) fill ten places shared five and
/// five, all ten agents run together, and every job's own answer goes back
/// to its own database. Staging is a queue made before several jobs at once
/// (one job doing at most): it is changed to allow them.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn ten_jobs_run_at_once_shared_by_the_databases() {
    use connecto_core::{
        Binding, ProviderScope, VaultControl,
        agentic_works::{Database, WorksAction},
    };
    let Rig {
        _pg,
        dsn,
        db: dev,
        _temp,
        project,
        service,
        workspace,
        ..
    } = rig().await;
    dev.batch_execute("CREATE DATABASE staging").await.unwrap();
    let (server, _) = dsn.rsplit_once('/').unwrap();
    let scope = ProviderScope::Project(project.clone());
    let vault = service.state.vault.clone();
    vault
        .put(&scope, "STAGING", "test", &format!("{server}/staging"))
        .unwrap();
    vault
        .set_bindings(&scope, "STAGING", vec![Binding::AgenticWorks])
        .unwrap();
    let s = &service.state;
    for (name, secret) in [("dev", "QUEUE"), ("staging", "STAGING")] {
        s.action(
            &project,
            WorksAction::Link {
                name: name.into(),
                database: Database::ConnectionString(secret.into()),
                create: true,
                password: None,
            },
        )
        .await
        .unwrap();
    }
    let staging = db::open(&format!("{server}/staging")).await.unwrap();
    staging
        .batch_execute("CREATE UNIQUE INDEX one_active_work ON connecto_works.connecto_jobs (status) WHERE status = 'doing'")
        .await
        .unwrap();
    s.action(
        &project,
        WorksAction::Publish {
            kind: "review".into(),
            instructions: "Review {{prompt}}".into(),
            schema: r#"{"type":"object","required":["answer"]}"#.into(),
            setup: None,
            prepare: None,
            databases: None,
        },
    )
    .await
    .unwrap();
    assert!(
        s.action(&project, WorksAction::Parallel { jobs: 0 })
            .await
            .is_err()
    );
    s.action(&project, WorksAction::Parallel { jobs: 10 })
        .await
        .unwrap();
    assert_eq!(service.view(&project).parallel, 10);
    // Each job holds its agent 12 s and answers with where it came from.
    let insert = "INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt,session_cli) VALUES('project','review',$1,'claude')";
    let prompt = |db: &str, n: usize| {
        format!(r#"CONNECTO-RUN:{{"hold": 12, "reply": {{"answer": "{db}-{n}"}}}}"#)
    };
    // Queued under Pause, so no look sees a part of them only.
    // Each job's id and the answer only it gives.
    workspace.pause_switch().pause();
    let mut wanted: Vec<(bool, i64, String)> = Vec::new();
    for n in 0..10 {
        let id: i64 = dev
            .query_one(&format!("{insert} RETURNING id"), &[&prompt("dev", n)])
            .await
            .unwrap()
            .get(0);
        wanted.push((true, id, format!("dev-{n}")));
    }
    for n in 0..6 {
        let id: i64 = staging
            .query_one(&format!("{insert} RETURNING id"), &[&prompt("staging", n)])
            .await
            .unwrap()
            .get(0);
        wanted.push((false, id, format!("staging-{n}")));
    }
    workspace.pause_switch().resume();
    type Row = (i64, String, Option<Value>);
    async fn rows(client: &Client) -> Vec<Row> {
        client
            .query(
                "SELECT id,status,result FROM connecto_works.connecto_jobs ORDER BY id",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect()
    }
    let doing = |list: &[Row]| list.iter().filter(|r| r.1 == "doing").count();
    // The most running at once, and how the databases shared them then.
    let mut most = (0, 0, 0);
    let until = Instant::now() + Duration::from_secs(180);
    loop {
        let view = service.view(&project);
        let of = |name: &str| view.running.iter().filter(|r| r.database == name).count();
        if view.running.len() > most.0 {
            most = (view.running.len(), of("dev"), of("staging"));
        }
        let (dr, sr) = (rows(&dev).await, rows(&staging).await);
        assert!(doing(&dr) + doing(&sr) <= 10, "{dr:?} {sr:?}");
        let finished = |list: &[Row]| list.iter().all(|r| r.1 == "done_from_agent");
        if finished(&dr) && finished(&sr) {
            break;
        }
        assert!(
            Instant::now() < until,
            "dev {dr:?} staging {sr:?} notice {}",
            view.notice
        );
        sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(most, (10, 5, 5), "the ten places, and how they were shared");
    // Ten agents started within a few seconds of each other: they ran
    // together, not one after another (each holds 12 s).
    let launches = s.hooks.launches.lock().unwrap().clone();
    assert_eq!(launches.len(), 16);
    let mut at: Vec<Instant> = launches.iter().map(|(_, at)| *at).collect();
    at.sort();
    assert!(
        at[9].duration_since(at[0]) < Duration::from_secs(10),
        "the first ten agents started over {:?}",
        at[9].duration_since(at[0])
    );
    // Each outcome is its own job's, in its own database.
    for (in_dev, id, answer) in &wanted {
        let client = if *in_dev { &dev } else { &staging };
        let row = job_row(client, *id).await;
        assert_eq!(
            (row.0.as_str(), row.1),
            ("done_from_agent", Some(json!({ "answer": answer }))),
            "job {id}"
        );
    }
    let single: bool = staging
        .query_one(
            "SELECT to_regclass('connecto_works.one_active_work') IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!single, "the old queue allows several jobs doing now");
    // Every journal row was written back and cleared; every agent's
    // terminal closed.
    let until = Instant::now() + Duration::from_secs(30);
    while !s.store.pendings(&project).unwrap().is_empty()
        || !service.view(&project).running.is_empty()
        || !job_terminals(&workspace, &project).is_empty()
    {
        assert!(
            Instant::now() < until,
            "{:?}",
            job_terminals(&workspace, &project)
        );
        sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn menu_author_publish_poll_capture_recover_and_pause() {
    use connecto_session::launch::project_id;
    use std::{fs, os::unix::fs::PermissionsExt};
    // Every field bound: the container and the folder live to the end.
    let Rig {
        _pg,
        dsn,
        db,
        _temp,
        root,
        folder,
        project,
        layout,
        workspace,
        audit,
        service,
        ..
    } = rig().await;
    service.submit(
        project.clone(),
        WorksAction::Link {
            name: "default".into(),
            database: connecto_core::agentic_works::Database::ConnectionString("QUEUE".into()),
            create: true,
            password: None,
        },
    );
    let until = Instant::now() + Duration::from_secs(15);
    while !service.view(&project).linked() {
        assert!(Instant::now() < until, "{}", service.view(&project).notice);
        sleep(Duration::from_millis(50)).await;
    }
    service.submit(
        project.clone(),
        WorksAction::Author {
            kind: "review".into(),
            cli: ToolKind::Claude,
        },
    );
    let until = Instant::now() + Duration::from_secs(45);
    let WorksDraft {
        instructions,
        schema,
        ..
    } = loop {
        if let Ok(draft) = service.draft(&project, "review") {
            break draft;
        }
        assert!(
            Instant::now() < until,
            "author: {} {:?}",
            service.view(&project).notice,
            workspace
                .relay(&project)
                .unwrap()
                .terminals()
                .view()
                .terminals
        );
        sleep(Duration::from_millis(100)).await;
    };
    service.submit(
        project.clone(),
        WorksAction::Publish {
            kind: "review".into(),
            instructions,
            schema,
            setup: None,
            prepare: None,
            databases: None,
        },
    );
    workspace.pause_switch().pause();
    db.batch_execute("INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt,session_cli,model,effort) VALUES('project','review','README','claude','sonnet','high')").await.unwrap();
    sleep(Duration::from_secs(6)).await;
    assert_eq!(
        db.query_one("SELECT status FROM connecto_works.connecto_jobs", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "to_do"
    );
    workspace.pause_switch().resume();
    let until = Instant::now() + Duration::from_secs(45);
    loop {
        let row = db
            .query_one(
                "SELECT status,result,reason FROM connecto_works.connecto_jobs",
                &[],
            )
            .await
            .unwrap();
        if row.get::<_, String>(0) == "done_from_agent" {
            assert_eq!(row.get::<_, Value>(1), json!({"answer":42}));
            break;
        }
        assert!(
            row.get::<_, Option<String>>(2).is_none() && Instant::now() < until,
            "{:?} {}",
            row.get::<_, Option<String>>(2),
            service.view(&project).notice
        );
        sleep(Duration::from_millis(100)).await;
    }
    let capture = fs::read_to_string(folder.join("dispatch.jsonl")).unwrap();
    let args: Value = serde_json::from_str(capture.trim()).unwrap();
    assert!(args["prompt"].as_str().unwrap().contains("Review README"));
    assert!(
        args["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "sonnet")
    );
    assert!(args["args"].as_array().unwrap().iter().any(|a| a == "high"));
    assert!(!capture.contains(&dsn));

    // Setup runs when publishing; a failing setup publishes nothing.
    fs::create_dir_all(folder.join("tool")).unwrap();
    for (name, body) in [
        ("setup.sh", "echo ok >> setup.ran"),
        ("broken.sh", "echo ffmpeg is missing >&2; exit 2"),
        (
            "prepare.sh",
            r#"cat > "$CONNECTO_JOB_DIR/in"; printf '{"dir":"%s","bytes":%s}' "$CONNECTO_JOB_DIR" "$(wc -c < "$CONNECTO_JOB_DIR/in" | tr -d ' ')""#,
        ),
        ("refuse.sh", "echo no pieces >&2; exit 4"),
    ] {
        let path = folder.join("tool").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let command = |name: &str| Some(format!(r#"{{"argv": ["tool/{name}"], "timeout_s": 30}}"#));
    let publish = |kind: &str, setup: Option<String>, prepare: Option<String>| {
        service.submit(
            project.clone(),
            WorksAction::Publish {
                kind: kind.into(),
                instructions: "Review {{prompt}}. Prepared: {{prepared}}".into(),
                schema: r#"{"type":"object","required":["answer"]}"#.into(),
                setup,
                prepare,
                databases: None,
            },
        );
    };
    // Setup runs in a shell terminal of the agentic jobs project in Connecto
    // Home; the failed one stays open there for the user to read.
    let connecto_home = root.join("connecto-home");
    fs::create_dir_all(&connecto_home).unwrap();
    workspace
        .set_connecto_home(Some(connecto_home.clone()))
        .unwrap();
    let jobs_id = project_id(&connecto_home.join(connecto_workspace::AGENTIC_JOBS_FOLDER));
    publish("broken", command("broken.sh"), command("prepare.sh"));
    let until = Instant::now() + Duration::from_secs(30);
    while !service.view(&project).notice.contains("failed (exit 2)") {
        assert!(Instant::now() < until, "{}", service.view(&project).notice);
        sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !service
            .view(&project)
            .types
            .iter()
            .any(|t| t.name == "broken")
    );
    let setup_terminals = || {
        workspace
            .relay(&jobs_id)
            .unwrap()
            .terminals()
            .view()
            .terminals
            .iter()
            .filter_map(|t| t.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(setup_terminals(), ["setup broken · project"]);
    let view = workspace.view();
    let jobs_group = view
        .projects
        .iter()
        .find(|p| p.id == jobs_id)
        .and_then(|p| p.group.clone());
    assert!(
        view.groups
            .iter()
            .any(|g| Some(&g.id) == jobs_group.as_ref() && g.name == "Connecto Internal")
    );
    publish("prep", command("setup.sh"), command("prepare.sh"));
    publish("refuse", None, command("refuse.sh"));
    let until = Instant::now() + Duration::from_secs(30);
    while service.view(&project).types.len() < 3 {
        assert!(Instant::now() < until, "{}", service.view(&project).notice);
        sleep(Duration::from_millis(50)).await;
    }
    let types = service.view(&project).types;
    assert!(
        types
            .iter()
            .any(|t| t.name == "prep" && t.setup && t.setup_ok_at.is_some())
    );
    assert_eq!(
        fs::read_to_string(folder.join("setup.ran")).unwrap(),
        "ok\n"
    );
    // The successful setup's terminal is closed; the failed one remains.
    assert_eq!(setup_terminals(), ["setup broken · project"]);

    // prepare's JSON reaches the agent's prompt; its folder is gone after.
    let finished = |prompt: &'static str| {
        let db = &db;
        async move {
            let until = Instant::now() + Duration::from_secs(45);
            loop {
                let row = db
                    .query_one(
                        "SELECT status,reason FROM connecto_works.connecto_jobs WHERE prompt=$1",
                        &[&prompt],
                    )
                    .await
                    .unwrap();
                let status: String = row.get(0);
                if status != "to_do" && status != "doing" {
                    return (status, row.get::<_, Option<String>>(1));
                }
                assert!(Instant::now() < until, "{prompt} still {status}");
                sleep(Duration::from_millis(100)).await;
            }
        }
    };
    db.batch_execute("INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt,session_cli) VALUES('project','prep','clip; $(touch pwned) `id`','claude')").await.unwrap();
    assert_eq!(
        finished("clip; $(touch pwned) `id`").await,
        ("done_from_agent".into(), None)
    );
    let capture = fs::read_to_string(folder.join("dispatch.jsonl")).unwrap();
    let last: Value = serde_json::from_str(capture.lines().last().unwrap()).unwrap();
    let sent = last["prompt"].as_str().unwrap();
    let dir = folder.canonicalize().unwrap().join(".connecto/works");
    assert!(
        sent.contains("Review clip; $(touch pwned) `id`. Prepared: {\"bytes\":25,\"dir\":")
            && sent.contains(&dir.display().to_string()),
        "{sent}"
    );
    assert!(!folder.join("pwned").exists());
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != ".gitignore")
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // A failing prepare fails the job without opening a session.
    db.batch_execute("INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt,session_cli) VALUES('project','refuse','nothing','claude')").await.unwrap();
    let (status, reason) = finished("nothing").await;
    assert_eq!(status, "failed");
    let reason = reason.unwrap();
    assert!(
        reason.contains("Prepare exited with code 4.") && reason.contains("no pieces"),
        "{reason}"
    );
    assert_eq!(
        fs::read_to_string(folder.join("dispatch.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );

    db.batch_execute("INSERT INTO connecto_works.connecto_jobs(project_name,work_type,prompt,session_cli) VALUES('project','review','wait-for-cancel','claude')").await.unwrap();
    let until = Instant::now() + Duration::from_secs(30);
    while fs::read_to_string(folder.join("dispatch.jsonl"))
        .unwrap()
        .lines()
        .count()
        < 3
    {
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(100)).await;
    }
    // Drop the worker's database connection after submission, then cancel.
    db.batch_execute("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND backend_type='client backend'").await.unwrap();
    workspace.pause_switch().pause();
    let until = Instant::now() + Duration::from_secs(10);
    let terminals = workspace.relay(&project).unwrap().terminals();
    while terminals.view().terminals.iter().any(|t| {
        t.name
            .as_deref()
            .is_some_and(|n| n.starts_with("agentic_works_"))
    }) {
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(100)).await;
    }
    service.stop();
    assert!(
        !service
            .state
            .store
            .pending(&project, "default")
            .unwrap()
            .is_empty()
    );
    let restarted = Works::start(
        &layout.data_root,
        workspace.clone(),
        service.state.vault.clone(),
        audit,
        Arc::new(TestAgents::default()),
    )
    .unwrap();
    workspace.pause_switch().resume();
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let row = db
            .query_one(
                "SELECT status,reason FROM connecto_works.connecto_jobs WHERE prompt='wait-for-cancel'",
                &[],
            )
            .await
            .unwrap();
        if row.get::<_, String>(0) == "failed" {
            assert!(row.get::<_, String>(1).contains("cancelled"));
            break;
        }
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        fs::read_to_string(folder.join("dispatch.jsonl"))
            .unwrap()
            .lines()
            .count(),
        3
    );
    restarted.stop();
    workspace.begin_shutdown();
    workspace.stop_everything();
    workspace.shut_down(Duration::from_secs(5));
}

/// A Claude terminal in the project whose agent (the fixture) runs the
/// `connecto` commands of `plan`, as an agent would, and answers with its
/// `reply`. What each command printed and its exit code.
async fn agent_runs(
    workspace: &Workspace,
    project: &str,
    folder: &Path,
    plan: &Value,
) -> Vec<Value> {
    let control = workspace.relay(project).unwrap().terminals();
    let id = control
        .add_with(
            ToolKind::Claude,
            None,
            Some(format!("CONNECTO-RUN:{plan}")),
            None,
            None,
        )
        .unwrap();
    runs_of(folder, &id).await
}

/// What terminal `id`'s agent ran, once it has.
async fn runs_of(folder: &Path, id: &str) -> Vec<Value> {
    let file = folder.join(format!("runs-{id}.json"));
    let until = Instant::now() + Duration::from_secs(40);
    while !file.is_file() {
        assert!(Instant::now() < until, "{id} ran nothing");
        sleep(Duration::from_millis(100)).await;
    }
    serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap()
}

/// Restart terminal `id` with a plan for its agent: what it ran.
async fn rerun(
    workspace: &Workspace,
    project: &str,
    folder: &Path,
    id: &str,
    plan: &Value,
) -> Vec<Value> {
    let control = workspace.relay(project).unwrap().terminals();
    // Its first start done, so the restart is a restart.
    let until = Instant::now() + Duration::from_secs(30);
    while !control
        .view()
        .terminals
        .iter()
        .any(|t| t.id == id && t.finished_turns > 0)
    {
        assert!(Instant::now() < until, "{id} never finished a turn");
        sleep(Duration::from_millis(100)).await;
    }
    std::fs::create_dir_all(folder.join("plans")).unwrap();
    std::fs::write(
        folder.join("plans").join(format!("{id}.json")),
        plan.to_string(),
    )
    .unwrap();
    let _ = std::fs::remove_file(folder.join(format!("runs-{id}.json")));
    // Restarted as the user does, from the pane: its agent is still running.
    control
        .view()
        .terminals
        .iter()
        .find(|t| t.id == id)
        .and_then(|t| t.control.clone())
        .unwrap()
        .request_restart();
    let file = folder.join(format!("runs-{id}.json"));
    let until = Instant::now() + Duration::from_secs(40);
    while !file.is_file() {
        if Instant::now() >= until {
            let view = control.view();
            let t = view.terminals.iter().find(|t| t.id == id);
            panic!(
                "{id} ran nothing after its restart: {:?} {:?} plan left: {}",
                t.map(|t| (t.state, t.finished_turns, t.parent.clone())),
                t.and_then(|t| t.control.as_ref().map(|c| c.stage())),
                folder.join("plans").join(format!("{id}.json")).is_file()
            );
        }
        sleep(Duration::from_millis(100)).await;
    }
    runs_of(folder, id).await
}

fn ran(runs: &[Value], at: usize, code: i64) -> (&str, &str) {
    let run = &runs[at];
    assert_eq!(run["code"], code, "{run}");
    (run["out"].as_str().unwrap(), run["err"].as_str().unwrap())
}

/// What an agent does when asked to "set up this agentic work", through the
/// real `connecto` command in project terminals: the command line, the wire,
/// the bridge naming the caller from the process tree and its launch mode
/// from the launch, the app, the vault's request flow, and PostgreSQL.
/// Connecto then runs the job and keeps its JSON in the row.
///
/// Boundaries: a Bypass agent sets everything up with the command alone;
/// the connection string reaches the vault only through a request the user
/// answers; an agent started in Prompt mode may only read, and its
/// definition goes to the sheet for the user to import; a job's own agent
/// cannot change Agentic Works; a Cloud SQL password is asked for by
/// Connecto for one instance and user, never taken from another secret, and
/// is never a connection string; no reply shows a vault value.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container and a fixture CLI in a real project PTY"]
async fn an_agent_sets_up_and_tests_a_work_with_connecto_works_alone() {
    use connecto_core::{
        AgentMode, AskAnswer, Binding, LaunchModes, ProviderScope, VaultControl,
        agentic_works::WorksBridge,
    };
    // The container and the folder bound too, so they live to the end.
    let Rig {
        _pg,
        dsn,
        db,
        _temp,
        root,
        folder,
        project,
        layout,
        workspace,
        service,
        ..
    } = rig().await;
    let _bridge = BRIDGE.lock().await;
    let vault = service.state.vault.clone();
    connecto_session::set_vault_bridge(vault.clone() as Arc<dyn connecto_core::VaultBridge>);
    connecto_session::set_works_bridge(service.clone() as Arc<dyn WorksBridge>);
    let def =
        r#"{"instructions":"Review {{prompt}}","schema":{"type":"object","required":["answer"]}}"#;
    let answer = |name: &str, value: &str| {
        let ask = VaultControl::view(&*vault)
            .asks
            .iter()
            .find(|a| a.name.starts_with(name))
            .cloned()
            .unwrap_or_else(|| panic!("no request for {name}"));
        vault
            .answer(
                ask.id,
                AskAnswer::Provide {
                    scope: ProviderScope::Project(project.clone()),
                    value: value.to_owned(),
                    bindings: vec![Binding::AgenticWorks],
                },
            )
            .unwrap();
        ask
    };

    // 1. Nothing is set up; the agent asks the user for the connection string.
    let runs = agent_runs(
        &workspace,
        &project,
        &folder,
        &json!({"runs": [
            {"args": ["works", "show"]},
            {"args": ["secret", "request", "WORKS_DB", "--agentic-works", "--kind", "password",
                      "--reason", "the Agentic Works queue"]},
            {"args": ["works", "link", "--secret", "WORKS_DB", "--create"]},
        ]}),
    )
    .await;
    assert!(ran(&runs, 0, 0).0.contains("Databases: none linked"));
    ran(&runs, 1, 0);
    assert!(
        ran(&runs, 2, 1)
            .1
            .contains("connecto secret request WORKS_DB --agentic-works")
    );
    let ask = answer("WORKS_DB", &dsn);
    assert_eq!(ask.proposed, [Binding::AgenticWorks]);

    // 2. A Bypass agent links, publishes and queues a job: the job's own
    // agent tries to change a type and leaks the connection string in its
    // answer.
    let job_prompt = format!(
        "README CONNECTO-RUN:{}",
        json!({"runs": [{"args": ["works", "publish", "evil"], "stdin": def},
                        {"args": ["works", "show"]}],
               "reply": {"answer": 42, "leak": dsn}})
    );
    let runs = agent_runs(
        &workspace,
        &project,
        &folder,
        &json!({"runs": [
            {"args": ["works", "test-db", "--secret", "WORKS_DB"]},
            {"args": ["works", "link", "--secret", "WORKS_DB", "--create"]},
            {"args": ["works", "check"], "stdin": def},
            {"args": ["works", "publish", "review"], "stdin": def},
            {"args": ["works", "test", "review", "--cli", "claude", "--model", "sonnet",
                      "--prompt", job_prompt]},
            {"args": ["works", "show"]},
        ]}),
    )
    .await;
    assert!(ran(&runs, 0, 0).0.contains("Queue schema absent"));
    assert!(ran(&runs, 1, 0).0.contains("Database default linked."));
    assert!(ran(&runs, 2, 0).0.contains("valid"));
    assert!(ran(&runs, 3, 0).0.contains("Published review."));
    let queued = ran(&runs, 4, 0).0;
    let id: i64 = queued.split_whitespace().nth(2).unwrap().parse().unwrap();
    let shown = ran(&runs, 5, 0).0;
    assert!(
        shown.contains("  default  WORKS_DB  (")
            && shown.contains("  review  (every database; no setup)"),
        "{shown}"
    );
    assert!(
        shown.contains("WORKS_DB") && !shown.contains(&dsn),
        "{shown}"
    );
    let until = Instant::now() + Duration::from_secs(45);
    loop {
        let row = db
            .query_one(
                "SELECT status,result,reason,model FROM connecto_works.connecto_jobs WHERE id=$1",
                &[&id],
            )
            .await
            .unwrap();
        if row.get::<_, String>(0) == "done_from_agent" {
            // The row is the producer's own, as the agent answered.
            assert_eq!(row.get::<_, Value>(1), json!({"answer": 42, "leak": dsn}));
            assert_eq!(row.get::<_, Option<String>>(3).as_deref(), Some("sonnet"));
            break;
        }
        assert!(
            Instant::now() < until,
            "{:?}",
            row.get::<_, Option<String>>(2)
        );
        sleep(Duration::from_millis(200)).await;
    }
    let job_runs: Vec<Value> = std::fs::read_dir(&folder)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("runs-claude"))
        .map(|e| {
            serde_json::from_str::<Vec<Value>>(&std::fs::read_to_string(e.path()).unwrap()).unwrap()
        })
        .find(|runs| {
            runs.first()
                .is_some_and(|r| r["args"][1] == "publish" && r["args"][2] == "evil")
        })
        .expect("the job's agent ran its commands");
    assert!(
        ran(&job_runs, 0, 1)
            .1
            .contains("a job's agent cannot change Agentic Works")
    );
    ran(&job_runs, 1, 0);
    assert!(service.state.store.definition(&project, "evil").is_err());
    // 3. The result reaches a terminal as JSON, the vault's value hidden.
    let runs = agent_runs(
        &workspace,
        &project,
        &folder,
        &json!({"runs": [
            {"args": ["works", "job", id.to_string()]},
            {"args": ["works", "job", "999999"]},
        ]}),
    )
    .await;
    let out = ran(&runs, 0, 0).0;
    assert!(!out.contains(&dsn), "{out}");
    let job: Value = serde_json::from_str(out).unwrap();
    assert_eq!(job["status"], "done_from_agent");
    assert_eq!(job["result"], json!({"answer": 42, "leak": "[REDACTED]"}));
    ran(&runs, 1, 1);

    // 3b. A job's lineage is the terminal's own, from its creation: what the
    // job's agent started, and what that starts, cannot change anything,
    // whatever the terminals are named and through a restart. The user then
    // closes the job's terminal before Connecto's own cleanup: all of them
    // close with it.
    let lineage = format!(
        "lineage CONNECTO-RUN:{}",
        json!({"runs": [{"args": ["add", "claude", "--name", "job-child"]}], "hold": 60})
    );
    let runs = agent_runs(
        &workspace,
        &project,
        &folder,
        &json!({"runs": [{"args": ["works", "test", "review", "--cli", "claude",
                                    "--prompt", lineage]}]}),
    )
    .await;
    let second: i64 = ran(&runs, 0, 0)
        .0
        .split_whitespace()
        .nth(2)
        .unwrap()
        .parse()
        .unwrap();
    let control = workspace.relay(&project).unwrap().terminals();
    let until = Instant::now() + Duration::from_secs(45);
    let (job_terminal, job_runs) = loop {
        let found = std::fs::read_dir(&folder).unwrap().flatten().find_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name
                .strip_prefix("runs-")?
                .strip_suffix(".json")?
                .to_owned();
            let runs: Vec<Value> =
                serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            (runs.first()?["args"] == json!(["add", "claude", "--name", "job-child"]))
                .then_some((id, runs))
        });
        if let Some(found) = found {
            break found;
        }
        assert!(Instant::now() < until, "the second job never ran");
        sleep(Duration::from_millis(100)).await;
    };
    let child = ran(&job_runs, 0, 0)
        .0
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    control.rename(&job_terminal, Some("plain".into())).unwrap();
    let child_runs = rerun(
        &workspace,
        &project,
        &folder,
        &child,
        &json!({"runs": [
            {"args": ["works", "publish", "evil2"], "stdin": def},
            {"args": ["add", "claude", "--name", "grandchild"]},
        ]}),
    )
    .await;
    assert!(
        ran(&child_runs, 0, 1).1.contains("or was started by one"),
        "{}",
        child_runs[0]
    );
    let grandchild = ran(&child_runs, 1, 0)
        .0
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let grandchild_runs = rerun(
        &workspace,
        &project,
        &folder,
        &grandchild,
        &json!({"runs": [
            {"args": ["works", "publish", "evil3"], "stdin": def},
            {"args": ["works", "show"]},
        ]}),
    )
    .await;
    assert!(
        ran(&grandchild_runs, 0, 1)
            .1
            .contains("or was started by one"),
        "{}",
        grandchild_runs[0]
    );
    ran(&grandchild_runs, 1, 0);
    for kind in ["evil2", "evil3"] {
        assert!(service.state.store.definition(&project, kind).is_err());
    }
    // Kept in the saved list, for the next launch.
    let list = layout
        .data_root
        .join("projects")
        .join(&project)
        .join("terminals.json");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(list).unwrap()).unwrap();
    for id in [&job_terminal, &child, &grandchild] {
        let entry = saved["terminals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id.as_str())
            .unwrap_or_else(|| panic!("{id} not saved: {saved}"));
        assert_eq!(entry["job"], true, "{entry}");
    }
    control.close(&job_terminal).unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    while control
        .view()
        .terminals
        .iter()
        .any(|t| [&job_terminal, &child, &grandchild].contains(&&t.id))
    {
        assert!(Instant::now() < until, "the job's terminals did not close");
        sleep(Duration::from_millis(100)).await;
    }
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let status: String = db
            .query_one(
                "SELECT status FROM connecto_works.connecto_jobs WHERE id=$1",
                &[&second],
            )
            .await
            .unwrap()
            .get(0);
        if status == "failed" {
            break;
        }
        assert!(Instant::now() < until, "{status}");
        sleep(Duration::from_millis(100)).await;
    }

    // 4. An agent started in Prompt mode only reads: it is refused what runs
    // commands or starts agents, and its definition goes to the user.
    std::fs::write(folder.join("key.json"), "{}").unwrap();
    let modes = connecto_session::launch::launch_modes();
    connecto_session::launch::set_launch_modes(
        LaunchModes::default().with(ToolKind::Claude, AgentMode::Prompt),
    );
    let reply: Value = serde_json::from_str(def).unwrap();
    let runs = agent_runs(
        &workspace,
        &project,
        &folder,
        &json!({"runs": [
        {"args": ["works", "link", "--secret", "WORKS_DB"]},
        {"args": ["works", "publish", "review2"], "stdin": def},
        {"args": ["works", "test", "review", "--prompt", "x"]},
        {"args": ["works", "setup", "review"]},
        {"args": ["works", "show"]},
        {"args": ["works", "test-db"]},
        {"args": ["works", "key-import", "key.json"]},
    ], "reply": reply}),
    )
    .await;
    connecto_session::launch::set_launch_modes(modes);
    for at in 0..4 {
        assert!(
            ran(&runs, at, 1).1.contains("started in prompt mode"),
            "{}",
            runs[at]
        );
    }
    assert!(ran(&runs, 1, 1).1.contains("Import author reply"));
    ran(&runs, 4, 0);
    assert!(ran(&runs, 5, 0).0.contains("Queue schema found"));
    assert!(ran(&runs, 6, 1).1.contains("Not imported"), "{}", runs[6]);
    assert!(service.state.store.definition(&project, "review2").is_err());
    let until = Instant::now() + Duration::from_secs(20);
    let draft = loop {
        if let Ok(draft) = service.draft(&project, "review2") {
            break draft;
        }
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(draft.instructions, "Review {{prompt}}");

    // 5. Cloud SQL: Connecto asks for the password of one user on one
    // instance; no other secret stands in for it, and it is never a
    // connection string. Each is refused before any connection is opened.
    let target = [
        "--cloudsql",
        "p:europe-west2:i",
        "--database",
        "app",
        "--adc",
    ];
    let with = |user: &str| {
        let mut args = vec!["works", "test-db"];
        args.extend(target);
        args.extend(["--user", user]);
        json!({"args": args})
    };
    let mut borrowed = with("works");
    borrowed["args"]
        .as_array_mut()
        .unwrap()
        .extend([json!("--password-secret"), json!("WORKS_DB")]);
    let runs = agent_runs(
        &workspace,
        &project,
        &folder,
        &json!({"runs": [
            with("works"), with("works"), borrowed,
        ]}),
    )
    .await;
    assert!(
        ran(&runs, 0, 1)
            .1
            .contains("Asked the user for the password of works on p:europe-west2:i")
    );
    assert!(ran(&runs, 1, 1).1.contains("already asked"));
    assert!(
        ran(&runs, 2, 1)
            .1
            .contains("unknown option \"--password-secret\"")
    );
    let asked = answer("cloudsql-pw-", "cloud-sql-password-123");
    assert!(asked.reason.contains("works") && asked.reason.contains("p:europe-west2:i"));
    let mut other = with("works");
    other["args"][3] = json!("q:europe-west2:j");
    let runs = agent_runs(
        &workspace,
        &project,
        &folder,
        &json!({"runs": [
            {"args": ["works", "link", "--secret", asked.name]},
            {"args": ["works", "test-db", "--secret", asked.name]},
            with("someone"),
            other,
        ]}),
    )
    .await;
    assert!(
        ran(&runs, 0, 1)
            .1
            .contains("is a Cloud SQL password, not a connection string")
    );
    assert!(ran(&runs, 1, 1).1.contains("is a Cloud SQL password"));
    assert!(
        ran(&runs, 2, 1)
            .1
            .contains("Asked the user for the password of someone")
    );
    assert!(ran(&runs, 3, 1).1.contains("on q:europe-west2:j"));
    for run in &runs {
        assert!(!run.to_string().contains("cloud-sql-password-123"), "{run}");
    }

    // Each change is audited with the terminal that made it.
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let log = std::fs::read_to_string(root.join("works-audit.jsonl")).unwrap_or_default();
        if log.contains("agentic_works.cli.publish") && log.contains("\"actor\":\"claude-") {
            assert!(!log.contains(&dsn));
            break;
        }
        assert!(Instant::now() < until, "{log}");
        sleep(Duration::from_millis(100)).await;
    }
}

#[test]
fn commands_accept_only_reviewed_argv_inside_the_project() {
    let ok = commands::parse(r#"{"argv":["tool/setup.sh","--x"],"timeout_s":600}"#).unwrap();
    assert_eq!(ok.argv, ["tool/setup.sh", "--x"]);
    assert_eq!(ok.timeout, Duration::from_secs(600));
    assert_eq!(
        commands::parse(r#"{"argv":["ffmpeg"]}"#).unwrap().timeout,
        Duration::from_secs(600)
    );
    assert!(commands::parse(r#"{"argv":["./x.sh"],"timeout_s":1800}"#).is_ok());
    for bad in [
        r#"["x"]"#,
        r#"{"argv":[]}"#,
        r#"{"argv":"x"}"#,
        r#"{"argv":[1]}"#,
        r#"{"argv":["x",2]}"#,
        r#"{"argv":["/bin/sh"]}"#,
        r#"{"argv":["../x.sh"]}"#,
        r#"{"argv":["tool/../../x.sh"]}"#,
        r#"{"argv":["."]}"#,
        r#"{"argv":[" x"]}"#,
        r#"{"argv":["x\u0000"]}"#,
        r#"{"argv":["x","a\nb"]}"#,
        r#"{"argv":["x","\u0003"]}"#,
        r#"{"argv":["x"],"timeout_s":0}"#,
        r#"{"argv":["x"],"timeout_s":1801}"#,
        r#"{"argv":["x"],"timeout_s":"60"}"#,
        r#"{"argv":["x"],"env":{"A":"B"}}"#,
    ] {
        assert!(commands::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn prepared_placeholder_matches_the_prepare_command_and_fills_in_one_pass() {
    let def = |instructions: &str, prepare: Option<&str>| store::Definition {
        instructions: instructions.into(),
        schema: "true".into(),
        setup: None,
        prepare: prepare.map(str::to_owned),
    };
    let prepare = Some(r#"{"argv":["p.py"]}"#);
    assert!(check(&def("{{prompt}} {{prepared}}", prepare)).is_ok());
    assert!(check(&def("{{prompt}}", prepare)).is_err());
    assert!(check(&def("{{prompt}} {{prepared}}", None)).is_err());
    assert!(check(&def("{{prompt}}", None)).is_ok());
    assert!(check(&def("{{prompt}} {{prepared}}", Some(r#"{"argv":["/x"]}"#))).is_err());
    assert_eq!(
        fill(
            "A {{prompt}} B {{prepared}} C",
            "{{prepared}}",
            "{\"x\":\"{{prompt}}\"}"
        ),
        "A {{prepared}} B {\"x\":\"{{prompt}}\"} C"
    );
}

#[test]
fn old_types_rows_without_commands_still_load() {
    let root = tempfile::tempdir().unwrap();
    {
        let db = rusqlite::Connection::open(root.path().join("works.sqlite3")).unwrap();
        db.execute_batch(
            "CREATE TABLE types(project TEXT,kind TEXT,instructions TEXT NOT NULL,schema TEXT NOT NULL,PRIMARY KEY(project,kind));
             INSERT INTO types VALUES ('p','review','Review {{prompt}}','true');",
        )
        .unwrap();
    }
    let store = store::Store::open(root.path()).unwrap();
    let def = store.definition("p", "review").unwrap();
    assert_eq!((def.setup.as_deref(), def.prepare.as_deref()), (None, None));
    let checked = check(&def).unwrap();
    assert!(checked.setup.is_none() && checked.prepare.is_none());
    assert_eq!(
        store.kinds("p").unwrap(),
        [connecto_core::agentic_works::WorkType {
            name: "review".into(),
            setup: false,
            setup_ok_at: None,
            databases: None,
        }]
    );
    // Reopening an upgraded store does not add the columns twice.
    drop(store);
    let store = store::Store::open(root.path()).unwrap();
    store.setup_ok("p", "review", Some("now")).unwrap();
    assert_eq!(
        store.kinds("p").unwrap()[0].setup_ok_at.as_deref(),
        Some("now")
    );
}

#[cfg(unix)]
mod commands_run {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    struct Project {
        _temp: tempfile::TempDir,
        root: PathBuf,
        audit_path: PathBuf,
        audit: connecto_audit::Audit,
        store: store::Store,
    }
    impl Project {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("project");
            fs::create_dir_all(root.join("tool")).unwrap();
            let audit_path = temp.path().join("audit.jsonl");
            let audit = connecto_audit::Audit::open(&audit_path).unwrap();
            let store = store::Store::open(temp.path()).unwrap();
            store
                .publish(
                    "p",
                    "video",
                    &store::Definition {
                        instructions: "{{prompt}} {{prepared}}".into(),
                        schema: "true".into(),
                        setup: Some("{}".into()),
                        prepare: Some("{}".into()),
                    },
                    None,
                    None,
                )
                .unwrap();
            Self {
                _temp: temp,
                root,
                audit_path,
                audit,
                store,
            }
        }
        fn script(&self, name: &str, body: &str) -> commands::Command {
            let path = self.root.join("tool").join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            commands::parse(&format!(r#"{{"argv":["tool/{name}"],"timeout_s":10}}"#)).unwrap()
        }
        fn runner(&self) -> commands::Runner<'_> {
            commands::Runner {
                audit: &self.audit,
                store: &self.store,
                project: "p",
                kind: "video",
                root: &self.root,
            }
        }
        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.root.join(name)).unwrap_or_default()
        }
    }
    fn job(prompt: &str) -> db::Job {
        db::Job {
            id: 42,
            token: "0123456789abcdef-token".into(),
            kind: "video".into(),
            prompt: prompt.into(),
            cli: "claude".into(),
            model: None,
            effort: None,
        }
    }
    fn never() -> bool {
        false
    }
    type NoSetup = fn() -> std::future::Ready<Result<()>>;
    async fn hidden(p: &Project, setup: &commands::Command) -> Result<()> {
        p.runner()
            .setup(setup, Duration::from_secs(10), &never)
            .await
    }

    #[tokio::test]
    async fn the_setup_line_runs_the_argv_word_for_word_and_reports_its_exit() {
        let p = Project::new();
        p.script(
            "args.sh",
            r#"printf '%s\n' "$PWD" "$CONNECTO_WORK_TYPE" "$@" > args.txt; exit 7"#,
        );
        let tricky = ["it's", "$(touch pwned)", "a b", "`id`", "\\", "*"];
        let mut argv = vec!["tool/args.sh".to_owned()];
        argv.extend(tricky.iter().map(|s| (*s).to_owned()));
        let cmd = commands::Command {
            argv,
            timeout: Duration::from_secs(5),
        };
        let private = tempfile::tempdir().unwrap();
        let (script, typed) =
            commands::setup_script(private.path(), &p.root, "vi'deo", &cmd).unwrap();
        assert_eq!(typed, format!("exec /bin/sh '{}'", script.display()));
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Typed into a login shell, as the setup terminal does.
        let status = Command::new("/bin/sh")
            .args(["-c", &typed])
            .current_dir("/")
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(7));
        let mut expected = vec![p.root.display().to_string(), "vi'deo".into()];
        expected.extend(tricky.iter().map(|s| (*s).to_owned()));
        assert_eq!(p.read("args.txt").lines().collect::<Vec<_>>(), expected);
        assert!(!p.root.join("pwned").exists() && !Path::new("/pwned").exists());
        let missing = commands::parse(r#"{"argv":["tool/none.sh"]}"#).unwrap();
        let line = commands::shell_line(&p.root, "video", &missing).unwrap();
        let status = Command::new("/bin/sh")
            .args(["-c", &line])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(127));
        assert!(commands::shell_line(&p.root, "a\nb", &missing).is_err());
    }
    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[tokio::test]
    async fn prepare_gets_stdin_bytes_job_env_and_returns_its_json() {
        let p = Project::new();
        let prompt = "{\"video\":\"a b\"}; $(rm -rf /) `id` | & > 'q' \"d\" \\ \n\u{e9}";
        let cmd = p.script(
            "prepare.sh",
            r#"cat > "$CONNECTO_JOB_DIR/stdin"
env | cut -d= -f1 | sort > env.txt
printf '%s %s %s\n' "$CONNECTO_JOB_ID" "$CONNECTO_JOB_DIR" "$CONNECTO_WORK_TYPE" > ids.txt
cp "$CONNECTO_JOB_DIR/stdin" stdin.txt
printf '{"pieces": [ {"file": "%s/0/segment.mp4"} ]}\n' "$CONNECTO_JOB_DIR""#,
        );
        let dir = commands::job_dir(&p.root, 42, &job("").token).unwrap();
        assert_eq!(dir, p.root.join(".connecto/works/42-0123456789ab"));
        let out = p
            .runner()
            .prepare(&cmd, None::<NoSetup>, &job(prompt), &dir, later(), &never)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            json!({"pieces":[{"file": format!("{}/0/segment.mp4", dir.display())}]})
        );
        assert_eq!(p.read("stdin.txt"), prompt);
        assert_eq!(
            p.read("ids.txt").trim(),
            format!("42 {} video", dir.display())
        );
        for name in p.read("env.txt").lines() {
            assert!(
                [
                    "PATH", "HOME", "LANG", "TMPDIR", "PWD", "SHLVL", "_", "OLDPWD"
                ]
                .contains(&name)
                    || name.starts_with("CONNECTO_"),
                "{name} leaked into the command's environment"
            );
        }
        assert_eq!(p.read(".connecto/works/.gitignore"), "*\n");
        commands::remove_job(&p.root, 42, &job("").token);
        assert!(!dir.exists());
        p.audit.flush(Duration::from_secs(5));
        let journal = fs::read_to_string(&p.audit_path).unwrap();
        assert!(journal.contains("agentic_works.prepare") && journal.contains("exit 0"));
        assert!(
            !journal.contains("segment.mp4"),
            "stdout must not be journalled"
        );
    }

    #[tokio::test]
    async fn prepare_failures_report_the_sanitized_stderr_tail() {
        let p = Project::new();
        let dir = commands::job_dir(&p.root, 42, &job("").token).unwrap();
        let run = |cmd: commands::Command| {
            let p = &p;
            let dir = dir.clone();
            async move {
                p.runner()
                    .prepare(&cmd, None::<NoSetup>, &job("x"), &dir, later(), &never)
                    .await
            }
        };
        let e = run(p.script(
            "fail.sh",
            "i=0; while [ $i -lt 40 ]; do echo line$i >&2; i=$((i+1)); done; printf 'bad\\033[31m\\n' >&2; exit 3",
        ))
        .await
        .unwrap_err();
        assert!(e.starts_with("Prepare exited with code 3."), "{e}");
        assert!(
            e.contains("line39") && !e.contains("line19\n") && !e.contains('\x1b'),
            "{e}"
        );
        let e = run(p.script("noise.sh", "echo not json"))
            .await
            .unwrap_err();
        assert!(e.contains("Prepare output"), "{e}");
        let e = run(p.script("big.sh", "head -c 300000 /dev/zero | tr '\\0' 'a'"))
            .await
            .unwrap_err();
        assert!(e.contains("more than 256 KiB"), "{e}");
        let slow = commands::parse(r#"{"argv":["tool/slow.sh"],"timeout_s":1}"#).unwrap();
        p.script("slow.sh", "sleep 30");
        let started = Instant::now();
        let e = run(slow).await.unwrap_err();
        assert!(
            e.contains("timed out") && started.elapsed() < Duration::from_secs(5),
            "{e}"
        );
        let missing = commands::parse(r#"{"argv":["tool/missing.sh"]}"#).unwrap();
        assert!(run(missing).await.unwrap_err().contains("Cannot start"));
    }

    #[tokio::test]
    async fn prepare_exit_75_reruns_setup_once_then_retries_once() {
        let p = Project::new();
        let setup = p.script("setup.sh", "echo run >> setup.count; touch ready");
        let prepare = p.script(
            "prepare.sh",
            "echo run >> prepare.count; [ -f ready ] || exit 75; echo '{\"ok\":true}'",
        );
        let dir = commands::job_dir(&p.root, 42, &job("").token).unwrap();
        let out = p
            .runner()
            .prepare(
                &prepare,
                Some(|| hidden(&p, &setup)),
                &job("x"),
                &dir,
                later(),
                &never,
            )
            .await
            .unwrap();
        assert_eq!(out, r#"{"ok":true}"#);
        assert_eq!(p.read("setup.count").lines().count(), 1);
        assert_eq!(p.read("prepare.count").lines().count(), 2);
        assert!(p.store.kinds("p").unwrap()[0].setup_ok_at.is_some());

        // Still missing after setup: one retry only, then the job fails.
        let always = p.script("always.sh", "echo run >> always.count; exit 75");
        let e = p
            .runner()
            .prepare(
                &always,
                Some(|| hidden(&p, &setup)),
                &job("x"),
                &dir,
                later(),
                &never,
            )
            .await
            .unwrap_err();
        assert!(e.contains("code 75"), "{e}");
        assert_eq!(p.read("always.count").lines().count(), 2);
        assert_eq!(p.read("setup.count").lines().count(), 2);

        // Setup itself fails: the job fails with setup's reason, and the
        // type no longer shows a successful setup.
        let broken = p.script("broken.sh", "echo ffmpeg is missing >&2; exit 1");
        let e = p
            .runner()
            .prepare(
                &always,
                Some(|| hidden(&p, &broken)),
                &job("x"),
                &dir,
                later(),
                &never,
            )
            .await
            .unwrap_err();
        assert!(
            e.contains("missing prerequisites") && e.contains("ffmpeg is missing"),
            "{e}"
        );
        assert!(p.store.kinds("p").unwrap()[0].setup_ok_at.is_none());
    }

    #[tokio::test]
    async fn cancel_kills_the_whole_process_group() {
        let p = Project::new();
        let cmd = p.script("hang.sh", "sleep 100 & echo $! > child.pid; sleep 100");
        let dir = commands::job_dir(&p.root, 42, &job("").token).unwrap();
        let start = Instant::now();
        let cancel = move || start.elapsed() > Duration::from_millis(500);
        let e = p
            .runner()
            .prepare(&cmd, None::<NoSetup>, &job("x"), &dir, later(), &cancel)
            .await
            .unwrap_err();
        assert!(e.contains("cancelled"), "{e}");
        assert!(start.elapsed() < Duration::from_secs(5));
        let pid: i32 = p.read("child.pid").trim().parse().unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        // Dead, or a zombie waiting for its new parent to reap it.
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
            && !Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().starts_with('Z'))
                .unwrap_or(false)
        {
            assert!(Instant::now() < until, "background child {pid} survived");
            sleep(Duration::from_millis(50)).await;
        }
        commands::remove_job(&p.root, 42, &job("").token);
        assert!(!dir.exists());
    }

    #[test]
    fn job_folders_refuse_links_and_sweep_only_job_names() {
        let p = Project::new();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), "x").unwrap();
        fs::create_dir_all(p.root.join(".connecto")).unwrap();
        std::os::unix::fs::symlink(outside.path(), p.root.join(".connecto/works")).unwrap();
        assert!(commands::job_dir(&p.root, 1, "0123456789abcdef").is_err());
        commands::sweep(&p.root);
        assert!(outside.path().join("keep").exists());
        fs::remove_file(p.root.join(".connecto/works")).unwrap();

        let a = commands::job_dir(&p.root, 1, "0123456789abcdef").unwrap();
        let b = commands::job_dir(&p.root, 2, "fedcba987654-x").unwrap();
        fs::write(a.join("piece"), "x").unwrap();
        let other = p.root.join(".connecto/works/notes");
        fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            p.root.join(".connecto/works/3-0123456789ab"),
        )
        .unwrap();
        commands::sweep(&p.root);
        assert!(!a.exists() && !b.exists());
        assert!(other.exists());
        assert!(fs::symlink_metadata(p.root.join(".connecto/works/3-0123456789ab")).is_err());
        assert!(outside.path().join("keep").exists());
    }
}

#[test]
fn links_before_cloud_sql_still_load_and_cloud_sql_links_round_trip() {
    use connecto_core::agentic_works::{CloudSql, Database, GoogleCredential, IpType};
    let dir = tempfile::tempdir().unwrap();
    {
        // A store from before the `target` column.
        let db = rusqlite::Connection::open(dir.path().join("works.sqlite3")).unwrap();
        db.execute_batch(
            "CREATE TABLE links(project TEXT PRIMARY KEY, credential TEXT NOT NULL, queue TEXT NOT NULL);
            INSERT INTO links VALUES ('old','QUEUE','q1');",
        )
        .unwrap();
    }
    let store = store::Store::open(dir.path()).unwrap();
    assert_eq!(
        store.linked("old", "default").unwrap(),
        Some((Database::ConnectionString("QUEUE".into()), "q1".into()))
    );
    let sql = Database::CloudSql(CloudSql {
        connection_name: "p:r:i".into(),
        database: "app".into(),
        user: "connecto_works".into(),
        ip_type: IpType::Psc,
        iam_auth: false,
        credential: GoogleCredential::Key("gcp-key-sa".into()),
        password_secret: "cloudsql-password-connecto_works".into(),
    });
    store.link("new", "default", &sql, "q2").unwrap();
    assert_eq!(
        store.linked("new", "default").unwrap(),
        Some((sql.clone(), "q2".into()))
    );
    // Relinking the same queue may change the database; another queue may not.
    let adc = Database::CloudSql(CloudSql {
        credential: GoogleCredential::Adc,
        iam_auth: true,
        password_secret: String::new(),
        ..match sql {
            Database::CloudSql(s) => s,
            Database::ConnectionString(_) => unreachable!(),
        }
    });
    store.link("new", "default", &adc, "q2").unwrap();
    assert_eq!(
        store.linked("new", "default").unwrap(),
        Some((adc.clone(), "q2".into()))
    );
    assert!(store.link("new", "default", &adc, "other").is_err());
    store
        .link(
            "new",
            "default",
            &Database::ConnectionString("DSN".into()),
            "q2",
        )
        .unwrap();
    assert_eq!(
        store.linked("new", "default").unwrap(),
        Some((Database::ConnectionString("DSN".into()), "q2".into()))
    );
    let raw = store::cloudsql_json(match &adc {
        Database::CloudSql(s) => s,
        Database::ConnectionString(_) => unreachable!(),
    });
    assert!(raw.contains(r#""credential":"adc""#) && raw.contains(r#""kind":"cloudsql""#));
}

/// A store from before named databases: its one link and its one outcome
/// still to write back become the database `default`, and a Cloud SQL link
/// keeps its target. Reopening changes nothing more.
#[test]
fn a_single_database_store_becomes_the_default_database() {
    use connecto_core::agentic_works::Database;
    let dir = tempfile::tempdir().unwrap();
    {
        let db = rusqlite::Connection::open(dir.path().join("works.sqlite3")).unwrap();
        db.execute_batch(
            r#"CREATE TABLE links(project TEXT PRIMARY KEY, credential TEXT NOT NULL, queue TEXT NOT NULL, target TEXT);
            INSERT INTO links VALUES ('plain','QUEUE','q1',NULL);
            INSERT INTO links VALUES ('sql','cloudsql-password-x','q2','{"kind":"cloudsql","connection_name":"p:r:i","database":"app","user":"u","ip_type":"PUBLIC","iam_auth":false,"credential":"adc","password_secret":"cloudsql-password-x"}');
            CREATE TABLE outcomes(project TEXT PRIMARY KEY,id INTEGER NOT NULL,token TEXT NOT NULL,name TEXT NOT NULL,status TEXT NOT NULL,value TEXT,reason TEXT,terminal TEXT);
            INSERT INTO outcomes VALUES ('plain',7,'tok','agentic_works_7_tok','done_from_agent','{"a":1}',NULL,'claude-3');"#,
        )
        .unwrap();
    }
    for _ in 0..2 {
        let store = store::Store::open(dir.path()).unwrap();
        let links = store.links("plain").unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(
            (
                links[0].name.as_str(),
                &links[0].database,
                links[0].queue.as_str()
            ),
            ("default", &Database::ConnectionString("QUEUE".into()), "q1")
        );
        let sql = store.links("sql").unwrap();
        assert!(
            matches!(&sql[..], [l] if l.name == "default"
                && matches!(&l.database, Database::CloudSql(c) if c.connection_name == "p:r:i" && c.password_secret == "cloudsql-password-x")),
            "{sql:?}"
        );
        assert!(store.password_in_use("sql", "cloudsql-password-x").unwrap());
        let pending = store.pending("plain", "default").unwrap().remove(0);
        assert_eq!(
            (pending.id, pending.token.as_str(), pending.value),
            (7, "tok", Some(json!({"a": 1})))
        );
        assert_eq!(pending.terminal.as_deref(), Some("claude-3"));
    }
    let tables: Vec<String> = rusqlite::Connection::open(dir.path().join("works.sqlite3"))
        .unwrap()
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('links','outcomes')",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(tables.is_empty(), "{tables:?}");
}

/// Several databases in one project: each its own queue and its own outcome
/// to write back, one queue under one name only, and removing one forgets
/// its outcome and leaves the others.
#[test]
fn a_project_links_several_databases_each_with_its_own_journal() {
    use connecto_core::agentic_works::Database;
    let dir = tempfile::tempdir().unwrap();
    let store = store::Store::open(dir.path()).unwrap();
    let dsn = |n: &str| Database::ConnectionString(n.into());
    store.link("p", "dev", &dsn("DEV"), "q-dev").unwrap();
    store
        .link("p", "production", &dsn("PROD"), "q-prod")
        .unwrap();
    store.link("other", "dev", &dsn("X"), "q-dev").unwrap();
    let why = store
        .link("p", "staging", &dsn("DEV"), "q-dev")
        .unwrap_err();
    assert!(
        why.contains("already linked") && why.contains("dev"),
        "{why}"
    );
    assert!(store.link("p", "dev", &dsn("DEV"), "q-prod").is_err());
    assert_eq!(
        store
            .links("p")
            .unwrap()
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        ["dev", "production"]
    );
    let job = |id| db::Job {
        id,
        token: format!("token-{id}"),
        kind: "review".into(),
        prompt: String::new(),
        cli: "claude".into(),
        model: None,
        effort: None,
    };
    // Jobs of each database at once, two of dev: each keeps its own
    // outcome. A job is journalled once.
    store.begin("p", "dev", &job(1), "a").unwrap();
    store.begin("p", "production", &job(3), "b").unwrap();
    store.begin("p", "dev", &job(2), "c").unwrap();
    assert!(store.begin("p", "dev", &job(2), "c").is_err());
    store
        .record("p", "token-1", "done_from_agent", Some(&json!("dev")), None)
        .unwrap();
    store
        .record(
            "p",
            "token-2",
            "done_from_agent",
            Some(&json!("dev 2")),
            None,
        )
        .unwrap();
    store
        .record("p", "token-3", "failed", None, Some("prod"))
        .unwrap();
    let dev = store.pending("p", "dev").unwrap();
    let prod = store.pending("p", "production").unwrap().remove(0);
    assert_eq!(
        dev.iter()
            .map(|p| (p.id, p.value.clone()))
            .collect::<Vec<_>>(),
        [(1, Some(json!("dev"))), (2, Some(json!("dev 2")))]
    );
    assert_eq!((prod.value, prod.reason.as_deref()), (None, Some("prod")));
    // Writing one back clears that one only.
    store.clear("p", "token-3").unwrap();
    store.clear("p", "token-2").unwrap();
    assert!(store.pending("p", "production").unwrap().is_empty());
    assert_eq!(store.pendings("p").unwrap().len(), 1);
    // Types: every database, or the ones named.
    let def = store::Definition {
        instructions: "{{prompt}}".into(),
        schema: "true".into(),
        setup: None,
        prepare: None,
    };
    store.publish("p", "all", &def, None, None).unwrap();
    store
        .publish(
            "p",
            "prod-only",
            &def,
            None,
            Some(&["production".to_owned()]),
        )
        .unwrap();
    assert_eq!(store.databases_of("p", "all").unwrap(), None);
    assert_eq!(
        store.databases_of("p", "prod-only").unwrap(),
        Some(vec!["production".to_owned()])
    );
    assert_eq!(
        store
            .kinds("p")
            .unwrap()
            .iter()
            .map(|k| k.databases.clone())
            .collect::<Vec<_>>(),
        [None, Some(vec!["production".to_owned()])]
    );
    // A definition and its databases come from one row.
    let (published, scope) = store.published("p", "prod-only").unwrap();
    assert_eq!(published, def);
    assert_eq!(scope, Some(vec!["production".to_owned()]));
    assert!(store.published("p", "nothing").is_err());
    // Removing dev drops its outcome, which is no longer written back, but
    // keeps its row (and terminal) until the cycle has closed it.
    assert_eq!(store.unlink("p", "dev").unwrap(), Some(dsn("DEV")));
    assert!(store.pending("p", "dev").unwrap().is_empty());
    assert!(store.dropped("p", "token-1").unwrap());
    assert!(store.dropped("p", "token-unknown").unwrap());
    let kept = store.pendings("p").unwrap();
    assert!(matches!(&kept[..], [k] if k.dropped && k.terminal.is_none() && k.db == "dev"));
    assert_eq!(store.links("p").unwrap().len(), 1);
    assert_eq!(store.links("other").unwrap().len(), 1);
    assert_eq!(store.unlink("p", "dev").unwrap(), None);
    // Linked again, dev takes a new job while the dropped row stays
    // (its job may still be stopping).
    store.link("p", "dev", &dsn("DEV"), "q-dev").unwrap();
    store.begin("p", "dev", &job(5), "e").unwrap();
    assert_eq!(store.pending("p", "dev").unwrap().remove(0).id, 5);
    assert!(store.dropped("p", "token-1").unwrap());
    assert!(!store.dropped("p", "token-5").unwrap());
    store.clear("p", "token-5").unwrap();
    store.unlink("p", "dev").unwrap();
    store.clear_dropped("p").unwrap();
    assert!(store.pendings("p").unwrap().is_empty());
    // An author keeps the databases it asked for.
    store
        .author("p", "review", "claude-1", Some(&["dev".to_owned()]))
        .unwrap();
    assert_eq!(
        store.author_of("p", "review").unwrap(),
        ("claude-1".to_owned(), Some(vec!["dev".to_owned()]))
    );
    store.author("p", "review", "claude-2", None).unwrap();
    assert_eq!(
        store.author_of("p", "review").unwrap(),
        ("claude-2".to_owned(), None)
    );
}

#[test]
fn databases_are_looked_at_in_turn_after_the_last_one_served() {
    let links: Vec<store::Link> = ["a", "b", "c"]
        .iter()
        .map(|n| store::Link {
            name: (*n).into(),
            database: connecto_core::agentic_works::Database::ConnectionString("S".into()),
            queue: (*n).into(),
        })
        .collect();
    let order = |last: Option<&str>| -> Vec<String> {
        rotation(links.clone(), last)
            .into_iter()
            .map(|l| l.name)
            .collect()
    };
    assert_eq!(order(None), ["a", "b", "c"]);
    assert_eq!(order(Some("a")), ["b", "c", "a"]);
    assert_eq!(order(Some("c")), ["a", "b", "c"]);
    assert_eq!(order(Some("gone")), ["a", "b", "c"]);
}

/// The places are shared: each next job comes from the database running
/// the fewest, in turn among equals; one with nothing waiting is passed
/// over. With no job running, it is the next database in turn, as with
/// one job at a time.
#[test]
fn databases_share_the_places_fairly() {
    let links: Vec<store::Link> = ["dev", "staging", "production"]
        .iter()
        .map(|n| store::Link {
            name: (*n).into(),
            database: connecto_core::agentic_works::Database::ConnectionString("S".into()),
            queue: (*n).into(),
        })
        .collect();
    let order = rotation(links, Some("dev"));
    let pick = |running: &[(&str, usize)], done: &[&str]| {
        let running: HashMap<String, usize> =
            running.iter().map(|(n, c)| ((*n).to_owned(), *c)).collect();
        let done: HashSet<String> = done.iter().map(|n| (*n).to_owned()).collect();
        next_database(&order, &running, &done).map(|at| order[at].name.clone())
    };
    assert_eq!(pick(&[], &[]).as_deref(), Some("staging"));
    assert_eq!(pick(&[], &["staging"]).as_deref(), Some("production"));
    // Ten places filled one by one, every database with work waiting:
    // they end up shared 4, 3, 3.
    let mut running: Vec<(&str, usize)> = vec![("dev", 0), ("staging", 0), ("production", 0)];
    for _ in 0..10 {
        let next = pick(&running, &[]).unwrap();
        running.iter_mut().find(|(n, _)| *n == next).unwrap().1 += 1;
    }
    assert_eq!(running, [("dev", 3), ("staging", 4), ("production", 3)]);
    // A database busy from before gets no more until the others catch up.
    assert_eq!(
        pick(&[("dev", 6), ("staging", 1)], &[]).as_deref(),
        Some("production")
    );
    assert_eq!(
        pick(&[("dev", 6), ("staging", 1)], &["production"]).as_deref(),
        Some("staging")
    );
    // Only dev has work: it takes every place.
    assert_eq!(
        pick(&[("dev", 6)], &["staging", "production"]).as_deref(),
        Some("dev")
    );
    assert_eq!(pick(&[], &["dev", "staging", "production"]), None);
}

/// A store from when the journal held one outcome per database keeps it,
/// and then holds one row per job; the number of jobs at once is 1 until
/// set, and kept.
#[test]
fn the_journal_becomes_one_row_per_job_and_parallel_defaults_to_one() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = rusqlite::Connection::open(dir.path().join("works.sqlite3")).unwrap();
        db.execute_batch(
            "CREATE TABLE databases(project TEXT NOT NULL,name TEXT NOT NULL,credential TEXT NOT NULL,queue TEXT NOT NULL,target TEXT,PRIMARY KEY(project,name));
            CREATE TABLE journal(project TEXT NOT NULL,db TEXT NOT NULL,id INTEGER NOT NULL,token TEXT NOT NULL,name TEXT NOT NULL,status TEXT NOT NULL,value TEXT,reason TEXT,terminal TEXT,dropped INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(project,db));
            INSERT INTO databases VALUES ('p','dev','DEV','q-dev',NULL);
            INSERT INTO journal VALUES ('p','dev',7,'tok-7','agentic_works_7','done_from_agent','1',NULL,'claude-3',0);",
        )
        .unwrap();
    }
    for _ in 0..2 {
        let store = store::Store::open(dir.path()).unwrap();
        let kept = store.pending("p", "dev").unwrap();
        assert!(
            matches!(&kept[..], [k] if k.id == 7 && k.token == "tok-7" && k.terminal.as_deref() == Some("claude-3")),
        );
        assert_eq!(store.parallel("p").unwrap(), 1);
    }
    let store = store::Store::open(dir.path()).unwrap();
    let job = |id: i64| db::Job {
        id,
        token: format!("tok-{id}"),
        kind: "review".into(),
        prompt: String::new(),
        cli: "claude".into(),
        model: None,
        effort: None,
    };
    store.begin("p", "dev", &job(8), "agentic_works_8").unwrap();
    assert_eq!(store.pending("p", "dev").unwrap().len(), 2);
    store.set_parallel("p", 10).unwrap();
    assert_eq!(store.parallel("p").unwrap(), 10);
    assert_eq!(store.parallel("other").unwrap(), 1);
    store.set_parallel("p", 1000).unwrap();
    assert_eq!(
        store::Store::open(dir.path())
            .unwrap()
            .parallel("p")
            .unwrap(),
        connecto_core::agentic_works::MAX_PARALLEL
    );
}

#[test]
fn vault_secret_names_are_plain_and_long_ones_stay_apart() {
    assert_eq!(
        secret_name("gcp-key-", "connecto-agentic-works@acme"),
        "gcp-key-connecto-agentic-works-acme"
    );
    let a = secret_name("gcp-key-", &format!("{}@one", "x".repeat(60)));
    let b = secret_name("gcp-key-", &format!("{}@two", "x".repeat(60)));
    assert_ne!(a, b);
    for name in [a, b] {
        assert!(name.len() <= connecto_core::vault::MAX_NAME);
        connecto_core::vault::check_name(&name).unwrap();
    }
}

#[test]
fn key_names_differ_for_every_account() {
    let a = key_secret_name("worker-ops@sample-project.iam.gserviceaccount.com");
    let b = key_secret_name("worker@ops-sample-project.iam.gserviceaccount.com");
    assert_ne!(a, b);
    assert!(a.starts_with("gcp-key-worker-ops-sample-project-"), "{a}");
    let long = key_secret_name(&format!("{}@p.iam.gserviceaccount.com", "x".repeat(80)));
    assert!(long.len() <= connecto_core::vault::MAX_NAME);
    connecto_core::vault::check_name(&long).unwrap();
}
