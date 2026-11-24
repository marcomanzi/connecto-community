//! The schema (spec §4.7), its forward migrations, and opening a database.
//!
//! The schema version lives in `meta` under `schema_version`. Opening for
//! writing runs every migration above it in one transaction; a database from
//! a newer build is refused, never touched. The project id lives in `meta`
//! too, is set once, and every table carries a `project_id` column that a
//! trigger checks against it on insert and update (spec D8): a row of
//! another project cannot be written, whatever the caller does.

use std::path::Path;
use std::time::Duration;

use connecto_core::memory::Room;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

use crate::error::{Result, StoreError};

/// Every migration, in order. Migration `i` takes the schema from version `i`
/// to `i + 1`. Append only: never edit one that has shipped.
pub(crate) const MIGRATIONS: &[&str] = &[V1, V2];

/// The schema version this build writes.
#[must_use]
pub fn current_version() -> u32 {
    u32::try_from(MIGRATIONS.len()).unwrap_or(u32::MAX)
}

/// Tables that hold project rows, for the project triggers.
const PROJECT_TABLES: &[&str] = &[
    "room",
    "memory",
    "memory_scope",
    "memory_anchor",
    "relation",
    "address_redirect",
    "evidence",
    "source_link",
    "annotation",
    "event",
    "batch",
    "run",
    "publication",
    "revision",
    "served",
    "schedule_state",
    "dream_staging",
    "bootstrap",
    "bootstrap_source",
    "bootstrap_checkpoint",
];

const V1: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;

CREATE TABLE room (
    project_id TEXT NOT NULL,
    slug TEXT NOT NULL,
    position INTEGER NOT NULL,
    holds_memories INTEGER NOT NULL CHECK (holds_memories IN (0, 1)),
    PRIMARY KEY (project_id, slug)
);

-- A memory row is never deleted. `live = 0` is a memory merged into another
-- (`merged_into`) or withdrawn by a revert of the version that created it.
CREATE TABLE memory (
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    project_id TEXT NOT NULL,
    room TEXT NOT NULL CHECK (room <> 'ingresso'),
    slug TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN
        ('fatto','procedura','decisione','problema','lavoro','convenzione')),
    standing TEXT NOT NULL CHECK (standing IN
        ('proposta','accettata','scartata','pianificato','in_corso','implementato',
         'ipotesi','accertato','documentata','eseguita')),
    summary TEXT NOT NULL,
    details TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN
        ('candidato','verificato','da_verificare','superato','conflitto')),
    last_confirmed_ms INTEGER,
    anchors_changed INTEGER NOT NULL DEFAULT 0 CHECK (anchors_changed IN (0, 1)),
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    version INTEGER NOT NULL,
    live INTEGER NOT NULL DEFAULT 1 CHECK (live IN (0, 1)),
    merged_into TEXT,
    FOREIGN KEY (project_id, room) REFERENCES room (project_id, slug)
);
CREATE UNIQUE INDEX memory_address ON memory (project_id, room, slug) WHERE live = 1;
CREATE INDEX memory_room ON memory (project_id, room, live);

CREATE TABLE memory_scope (
    project_id TEXT NOT NULL,
    memory_id TEXT NOT NULL REFERENCES memory (id),
    scope TEXT NOT NULL,
    gone INTEGER NOT NULL DEFAULT 0 CHECK (gone IN (0, 1)),
    PRIMARY KEY (memory_id, scope)
);
CREATE INDEX memory_scope_value ON memory_scope (project_id, scope);

CREATE TABLE memory_anchor (
    project_id TEXT NOT NULL,
    memory_id TEXT NOT NULL REFERENCES memory (id),
    path TEXT NOT NULL,
    blob TEXT NOT NULL,
    PRIMARY KEY (memory_id, path)
);

CREATE TABLE relation (
    project_id TEXT NOT NULL,
    from_id TEXT NOT NULL REFERENCES memory (id),
    kind TEXT NOT NULL CHECK (kind IN
        ('sostituisce','contraddice','dipende_da','vedi_anche','deriva_da')),
    to_id TEXT NOT NULL REFERENCES memory (id),
    CHECK (from_id <> to_id),
    PRIMARY KEY (from_id, kind, to_id)
);
CREATE INDEX relation_to ON relation (to_id);

-- An old address and the memory it now leads to.
CREATE TABLE address_redirect (
    project_id TEXT NOT NULL,
    address TEXT NOT NULL,
    memory_id TEXT NOT NULL REFERENCES memory (id),
    version INTEGER NOT NULL,
    PRIMARY KEY (project_id, address)
);

