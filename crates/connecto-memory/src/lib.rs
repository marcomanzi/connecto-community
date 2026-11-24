//! Project memory: a per-project store of memories and annotations that agents
//! read with `connecto memory`, kept alive by two model cycles (spec
//! `docs/specs/project-memory.md`). The shared vocabulary (ids, records,
//! the rule against self-confirmation, the view) is
//! `connecto_core::memory`; this crate is the store and, later, the cycles.
//!
//! It knows nothing of PTYs or the bridge: it talks to the rest of Connecto
//! through [`Host`], implemented in `connecto-app`, so the engine is tested
//! with [`testing::FakeHost`].
//!
//! # Storage
//!
//! One SQLite file per project, `<data>/projects/<id>/memory/memory.db`
//! ([`MemoryPaths`]), in WAL mode, with FTS5 (ADR 0024). The schema
//! ([`schema`]) is migrated forward when the writer opens it; a database
//! from a newer build is refused untouched. The project id is stored in
//! `meta` and checked on every open; every row carries it, and triggers
//! refuse a row of another project (spec D8).
//!
//! # Threads
//!
//! [`Writer`] runs one thread per project that owns the only write
//! connection ([`Store`]) and takes jobs over a bounded channel (spec D3).
//! [`Reader`] is a read-only connection for a reader thread;
//! [`Reader::snapshot`] gives one consistent version for a read made of
//! several queries. Nothing here runs on the UI thread.
//!
//! # API
//!
//! - [`Store`]: [`Store::insert_evidence`] (with `exposed_to` from the
//!   `served` log), [`Store::record_served`], [`Store::create_annotation`],
//!   [`Store::publish`] and [`Store::revert`] ([`publish`]),
//!   [`Store::set_freshness`], [`Store::set_scope_gone`]; the queue, runs,
//!   schedule, staging and initial-build primitives in [`queue`].
//! - [`Read`]: by id or address ([`Read::resolve`], following redirects and
//!   merges), [`Read::detail`], [`Read::room`], [`Read::search`] (FTS over
//!   memories and annotations), [`Read::card`] (the "single view": open
//!   annotations beside a memory, contradictions first), [`Read::counts`],
//!   [`Read::served_to`], [`Read::lineage`], [`Read::versions`].
//! - [`export::export`]: JSON and Markdown.
//!
//! # Invariants the store enforces
//!
//! Whatever the caller: every memory and annotation has a source of this
//! project; no memory lives in the entrance; live addresses are unique; ids
//! are never reused; `verificato` needs
//! [`connecto_core::memory::may_verify`] over the memory's evidence and
//! lineage, and every standing above the weakest needs
//! [`connecto_core::memory::supports_standing`]; status and freshness are
//! separate columns that no write derives from each other; publications are
//! atomic and never delete a memory (spec D10).

pub mod ask;
pub mod bootstrap;
pub mod bootstrap_inventory;
pub mod bootstrap_packet;
pub mod bootstrap_reconcile;
pub mod capture;
mod codec;
pub mod commands;
pub mod develop;
pub mod dream;
pub mod engine;
mod engine_store;
pub mod error;
pub mod export;
pub mod gate;
pub mod host;
pub mod ingest;
pub mod operations;
pub mod prompt;
pub mod publish;
pub mod queue;
pub mod read;
pub mod render;
pub mod scheduler;
pub mod schema;
mod store;
pub mod testing;
pub mod user_statement;
pub mod validate;
mod writer;

pub use capture::{CaptureHandle, Captured, CapturedReply, Happened};
pub use codec::MemoryRow;
pub use engine::{
    DELETION_SUPPORTED, Engine, EngineConfig, NOT_YET, delete_memory_files, deletion_refusal,
    unsupported_message,
};
pub use engine_store::RunEnd;
pub use error::{Result, StoreError};
pub use host::{
    ChangedFile, Commit, GitHistory, GitSnapshot, Host, LoggedReply, ModelJob, ModelReply,
    OpenProject, PastConversation, PastConversations, PastTurn, ProjectFile, ProjectFiles,
    ReplySource, RunError, TurnRole, WallClock,
};
pub use publish::{
    Absorb, MemoryOp, MemoryUpdate, NewMemory, Publication, PublishStep, Published, Reverted,
    SkipReason,
};
pub use queue::{
    BatchDone, BatchRow, BatchState, BootstrapRow, BootstrapSource, Checkpoint, Enqueued, EventRow,
    EventState, NewEvent, ScheduleEntry, ScheduleState, StagedOp,
};
pub use read::{Counts, Found, Read, Reader, SearchQuery, Snapshot};
pub use store::{MemoryPaths, NewAnnotation, NewEvidence, Store};
pub use writer::{PostError, WRITER_QUEUE, Writer, WriterHandle};

/// Lock `m`, recovering the data from a poisoned lock.
pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The SQLite library version this build links, e.g. `3.54.1`.
#[must_use]
pub fn sqlite_version() -> &'static str {
    rusqlite::version()
}
