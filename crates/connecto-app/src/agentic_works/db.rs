use postgres_native_tls::MakeTlsConnector;
use serde_json::Value;
use std::{str::FromStr, time::Duration};
use tokio::time::timeout;
use tokio_postgres::{
    Client, Config, GenericClient,
    config::{Host, SslMode},
};
use uuid::Uuid;

pub(super) type Result<T> = std::result::Result<T, String>;

pub(super) async fn open(dsn: &str) -> Result<Client> {
    let mut config = Config::from_str(dsn).map_err(|_| "Invalid PostgreSQL connection string")?;
    if config.get_hosts().iter().any(|host| match host {
        Host::Tcp(s) => {
            s != "localhost"
                && s.parse::<std::net::IpAddr>()
                    .map_or(true, |ip| !ip.is_loopback())
        }
        #[cfg(unix)]
        Host::Unix(_) => false,
    }) {
        config.ssl_mode(SslMode::Require);
    }
    config.connect_timeout(Duration::from_secs(4));
    let tls = native_tls::TlsConnector::new().map_err(|_| "Cannot initialize PostgreSQL TLS")?;
    let (client, connection) = timeout(Duration::from_secs(5), config.connect(MakeTlsConnector::new(tls)))
        .await.map_err(|_| "PostgreSQL connection timed out")?.map_err(|_| "Cannot connect to PostgreSQL. Check the project credential, TLS and database availability.")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    init(&client).await?;
    Ok(client)
}

/// The session settings every Agentic Works connection runs with.
pub(super) async fn init(client: &Client) -> Result<()> {
    timeout(
        Duration::from_secs(5),
        client.batch_execute(
            "SET search_path = pg_catalog, pg_temp; SET statement_timeout = '4s'; SET lock_timeout = '3s'",
        ),
    )
    .await
    .map_err(|_| "PostgreSQL initialization timed out")?
    .map_err(error)
}

/// Whether the queue's table is there (for Test connection).
pub(super) async fn queue_present(client: &Client) -> Result<bool> {
    Ok(client
        .query_one(
            "SELECT to_regclass('connecto_works.connecto_jobs') IS NOT NULL",
            &[],
        )
        .await
        .map_err(error)?
        .get(0))
}

fn error(err: tokio_postgres::Error) -> String {
    err.code().map_or_else(
        || "PostgreSQL connection interrupted".into(),
        |code| format!("PostgreSQL refused queue operation ({})", code.code()),
    )
}

