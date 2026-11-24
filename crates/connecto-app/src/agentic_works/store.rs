use super::db::Result;
use connecto_core::ToolKind;
use connecto_core::agentic_works::{
    CloudSql, DEFAULT_DATABASE, Database, GoogleCredential, IpType, JobAgent, MAX_PARALLEL,
    WorkType,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};

pub(super) struct Store(Mutex<Connection>);
/// A published work type as stored: `setup` and `prepare` are the reviewed
/// JSON commands, None for types published without them (and for rows from
/// before they existed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Definition {
    pub instructions: String,
    pub schema: String,
    pub setup: Option<String>,
    pub prepare: Option<String>,
}
/// A linked database: its name in the project, how to reach it, and the
/// identity of its queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Link {
    pub name: String,
    pub database: Database,
    pub queue: String,
}
pub(super) struct Pending {
    /// The linked database the job came from, and goes back to.
    pub db: String,
    /// The database was removed by force: the outcome is not written back.
    pub dropped: bool,
    pub id: i64,
    pub token: String,
    pub name: String,
    pub terminal: Option<String>,
    pub status: String,
    pub value: Option<Value>,
    pub reason: Option<String>,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        let db = Connection::open(root.join("works.sqlite3")).map_err(local)?;
        db.busy_timeout(std::time::Duration::from_secs(3))
            .map_err(local)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS types(project TEXT,kind TEXT,instructions TEXT NOT NULL,schema TEXT NOT NULL,PRIMARY KEY(project,kind));
            CREATE TABLE IF NOT EXISTS authors(project TEXT,kind TEXT,terminal TEXT NOT NULL,PRIMARY KEY(project,kind));
            CREATE TABLE IF NOT EXISTS passwords(project TEXT NOT NULL,instance TEXT NOT NULL,user TEXT NOT NULL,secret TEXT NOT NULL,PRIMARY KEY(project,instance,user));").map_err(local)?;
        // Columns added after the first release: older stores gain them empty.
        for column in ["setup", "prepare", "setup_ok_at", "databases"] {
            if !columns(&db, "types")?.iter().any(|c| c == column) {
                db.execute_batch(&format!("ALTER TABLE types ADD COLUMN {column} TEXT"))
                    .map_err(local)?;
            }
        }
        migrate_to_named_databases(&db)?;
        migrate_to_a_journal_per_job(&db)?;
        // How many jobs each project runs at once; a project without a row
        // runs one.
        db.execute_batch("CREATE TABLE IF NOT EXISTS settings(project TEXT PRIMARY KEY,parallel INTEGER NOT NULL DEFAULT 1);")
            .map_err(local)?;
        // The agent for jobs that name none (NULL: Automatic), and its model
        // (NULL: the CLI's default).
        for column in ["agent", "agent_model"] {
            if !columns(&db, "settings")?.iter().any(|c| c == column) {
                db.execute_batch(&format!("ALTER TABLE settings ADD COLUMN {column} TEXT"))
                    .map_err(local)?;
            }
        }
        // The databases an agent asked a handed-over definition for.
        if !columns(&db, "authors")?.iter().any(|c| c == "databases") {
            db.execute_batch("ALTER TABLE authors ADD COLUMN databases TEXT")
                .map_err(local)?;
        }
        Ok(Self(Mutex::new(db)))
    }
    fn db(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Link `database` as `name`, or change the link of that name while its
    /// queue stays the same. One queue is linked under one name only.
    pub fn link(&self, project: &str, name: &str, database: &Database, queue: &str) -> Result<()> {
        let (credential, target) = match database {
            Database::ConnectionString(secret) => (secret.clone(), None),
            Database::CloudSql(sql) => (sql.password_secret.clone(), Some(cloudsql_json(sql))),
        };
        let db = self.db();
        let other: Option<String> = db
            .query_row(
                "SELECT name FROM databases WHERE project=?1 AND queue=?2 AND name<>?3",
                [project, queue, name],
                |r| r.get(0),
            )
            .optional()
            .map_err(local)?;
        if let Some(other) = other {
            return Err(format!(
                "That queue is already linked to this project as the database {other}."
            ));
        }
        let n = db
            .execute(
                "INSERT INTO databases(project,name,credential,queue,target) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(project,name) DO UPDATE SET credential=excluded.credential,target=excluded.target WHERE queue=excluded.queue",
                params![project, name, credential, queue, target],
            )
            .map_err(local)?;
        if n == 0 {
            return Err(format!(
                "The database {name} of this project is a different queue: remove it first, or link this one under another name."
            ));
        }
        Ok(())
    }
    /// The database linked as `name`, and its queue.
    pub fn linked(&self, project: &str, name: &str) -> Result<Option<(Database, String)>> {
        Ok(self
            .links(project)?
            .into_iter()
            .find(|l| l.name == name)
            .map(|l| (l.database, l.queue)))
    }
    /// Every database linked to `project`, by name.
    pub fn links(&self, project: &str) -> Result<Vec<Link>> {
        let db = self.db();
        let mut s = db
            .prepare(
                "SELECT name,credential,queue,target FROM databases WHERE project=?1 ORDER BY name",
            )
            .map_err(local)?;
        let rows = s
            .query_map([project], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(local)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(local)?;
        rows.into_iter()
            .map(|(name, credential, queue, target)| {
                let database = match target {
                    None => Database::ConnectionString(credential),
                    Some(raw) => Database::CloudSql(cloudsql_from_json(&raw)?),
                };
                Ok(Link {
                    name,
                    database,
                    queue,
                })
            })
            .collect()
    }
    /// Forget the database linked as `name`, and drop the outcome still to
    /// be written back to it (its journal row stays, marked, until its
    /// terminal is closed). Ok holds the database it was.
    pub fn unlink(&self, project: &str, name: &str) -> Result<Option<Database>> {
        let link = self.linked(project, name)?;
        let mut db = self.db();
        let tx = db.transaction().map_err(local)?;
        tx.execute(
            "DELETE FROM databases WHERE project=?1 AND name=?2",
            [project, name],
        )
        .map_err(local)?;
        // The job's terminal is still closed by the cycle; only its outcome
        // is no longer written back.
        tx.execute(
            "UPDATE journal SET dropped=1 WHERE project=?1 AND db=?2",
            [project, name],
        )
        .map_err(local)?;
        tx.commit().map_err(local)?;
        Ok(link.map(|l| l.0))
    }
    /// Whether a linked Cloud SQL database signs in with the password kept
    /// under `secret`.
    pub fn password_in_use(&self, project: &str, secret: &str) -> Result<bool> {
        Ok(self.links(project)?.iter().any(
            |l| matches!(&l.database, Database::CloudSql(sql) if sql.password_secret == secret),
        ))
    }
    /// Publish `kind` for the databases named (None: every one).
    pub fn publish(
        &self,
        project: &str,
        kind: &str,
        definition: &Definition,
        setup_ok_at: Option<&str>,
        databases: Option<&[String]>,
    ) -> Result<()> {
        let databases = databases
            .map(|list| serde_json::to_string(list).map_err(|e| e.to_string()))
            .transpose()?;
        self.db()
            .execute(
                "INSERT OR REPLACE INTO types(project,kind,instructions,schema,setup,prepare,setup_ok_at,databases) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    project,
                    kind,
                    definition.instructions,
                    definition.schema,
                    definition.setup,
                    definition.prepare,
                    setup_ok_at,
                    databases
                ],
            )
            .map_err(local)?;
        Ok(())
    }
    /// A published type's definition and the databases it is published for
    /// (None: every one), read from one row, so a job never pairs one
    /// publication's commands with another's databases.
    pub fn published(
        &self,
        project: &str,
        kind: &str,
    ) -> Result<(Definition, Option<Vec<String>>)> {
        let row = self
            .db()
            .query_row(
                "SELECT instructions,schema,setup,prepare,databases FROM types WHERE project=?1 AND kind=?2",
                [project, kind],
                |r| {
                    Ok((
                        Definition {
                            instructions: r.get(0)?,
                            schema: r.get(1)?,
                            setup: r.get(2)?,
                            prepare: r.get(3)?,
                        },
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(local)?
            .ok_or("No published definition for the requested work type.")?;
        let databases = row.1.as_deref().map(databases_from_json).transpose()?;
        Ok((row.0, databases))
    }
    /// The databases `kind` is published for: None for every one, and for
    /// a type that is not published.
    pub fn databases_of(&self, project: &str, kind: &str) -> Result<Option<Vec<String>>> {
        let raw: Option<Option<String>> = self
            .db()
            .query_row(
                "SELECT databases FROM types WHERE project=?1 AND kind=?2",
                [project, kind],
                |r| r.get(0),
            )
            .optional()
            .map_err(local)?;
        raw.flatten()
            .as_deref()
            .map(databases_from_json)
            .transpose()
    }
    pub fn definition(&self, project: &str, kind: &str) -> Result<Definition> {
        self.db()
            .query_row(
                "SELECT instructions,schema,setup,prepare FROM types WHERE project=?1 AND kind=?2",
                [project, kind],
                |r| {
                    Ok(Definition {
                        instructions: r.get(0)?,
                        schema: r.get(1)?,
                        setup: r.get(2)?,
                        prepare: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(local)?
            .ok_or_else(|| "No published definition for the requested work type.".into())
    }
    /// Record when `setup` last succeeded, or None after it failed.
    pub fn setup_ok(&self, project: &str, kind: &str, at: Option<&str>) -> Result<()> {
        self.db()
            .execute(
                "UPDATE types SET setup_ok_at=?3 WHERE project=?1 AND kind=?2",
                params![project, kind, at],
            )
            .map_err(local)?;
        Ok(())
    }
    pub fn kinds(&self, project: &str) -> Result<Vec<WorkType>> {
        let db = self.db();
        let mut s = db
            .prepare("SELECT kind,setup IS NOT NULL,setup_ok_at,databases FROM types WHERE project=? ORDER BY kind")
            .map_err(local)?;
        let rows = s
            .query_map([project], |r| {
                Ok((
                    WorkType {
                        name: r.get(0)?,
                        setup: r.get(1)?,
                        setup_ok_at: r.get(2)?,
                        databases: None,
                    },
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(local)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(local)?;
        rows.into_iter()
            .map(|(mut kind, raw)| {
                kind.databases = raw.as_deref().map(databases_from_json).transpose()?;
                Ok(kind)
            })
            .collect()
    }
    /// `terminal` authors `kind`; `databases` are those it asked the type
    /// for (None: every one), for the user to review with the definition.
    pub fn author(
        &self,
        project: &str,
        kind: &str,
        terminal: &str,
        databases: Option<&[String]>,
    ) -> Result<()> {
        let databases = databases
            .map(|list| serde_json::to_string(list).map_err(|e| e.to_string()))
            .transpose()?;
        self.db()
            .execute(
                "INSERT OR REPLACE INTO authors(project,kind,terminal,databases) VALUES (?1,?2,?3,?4)",
                params![project, kind, terminal, databases],
            )
            .map_err(local)?;
        Ok(())
    }
    /// The terminal authoring `kind`, and the databases it asked for.
    pub fn author_of(&self, project: &str, kind: &str) -> Result<(String, Option<Vec<String>>)> {
        let (terminal, raw): (String, Option<String>) = self
            .db()
            .query_row(
                "SELECT terminal,databases FROM authors WHERE project=?1 AND kind=?2",
                [project, kind],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(local)?
            .ok_or("Start an authoring session for this work type.")?;
        Ok((
            terminal,
            raw.as_deref().map(databases_from_json).transpose()?,
        ))
    }
    /// How many jobs `project` runs at once: 1 unless set.
    pub fn parallel(&self, project: &str) -> Result<u32> {
        let n: Option<i64> = self
            .db()
            .query_row(
                "SELECT parallel FROM settings WHERE project=?1",
                [project],
                |r| r.get(0),
            )
            .optional()
            .map_err(local)?;
        Ok(n.map_or(1, |n| u32::try_from(n).unwrap_or(1).clamp(1, MAX_PARALLEL)))
    }
    pub fn set_parallel(&self, project: &str, jobs: u32) -> Result<()> {
        self.db()
            .execute(
                "INSERT INTO settings(project,parallel) VALUES (?1,?2) ON CONFLICT(project) DO UPDATE SET parallel=excluded.parallel",
                params![project, jobs.clamp(1, MAX_PARALLEL)],
            )
            .map_err(local)?;
        Ok(())
    }
    /// The agent `project`'s jobs that name none run on: Automatic unless
    /// set. A saved CLI this build does not know reads as Automatic.
    pub fn job_agent(&self, project: &str) -> Result<JobAgent> {
        let row: Option<(Option<String>, Option<String>)> = self
            .db()
            .query_row(
                "SELECT agent,agent_model FROM settings WHERE project=?1",
                [project],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(local)?;
        Ok(match row {
            Some((Some(tool), model)) => match ToolKind::from_name(&tool) {
                Some(tool) if tool.is_agent() => JobAgent::Cli { tool, model },
                _ => JobAgent::Automatic,
            },
            _ => JobAgent::Automatic,
        })
    }
    pub fn set_job_agent(&self, project: &str, agent: &JobAgent) -> Result<()> {
        let (tool, model) = match agent {
            JobAgent::Automatic => (None, None),
            JobAgent::Cli { tool, model } => (Some(tool.name()), model.as_deref()),
        };
        self.db()
            .execute(
                "INSERT INTO settings(project,agent,agent_model) VALUES (?1,?2,?3) ON CONFLICT(project) DO UPDATE SET agent=excluded.agent,agent_model=excluded.agent_model",
                params![project, tool, model],
            )
            .map_err(local)?;
        Ok(())
    }
    /// Record, before dispatch, that the job `job` of the database `db`
    /// runs. Each job has its own row, keyed by its token.
    pub fn begin(&self, project: &str, db: &str, job: &super::db::Job, name: &str) -> Result<()> {
        self.db().execute("INSERT INTO journal(project,db,id,token,name,status,value,reason,terminal) VALUES (?1,?2,?3,?4,?5,'failed',NULL,'Connecto stopped before recording a final response. Work will not be repeated.',NULL)",params![project,db,job.id,job.token,name]).map_err(local)?;
        Ok(())
    }
    pub fn attach(&self, project: &str, token: &str, terminal: &str) -> Result<()> {
        self.db()
            .execute(
                "UPDATE journal SET terminal=?3 WHERE project=?1 AND token=?2",
                [project, token, terminal],
            )
            .map_err(local)?;
        Ok(())
    }
    pub fn record(
        &self,
        project: &str,
        token: &str,
        status: &str,
        value: Option<&Value>,
        reason: Option<&str>,
    ) -> Result<()> {
        self.db()
            .execute(
                "UPDATE journal SET status=?3,value=?4,reason=?5 WHERE project=?1 AND token=?2",
                params![project, token, status, value.map(Value::to_string), reason],
            )
            .map_err(local)?;
        Ok(())
    }
    /// The jobs of the database `db` running, or with an outcome still to
    /// be written back to it (not those dropped when it was removed).
    pub fn pending(&self, project: &str, db: &str) -> Result<Vec<Pending>> {
        Ok(self
            .pendings(project)?
            .into_iter()
            .filter(|p| p.db == db && !p.dropped)
            .collect())
    }
    /// Whether the job `token` lost its database (a forced removal). A job
    /// whose row is gone has lost it too.
    pub fn dropped(&self, project: &str, token: &str) -> Result<bool> {
        // By the journal's key: every running agent asks this often.
        let dropped: Option<bool> = self
            .db()
            .query_row(
                "SELECT dropped FROM journal WHERE project=?1 AND token=?2",
                [project, token],
                |r| r.get(0),
            )
            .optional()
            .map_err(local)?;
        Ok(dropped.unwrap_or(true))
    }
    /// Forget dropped outcomes, once their terminals are closed.
    pub fn clear_dropped(&self, project: &str) -> Result<()> {
        self.db()
            .execute(
                "DELETE FROM journal WHERE project=?1 AND dropped=1",
                [project],
            )
            .map_err(local)?;
        Ok(())
    }
    /// Every job of `project` journalled, whatever its database, with those
    /// whose outcome was dropped: each has a terminal to close.
    pub fn pendings(&self, project: &str) -> Result<Vec<Pending>> {
        let db = self.db();
        let mut s = db
            .prepare("SELECT db,id,token,name,status,value,reason,terminal,dropped FROM journal WHERE project=?1 ORDER BY db,rowid")
            .map_err(local)?;
        let rows = s
            .query_map([project], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get::<_, bool>(8)?,
                ))
            })
            .map_err(local)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(local)?;
        rows.into_iter()
            .map(
                |(db, id, token, name, status, raw, reason, terminal, dropped)| {
                    Ok(Pending {
                        db,
                        dropped,
                        id,
                        token,
                        name,
                        status,
                        reason,
                        terminal,
                        value: raw
                            .map(|s| serde_json::from_str(&s))
                            .transpose()
                            .map_err(|_| "Invalid local outcome JSON")?,
                    })
                },
            )
            .collect()
    }
    /// The secret Connecto asked the user for as the password of `user` on
    /// the Cloud SQL `instance`: usable for that destination only.
    pub fn password_for(
        &self,
        project: &str,
        instance: &str,
        user: &str,
    ) -> Result<Option<String>> {
        self.db()
            .query_row(
                "SELECT secret FROM passwords WHERE project=?1 AND instance=?2 AND user=?3",
                [project, instance, user],
                |r| r.get(0),
            )
            .optional()
            .map_err(local)
    }
    /// Remember that Connecto asked for `secret` as that password.
    pub fn bind_password(
        &self,
        project: &str,
        instance: &str,
        user: &str,
        secret: &str,
    ) -> Result<()> {
        self.db()
            .execute(
                "INSERT OR REPLACE INTO passwords VALUES (?1,?2,?3,?4)",
                [project, instance, user, secret],
            )
            .map_err(local)?;
        Ok(())
    }
    /// Whether `secret` is a Cloud SQL password Connecto asked for.
    pub fn is_password(&self, project: &str, secret: &str) -> Result<bool> {
        self.db()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM passwords WHERE project=?1 AND secret=?2)",
                [project, secret],
                |r| r.get(0),
            )
            .map_err(local)
    }
    pub fn unbind_password(&self, project: &str, secret: &str) -> Result<()> {
        self.db()
            .execute(
                "DELETE FROM passwords WHERE project=?1 AND secret=?2",
                [project, secret],
            )
            .map_err(local)?;
        Ok(())
    }
    pub fn clear(&self, project: &str, token: &str) -> Result<()> {
        self.db()
            .execute(
                "DELETE FROM journal WHERE project=?1 AND token=?2",
                [project, token],
            )
            .map_err(local)?;
        Ok(())
    }
}
/// A Cloud SQL link as stored: names only, never a secret's value.
pub(super) fn cloudsql_json(sql: &CloudSql) -> String {
    serde_json::json!({
        "kind": "cloudsql",
        "connection_name": sql.connection_name,
        "database": sql.database,
        "user": sql.user,
        "ip_type": sql.ip_type.word(),
        "iam_auth": sql.iam_auth,
        "credential": match &sql.credential {
            GoogleCredential::Key(name) => format!("key:{name}"),
            GoogleCredential::Adc => "adc".to_owned(),
        },
        "password_secret": sql.password_secret,
    })
    .to_string()
}

pub(super) fn cloudsql_from_json(raw: &str) -> Result<CloudSql> {
    let bad = || "Agentic Works local store: an unreadable database link.".to_owned();
    let v: Value = serde_json::from_str(raw).map_err(|_| bad())?;
    if v.get("kind").and_then(Value::as_str) != Some("cloudsql") {
        return Err(bad());
    }
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let credential = match s("credential").as_str() {
        "adc" => GoogleCredential::Adc,
        other => GoogleCredential::Key(other.strip_prefix("key:").ok_or_else(bad)?.to_owned()),
    };
    Ok(CloudSql {
        connection_name: s("connection_name"),
        database: s("database"),
        user: s("user"),
        ip_type: IpType::from_word(&s("ip_type")).ok_or_else(bad)?,
        iam_auth: v.get("iam_auth").and_then(Value::as_bool).unwrap_or(false),
        credential,
        password_secret: s("password_secret"),
    })
}

/// The names of `table`'s columns; none when it does not exist.
fn columns(db: &Connection, table: &str) -> Result<Vec<String>> {
    db.prepare("SELECT name FROM pragma_table_info(?1)")
        .and_then(|mut s| {
            s.query_map([table], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(local)
}

/// Named databases: a project links several (`databases`), and each keeps
/// its own outcome to write back (`journal`). A store from before them has
/// one link per project (`links`) and one outcome (`outcomes`): both become
/// the database named `default`, in one transaction.
fn migrate_to_named_databases(db: &Connection) -> Result<()> {
    db.execute_batch("BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS databases(project TEXT NOT NULL,name TEXT NOT NULL,credential TEXT NOT NULL,queue TEXT NOT NULL,target TEXT,PRIMARY KEY(project,name));
        CREATE TABLE IF NOT EXISTS journal(project TEXT NOT NULL,db TEXT NOT NULL,id INTEGER NOT NULL,token TEXT NOT NULL,name TEXT NOT NULL,status TEXT NOT NULL,value TEXT,reason TEXT,terminal TEXT,dropped INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(project,db));")
        .map_err(local)?;
    let moved = (|| {
        let links = columns(db, "links")?;
        if !links.is_empty() {
            // A link from before Cloud SQL has no target: a connection string.
            let target = if links.iter().any(|c| c == "target") {
                "target"
            } else {
                "NULL"
            };
            db.execute(
                &format!("INSERT OR IGNORE INTO databases(project,name,credential,queue,target) SELECT project,?1,credential,queue,{target} FROM links"),
                [DEFAULT_DATABASE],
            )
            .map_err(local)?;
            db.execute_batch("DROP TABLE links").map_err(local)?;
        }
        if !columns(db, "outcomes")?.is_empty() {
            db.execute(
                "INSERT OR IGNORE INTO journal(project,db,id,token,name,status,value,reason,terminal) SELECT project,?1,id,token,name,status,value,reason,terminal FROM outcomes",
                [DEFAULT_DATABASE],
            )
            .map_err(local)?;
            db.execute_batch("DROP TABLE outcomes").map_err(local)?;
        }
        Ok(())
    })();
    match moved {
        Ok(()) => db.execute_batch("COMMIT").map_err(local),
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Several jobs at once: the journal holds a row per job, keyed by its
/// token, instead of one per database. A store from before keeps its rows
/// (at most one per database), in one transaction.
fn migrate_to_a_journal_per_job(db: &Connection) -> Result<()> {
    let keyed: Vec<String> = db
        .prepare("SELECT name FROM pragma_table_info('journal') WHERE pk>0 ORDER BY pk")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(local)?;
    if keyed != ["project", "db"] {
        return Ok(());
    }
    db.execute_batch("BEGIN IMMEDIATE;
        CREATE TABLE journal_per_job(project TEXT NOT NULL,db TEXT NOT NULL,id INTEGER NOT NULL,token TEXT NOT NULL,name TEXT NOT NULL,status TEXT NOT NULL,value TEXT,reason TEXT,terminal TEXT,dropped INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(project,token));
        INSERT INTO journal_per_job SELECT project,db,id,token,name,status,value,reason,terminal,dropped FROM journal;
        DROP TABLE journal;
        ALTER TABLE journal_per_job RENAME TO journal;
        COMMIT;")
        .map_err(|e| {
            let _ = db.execute_batch("ROLLBACK");
            local(e)
        })
}

fn databases_from_json(raw: &str) -> Result<Vec<String>> {
    serde_json::from_str(raw)
        .map_err(|_| "Agentic Works local store: an unreadable list of databases.".to_owned())
}

fn local(e: rusqlite::Error) -> String {
    format!("Agentic Works local store: {e}")
}
