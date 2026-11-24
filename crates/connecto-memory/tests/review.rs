//! Regressions from backend review round 1 of T2: each test is a reviewer's
//! reproduction, plus the same-class paths found while fixing them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{
    Fixture, PROJECT, T0, create, evidence_in, new_memory, new_note, note, publication, publish,
    user,
};
use connecto_core::memory::{
    Anchor, AnnotationKind, AnnotationOrigin, AnnotationTarget, EvidenceKind, MemoryError,
    MemoryId, MemoryKind, MemoryStatus, Relation, RelationKind, Room, Scope, Standing, Subject,
    Verb, Verification,
};
use connecto_memory::{
    Absorb, MemoryOp, MemoryUpdate, NewEvidence, SearchQuery, Store, StoreError,
};

/// Session `s` was shown annotation P, then claimed something: the claim is
/// independent of a memory only as long as P is not in its lineage.
fn claim_after_reading(
    store: &mut Store,
    p: &connecto_core::memory::Annotation,
) -> connecto_core::memory::EvidenceId {
    store
        .record_served(
            Some("s"),
            None,
            Verb::Search,
            &[Subject::Annotation(p.id.clone())],
            T0 + 1,
        )
        .unwrap();
    evidence_in(store, EvidenceKind::AgentClaim, Some("s"), T0 + 2).id
}

fn absorb_only(
    store: &Store,
    annotation: &connecto_core::memory::Annotation,
    into: &MemoryId,
) -> connecto_memory::Publication {
    let mut p = publication(store, vec![]);
    p.absorb = vec![Absorb {
        annotation: annotation.id.clone(),
        into: Some(into.clone()),
    }];
    p
}

#[test]
fn finding_1_absorbing_into_a_verified_memory_rechecks_its_support() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let p = note(
        &mut f.store,
        AnnotationTarget::New {
            room: Room::Workshop,
        },
        AnnotationKind::Integration,
        &[e],
    );
    let claim = claim_after_reading(&mut f.store, &p);
    let mut x = new_memory("officina/x", MemoryKind::Fact, &[claim]);
    x.status = MemoryStatus::Verified;
    let x = publish(&mut f.store, vec![MemoryOp::Create(x)]).created[0].clone();
    // A memory downstream of X, verified by the same claim.
    let mut y = new_memory("officina/y", MemoryKind::Fact, &[claim]);
    y.status = MemoryStatus::Verified;
    y.relations.push(Relation {
        kind: RelationKind::DerivedFrom,
        target: x.clone(),
    });
    let y = publish(&mut f.store, vec![MemoryOp::Create(y)]).created[0].clone();
    let version = f.store.version().unwrap();
    let err = f.store.publish(absorb_only(&f.store, &p, &x)).unwrap_err();
    assert!(matches!(err, StoreError::NotVerifiable(_)), "{err}");
    assert_eq!(f.store.version().unwrap(), version);
    assert!(f.store.read().annotation(&p.id).unwrap().unwrap().is_open());
    // Lowering X first is not enough: Y, which derives from X, is still
    // verified by the same claim.
    let mut p2 = absorb_only(&f.store, &p, &x);
    p2.ops = vec![MemoryOp::SetStatus {
        id: x.clone(),
        status: MemoryStatus::ToVerify,
        sources: vec![],
    }];
    let err = f.store.publish(p2.clone()).unwrap_err();
    assert!(
        matches!(err, StoreError::NotVerifiable(ref id) if id == y.as_str()),
        "{err}"
    );
    p2.ops.push(MemoryOp::SetStatus {
        id: y,
        status: MemoryStatus::ToVerify,
        sources: vec![],
    });
    f.store.publish(p2).unwrap();
}