-- `seq` is the evidence id (`e-<seq>`): the event's sequence number, or one
-- taken from the same counter for evidence without an event.
CREATE TABLE evidence (
    seq INTEGER PRIMARY KEY,
    project_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN
        ('dichiarazione_utente','osservazione_codice','esito_riportato',
         'affermazione_agente','proposta_agente','documento_progetto')),
    created_ms INTEGER NOT NULL,
    terminal TEXT,
    tool_session TEXT,
    turn TEXT,
    branch TEXT,
    head TEXT,
    log_path TEXT,
    log_offset INTEGER,
    files TEXT NOT NULL,
    excerpt TEXT NOT NULL,
    exposed_to TEXT NOT NULL
);

-- A memory or annotation (`subject`, by id) and one of its sources.
CREATE TABLE source_link (
    project_id TEXT NOT NULL,
    subject TEXT NOT NULL,
    evidence_seq INTEGER NOT NULL REFERENCES evidence (seq),
    PRIMARY KEY (subject, evidence_seq)
);
CREATE INDEX source_link_evidence ON source_link (evidence_seq);

CREATE TABLE annotation (
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    project_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('integrazione','correzione','contraddizione','task')),
    target_memory TEXT REFERENCES memory (id),
    new_room TEXT CHECK (new_room IS NULL OR new_room <> 'ingresso'),
    text TEXT NOT NULL,
    scopes TEXT NOT NULL,
    verification TEXT NOT NULL CHECK (verification IN
        ('non_verificata','verificata_da_evidenza','dichiarata_dall_utente')),
    origin TEXT NOT NULL,
    memory_kind TEXT,
    standing TEXT,
    absorbed_in INTEGER,
    absorbed_into TEXT REFERENCES memory (id),
    CHECK ((target_memory IS NULL) <> (new_room IS NULL)),
    UNIQUE (project_id, seq)
);
CREATE INDEX annotation_target ON annotation (target_memory, absorbed_in);
CREATE INDEX annotation_open ON annotation (project_id, absorbed_in, seq);

-- The event queue (spec §5.3, §5.4).
CREATE TABLE event (
    seq INTEGER PRIMARY KEY,
    project_id TEXT NOT NULL,
    dedup_key TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('in_coda','considerati','fallito','interno')),
    priority INTEGER NOT NULL DEFAULT 0 CHECK (priority IN (0, 1)),
    created_ms INTEGER NOT NULL,
    payload TEXT NOT NULL,
    batch_id INTEGER REFERENCES batch (id),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_ms INTEGER,
    last_error TEXT,
    UNIQUE (project_id, dedup_key)
);
CREATE INDEX event_state ON event (project_id, state, seq);

CREATE TABLE batch (
    id INTEGER PRIMARY KEY,
    project_id TEXT NOT NULL,
    cycle TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('in_corso','completato','fallito','in_coda','sospeso')),
    created_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_ms INTEGER,
    last_error TEXT,
    -- Why a held (`sospeso`) batch waits: a cause for the user, not a failure.
    hold_reason TEXT,
    run_id INTEGER REFERENCES run (id)
);

CREATE TABLE run (
    id INTEGER PRIMARY KEY,
    project_id TEXT NOT NULL,
    cycle TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    effort TEXT NOT NULL,
    started_ms INTEGER NOT NULL,
    finished_ms INTEGER,
    chars_in INTEGER NOT NULL DEFAULT 0,
    chars_out INTEGER NOT NULL DEFAULT 0,
    outcome TEXT NOT NULL,
    written INTEGER NOT NULL DEFAULT 0,
    rejected INTEGER NOT NULL DEFAULT 0,
    version INTEGER,
    reason TEXT,
    pause_epoch INTEGER NOT NULL DEFAULT 0,
    recovers TEXT NOT NULL DEFAULT ''
);
CREATE INDEX run_cycle ON run (project_id, cycle, id);

-- One row per published memory version.
CREATE TABLE publication (
    version INTEGER PRIMARY KEY,
    project_id TEXT NOT NULL,
    published_ms INTEGER NOT NULL,
    cycle TEXT NOT NULL,
    operations INTEGER NOT NULL,
    reverts INTEGER,
    run_id INTEGER
);

-- The image of every subject a version changed, as it was before. `subject`
-- is a memory id, an annotation id, or `@` and an address (a redirect);
-- `image` is NULL when the subject did not exist before.
CREATE TABLE revision (
    project_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    subject TEXT NOT NULL,
    image TEXT,
    PRIMARY KEY (version, subject)
);

