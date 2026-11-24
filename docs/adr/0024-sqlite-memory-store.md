# 0024: The project memory is a SQLite database per project, bundled SQLite with FTS5

Status: Accepted. Spike S3 of `docs/specs/project-memory.md` §9; measurements in
`docs/specs/project-memory.work/spikes.md`.

## Context

Project memory (spec `project-memory.md`) needs relations (memories, annotations,
evidence, links, redirects, revisions), transactions that publish a whole dream
at once or not at all, and text search. Everything Connecto stores today is JSON
written atomically under `<data>/projects/<id>/` (spec §2.7); the workspace had no
database dependency. A server is out of scope.

## Decision

1. The memory of a project is one SQLite file,
   `<data>/projects/<id>/memory/memory.db`, outside the repository, in WAL mode.
2. SQLite comes from `rusqlite` (`0.42`, `default-features = false`,
   `features = ["bundled"]`), declared once in the root `Cargo.toml`. `bundled`
   compiles the SQLite amalgamation shipped in `libsqlite3-sys` (`0.40`, SQLite
   3.54.1 at the time of writing) with the C compiler of the build, so every
   platform runs the same SQLite with the same options, and no system library is
   needed. Its build flags turn on FTS5 (`-DSQLITE_ENABLE_FTS5`), JSON, and
   `SQLITE_THREADSAFE=1`; it also sets `SQLITE_DEFAULT_FOREIGN_KEYS=1`, which the
   schema may rely on.
3. Only `connecto-memory` depends on `rusqlite`. One thread per project owns the
   write connection; bridge reads open their own read-only connections (spec D3,
   ADR 0002: threads, no async).
4. Text search is FTS5 (`memory_fts`, `annotation_fts`). Embeddings are out of
   scope; the schema leaves room.
5. The schema version lives in `meta` (`schema_version`; 2 at the time of
   writing). Opening for writing runs the missing forward migrations in one
   transaction; a database written by a newer build is refused and left
   untouched. Every table carries `project_id`, checked by a trigger against the
   one in `meta`.

## Evidence (S3)

- `cargo test -p connecto-memory` on macOS (aarch64): the smoke test opens a WAL
  database, creates an external-content FTS5 table, answers a `MATCH` query
  ranked by `rank`, and a second read-only connection sees the committed rows and
  cannot write.
- Size: a release binary that opens a database and runs an FTS5 query is
  1.84 MB larger than the same binary without `rusqlite` (aarch64: 2,277,392 vs
  432,928 bytes; x86_64: 2,273,800 vs 425,424). Measured on a probe, not on
  `connecto-app`, which did not link the crate yet (it does now; not
  re-measured). Against the current
  `connecto-app` release binary (40.4 MB aarch64) that is about +4.6 % per
  architecture, about +3.7 MB in the universal build.
- Licences: `rusqlite` MIT, `libsqlite3-sys` MIT, SQLite public domain; new
  transitive crates `fallible-iterator`, `fallible-streaming-iterator`, `vcpkg`
  are MIT/Apache-2.0; `bitflags`, `smallvec`, `cc`, `pkg-config` were already in
  the tree. All are compatible with AGPL-3.0-only.
- Windows: **not verified.** `cargo check --target x86_64-pc-windows-msvc` from
  macOS stops in the `libsqlite3-sys` build script because there are no Windows
  SDK headers here (`stdlib.h` not found); that is the cross setup, not the
  crate. The repository's only Windows build is the manual `release-build`
  workflow (`windows-2022`, MSVC, Visual Studio 2022 build tools), which has the
  C toolchain `bundled` needs. The first Windows run of that workflow after
  `connecto-memory` is linked into the app is the check.

## Consequences

- A C compile of the SQLite amalgamation is part of every clean build (about
  10 s on an M-series Mac in debug).
- Release builds need a working C compiler on every platform. macOS and Linux
  runners already compile C for other crates; Windows needs the MSVC build tools
  the runner image has.
- SQLite upgrades come with `rusqlite`/`libsqlite3-sys` upgrades, not with the OS.
- The memory is the first state Connecto keeps outside JSON. Backups and export
  (spec §8.5) read it through the crate, never by copying the file while it is
  open in WAL mode.

**Reverse if** the Windows release build cannot compile the bundled source, or
the size cost becomes a problem; the fallback is the system SQLite on macOS and
Linux, which loses the guarantee that FTS5 is present.