#[test]
fn same_class_a_merge_or_derivation_cannot_leave_an_unsupported_claim() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let read_me = create(&mut f.store, "officina/read-me", MemoryKind::Fact, &[e]);
    f.store
        .record_served(
            Some("s"),
            None,
            Verb::Read,
            &[Subject::Memory(read_me.clone())],
            T0 + 1,
        )
        .unwrap();
    let claim = evidence_in(&mut f.store, EvidenceKind::AgentClaim, Some("s"), T0 + 2).id;
    let mut x = new_memory("officina/x", MemoryKind::Fact, &[claim]);
    x.status = MemoryStatus::Verified;
    let x = publish(&mut f.store, vec![MemoryOp::Create(x)]).created[0].clone();
    // R derives from what session `s` read and rests on a claim of `s`: a
    // candidate. Merging R into X brings `read_me` into X's lineage.
    let claim_r = evidence_in(&mut f.store, EvidenceKind::AgentClaim, Some("s"), T0 + 3).id;
    let mut r = new_memory("officina/r", MemoryKind::Fact, &[claim_r]);
    r.relations.push(Relation {
        kind: RelationKind::DerivedFrom,
        target: read_me.clone(),
    });
    let r = publish(&mut f.store, vec![MemoryOp::Create(r)]).created[0].clone();
    for op in [
        MemoryOp::Merge {
            into: x.clone(),
            from: vec![r],
        },
        MemoryOp::Link {
            from: x.clone(),
            kind: RelationKind::DerivedFrom,
            to: read_me.clone(),
        },
    ] {
        let err = f
            .store
            .publish(publication(&f.store, vec![op]))
            .unwrap_err();
        assert!(matches!(err, StoreError::NotVerifiable(_)), "{err}");
    }
    // An open annotation verified by the same kind of claim is rechecked too.
    let p = note(
        &mut f.store,
        AnnotationTarget::New {
            room: Room::Workshop,
        },
        AnnotationKind::Integration,
        &[e],
    );
    let z = create(&mut f.store, "officina/z", MemoryKind::Fact, &[e]);
    let claim2 = claim_after_reading(&mut f.store, &p);
    let mut verified_note = new_note(
        AnnotationTarget::Memory(z.clone()),
        AnnotationKind::Integration,
        &[claim2],
    );
    verified_note.verification = Verification::VerifiedByEvidence;
    f.store.create_annotation(verified_note).unwrap();
    let err = f.store.publish(absorb_only(&f.store, &p, &z)).unwrap_err();
    assert!(matches!(err, StoreError::NotVerifiable(_)), "{err}");
}

#[test]
fn finding_2_revert_never_restores_an_unsupported_claim() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let p = note(
        &mut f.store,
        AnnotationTarget::New {
            room: Room::Workshop,
        },
        AnnotationKind::Integration,
        &[e],
    );
    let claim = claim_after_reading(&mut f.store, &p);
    let mut x = new_memory("officina/x", MemoryKind::Fact, &[claim]);
    x.status = MemoryStatus::Verified;
    x.standing = Some(Standing::Established);
    let x = publish(&mut f.store, vec![MemoryOp::Create(x)]).created[0].clone();
    // Downgrade, then absorb P (which the claim's session had read) into X.
    let mut down = MemoryUpdate::new(x.clone());
    down.standing = Some(Standing::Hypothesis);
    let downgrade = publish(
        &mut f.store,
        vec![
            MemoryOp::Update(down),
            MemoryOp::SetStatus {
                id: x.clone(),
                status: MemoryStatus::ToVerify,
                sources: vec![],
            },
        ],
    )
    .version;
    f.store.publish(absorb_only(&f.store, &p, &x)).unwrap();
    // Reverting the downgrade restores the images, then lowers what they
    // claim but the evidence no longer supports.
    let reverted = f.store.revert(downgrade, T0 + 50).unwrap();
    assert_eq!(reverted.lowered, vec![Subject::Memory(x.clone())]);
    let now = f.store.read().memory(&x).unwrap().unwrap();
    assert_eq!(now.status, MemoryStatus::ToVerify);
    assert_eq!(now.standing, Standing::Hypothesis);
    assert_eq!(now.version, reverted.version);
    // Reverting the revert succeeds and still leaves no unsupported claim.
    f.store.revert(reverted.version, T0 + 60).unwrap();
    let again = f.store.read().memory(&x).unwrap().unwrap();
    assert_ne!(again.status, MemoryStatus::Verified);
}