-- What `connecto memory` showed to which tool session (spec §6).
CREATE TABLE served (
    id INTEGER PRIMARY KEY,
    project_id TEXT NOT NULL,
    tool_session TEXT,
    terminal TEXT,
    subject TEXT NOT NULL,
    verb TEXT NOT NULL,
    at_ms INTEGER NOT NULL
);
CREATE INDEX served_session ON served (project_id, tool_session, at_ms);
CREATE INDEX served_terminal ON served (project_id, terminal, at_ms);

-- At most one run per (program, local date) (spec D11, §7). A held row
-- does not count as done: the date can be claimed again.
CREATE TABLE schedule_state (
    project_id TEXT NOT NULL,
    program TEXT NOT NULL,
    local_date TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('running','done','held','missed','recovered')),
    reason TEXT,
    run_id INTEGER,
    recovered_by INTEGER,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (project_id, program, local_date)
);

-- Dream operations gathered across several runs, published together.
CREATE TABLE dream_staging (
    id INTEGER PRIMARY KEY,
    project_id TEXT NOT NULL,
    run_id INTEGER NOT NULL,
    room TEXT,
    payload TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX dream_staging_run ON dream_staging (run_id);

CREATE TABLE bootstrap (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    state TEXT NOT NULL,
    options TEXT NOT NULL,
    reason TEXT,
    started_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL
);

CREATE TABLE bootstrap_source (
    project_id TEXT NOT NULL,
    bootstrap_id TEXT NOT NULL REFERENCES bootstrap (id),
    path TEXT NOT NULL,
    class TEXT NOT NULL,
    hash TEXT,
    state TEXT NOT NULL,
    PRIMARY KEY (bootstrap_id, path)
);

CREATE TABLE bootstrap_checkpoint (
    project_id TEXT NOT NULL,
    bootstrap_id TEXT NOT NULL REFERENCES bootstrap (id),
    packet INTEGER NOT NULL,
    state TEXT NOT NULL,
    run_id INTEGER,
    payload TEXT NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (bootstrap_id, packet)
);

-- Text search. The store keeps these in step with `memory` (live rows only)
-- and `annotation`, by rowid.
CREATE VIRTUAL TABLE memory_fts USING fts5 (
    address, summary, details, tokenize = 'unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE annotation_fts USING fts5 (
    text, tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER meta_project_fixed BEFORE UPDATE ON meta
WHEN OLD.key = 'project_id' AND NEW.value IS NOT OLD.value
BEGIN SELECT RAISE(ABORT, 'the project id of a memory database never changes'); END;
CREATE TRIGGER meta_project_kept BEFORE DELETE ON meta
WHEN OLD.key = 'project_id'
BEGIN SELECT RAISE(ABORT, 'the project id of a memory database never changes'); END;
";

/// Indexes for the lineage walk ([`crate::Read::lineage`]) and the merge
/// chain, which the store-wide support check runs on every publication.
const V2: &str = "
CREATE INDEX memory_merged_into ON memory (merged_into) WHERE merged_into IS NOT NULL;
CREATE INDEX annotation_absorbed_into ON annotation (absorbed_into) WHERE absorbed_into IS NOT NULL;
";

/// The triggers that keep every row in the database's project.
fn project_triggers() -> String {
    let mut sql = String::new();
    for table in PROJECT_TABLES {
        for (when, event) in [("insert", "INSERT"), ("update", "UPDATE OF project_id")] {
            sql.push_str(&format!(
                "CREATE TRIGGER {table}_project_{when} BEFORE {event} ON {table}
                 WHEN NEW.project_id IS NOT (SELECT value FROM meta WHERE key = 'project_id')
                 BEGIN SELECT RAISE(ABORT, 'a row of another project'); END;\n"
            ));
        }
    }
    sql
}

fn apply_migration(tx: &Transaction<'_>, index: usize, sql: &str) -> Result<()> {
    tx.execute_batch(sql)?;
    if index == 0 {
        tx.execute_batch(&project_triggers())?;
    }
    Ok(())
}

/// The schema version stored in the database; 0 for an empty one.
pub(crate) fn stored_version(conn: &Connection) -> Result<u32> {
    let has_meta: bool = conn.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
        [],
        |row| row.get(0),
    )?;
    if !has_meta {
        return Ok(0);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match value {
        None => Ok(0),
        Some(text) => text
            .parse()
            .map_err(|_| StoreError::Corrupt("schema version")),
    }
}

/// Bring the schema to the last of `migrations`, in one transaction.
pub(crate) fn migrate_with(conn: &mut Connection, migrations: &[&str]) -> Result<u32> {
    let supported = u32::try_from(migrations.len()).unwrap_or(u32::MAX);
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let found = stored_version(&tx)?;
    if found > supported {
        return Err(StoreError::NewerSchema { found, supported });
    }
    for (index, sql) in migrations.iter().enumerate().skip(found as usize) {
        apply_migration(&tx, index, sql)?;
    }
    if found != supported {
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [supported.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(supported)
}

/// Whether `id` can be a project id: 1 to 128 printable characters.
pub(crate) fn valid_project_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}

/// Record the project id on a new database, or check it on an existing one;
/// create the rooms.
pub(crate) fn claim_project(conn: &mut Connection, project_id: &str) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let stored: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key = 'project_id'", [], |r| {
            r.get(0)
        })
        .optional()?;
    match stored {
        Some(stored) if stored != project_id => return Err(StoreError::WrongProject),
        Some(_) => {}
        None => {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('project_id', ?1)",
                [project_id],
            )?;
            for key in ["memory_version", "annotation_seq", "event_seq"] {
                tx.execute(
                    "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, '0')",
                    [key],
                )?;
            }
        }
    }
    for (position, room) in Room::ALL.iter().enumerate() {
        tx.execute(
            "INSERT OR IGNORE INTO room (project_id, slug, position, holds_memories)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                project_id,
                room.word(),
                i64::try_from(position).unwrap_or(i64::MAX),
                room.holds_memories()
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// Open (creating if needed) the database for writing: WAL, migrated, and
/// claimed by `project_id`.
pub(crate) fn open_write(path: &Path, project_id: &str) -> Result<Connection> {
    open_write_with(path, project_id, MIGRATIONS)
}

pub(crate) fn open_write_with(
    path: &Path,
    project_id: &str,
    migrations: &[&str],
) -> Result<Connection> {
    if !valid_project_id(project_id) {
        return Err(StoreError::BadProjectId);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    // Before anything persistent (journal mode, migrations, rows): a
    // database of another project or of a newer build is left untouched.
    check_before_writing(&conn, project_id, migrations)?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Corrupt("journal mode"));
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    migrate_with(&mut conn, migrations)?;
    claim_project(&mut conn, project_id)?;
    Ok(conn)
}

/// The project id recorded in `meta`, if the database has one.
fn stored_project(conn: &Connection) -> Result<Option<String>> {
    let has_meta: bool = conn.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
        [],
        |row| row.get(0),
    )?;
    if !has_meta {
        return Ok(None);
    }
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = 'project_id'", [], |r| {
            r.get(0)
        })
        .optional()?)
}