// Schema and relations are owned by the connecting role. A writer can start an agent.
// Reject even column-level non-owner grants and writable schemas, including PUBLIC.
async fn validate(db: &impl GenericClient, project: &str) -> Result<String> {
    let ok: bool = db.query_one("SELECT n.nspowner = current_user::regrole AND
        NOT EXISTS (SELECT FROM aclexplode(n.nspacl) a WHERE a.grantee <> n.nspowner AND privilege_type='CREATE')
        AND EXISTS (SELECT FROM pg_class seq
            WHERE seq.oid=pg_get_serial_sequence('connecto_works.connecto_jobs','id')::regclass
            AND seq.relnamespace=n.oid AND seq.relowner=n.nspowner AND seq.relkind='S'
            AND NOT EXISTS (SELECT FROM aclexplode(seq.relacl) a
                WHERE a.grantee<>seq.relowner AND privilege_type<>'SELECT'))
        AND (SELECT count(*)=2 FROM pg_class c WHERE c.relnamespace=n.oid AND c.relname IN ('connecto_jobs','project')
        AND c.relkind='r' AND c.relowner=n.nspowner AND NOT c.relrowsecurity AND NOT c.relhassubclass
        AND NOT EXISTS (SELECT FROM aclexplode(c.relacl) a WHERE a.grantee<>c.relowner AND privilege_type <> 'SELECT')
        AND NOT EXISTS (SELECT FROM pg_attribute p CROSS JOIN LATERAL aclexplode(p.attacl) a
            WHERE p.attrelid=c.oid AND a.grantee<>c.relowner AND privilege_type <> 'SELECT'))
        FROM pg_namespace n WHERE n.nspname='connecto_works'", &[]).await.map_err(error)?.get(0);
    if !ok {
        return Err("Queue schema and tables must be private, owned by the connecting role, with writes restricted to the owner.".into());
    }
    let cols = db
        .query(
            "SELECT attname, format_type(atttypid, atttypmod),attnotnull FROM pg_attribute
        WHERE attrelid='connecto_works.connecto_jobs'::regclass AND attnum>0 AND NOT attisdropped",
            &[],
        )
        .await
        .map_err(error)?;
    let cols: std::collections::HashMap<String, (String, bool)> = cols
        .iter()
        .map(|r| (r.get(0), (r.get(1), r.get(2))))
        .collect();
    for (key, ty) in [
        ("id", "bigint"),
        ("created_at", "timestamp with time zone"),
        ("claimed_at", "timestamp with time zone"),
        ("result", "jsonb"),
    ]
    .into_iter()
    .chain(
        [
            "project_name",
            "work_type",
            "prompt",
            "session_cli",
            "model",
            "effort",
            "status",
            "attempt",
            "reason",
        ]
        .map(|s| (s, "text")),
    ) {
        let required = matches!(
            key,
            "id" | "created_at"
                | "project_name"
                | "work_type"
                | "prompt"
                | "session_cli"
                | "status"
        );
        if !cols
            .get(key)
            .is_some_and(|(actual, non_null)| actual == ty && *non_null == required)
        {
            return Err(format!(
                "Queue column {key} requires type {ty} and NOT NULL = {required}."
            ));
        }
    }
    let pk:bool=db.query_one("SELECT EXISTS (SELECT FROM pg_constraint c JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attname='id' WHERE c.conrelid='connecto_works.connecto_jobs'::regclass AND c.contype='p' AND c.conkey=ARRAY[a.attnum])",&[]).await.map_err(error)?.get(0);
    if !pk {
        return Err("Queue requires a primary key on id.".into());
    }
    db.query_one(
        "SELECT queue_id FROM connecto_works.project WHERE singleton AND project_id=$1 AND (SELECT count(*)=1 FROM connecto_works.project)",
        &[&project],
    )
    .await
    .map_err(|_| {
        "Database is linked to a different project or has invalid queue metadata.".to_owned()
    })?
    .try_get(0)
    .map_err(error)
}

pub(super) async fn setup(db: &mut Client, project: &str, create: bool) -> Result<String> {
    let tx = db.transaction().await.map_err(error)?;
    if create {
        tx.batch_execute(include_str!("queue.sql"))
            .await
            .map_err(error)?;
        tx.execute(
            "INSERT INTO connecto_works.project(project_id,queue_id) VALUES($1,$2)",
            &[&project, &Uuid::new_v4().to_string()],
        )
        .await
        .map_err(error)?;
    }
    let id = validate(&tx, project).await?;
    tx.commit().await.map_err(error)?;
    Ok(id)
}

pub(super) struct Job {
    pub id: i64,
    pub token: String,
    pub kind: String,
    pub prompt: String,
    pub cli: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// The index queues were made with, allowing one job doing: exactly as
/// `pg_get_indexdef` prints it.
const ONE_ACTIVE_WORK: &str = "CREATE UNIQUE INDEX one_active_work ON connecto_works.connecto_jobs USING btree (status) WHERE (status = 'doing'::text)";

/// Tests' pauses in a claim of the queue named (by its identity, unique to
/// each test's queue), after its first read and before it takes the lock:
/// each runs once, in the first claim of its queue.
#[cfg(test)]
pub(super) static BEFORE_LOCK: std::sync::Mutex<Vec<Pause>> = std::sync::Mutex::new(Vec::new());

/// A test's pause: the name it is for, and what it runs there.
#[cfg(test)]
pub(super) type Pause = (String, Box<dyn FnOnce() + Send>);

/// Whether the index of queues made before several jobs at once is on the
/// queue's table, exactly as it was made: an index of that name the owner
/// made otherwise is left alone.
const LEGACY_PRESENT: &str =
    "SELECT EXISTS (SELECT FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid
    JOIN pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname='connecto_works' AND c.relname='one_active_work'
    AND i.indrelid='connecto_works.connecto_jobs'::regclass
    AND pg_get_indexdef(i.indexrelid)=$1)";

/// Whether `doing_work` is there, and valid (a build that was interrupted
/// leaves it invalid). None when it is not there.
const DOING_WORK_VALID: &str = "SELECT i.indisvalid FROM pg_index i
    WHERE i.indexrelid=to_regclass('connecto_works.doing_work')
    AND i.indrelid='connecto_works.connecto_jobs'::regclass";

/// How long building the index of jobs doing may take: once per queue.
pub(super) const MIGRATION_LIMIT: Duration = Duration::from_secs(5 * 60);

/// Let a queue made before several jobs at once have several jobs doing:
/// its index allowing one (`one_active_work`) is replaced by `doing_work`.
/// The new index is built first, concurrently and outside any transaction
/// (producers and claims go on meanwhile), within [`MIGRATION_LIMIT`]; only
/// once it is valid is the old one dropped, in a short transaction (with a
/// claim's limits: when it cannot have its lock in time, the next look tries
/// again, the new index already there). Nothing
/// to do for a queue made since, or already changed.
///
/// One Connecto at a time changes a queue (several Macs may share one): the
/// whole change holds a session lock of its own, and a Connecto that
/// cannot have it leaves the queue for this look. Ok holds whether the
/// queue allows several jobs now.
pub(super) async fn allow_several(
    db: &mut Client,
    project: &str,
    queue: &str,
    keep: &(dyn Fn() -> bool + Send + Sync),
) -> Result<bool> {
    let mine: bool = db
        .query_one("SELECT pg_try_advisory_lock(903410245042::bigint)", &[])
        .await
        .map_err(error)?
        .get(0);
    if !mine {
        return Ok(false);
    }
    let changed = change_for_several(db, project, queue, keep).await;
    // A lost connection releases it as well.
    let _ = db
        .query_one("SELECT pg_advisory_unlock(903410245042::bigint)", &[])
        .await;
    changed.map(|()| true)
}

/// Whether no session holds the change's lock any more (none is changing
/// the queue): checked on a fresh connection, to that same queue.
pub(super) async fn change_over(db: &mut Client, project: &str, queue: &str) -> Result<bool> {
    let tx = db.transaction().await.map_err(error)?;
    if validate(&tx, project).await? != queue {
        return Err("The linked queue identity changed.".into());
    }
    tx.commit().await.map_err(error)?;
    let free: bool = db
        .query_one("SELECT pg_try_advisory_lock(903410245042::bigint)", &[])
        .await
        .map_err(error)?
        .get(0);
    if free {
        db.query_one("SELECT pg_advisory_unlock(903410245042::bigint)", &[])
            .await
            .map_err(error)?;
    }
    Ok(free)
}

async fn change_for_several(
    db: &mut Client,
    project: &str,
    queue: &str,
    keep: &(dyn Fn() -> bool + Send + Sync),
) -> Result<()> {
    let tx = db.transaction().await.map_err(error)?;
    if validate(&tx, project).await? != queue {
        return Err("The linked queue identity changed.".into());
    }
    let legacy: bool = tx
        .query_one(LEGACY_PRESENT, &[&ONE_ACTIVE_WORK])
        .await
        .map_err(error)?
        .get(0);
    let valid: Option<bool> = tx
        .query_opt(DOING_WORK_VALID, &[])
        .await
        .map_err(error)?
        .map(|r| r.get(0));
    tx.commit().await.map_err(error)?;
    if !legacy {
        return Ok(());
    }
    if valid != Some(true) {
        // The build waits for every transaction writing to the queue: its
        // own limits, not a claim's.
        let secs = MIGRATION_LIMIT.as_secs();
        let limit = format!("SET statement_timeout = '{secs}s'; SET lock_timeout = '{secs}s'");
        db.batch_execute(&limit).await.map_err(error)?;
        // When Connecto lets go of the connection (closing), the server
        // stops the build within a second (PostgreSQL 14 and later).
        let _ = db
            .batch_execute("SET client_connection_check_interval = '1s'")
            .await;
        let built = async {
            if valid == Some(false) {
                db.batch_execute("DROP INDEX CONCURRENTLY IF EXISTS connecto_works.doing_work")
                    .await?;
            }
            db.batch_execute(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS doing_work ON connecto_works.connecto_jobs (claimed_at) WHERE status = 'doing'",
            )
            .await
        }
        .await;
        // Back to the limits every statement of the queue has.
        let restored = db
            .batch_execute("SET statement_timeout = '4s'; SET lock_timeout = '3s'")
            .await;
        built.map_err(error)?;
        restored.map_err(error)?;
    }
    let tx = db.transaction().await.map_err(error)?;
    let valid: Option<bool> = tx
        .query_opt(DOING_WORK_VALID, &[])
        .await
        .map_err(error)?
        .map(|r| r.get(0));
    if valid != Some(true) {
        return Err("The index of jobs doing could not be built.".into());
    }
    let legacy: bool = tx
        .query_one(LEGACY_PRESENT, &[&ONE_ACTIVE_WORK])
        .await
        .map_err(error)?
        .get(0);
    if legacy {
        // Asked last, just before: the database may have been removed from
        // the project while the new index was built.
        if !keep() {
            return Err("The change was called off.".into());
        }
        tx.batch_execute("DROP INDEX connecto_works.one_active_work")
            .await
            .map_err(error)?;
    }
    tx.commit().await.map_err(error)
}

/// Take the newest waiting job of `name`, unless `still` says no once it is
/// chosen (it is then left waiting) or the queue already has
/// `cap` jobs doing (whoever took them). With a cap of 1 this is one job at
/// a time, as queues were made; a larger cap needs [`allow_several`] first
/// on a queue made before.
///
/// The cap counts every job doing in this queue: two Connectos sharing it
/// (two Macs) each claim up to their own cap of that one count, so the one
/// with the lower setting takes nothing while the other fills its places.
pub(super) async fn claim(
    db: &mut Client,
    project: &str,
    queue: &str,
    name: &str,
    cap: u32,
    still: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Option<Job>> {
    let cap = i64::from(cap.max(1));
    // READ COMMITTED whatever the role's default: each statement after the
    // lock sees the claims committed before it, the count included.
    let tx = db
        .build_transaction()
        .isolation_level(tokio_postgres::IsolationLevel::ReadCommitted)
        .start()
        .await
        .map_err(error)?;
    if validate(&tx, project).await? != queue {
        return Err("The linked queue identity changed.".into());
    }
    #[cfg(test)]
    {
        let pause = {
            let mut hooks = BEFORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            hooks
                .iter()
                .position(|(q, _)| q == queue)
                .map(|at| hooks.remove(at).1)
        };
        if let Some(pause) = pause {
            pause();
        }
    }
    // Separate statements under READ COMMITTED: the winner sees the previous
    // claim after acquiring the lock. A crash releases the transaction lock.
    let locked: bool = tx
        .query_one(
            "SELECT pg_try_advisory_xact_lock(903410245041::bigint)",
            &[],
        )
        .await
        .map_err(error)?
        .get(0);
    if !locked {
        return Ok(None);
    }
    tx.execute("UPDATE connecto_works.connecto_jobs SET status='timeout',reason='Claim expired after 30 minutes' WHERE status='doing' AND claimed_at<=clock_timestamp()-interval '30 minutes'",&[]).await.map_err(error)?;
    let token = Uuid::new_v4().to_string();
    // The advisory lock serializes claims; SKIP LOCKED passes over a row a
    // producer is still changing.
    let row=tx.query_opt("UPDATE connecto_works.connecto_jobs SET status='doing',attempt=$2,claimed_at=clock_timestamp(),result=NULL,reason=NULL
        WHERE id=(SELECT id FROM connecto_works.connecto_jobs WHERE project_name=$1 AND status='to_do'
        AND (SELECT count(*) FROM connecto_works.connecto_jobs WHERE status='doing')<$3
        ORDER BY created_at DESC,id DESC LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id,work_type,prompt,session_cli,model,effort",&[&name,&token,&cap]).await.map_err(error)?;
    let job = row
        .map(|r| -> Result<Job> {
            Ok(Job {
                id: r.try_get(0).map_err(error)?,
                token,
                kind: r.try_get(1).map_err(error)?,
                prompt: r.try_get(2).map_err(error)?,
                cli: r.try_get(3).map_err(error)?,
                model: r.try_get(4).map_err(error)?,
                effort: r.try_get(5).map_err(error)?,
            })
        })
        .transpose()?;
    // Paused (and maybe resumed) since this claim began: the job is left
    // waiting, for a look that knows of the pause.
    if job.is_some() && !still() {
        tx.rollback().await.map_err(error)?;
        return Ok(None);
    }
    tx.commit().await.map_err(error)?;
    Ok(job)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish(
    db: &mut Client,
    project: &str,
    queue: &str,
    id: i64,
    token: &str,
    status: &str,
    value: Option<&Value>,
    reason: Option<&str>,
) -> Result<()> {
    let tx = db.transaction().await.map_err(error)?;
    if validate(&tx, project).await? != queue {
        return Err("The linked queue identity changed.".into());
    }
    tx.execute("UPDATE connecto_works.connecto_jobs SET status=$3,result=$4,reason=$5 WHERE id=$1 AND attempt=$2 AND status='doing'", &[&id,&token,&status,&value,&reason]).await.map_err(error)?;
    tx.commit().await.map_err(error)
}

/// Queue a job, as a producer would (`connecto works test`). Ok holds its id.
#[allow(clippy::too_many_arguments)]
pub(super) async fn enqueue(
    db: &mut Client,
    project: &str,
    queue: &str,
    name: &str,
    kind: &str,
    prompt: &str,
    cli: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<i64> {
    let tx = db.transaction().await.map_err(error)?;
    if validate(&tx, project).await? != queue {
        return Err("The linked queue identity changed.".into());
    }
    let id = tx
        .query_one(
            "INSERT INTO connecto_works.connecto_jobs (project_name,work_type,prompt,session_cli,model,effort) VALUES ($1,$2,$3,$4,$5,$6) RETURNING id",
            &[&name, &kind, &prompt, &cli, &model, &effort],
        )
        .await
        .map_err(error)?
        .try_get(0)
        .map_err(error)?;
    tx.commit().await.map_err(error)?;
    Ok(id)
}

/// A job's row, as `connecto works job` shows it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct JobRow {
    pub id: i64,
    pub project_name: String,
    pub kind: String,
    pub status: String,
    pub result: Option<Value>,
    pub reason: Option<String>,
}

pub(super) async fn job(
    db: &mut Client,
    project: &str,
    queue: &str,
    id: i64,
) -> Result<Option<JobRow>> {
    let tx = db.transaction().await.map_err(error)?;
    if validate(&tx, project).await? != queue {
        return Err("The linked queue identity changed.".into());
    }
    let row = tx
        .query_opt(
            "SELECT id,project_name,work_type,status,result,reason FROM connecto_works.connecto_jobs WHERE id=$1",
            &[&id],
        )
        .await
        .map_err(error)?;
    let job = row
        .map(|r| -> Result<JobRow> {
            Ok(JobRow {
                id: r.try_get(0).map_err(error)?,
                project_name: r.try_get(1).map_err(error)?,
                kind: r.try_get(2).map_err(error)?,
                status: r.try_get(3).map_err(error)?,
                result: r.try_get(4).map_err(error)?,
                reason: r.try_get(5).map_err(error)?,
            })
        })
        .transpose()?;
    tx.commit().await.map_err(error)?;
    Ok(job)
}