#[test]
fn finding_3_relations_may_name_memories_created_in_the_same_publication() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let a = MemoryId::parse("m-aaaaaaaa").unwrap();
    let b = MemoryId::parse("m-bbbbbbbb").unwrap();
    let mut ma = new_memory("officina/a", MemoryKind::Fact, &[e]);
    ma.id = Some(a.clone());
    let mut mb = new_memory("officina/b", MemoryKind::Fact, &[e]);
    mb.id = Some(b.clone());
    publish(
        &mut f.store,
        vec![
            MemoryOp::Create(ma),
            MemoryOp::Create(mb),
            MemoryOp::Link {
                from: a.clone(),
                kind: RelationKind::SeeAlso,
                to: b.clone(),
            },
        ],
    );
    assert_eq!(
        f.store.read().memory(&a).unwrap().unwrap().relations.len(),
        1
    );
    // A relation given at creation to a memory created earlier in the same
    // publication, with an id that sorts after it.
    let c = MemoryId::parse("m-cccccccc").unwrap();
    let z = MemoryId::parse("m-zzzzzzzz").unwrap();
    let mut mz = new_memory("officina/z", MemoryKind::Fact, &[e]);
    mz.id = Some(z.clone());
    let mut mc = new_memory("officina/c", MemoryKind::Fact, &[e]);
    mc.id = Some(c.clone());
    mc.relations.push(Relation {
        kind: RelationKind::DependsOn,
        target: z,
    });
    publish(
        &mut f.store,
        vec![MemoryOp::Create(mz), MemoryOp::Create(mc)],
    );
    assert_eq!(
        f.store.read().memory(&c).unwrap().unwrap().relations.len(),
        1
    );
}

#[test]
fn finding_4_nested_values_are_validated_before_storage() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let bad_anchor = Anchor {
        path: "../outside".to_owned(),
        blob: "invalid".to_owned(),
    };
    let mut m = new_memory("officina/a", MemoryKind::Fact, &[e]);
    m.anchors = vec![bad_anchor.clone()];
    let err = f
        .store
        .publish(publication(&f.store, vec![MemoryOp::Create(m)]))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(MemoryError::BadPath)),
        "{err}"
    );
    let mut m = new_memory("officina/a", MemoryKind::Fact, &[e]);
    m.scopes = vec![Scope::Component("../x".to_owned())];
    let err = f
        .store
        .publish(publication(&f.store, vec![MemoryOp::Create(m)]))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(MemoryError::BadScope)),
        "{err}"
    );
    let a = create(&mut f.store, "officina/a", MemoryKind::Fact, &[e]);
    let mut u = MemoryUpdate::new(a.clone());
    u.anchors = Some(vec![Anchor {
        path: "src/lib.rs".to_owned(),
        blob: "invalid".to_owned(),
    }]);
    let err = f
        .store
        .publish(publication(&f.store, vec![MemoryOp::Update(u)]))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(MemoryError::BadBlob)),
        "{err}"
    );
    let mut u = MemoryUpdate::new(a.clone());
    u.scopes = Some(vec![Scope::Branch("a\nb".to_owned())]);
    assert!(
        f.store
            .publish(publication(&f.store, vec![MemoryOp::Update(u)]))
            .is_err()
    );
    // Evidence files.
    let mut ev = NewEvidence::new(EvidenceKind::CodeObservation, T0, common::red("x"));
    ev.files = vec![bad_anchor];
    assert!(matches!(
        f.store.insert_evidence(ev),
        Err(StoreError::Invalid(MemoryError::BadPath))
    ));
    // Annotation scopes and origin.
    let mut n = new_note(
        AnnotationTarget::Memory(a.clone()),
        AnnotationKind::Integration,
        &[e],
    );
    n.scopes = vec![Scope::Worktree(" padded".to_owned())];
    assert!(matches!(
        f.store.create_annotation(n),
        Err(StoreError::Invalid(MemoryError::BadScope))
    ));
    let mut n = new_note(
        AnnotationTarget::Memory(a.clone()),
        AnnotationKind::Integration,
        &[e],
    );
    n.origin = AnnotationOrigin::Agent {
        terminal: "a\nb".to_owned(),
    };
    assert!(matches!(
        f.store.create_annotation(n),
        Err(StoreError::Invalid(MemoryError::BadOrigin))
    ));
    // Everything stored still reads back.
    assert_eq!(f.store.read().memories().unwrap().len(), 1);
    assert!(f.store.read().memory(&a).unwrap().is_some());
}

