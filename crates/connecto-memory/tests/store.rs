//! Evidence, the `served` log, annotations, and the invariants the store
//! enforces whatever the caller (spec §4.4, §4.5, D8).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{
    Fixture, PROJECT, T0, create, evidence_in, new_memory, new_note, note, publication, publish,
    red, user,
};
use connecto_core::memory::{
    Address, AnnotationKind, AnnotationTarget, EvidenceId, EvidenceKind, MemoryError, MemoryId,
    MemoryKind, MemoryStatus, Relation, RelationKind, Room, Standing, Subject, Verb, Verification,
};
use connecto_memory::{MemoryOp, NewEvidence, StoreError};

#[test]
fn exposed_to_comes_from_what_the_session_was_shown_before() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let x = create(
        &mut f.store,
        "officina/rilascio",
        MemoryKind::Procedure,
        &[e],
    );
    let y = create(&mut f.store, "officina/build", MemoryKind::Procedure, &[e]);
    f.store
        .record_served(
            Some("sess-b"),
            Some("term-sess-b"),
            Verb::Read,
            &[Subject::Memory(x.clone())],
            T0 + 10,
        )
        .unwrap();
    f.store
        .record_served(
            Some("sess-c"),
            None,
            Verb::Read,
            &[Subject::Memory(y.clone())],
            T0 + 10,
        )
        .unwrap();
    // Before being shown: nothing.
    let early = evidence_in(
        &mut f.store,
        EvidenceKind::AgentClaim,
        Some("sess-b"),
        T0 + 5,
    );
    assert!(early.exposed_to.is_empty());
    // After: what this session was shown, and only that.
    let late = evidence_in(
        &mut f.store,
        EvidenceKind::AgentClaim,
        Some("sess-b"),
        T0 + 20,
    );
    assert_eq!(late.exposed_to, vec![Subject::Memory(x.clone())]);
    // Stored and read back the same.
    let read = f.store.read().evidence(late.id).unwrap().unwrap();
    assert_eq!(read, late);
    assert_eq!(
        f.store
            .read()
            .served_to(Some("sess-c"), None, u64::MAX)
            .unwrap(),
        vec![Subject::Memory(y)]
    );
}

#[test]
fn evidence_ids_follow_the_event_counter_and_never_collide() {
    let mut f = Fixture::new();
    let a = user(&mut f.store);
    let b = user(&mut f.store);
    assert_eq!(a, EvidenceId::new(1));
    assert_eq!(b, EvidenceId::new(2));
    let mut tied = NewEvidence::new(EvidenceKind::UserStatement, T0, red("x"));
    tied.event_seq = Some(2);
    assert!(matches!(
        f.store.insert_evidence(tied.clone()),
        Err(StoreError::EvidenceExists(_))
    ));
    tied.event_seq = Some(50);
    assert!(matches!(
        f.store.insert_evidence(tied),
        Err(StoreError::UnknownEvent(50))
    ));
}

#[test]
fn annotations_get_increasing_seq_and_are_visible_at_once() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let first = note(
        &mut f.store,
        AnnotationTarget::New {
            room: Room::Decisions,
        },
        AnnotationKind::Integration,
        &[e],
    );
    let second = note(
        &mut f.store,
        AnnotationTarget::New {
            room: Room::Problems,
        },
        AnnotationKind::Task,
        &[e],
    );
    assert_eq!((first.seq, second.seq), (1, 2));
    assert_eq!(f.store.read().annotation_seq().unwrap(), 2);
    let open = f.store.read().open_annotations().unwrap();
    assert_eq!(open, vec![first.clone(), second]);
    assert_eq!(f.store.read().annotation(&first.id).unwrap(), Some(first));
}

#[test]
fn every_annotation_needs_a_source_of_this_project() {
    let mut f = Fixture::new();
    let target = AnnotationTarget::New {
        room: Room::Workshop,
    };
    let err = f
        .store
        .create_annotation(new_note(target.clone(), AnnotationKind::Integration, &[]))
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(MemoryError::NoSources)));
    let err = f
        .store
        .create_annotation(new_note(
            target,
            AnnotationKind::Integration,
            &[EvidenceId::new(7)],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownEvidence(_)));
    // A contradiction must name an existing memory.
    let e = user(&mut f.store);
    let err = f
        .store
        .create_annotation(new_note(
            AnnotationTarget::Memory(MemoryId::from_entropy([9; 5])),
            AnnotationKind::Contradiction,
            &[e],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownMemory(_)));
    let err = f
        .store
        .create_annotation(new_note(
            AnnotationTarget::New {
                room: Room::Entrance,
            },
            AnnotationKind::Integration,
            &[e],
        ))
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid(MemoryError::EntranceHoldsNothing)
    ));
    assert_eq!(f.store.read().annotation_seq().unwrap(), 0);
}

