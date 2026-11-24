//! Schema, migrations, project ownership and the constraints the database
//! itself enforces.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Fixture, PROJECT, T0, red, user};
use connecto_core::memory::{EvidenceKind, Room};
use connecto_memory::schema::current_version;
use connecto_memory::{NewEvidence, Reader, Store, StoreError};
use rusqlite::Connection;

#[test]
fn a_new_database_is_wal_at_the_current_schema_with_every_room() {
    let f = Fixture::new();
    let conn = Connection::open(f.db()).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, current_version().to_string());
    let rooms: Vec<String> = conn
        .prepare("SELECT slug FROM room ORDER BY position")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let expected: Vec<&str> = Room::ALL.iter().map(|r| r.word()).collect();
    assert_eq!(rooms, expected);
    for table in [
        "meta",
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
        "revision",
        "served",
        "schedule_state",
        "dream_staging",
        "bootstrap",
        "bootstrap_source",
        "bootstrap_checkpoint",
        "memory_fts",
        "annotation_fts",
    ] {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "{table}");
    }
    assert_eq!(f.store.version().unwrap(), 0);
}

#[test]
fn reopening_is_idempotent_and_keeps_data() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let db = f.db();
    drop(f.store);
    let store = Store::open(&db, PROJECT).unwrap();
    assert!(store.read().evidence(e).unwrap().is_some());
    let rooms: i64 = store
        .read()
        .connection()
        .query_row("SELECT count(*) FROM room", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rooms, i64::try_from(Room::ALL.len()).unwrap());
}

#[test]
fn a_forward_migration_runs_once_and_keeps_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("memory.db");
    let v1 = connecto_memory::schema::current_version() as usize;
    let mut store = Store::open(&db, PROJECT).unwrap();
    let e = user(&mut store);
    drop(store);
    // A build with one more migration.
    let mut migrations: Vec<&str> = vec![""; v1];
    // The shipped migrations are not needed again: only the new one runs.
    migrations.push("CREATE TABLE extra (project_id TEXT NOT NULL, x INTEGER);");
    let store = Store::open_with_migrations(&db, PROJECT, &migrations).unwrap();
    assert!(store.read().evidence(e).unwrap().is_some());
    let version: String = store
        .read()
        .connection()
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, (v1 + 1).to_string());
    drop(store);
    // Running it again is a no-op.
    Store::open_with_migrations(&db, PROJECT, &migrations).unwrap();
    // And this build now refuses the newer database, untouched.
    match Store::open(&db, PROJECT) {
        Err(StoreError::NewerSchema { found, supported }) => {
            assert_eq!(found as usize, v1 + 1);
            assert_eq!(supported as usize, v1);
        }
        other => panic!("expected a refusal, got {:?}", other.err()),
    }
    match Reader::open(&db, PROJECT) {
        Err(StoreError::NewerSchema { .. }) => {}
        other => panic!("expected a refusal, got {:?}", other.err()),
    }
}

#[test]
fn a_database_of_another_project_is_refused() {
    let f = Fixture::new();
    let db = f.db();
    drop(f.store);
    assert!(matches!(
        Store::open(&db, "proj-b"),
        Err(StoreError::WrongProject)
    ));
    assert!(matches!(
        Reader::open(&db, "proj-b"),
        Err(StoreError::WrongProject)
    ));
    assert!(Store::open(&db, PROJECT).is_ok());
    assert!(matches!(
        Store::open(&db, ""),
        Err(StoreError::BadProjectId)
    ));
}

#[test]
fn rows_of_another_project_cannot_be_written_even_by_hand() {
    let mut f = Fixture::new();
    let conn = f.store.connection_mut();
    let err = conn
        .execute(
            "INSERT INTO evidence (seq, project_id, kind, created_ms, files, excerpt, exposed_to)
             VALUES (99, 'proj-b', 'dichiarazione_utente', 0, '[]', '', '[]')",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("another project"), "{err}");
    let err = conn
        .execute(
            "UPDATE meta SET value = 'proj-b' WHERE key = 'project_id'",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("never changes"), "{err}");
    // And through the API every row carries the project.
    let e = f
        .store
        .insert_evidence(NewEvidence::new(EvidenceKind::UserStatement, T0, red("x")))
        .unwrap();
    assert_eq!(e.project_id, PROJECT);
}

#[test]
fn the_entrance_and_unknown_words_are_refused_by_the_schema() {
    let mut f = Fixture::new();
    let conn = f.store.connection_mut();
    let insert = |room: &str, status: &str| {
        conn.execute(
            "INSERT INTO memory (id, project_id, room, slug, kind, standing, summary, details,
                 status, created_ms, updated_ms, version)
             VALUES ('m-aaaaaaaa', ?1, ?2, 'x', 'fatto', 'ipotesi', 's', '', ?3, 0, 0, 1)",
            rusqlite::params![PROJECT, room, status],
        )
    };
    assert!(insert("ingresso", "candidato").is_err());
    assert!(insert("cantina", "candidato").is_err());
    assert!(insert("officina", "falso").is_err());
}

#[test]
fn a_reader_cannot_write() {
    let f = Fixture::new();
    let reader = Reader::open(&f.db(), PROJECT).unwrap();
    assert!(
        reader
            .read()
            .connection()
            .execute("DELETE FROM room", [])
            .is_err()
    );
}