#[test]
fn finding_5_a_foreign_database_is_refused_before_any_change() {
    let f = Fixture::new();
    let db = f.db();
    drop(f.store);
    let schema_of = |db: &std::path::Path| -> (String, i64) {
        let conn = rusqlite::Connection::open(db).unwrap();
        let version = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let tables = conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();
        (version, tables)
    };
    let before = schema_of(&db);
    let mut migrations: Vec<&str> = vec![""; connecto_memory::schema::current_version() as usize];
    migrations.push("CREATE TABLE extra (project_id TEXT NOT NULL);");
    assert!(matches!(
        Store::open_with_migrations(&db, "proj-b", &migrations),
        Err(StoreError::WrongProject)
    ));
    assert_eq!(
        schema_of(&db),
        before,
        "no migration ran on a foreign database"
    );
    Store::open(&db, PROJECT).unwrap();
}

#[test]
fn finding_6_a_merge_leaves_later_annotations_intact_and_reads_follow_it() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let into = create(&mut f.store, "problemi/flaky", MemoryKind::Problem, &[e]);
    let from = create(
        &mut f.store,
        "problemi/flaky-dup",
        MemoryKind::Problem,
        &[e],
    );
    let early = note(
        &mut f.store,
        AnnotationTarget::Memory(from.clone()),
        AnnotationKind::Integration,
        &[e],
    );
    let cutoff = f.store.read().annotation_seq().unwrap();
    let late = note(
        &mut f.store,
        AnnotationTarget::Memory(from.clone()),
        AnnotationKind::Contradiction,
        &[e],
    );
    let mut p = publication(
        &f.store,
        vec![MemoryOp::Merge {
            into: into.clone(),
            from: vec![from.clone()],
        }],
    );
    p.cutoff = cutoff;
    f.store.publish(p).unwrap();
    let read = f.store.read();
    assert_eq!(
        read.annotation(&late.id).unwrap().unwrap(),
        late,
        "the later row is untouched"
    );
    assert_eq!(read.annotation(&early.id).unwrap().unwrap(), early);
    // Reads show both beside the memory they were merged into, contradiction first.
    let open: Vec<_> = read
        .annotations_on(&into, true)
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(open, [late.id.clone(), early.id.clone()]);
    assert_eq!(
        read.memory(&into)
            .unwrap()
            .unwrap()
            .freshness
            .open_annotations,
        2
    );
    let card = &read.room(Room::Problems).unwrap()[0];
    assert_eq!(card.open.len(), 2);
}

#[test]
fn finding_7_search_leaves_out_annotations_in_the_archive_unless_asked() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let old = create(&mut f.store, "officina/old", MemoryKind::Procedure, &[e]);
    let on_old = note(
        &mut f.store,
        AnnotationTarget::Memory(old.clone()),
        AnnotationKind::Integration,
        &[e],
    );
    publish(&mut f.store, vec![MemoryOp::Archive { id: old }]);
    let for_archive = note(
        &mut f.store,
        AnnotationTarget::New {
            room: Room::Archive,
        },
        AnnotationKind::Integration,
        &[e],
    );
    let live = create(&mut f.store, "officina/live", MemoryKind::Procedure, &[e]);
    let on_live = note(
        &mut f.store,
        AnnotationTarget::Memory(live),
        AnnotationKind::Integration,
        &[e],
    );
    let ids = |all: bool, room: Option<Room>| -> Vec<String> {
        let mut q = SearchQuery::new("note");
        q.all = all;
        q.room = room;
        f.store
            .read()
            .search(&q)
            .unwrap()
            .into_iter()
            .map(|h| match h {
                connecto_core::memory::SearchHit::Annotation(a) => a.id.to_string(),
                connecto_core::memory::SearchHit::Memory(m) => m.id.to_string(),
            })
            .collect()
    };
    assert_eq!(ids(false, None), [on_live.id.to_string()]);
    let mut all = ids(true, None);
    all.sort();
    let mut expected = vec![
        on_old.id.to_string(),
        for_archive.id.to_string(),
        on_live.id.to_string(),
    ];
    expected.sort();
    assert_eq!(all, expected);
    let mut archive = ids(false, Some(Room::Archive));
    archive.sort();
    let mut expected = vec![on_old.id.to_string(), for_archive.id.to_string()];
    expected.sort();
    assert_eq!(archive, expected);
}