#[test]
fn two_projects_never_see_each_others_rows_and_foreign_ids_are_rejected() {
    let mut a = Fixture::for_project("proj-a");
    let mut b = Fixture::for_project("proj-b");
    let ea = user(&mut a.store);
    let ma = create(
        &mut a.store,
        "decisioni/sqlite",
        MemoryKind::Decision,
        &[ea],
    );
    let na = note(
        &mut a.store,
        AnnotationTarget::Memory(ma.clone()),
        AnnotationKind::Integration,
        &[ea],
    );
    // B has none of A's rows.
    assert!(b.store.read().memory(&ma).unwrap().is_none());
    assert!(b.store.read().annotation(&na.id).unwrap().is_none());
    assert!(b.store.read().evidence(ea).unwrap().is_none());
    assert!(b.store.read().memories().unwrap().is_empty());
    // A's ids in B's operations are refused, and nothing changes.
    let err = b
        .store
        .publish(publication(
            &b.store,
            vec![MemoryOp::SetStatus {
                id: ma.clone(),
                status: MemoryStatus::ToVerify,
                sources: vec![],
            }],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownMemory(_)));
    let err = b
        .store
        .publish(publication(
            &b.store,
            vec![MemoryOp::Create(new_memory(
                "decisioni/sqlite",
                MemoryKind::Decision,
                &[ea],
            ))],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownEvidence(_)));
    let err = b
        .store
        .create_annotation(new_note(
            AnnotationTarget::Memory(ma.clone()),
            AnnotationKind::Contradiction,
            &[ea],
        ))
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::UnknownMemory(_) | StoreError::UnknownEvidence(_)
    ));
    // A's id is unknown in B; a publication addressed to A is refused by B.
    let mut p = publication(&a.store, vec![]);
    p.project_id = "proj-b".to_owned();
    p.ops.push(MemoryOp::Archive { id: ma });
    assert!(matches!(
        b.store.publish(p.clone()),
        Err(StoreError::UnknownMemory(_))
    ));
    p.project_id = "proj-a".to_owned();
    assert!(matches!(b.store.publish(p), Err(StoreError::WrongProject)));
    assert_eq!(b.store.version().unwrap(), 0);
}

#[test]
fn an_agent_repeating_what_it_read_cannot_verify_it() {
    let mut f = Fixture::new();
    let code = evidence_in(&mut f.store, EvidenceKind::CodeObservation, None, T0).id;
    let y = create(
        &mut f.store,
        "architettura/store",
        MemoryKind::Fact,
        &[code],
    );
    // Session S read Y, then claimed something; X is made from that claim and
    // derives from Y.
    f.store
        .record_served(
            Some("s"),
            None,
            Verb::Read,
            &[Subject::Memory(y.clone())],
            T0 + 1,
        )
        .unwrap();
    let claim = evidence_in(&mut f.store, EvidenceKind::AgentClaim, Some("s"), T0 + 2).id;
    let mut x_new = new_memory("architettura/writer", MemoryKind::Fact, &[claim]);
    x_new.relations.push(Relation {
        kind: RelationKind::DerivedFrom,
        target: y.clone(),
    });
    let x = publish(&mut f.store, vec![MemoryOp::Create(x_new)]).created[0].clone();
    // Session B reads X and repeats it: refused, as is a direct create.
    f.store
        .record_served(
            Some("b"),
            None,
            Verb::Brief,
            &[Subject::Memory(x.clone())],
            T0 + 3,
        )
        .unwrap();
    let mut repeats = Vec::new();
    for kind in [
        EvidenceKind::AgentClaim,
        EvidenceKind::ReportedOutcome,
        EvidenceKind::AgentProposal,
    ] {
        repeats.push(evidence_in(&mut f.store, kind, Some("b"), T0 + 4).id);
    }
    let version = f.store.version().unwrap();
    let err = f
        .store
        .publish(publication(
            &f.store,
            vec![MemoryOp::SetStatus {
                id: x.clone(),
                status: MemoryStatus::Verified,
                sources: repeats.clone(),
            }],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::NotVerifiable(_)), "{err}");
    assert_eq!(f.store.version().unwrap(), version);
    assert_eq!(
        f.store.read().memory(&x).unwrap().unwrap().status,
        MemoryStatus::Candidate
    );
    // The same with a create that claims `verificato` at once.
    let mut direct = new_memory("architettura/echo", MemoryKind::Fact, &[claim]);
    direct.status = MemoryStatus::Verified;
    direct.relations.push(Relation {
        kind: RelationKind::DerivedFrom,
        target: y,
    });
    let err = f
        .store
        .publish(publication(&f.store, vec![MemoryOp::Create(direct)]))
        .unwrap_err();
    assert!(matches!(err, StoreError::NotVerifiable(_)));
    // The user saying so is independent.
    let said = user(&mut f.store);
    publish(
        &mut f.store,
        vec![MemoryOp::SetStatus {
            id: x.clone(),
            status: MemoryStatus::Verified,
            sources: vec![said],
        }],
    );
    let x_now = f.store.read().memory(&x).unwrap().unwrap();
    assert_eq!(x_now.status, MemoryStatus::Verified);
    // Status and freshness are separate: verifying did not touch freshness.
    assert_eq!(x_now.freshness.last_confirmed_ms, None);
    f.store.set_freshness(&x, Some(T0 + 9), true).unwrap();
    let x_now = f.store.read().memory(&x).unwrap().unwrap();
    assert_eq!(x_now.status, MemoryStatus::Verified);
    assert!(x_now.freshness.anchors_changed);
    assert_eq!(x_now.freshness.last_confirmed_ms, Some(T0 + 9));
}

