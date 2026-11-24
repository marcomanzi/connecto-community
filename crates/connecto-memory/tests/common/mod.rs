//! Helpers shared by the integration tests.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod engine;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use connecto_core::Redacted;
use connecto_core::memory::{
    Address, Annotation, AnnotationKind, AnnotationOrigin, AnnotationTarget, Cycle, Evidence,
    EvidenceId, EvidenceKind, MemoryId, MemoryKind, MemoryStatus, Scope, Verification,
};
use connecto_memory::{
    MemoryOp, NewAnnotation, NewEvidence, NewMemory, Publication, Published, Store,
};

pub const PROJECT: &str = "proj-a";
pub const T0: u64 = 1_794_614_400_000;

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub store: Store,
}

impl Fixture {
    pub fn new() -> Self {
        Self::for_project(PROJECT)
    }

    pub fn for_project(project: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("memory.db"), project).unwrap();
        Self { dir, store }
    }

    pub fn db(&self) -> PathBuf {
        self.dir.path().join("memory.db")
    }
}

/// Wait up to 15 seconds for `ok`, polling every 10 ms.
pub fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn red(text: &str) -> Redacted<String> {
    Redacted::assume(text.to_owned())
}

pub fn evidence_in(
    store: &mut Store,
    kind: EvidenceKind,
    session: Option<&str>,
    at: u64,
) -> Evidence {
    let mut new = NewEvidence::new(kind, at, red("an excerpt"));
    new.tool_session = session.map(str::to_owned);
    new.terminal = session.map(|s| format!("term-{s}"));
    store.insert_evidence(new).unwrap()
}

pub fn user(store: &mut Store) -> EvidenceId {
    evidence_in(store, EvidenceKind::UserStatement, Some("u"), T0).id
}

pub fn new_memory(address: &str, kind: MemoryKind, sources: &[EvidenceId]) -> NewMemory {
    NewMemory {
        id: None,
        address: Address::parse(address).unwrap(),
        kind,
        standing: None,
        summary: red(&format!("Summary of {address}")),
        details: red("Details."),
        scopes: vec![Scope::Project],
        status: MemoryStatus::Candidate,
        anchors: Vec::new(),
        sources: sources.to_vec(),
        relations: Vec::new(),
    }
}

pub fn publication(store: &Store, ops: Vec<MemoryOp>) -> Publication {
    Publication {
        project_id: store.project_id().to_owned(),
        cycle: Cycle::Dream,
        base_version: None,
        cutoff: store.read().annotation_seq().unwrap(),
        ops,
        absorb: Vec::new(),
        run_id: None,
        now_ms: T0 + 1000,
    }
}

pub fn publish(store: &mut Store, ops: Vec<MemoryOp>) -> Published {
    let p = publication(store, ops);
    store.publish(p).unwrap()
}

/// Create one memory and return its id.
pub fn create(
    store: &mut Store,
    address: &str,
    kind: MemoryKind,
    sources: &[EvidenceId],
) -> MemoryId {
    let published = publish(
        store,
        vec![MemoryOp::Create(new_memory(address, kind, sources))],
    );
    published.created[0].clone()
}

pub fn new_note(
    target: AnnotationTarget,
    kind: AnnotationKind,
    sources: &[EvidenceId],
) -> NewAnnotation {
    NewAnnotation {
        id: None,
        created_ms: T0,
        kind,
        target,
        text: red("A note about it."),
        scopes: vec![Scope::Project],
        sources: sources.to_vec(),
        verification: Verification::Unverified,
        origin: AnnotationOrigin::Agent {
            terminal: "claude-1".to_owned(),
        },
        memory_kind: None,
        standing: None,
    }
}

pub fn note(
    store: &mut Store,
    target: AnnotationTarget,
    kind: AnnotationKind,
    sources: &[EvidenceId],
) -> Annotation {
    store
        .create_annotation(new_note(target, kind, sources))
        .unwrap()
}