/// Refuse, without writing, a database that belongs to another project or
/// to a build with more migrations than `migrations`.
fn check_before_writing(conn: &Connection, project_id: &str, migrations: &[&str]) -> Result<()> {
    if stored_project(conn)?.is_some_and(|stored| stored != project_id) {
        return Err(StoreError::WrongProject);
    }
    let found = stored_version(conn)?;
    let supported = u32::try_from(migrations.len()).unwrap_or(u32::MAX);
    if found > supported {
        return Err(StoreError::NewerSchema { found, supported });
    }
    Ok(())
}

/// Open an existing database read-only, after checking its schema and
/// project. Never migrates: a database older than this build is refused
/// until the writer has opened it.
pub(crate) fn open_read(path: &Path, project_id: &str) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    configure(&conn)?;
    let found = stored_version(&conn)?;
    let supported = current_version();
    if found > supported {
        return Err(StoreError::NewerSchema { found, supported });
    }
    if found < supported {
        return Err(StoreError::OlderSchema { found, supported });
    }
    let stored: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'project_id'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if stored.as_deref() != Some(project_id) {
        return Err(StoreError::WrongProject);
    }
    Ok(conn)
}

/// A numeric value of `meta`.
pub(crate) fn meta_u64(conn: &Connection, key: &str) -> Result<u64> {
    let text: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?;
    text.unwrap_or_else(|| "0".to_owned())
        .parse()
        .map_err(|_| StoreError::Corrupt("meta counter"))
}

pub(crate) fn set_meta_u64(conn: &Connection, key: &str, value: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value.to_string()],
    )?;
    Ok(())
}

/// Take the next value of a `meta` counter (inside the caller's transaction).
pub(crate) fn next_counter(conn: &Connection, key: &str) -> Result<u64> {
    let next = meta_u64(conn, key)?.saturating_add(1);
    set_meta_u64(conn, key, next)?;
    Ok(next)
}
