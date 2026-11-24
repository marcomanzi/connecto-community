//! Regressions from backend review round 2 of T2, and their siblings.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::{
    Fixture, T0, create, evidence_in, new_memory, new_note, note, publication, publish, user,
};
use connecto_core::memory::{
    Anchor, AnnotationKind, AnnotationTarget, EvidenceKind, LocalDate, Lookup, MemoryError,
    MemoryId, MemoryKind, MemoryStatus, Room, SearchHit, Standing, Subject, Verb, Verification,
};
use connecto_memory::{MemoryOp, MemoryUpdate, ScheduleState, SearchQuery, Store, StoreError};

const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";

/// `n` memories `officina/m-<i>`, created in one publication.
fn many(store: &mut Store, n: usize, status: MemoryStatus) -> Vec<MemoryId> {
    let e = user(store);
    let ops = (0..n)
        .map(|i| {
            let mut m = new_memory(&format!("officina/m-{i}"), MemoryKind::Fact, &[e]);
            m.status = status;
            MemoryOp::Create(m)
        })
        .collect();
    publish(store, ops).created
}

#[test]
fn finding_1_merge_chains_longer_than_32_are_followed_to_the_end() {
    let mut f = Fixture::new();
    let chain = many(&mut f.store, 35, MemoryStatus::Candidate);
    let last = chain.last().unwrap().clone();
    // A session read the final survivor, then its claim verified a note on
    // the first memory of the chain (independent at the time).
    f.store
        .record_served(
            Some("s"),
            None,
            Verb::Read,
            &[Subject::Memory(last.clone())],
            T0 + 1,
        )
        .unwrap();
    let claim = evidence_in(&mut f.store, EvidenceKind::AgentClaim, Some("s"), T0 + 2).id;
    let mut n = new_note(
        AnnotationTarget::Memory(chain[0].clone()),
        AnnotationKind::Integration,
        &[claim],
    );
    n.verification = Verification::VerifiedByEvidence;
    n.memory_kind = Some(MemoryKind::Fact);
    n.standing = Some(Standing::Established);
    let n = f.store.create_annotation(n).unwrap();
    // 33 merges: chain[0] -> chain[1] -> ... -> chain[33].
    for i in 0..33 {
        publish(
            &mut f.store,
            vec![MemoryOp::Merge {
                into: chain[i + 1].clone(),
                from: vec![chain[i].clone()],
            }],
        );
    }
    // Reads follow all 33 hops.
    let read = f.store.read();
    assert_eq!(read.merge_end(&chain[0]).unwrap(), chain[33]);
    match read.resolve(&Lookup::Memory(chain[0].clone())).unwrap() {
        Some(connecto_memory::Found::Memory { memory, .. }) => assert_eq!(memory.id, chain[33]),
        other => panic!("{other:?}"),
    }
    assert!(
        read.annotations_on(&chain[33], true)
            .unwrap()
            .iter()
            .any(|a| a.id == n.id)
    );
    // The 34th merge brings the survivor the session had read into the
    // note's lineage: refused, however long the chain.
    let err = f
        .store
        .publish(publication(
            &f.store,
            vec![MemoryOp::Merge {
                into: last.clone(),
                from: vec![chain[33].clone()],
            }],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::NotVerifiable(_)), "{err}");
    // Search follows the whole chain to the room: archive the end and the
    // note on the first memory leaves default search.
    publish(
        &mut f.store,
        vec![MemoryOp::Archive {
            id: chain[33].clone(),
        }],
    );
    let mut q = SearchQuery::new("note");
    assert!(f.store.read().search(&q).unwrap().is_empty());
    q.all = true;
    assert!(
        matches!(&f.store.read().search(&q).unwrap()[..], [SearchHit::Annotation(a)] if a.id == n.id)
    );
}

#[test]
fn finding_1_a_merge_cycle_fails_closed() {
    let mut f = Fixture::new();
    let ids = many(&mut f.store, 2, MemoryStatus::Candidate);
    // No write path makes a cycle; forge one to prove reads end and refuse.
    let conn = f.store.connection_mut();
    for (a, b) in [(&ids[0], &ids[1]), (&ids[1], &ids[0])] {
        conn.execute(
            "UPDATE memory SET live = 0, merged_into = ?2 WHERE id = ?1",
            [a.as_str(), b.as_str()],
        )
        .unwrap();
    }
    assert!(matches!(
        f.store.read().merge_end(&ids[0]),
        Err(StoreError::Corrupt("merge cycle"))
    ));
    assert!(
        f.store
            .read()
            .resolve(&Lookup::Memory(ids[0].clone()))
            .is_err()
    );
    // Search over a cycle ends.
    let mut q = SearchQuery::new("summary");
    q.all = true;
    f.store.read().search(&q).unwrap();
}

