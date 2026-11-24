//! Spike S3: the bundled SQLite opens a database in WAL mode and runs an FTS5
//! query, and a second, read-only connection sees a committed write.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};

/// A database file of its own for this test, removed at the end.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("connecto-memory-{name}-{}.db", std::process::id()));
        Self(path)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut file = self.0.clone().into_os_string();
            file.push(suffix);
            let _ = std::fs::remove_file(file);
        }
    }
}

#[test]
fn bundled_sqlite_has_wal_and_fts5() {
    let db = TempDb::new("smoke");
    let conn = Connection::open(&db.0).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");

    conn.execute_batch(
        "CREATE TABLE memory (id TEXT PRIMARY KEY, room TEXT NOT NULL, summary TEXT NOT NULL);
         CREATE VIRTUAL TABLE memory_fts USING fts5(summary, content='memory', content_rowid='rowid');
         INSERT INTO memory (id, room, summary) VALUES
             ('m-aaaaaaaa', 'officina', 'Release macOS: sign with Developer ID, then notarize'),
             ('m-bbbbbbbb', 'problemi', 'The Windows build needs the MSVC toolchain');
         INSERT INTO memory_fts (rowid, summary) SELECT rowid, summary FROM memory;",
    )
    .unwrap();

    let hits: Vec<String> = conn
        .prepare(
            "SELECT m.id FROM memory_fts f JOIN memory m ON m.rowid = f.rowid
             WHERE memory_fts MATCH ?1 ORDER BY rank",
        )
        .unwrap()
        .query_map(["notarize"], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(hits, ["m-aaaaaaaa"]);

    // A read-only connection, as the bridge threads will use, sees the commit.
    let reader = Connection::open_with_flags(&db.0, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let count: i64 = reader
        .query_row(
            "SELECT count(*) FROM memory_fts WHERE memory_fts MATCH 'windows OR macos'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
    assert!(
        reader
            .execute("INSERT INTO memory VALUES ('m-c', 'x', 'y')", [])
            .is_err(),
        "a read-only connection cannot write"
    );

    assert!(connecto_memory::sqlite_version().starts_with('3'));
}