#[test]
fn a_proposal_is_not_an_accepted_decision() {
    let mut f = Fixture::new();
    let proposal = evidence_in(&mut f.store, EvidenceKind::AgentProposal, Some("a"), T0).id;
    let mut decision = new_memory("decisioni/sqlite", MemoryKind::Decision, &[proposal]);
    decision.standing = Some(Standing::Accepted);
    let err = f
        .store
        .publish(publication(
            &f.store,
            vec![MemoryOp::Create(decision.clone())],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::StandingUnsupported(_)));
    // Proposed is fine; the user's word makes it accepted.
    decision.standing = None;
    let id = publish(&mut f.store, vec![MemoryOp::Create(decision)]).created[0].clone();
    assert_eq!(
        f.store.read().memory(&id).unwrap().unwrap().standing,
        Standing::Proposed
    );
    let said = user(&mut f.store);
    let mut update = connecto_memory::MemoryUpdate::new(id.clone());
    update.standing = Some(Standing::Accepted);
    update.add_sources = vec![said];
    publish(&mut f.store, vec![MemoryOp::Update(update)]);
    assert_eq!(
        f.store.read().memory(&id).unwrap().unwrap().standing,
        Standing::Accepted
    );
}

#[test]
fn annotation_verification_and_standing_are_checked() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let x = create(&mut f.store, "problemi/flaky", MemoryKind::Problem, &[e]);
    f.store
        .record_served(
            Some("b"),
            None,
            Verb::Read,
            &[Subject::Memory(x.clone())],
            T0 + 1,
        )
        .unwrap();
    let echo = evidence_in(&mut f.store, EvidenceKind::AgentClaim, Some("b"), T0 + 2).id;
    let mut n = new_note(
        AnnotationTarget::Memory(x),
        AnnotationKind::Integration,
        &[echo],
    );
    n.verification = Verification::VerifiedByEvidence;
    assert!(matches!(
        f.store.create_annotation(n.clone()),
        Err(StoreError::NotVerifiable(_))
    ));
    n.verification = Verification::UserStated;
    assert!(matches!(
        f.store.create_annotation(n.clone()),
        Err(StoreError::NotUserStated(_))
    ));
    n.verification = Verification::Unverified;
    n.memory_kind = Some(MemoryKind::Decision);
    n.standing = Some(Standing::Accepted);
    assert!(matches!(
        f.store.create_annotation(n.clone()),
        Err(StoreError::StandingUnsupported(_))
    ));
    n.standing = Some(Standing::Proposed);
    assert!(f.store.create_annotation(n).is_ok());
}

#[test]
fn memories_never_live_in_the_entrance_and_addresses_are_unique() {
    let mut f = Fixture::new();
    assert!(Address::parse("ingresso/brief").is_err());
    let e = user(&mut f.store);
    create(&mut f.store, "officina/build", MemoryKind::Procedure, &[e]);
    let err = f
        .store
        .publish(publication(
            &f.store,
            vec![MemoryOp::Create(new_memory(
                "officina/build",
                MemoryKind::Procedure,
                &[e],
            ))],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::AddressTaken(_)));
    // Two creates at one address in one publication.
    let err = f
        .store
        .publish(publication(
            &f.store,
            vec![
                MemoryOp::Create(new_memory("officina/test", MemoryKind::Procedure, &[e])),
                MemoryOp::Create(new_memory("officina/test", MemoryKind::Procedure, &[e])),
            ],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::AddressTaken(_)));
    // A memory without sources.
    let err = f
        .store
        .publish(publication(
            &f.store,
            vec![MemoryOp::Create(new_memory(
                "officina/none",
                MemoryKind::Procedure,
                &[],
            ))],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(MemoryError::NoSources)));
    // An id is never reused.
    let id = MemoryId::from_entropy([4; 5]);
    let mut m = new_memory("officina/one", MemoryKind::Procedure, &[e]);
    m.id = Some(id.clone());
    publish(&mut f.store, vec![MemoryOp::Create(m.clone())]);
    m.address = Address::parse("officina/two").unwrap();
    assert!(matches!(
        f.store
            .publish(publication(&f.store, vec![MemoryOp::Create(m)])),
        Err(StoreError::IdTaken(_))
    ));
    assert_eq!(f.store.read().memories().unwrap().len(), 2);
    let _ = PROJECT;
}
