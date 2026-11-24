//! What the store refuses, and why.
//!
//! Messages name ids (which are validated and safe to show) but never repeat
//! free text: it may come from an agent or a model.

use connecto_core::memory::{AnnotationId, EvidenceId, MemoryError, MemoryId, Subject};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("memory database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("memory folder: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "the memory database was written by a newer Connecto (schema {found}, this build knows {supported}); update Connecto to open it"
    )]
    NewerSchema { found: u32, supported: u32 },
    #[error(
        "the memory database is at schema {found}, older than {supported}; it is upgraded when the project's memory is opened for writing"
    )]
    OlderSchema { found: u32, supported: u32 },
    #[error("the memory database belongs to another project")]
    WrongProject,
    #[error("not a project id")]
    BadProjectId,
    #[error("{0}")]
    Invalid(#[from] MemoryError),
    #[error("no memory {0} in this project")]
    UnknownMemory(MemoryId),
    #[error("no annotation {0} in this project")]
    UnknownAnnotation(AnnotationId),
    #[error("{0} changed since you saw it: review it and confirm again")]
    Changed(Subject),
    /// A memory an Accept or Reject cannot act on; the sentence says why.
    #[error("{0}")]
    Retired(&'static str),
    #[error("you already stated this as note {0}")]
    AlreadyStated(AnnotationId),
    #[error("no evidence {0} in this project")]
    UnknownEvidence(EvidenceId),
    #[error("evidence {0} already exists")]
    EvidenceExists(EvidenceId),
    #[error("the id {0} is already used")]
    IdTaken(String),
    #[error("the address {0} is already used")]
    AddressTaken(String),
    #[error("{0} cannot be verified: no independent evidence among its sources")]
    NotVerifiable(String),
    #[error("{0}: its sources do not support this standing")]
    StandingUnsupported(String),
    #[error("annotation {0} is stated by the user but has no user statement among its sources")]
    NotUserStated(AnnotationId),
    #[error("a merge needs distinct memories other than the one kept")]
    BadMerge,
    #[error("a publication needs at least one change")]
    EmptyPublication,
    #[error("the memory is at version {found}, not at the base version {expected}")]
    StaleBase { expected: u64, found: u64 },
    #[error("no memory version {0}")]
    UnknownVersion(u64),
    #[error("version {version} cannot be reverted: {subject} changed after it")]
    RevertConflict { version: u64, subject: String },
    #[error("not a calendar date")]
    BadDate,
    #[error("no event {0} in this project")]
    UnknownEvent(u64),
    #[error("no batch {0} in this project")]
    UnknownBatch(u64),
    #[error("no run {0} in this project")]
    UnknownRun(u64),
    #[error("no initial build {0} in this project")]
    UnknownBootstrap(String),
    #[error("the memory database holds a value this build cannot read ({0})")]
    Corrupt(&'static str),
    #[error("the publication was stopped before it committed")]
    Aborted,
    #[error("the memory writer has stopped")]
    WriterGone,
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;