#[test]
fn finding_2_every_written_row_is_validated_even_when_merged() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let keep = create(&mut f.store, "officina/keep", MemoryKind::Fact, &[e]);
    let gone = MemoryId::parse("m-aaaaaaaa").unwrap();
    let mut bad = new_memory("officina/bad", MemoryKind::Fact, &[e]);
    bad.id = Some(gone.clone());
    bad.anchors = vec![Anchor {
        path: "../outside".to_owned(),
        blob: "invalid".to_owned(),
    }];
    let err = f
        .store
        .publish(publication(
            &f.store,
            vec![
                MemoryOp::Create(bad),
                MemoryOp::Merge {
                    into: keep.clone(),
                    from: vec![gone.clone()],
                },
            ],
        ))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(MemoryError::BadPath)),
        "{err}"
    );
    assert!(f.store.read().memory_row(&gone).unwrap().is_none());
    // Two anchors for one path would collapse on disk: refused.
    let mut u = MemoryUpdate::new(keep.clone());
    u.anchors = Some(vec![
        Anchor::new("src/lib.rs", BLOB).unwrap(),
        Anchor::new("src/lib.rs", &"a".repeat(40)).unwrap(),
    ]);
    assert!(matches!(
        f.store
            .publish(publication(&f.store, vec![MemoryOp::Update(u)])),
        Err(StoreError::Invalid(MemoryError::BadPath))
    ));
    // Every row in the store reads back.
    for row in f.store.read().memory_rows().unwrap() {
        assert!(f.store.read().memory_row(&row.memory.id).unwrap().is_some());
    }
}

#[test]
fn finding_2_sibling_dates_that_would_not_read_back_are_refused() {
    let mut f = Fixture::new();
    let bad = LocalDate {
        year: 2026,
        month: 13,
        day: 1,
    };
    assert!(matches!(
        f.store.claim_schedule("dream", bad, None, T0),
        Err(StoreError::BadDate)
    ));
    assert!(matches!(
        f.store
            .set_schedule("dream", bad, &ScheduleState::Done, None, None, T0),
        Err(StoreError::BadDate)
    ));
    let choice = connecto_core::memory::ModelChoice::new(
        connecto_core::memory::Provider::Claude,
        "haiku",
        "low",
    );
    let run = f
        .store
        .start_run(connecto_core::memory::Cycle::Dream, &choice, T0, 0)
        .unwrap();
    assert!(matches!(
        f.store.set_run_recovers(run, &[bad]),
        Err(StoreError::BadDate)
    ));
    assert!(f.store.schedule("dream").unwrap().is_empty());
    assert!(f.store.run(run).unwrap().is_some());
}

#[test]
fn finding_3_lineage_queries_use_indexes() {
    let f = Fixture::new();
    let conn = f.store.read().connection();
    for sql in [
        "SELECT id FROM memory WHERE merged_into = 'm-aaaaaaaa'",
        "SELECT id FROM annotation WHERE absorbed_into = 'm-aaaaaaaa'",
        "SELECT id FROM memory WHERE merged_into = 'm-aaaaaaaa'
         UNION SELECT to_id FROM relation WHERE from_id = 'm-aaaaaaaa' AND kind = 'deriva_da'",
    ] {
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().all(
                |step| !step.starts_with("SCAN memory") && !step.starts_with("SCAN annotation")
            ),
            "{sql}: {plan:?}"
        );
    }
}

/// One-row update with `n` verified memories in the store.
fn one_update(n: usize) -> Duration {
    let mut f = Fixture::new();
    let ids = many(&mut f.store, n, MemoryStatus::Verified);
    let mut u = MemoryUpdate::new(ids[n / 2].clone());
    u.details = Some(common::red("edited"));
    let start = Instant::now();
    f.store
        .publish(publication(&f.store, vec![MemoryOp::Update(u)]))
        .unwrap();
    start.elapsed()
}

#[test]
fn finding_3_the_store_wide_check_scales_linearly() {
    // Debug build, generous bounds: catches a return to per-check scans.
    let small = one_update(500);
    let large = one_update(2000);
    assert!(large < Duration::from_secs(5), "{large:?}");
    // Four times the rows: far under the sixteen times of a quadratic check.
    assert!(
        large < small * 10 + Duration::from_millis(200),
        "{small:?} -> {large:?}"
    );
}

/// Release timings for the report: `cargo test --release -p connecto-memory
/// --test review2 -- --ignored --nocapture`.
#[test]
#[ignore = "timing report, run in release by hand"]
fn finding_3_timings() {
    for n in [1000, 3000, 6000] {
        println!("{n} verified memories: one-row update {:?}", one_update(n));
    }
}

#[test]
fn global_check_still_refuses_after_the_optimisation() {
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
    f.store
        .record_served(
            Some("s"),
            None,
            Verb::Search,
            &[Subject::Annotation(p.id.clone())],
            T0 + 1,
        )
        .unwrap();
    let claim = evidence_in(&mut f.store, EvidenceKind::AgentClaim, Some("s"), T0 + 2).id;
    let mut x = new_memory("officina/x", MemoryKind::Fact, &[claim]);
    x.status = MemoryStatus::Verified;
    x.standing = Some(Standing::Established);
    let x = publish(&mut f.store, vec![MemoryOp::Create(x)]).created[0].clone();
    let mut pubn = publication(
        &f.store,
        vec![MemoryOp::SetStatus {
            id: x.clone(),
            status: MemoryStatus::ToVerify,
            sources: vec![],
        }],
    );
    pubn.absorb = vec![connecto_memory::Absorb {
        annotation: p.id,
        into: Some(x),
    }];
    // Status lowered, but the standing still claims more than the evidence.
    assert!(matches!(
        f.store.publish(pubn),
        Err(StoreError::StandingUnsupported(_))
    ));
}
