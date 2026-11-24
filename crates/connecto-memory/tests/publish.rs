//! Atomic publication, redirects, revert, the cutoff, and crash atomicity
//! (spec §5.5, §11.2 "Crash e atomicità", "Cutoff").

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Fixture, PROJECT, T0, create, new_memory, note, publication, publish, red, user};
use connecto_core::memory::{
    Address, AnnotationKind, AnnotationTarget, Lookup, MemoryKind, MemoryStatus, ReadAnswer,
    RelationKind, Room,
};
use connecto_memory::{
    Absorb, Found, MemoryOp, MemoryUpdate, PublishStep, SkipReason, Store, StoreError,
};
use serde_json::Value;

fn address(text: &str) -> Address {
    Address::parse(text).unwrap()
}

fn found_id(store: &Store, lookup: &str) -> Option<(String, Option<String>)> {
    match store
        .read()
        .resolve(&Lookup::parse(lookup).unwrap())
        .unwrap()
    {
        Some(Found::Memory {
            memory,
            redirected_from,
        }) => Some((
            memory.id.to_string(),
            redirected_from.map(|a| a.to_string()),
        )),
        _ => None,
    }
}

#[test]
fn every_operation_applies_and_bumps_the_version_once() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let e2 = user(&mut f.store);
    let published = publish(
        &mut f.store,
        vec![
            MemoryOp::Create(new_memory("officina/build", MemoryKind::Procedure, &[e])),
            MemoryOp::Create(new_memory(
                "officina/build-old",
                MemoryKind::Procedure,
                &[e],
            )),
            MemoryOp::Create(new_memory("problemi/flaky", MemoryKind::Problem, &[e])),
            MemoryOp::Create(new_memory("problemi/flaky-dup", MemoryKind::Problem, &[e2])),
        ],
    );
    assert_eq!(published.version, 1);
    let [build, old, flaky, dup] = <[_; 4]>::try_from(published.created).unwrap();
    let mut update = MemoryUpdate::new(build.clone());
    update.summary = Some(red("Build with cargo build --workspace."));
    let v2 = publish(
        &mut f.store,
        vec![
            MemoryOp::Update(update),
            MemoryOp::Supersede {
                old: old.clone(),
                by: build.clone(),
            },
            MemoryOp::Merge {
                into: flaky.clone(),
                from: vec![dup.clone()],
            },
            MemoryOp::SetStatus {
                id: flaky.clone(),
                status: MemoryStatus::Conflict,
                sources: vec![],
            },
            MemoryOp::Link {
                from: flaky.clone(),
                kind: RelationKind::SeeAlso,
                to: build.clone(),
            },
            MemoryOp::Move {
                id: build.clone(),
                to: address("convenzioni/build"),
            },
            MemoryOp::Archive { id: old.clone() },
        ],
    );
    assert_eq!(v2.version, 2);
    assert_eq!(f.store.version().unwrap(), 2);
    let read = f.store.read();
    let b = read.memory(&build).unwrap().unwrap();
    assert_eq!(b.summary, "Build with cargo build --workspace.");
    assert_eq!(b.address, address("convenzioni/build"));
    assert_eq!(b.version, 2);
    assert!(
        b.relations
            .iter()
            .any(|r| r.kind == RelationKind::Supersedes && r.target == old)
    );
    let o = read.memory(&old).unwrap().unwrap();
    assert_eq!(o.status, MemoryStatus::Superseded);
    assert_eq!(o.address, address("archivio/build-old"));
    let fl = read.memory(&flaky).unwrap().unwrap();
    assert_eq!(fl.status, MemoryStatus::Conflict);
    assert_eq!(fl.sources.len(), 2, "the merged memory's sources join it");
    assert!(read.memory(&dup).unwrap().is_none(), "merged away");
    assert!(
        read.memory_row(&dup).unwrap().is_some(),
        "but never deleted"
    );
    // Old addresses and the merged id lead to the memory that holds them now.
    assert_eq!(
        found_id(&f.store, "officina/build"),
        Some((build.to_string(), Some("officina/build".to_owned())))
    );
    assert_eq!(
        found_id(&f.store, "officina/build-old"),
        Some((old.to_string(), Some("officina/build-old".to_owned())))
    );
    assert_eq!(
        found_id(&f.store, "problemi/flaky-dup"),
        Some((flaky.to_string(), Some("problemi/flaky-dup".to_owned())))
    );
    assert_eq!(
        found_id(&f.store, dup.as_str()).map(|(id, _)| id),
        Some(flaky.to_string())
    );
    assert_eq!(found_id(&f.store, "officina/nothing"), None);
    // Both versions are listed, newest first.
    let versions = f.store.read().versions(10).unwrap();
    assert_eq!(
        versions.iter().map(|v| v.version).collect::<Vec<_>>(),
        [2, 1]
    );
    assert_eq!(versions[0].operations, 7);
}

#[test]
fn two_memories_can_trade_addresses_in_one_publication() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let a = create(&mut f.store, "officina/a", MemoryKind::Procedure, &[e]);
    let b = create(&mut f.store, "officina/b", MemoryKind::Procedure, &[e]);
    let tmp = address("officina/tmp");
    publish(
        &mut f.store,
        vec![
            MemoryOp::Move {
                id: a.clone(),
                to: tmp,
            },
            MemoryOp::Move {
                id: b.clone(),
                to: address("officina/a"),
            },
            MemoryOp::Move {
                id: a.clone(),
                to: address("officina/b"),
            },
        ],
    );
    let read = f.store.read();
    assert_eq!(
        read.memory(&a).unwrap().unwrap().address,
        address("officina/b")
    );
    assert_eq!(
        read.memory(&b).unwrap().unwrap().address,
        address("officina/a")
    );
}

#[test]
fn a_failed_publication_changes_nothing() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let a = create(&mut f.store, "officina/a", MemoryKind::Procedure, &[e]);
    let before = f.store.read().memory(&a).unwrap();
    let mut update = MemoryUpdate::new(a.clone());
    update.summary = Some(red("changed"));
    let err = f
        .store
        .publish(publication(
            &f.store,
            vec![
                MemoryOp::Update(update),
                MemoryOp::Archive {
                    id: connecto_core::memory::MemoryId::from_entropy([7; 5]),
                },
            ],
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownMemory(_)));
    assert_eq!(f.store.read().memory(&a).unwrap(), before);
    assert_eq!(f.store.version().unwrap(), 1);
    // A stale base version is refused.
    let mut p = publication(&f.store, vec![MemoryOp::Archive { id: a }]);
    p.base_version = Some(0);
    assert!(matches!(
        f.store.publish(p),
        Err(StoreError::StaleBase {
            expected: 0,
            found: 1
        })
    ));
    assert!(matches!(
        f.store.publish(publication(&f.store, vec![])),
        Err(StoreError::EmptyPublication)
    ));
}

#[test]
fn only_annotations_up_to_the_cutoff_are_absorbed() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let m = create(&mut f.store, "decisioni/sqlite", MemoryKind::Decision, &[e]);
    let before = note(
        &mut f.store,
        AnnotationTarget::Memory(m.clone()),
        AnnotationKind::Integration,
        &[e],
    );
    // The dream fixes its cutoff here; then a note arrives while it runs.
    let cutoff = f.store.read().annotation_seq().unwrap();
    let during = note(
        &mut f.store,
        AnnotationTarget::Memory(m.clone()),
        AnnotationKind::Contradiction,
        &[e],
    );
    let mut update = MemoryUpdate::new(m.clone());
    update.details = Some(red("Absorbed the note."));
    let mut p = publication(&f.store, vec![MemoryOp::Update(update)]);
    p.cutoff = cutoff;
    p.absorb = vec![
        Absorb {
            annotation: before.id.clone(),
            into: Some(m.clone()),
        },
        Absorb {
            annotation: during.id.clone(),
            into: Some(m.clone()),
        },
    ];
    let published = f.store.publish(p).unwrap();
    assert_eq!(published.absorbed, vec![before.id.clone()]);
    assert_eq!(
        published.skipped,
        vec![(during.id.clone(), SkipReason::AfterCutoff)]
    );
    let read = f.store.read();
    assert_eq!(
        read.annotation(&before.id).unwrap().unwrap().absorbed_in,
        Some(published.version)
    );
    // The late note is open and intact.
    assert_eq!(read.annotation(&during.id).unwrap().unwrap(), during);
    assert_eq!(read.open_annotations().unwrap(), vec![during.clone()]);
    // An absorbed annotation joins the memory's lineage.
    assert!(
        read.lineage(&m)
            .unwrap()
            .contains(&connecto_core::memory::Subject::Annotation(before.id))
    );
}

#[test]
fn revert_restores_images_reopens_absorbed_notes_and_is_itself_a_version() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let a = create(&mut f.store, "officina/a", MemoryKind::Procedure, &[e]);
    let b = create(&mut f.store, "officina/b", MemoryKind::Procedure, &[e]);
    let n = note(
        &mut f.store,
        AnnotationTarget::Memory(a.clone()),
        AnnotationKind::Integration,
        &[e],
    );
    let image_a = f.store.read().memory(&a).unwrap().unwrap();
    let image_b = f.store.read().memory(&b).unwrap().unwrap();
    let mut update = MemoryUpdate::new(a.clone());
    update.summary = Some(red("rewritten"));
    let mut p = publication(
        &f.store,
        vec![
            MemoryOp::Update(update),
            MemoryOp::Merge {
                into: a.clone(),
                from: vec![b.clone()],
            },
            MemoryOp::Create(new_memory("officina/c", MemoryKind::Procedure, &[e])),
            MemoryOp::Move {
                id: a.clone(),
                to: address("convenzioni/a"),
            },
        ],
    );
    p.absorb = vec![Absorb {
        annotation: n.id.clone(),
        into: Some(a.clone()),
    }];
    let v3 = f.store.publish(p).unwrap();
    let c = v3.created[0].clone();
    // A note that arrives after the version: revert leaves it alone.
    let late = note(
        &mut f.store,
        AnnotationTarget::Memory(a.clone()),
        AnnotationKind::Task,
        &[e],
    );
    let reverted = f.store.revert(v3.version, T0 + 5000).unwrap();
    assert_eq!(reverted.version, v3.version + 1);
    assert_eq!(reverted.reopened, vec![n.id.clone()]);
    let read = f.store.read();
    let a_now = read.memory(&a).unwrap().unwrap();
    assert_eq!(a_now.summary, image_a.summary);
    assert_eq!(a_now.address, image_a.address);
    assert_eq!(a_now.sources, image_a.sources);
    assert_eq!(a_now.version, reverted.version);
    let b_now = read.memory(&b).unwrap().unwrap();
    assert_eq!(b_now.address, image_b.address);
    assert!(read.memory(&c).unwrap().is_none(), "withdrawn");
    assert!(read.memory_row(&c).unwrap().is_some(), "never deleted");
    assert!(read.annotation(&n.id).unwrap().unwrap().is_open());
    assert_eq!(read.annotation(&late.id).unwrap().unwrap(), late);
    // Addresses lead where they did before the version.
    assert_eq!(found_id(&f.store, "officina/a").unwrap().0, a.to_string());
    assert_eq!(found_id(&f.store, "officina/b").unwrap().0, b.to_string());
    assert_eq!(found_id(&f.store, "convenzioni/a"), None);
    // Reverting again is refused; reverting the revert brings v3 back.
    assert!(matches!(
        f.store.revert(v3.version, T0 + 6000),
        Err(StoreError::RevertConflict { .. })
    ));
    let again = f.store.revert(reverted.version, T0 + 7000).unwrap();
    assert_eq!(
        f.store.read().memory(&a).unwrap().unwrap().summary,
        "rewritten"
    );
    assert!(f.store.read().memory(&c).unwrap().is_some());
    assert!(!f.store.read().annotation(&n.id).unwrap().unwrap().is_open());
    let versions = f.store.read().versions(10).unwrap();
    assert_eq!(versions[0].version, again.version);
    assert_eq!(versions[0].reverts, Some(reverted.version));
    assert!(matches!(
        f.store.revert(99, T0),
        Err(StoreError::UnknownVersion(99))
    ));
}

#[test]
fn revert_is_refused_when_a_later_version_changed_the_same_memory() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let a = create(&mut f.store, "officina/a", MemoryKind::Procedure, &[e]);
    let mut u = MemoryUpdate::new(a.clone());
    u.details = Some(red("v2"));
    let v2 = publish(&mut f.store, vec![MemoryOp::Update(u.clone())]).version;
    u.details = Some(red("v3"));
    publish(&mut f.store, vec![MemoryOp::Update(u)]);
    assert!(matches!(
        f.store.revert(v2, T0),
        Err(StoreError::RevertConflict { .. })
    ));
}

/// The state that must survive a crash: version, every memory row, every
/// annotation and redirect.
fn state(db: &std::path::Path) -> Value {
    let store = Store::open(db, PROJECT).unwrap();
    let json = connecto_memory::export::to_json(&store.read(), 0).unwrap();
    serde_json::json!({
        "version": json["memory_version"],
        "memories": json["memories"],
        "annotations": json["annotations"],
        "redirects": json["redirects"],
        "relations": json["relations"],
    })
}

/// A store at version 1 with an annotation, and a second publication that
/// touches every kind of row.
fn crash_setup(db: &std::path::Path) {
    let mut store = Store::open(db, PROJECT).unwrap();
    let e = user(&mut store);
    publish(
        &mut store,
        vec![
            MemoryOp::Create(new_memory("officina/a", MemoryKind::Procedure, &[e])),
            MemoryOp::Create(new_memory("officina/b", MemoryKind::Procedure, &[e])),
        ],
    );
    let a = store.read().memories().unwrap()[0].id.clone();
    note(
        &mut store,
        AnnotationTarget::Memory(a),
        AnnotationKind::Integration,
        &[e],
    );
}

fn crash_publication(store: &Store) -> connecto_memory::Publication {
    let mems = store.read().memories().unwrap();
    let (a, b) = (mems[0].id.clone(), mems[1].id.clone());
    let e = mems[0].sources[0].evidence;
    let n = store.read().open_annotations().unwrap()[0].id.clone();
    let mut update = MemoryUpdate::new(a.clone());
    update.summary = Some(red("new summary"));
    let mut p = publication(
        store,
        vec![
            MemoryOp::Update(update),
            MemoryOp::Create(new_memory("problemi/new", MemoryKind::Problem, &[e])),
            MemoryOp::Merge {
                into: a.clone(),
                from: vec![b],
            },
            MemoryOp::Move {
                id: a.clone(),
                to: address("convenzioni/a"),
            },
        ],
    );
    p.absorb = vec![Absorb {
        annotation: n,
        into: Some(a),
    }];
    p
}

/// Run in a child process by the test below: publish and stop the process
/// dead at the step named by the environment, with the transaction open.
#[test]
fn crash_child() {
    let (Ok(db), Ok(step)) = (
        std::env::var("CONNECTO_MEMORY_CRASH_DB"),
        std::env::var("CONNECTO_MEMORY_CRASH_STEP"),
    ) else {
        return;
    };
    let mut store = Store::open(std::path::Path::new(&db), PROJECT).unwrap();
    let p = crash_publication(&store);
    let target = PublishStep::all(p.ops.len())[step.parse::<usize>().unwrap()];
    let _ = store.publish_with(p, &mut |s| {
        if s == target {
            // No destructor, no rollback: as if the process were killed.
            std::process::exit(77);
        }
        false
    });
    std::process::exit(78);
}

#[test]
fn a_crash_at_any_point_of_a_publication_leaves_the_old_version() {
    let probe = tempfile::tempdir().unwrap();
    crash_setup(&probe.path().join("memory.db"));
    let steps = {
        let store = Store::open(&probe.path().join("memory.db"), PROJECT).unwrap();
        PublishStep::all(crash_publication(&store).ops.len())
    };
    let exe = std::env::current_exe().unwrap();
    for (index, step) in steps.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        crash_setup(&db);
        let before = state(&db);
        let status = std::process::Command::new(&exe)
            .args(["crash_child", "--exact", "--nocapture", "--test-threads=1"])
            .env("CONNECTO_MEMORY_CRASH_DB", &db)
            .env("CONNECTO_MEMORY_CRASH_STEP", index.to_string())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(77), "the child stopped at {step:?}");
        assert_eq!(state(&db), before, "after a crash at {step:?}");
        // And the publication still goes through afterwards.
        let mut store = Store::open(&db, PROJECT).unwrap();
        let p = crash_publication(&store);
        assert_eq!(store.publish(p).unwrap().version, 2);
    }
}

#[test]
fn an_abandoned_publication_at_each_step_leaves_old_or_new() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("memory.db");
    crash_setup(&db);
    let before = state(&db);
    let steps = PublishStep::all(
        crash_publication(&Store::open(&db, PROJECT).unwrap())
            .ops
            .len(),
    );
    for step in &steps {
        let mut store = Store::open(&db, PROJECT).unwrap();
        let p = crash_publication(&store);
        let err = store.publish_with(p, &mut |s| s == *step).unwrap_err();
        assert!(matches!(err, StoreError::Aborted));
        drop(store);
        assert_eq!(state(&db), before, "stopped at {step:?}");
    }
    let mut store = Store::open(&db, PROJECT).unwrap();
    let p = crash_publication(&store);
    store.publish(p).unwrap();
    drop(store);
    let after = state(&db);
    assert_eq!(after["version"], 2);
    assert_ne!(after, before);
    // Revert takes it back to exactly the old state, as a new version.
    let mut store = Store::open(&db, PROJECT).unwrap();
    store.revert(2, T0).unwrap();
    drop(store);
    let reverted = state(&db);
    assert_eq!(reverted["version"], 3);
    let strip = |v: &Value| {
        let mut v = v.clone();
        for m in v["memories"].as_array_mut().unwrap() {
            if let Some(o) = m.as_object_mut() {
                o.remove("version");
                o.remove("updated_ms");
            }
        }
        v.as_object_mut().unwrap().remove("version");
        v
    };
    // Same rows, except the memory created by v2, which is withdrawn.
    let mut expected = strip(&before);
    let mut got = strip(&reverted);
    let created: Vec<Value> = got["memories"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["live"] == false && m["merged_into"].is_null())
        .cloned()
        .collect();
    assert_eq!(created.len(), 1);
    got["memories"]
        .as_array_mut()
        .unwrap()
        .retain(|m| !(m["live"] == false && m["merged_into"].is_null()));
    expected["redirects"] = got["redirects"].clone();
    assert_eq!(got, expected);
    assert!(
        got["redirects"].as_array().unwrap().is_empty(),
        "the redirects v2 made are gone with it"
    );
    let _ = Room::Archive;
    let _ = ReadAnswer::NotFound;
}

/// What `then` writes commits with the publication or not at all: a pause
/// that arrives while it runs aborts the commit (the gate is asked again
/// after it), and a failing `then` rolls the publication back.
#[test]
fn work_done_with_a_publication_commits_with_it_or_not_at_all() {
    use std::cell::Cell;
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let mark = |conn: &rusqlite::Connection| {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('test.then', 'written')",
            [],
        )
        .map(|_| ())
        .map_err(StoreError::from)
    };
    let written = |store: &mut Store| {
        store
            .connection_mut()
            .query_row(
                "SELECT COUNT(*) FROM meta WHERE key = 'test.then'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };

    // A pause set during `then`: aborted, nothing written.
    let paused = Cell::new(false);
    let p = publication(
        &f.store,
        vec![MemoryOp::Create(new_memory(
            "officina/a",
            MemoryKind::Procedure,
            &[e],
        ))],
    );
    let result = f.store.publish_with_then(
        p,
        &mut |step| step == PublishStep::Commit && paused.get(),
        &mut |conn, _| {
            paused.set(true);
            mark(conn)
        },
    );
    assert!(matches!(result, Err(StoreError::Aborted)), "{result:?}");
    assert_eq!(f.store.version().unwrap(), 0);
    assert_eq!(written(&mut f.store), 0);

    // A failing `then`: its error, nothing written.
    let p = publication(
        &f.store,
        vec![MemoryOp::Create(new_memory(
            "officina/a",
            MemoryKind::Procedure,
            &[e],
        ))],
    );
    let result = f
        .store
        .publish_with_then(p, &mut |_| false, &mut |conn, _| {
            mark(conn)?;
            Err(StoreError::Corrupt("then failed"))
        });
    assert!(
        matches!(result, Err(StoreError::Corrupt("then failed"))),
        "{result:?}"
    );
    assert_eq!(f.store.version().unwrap(), 0);
    assert_eq!(written(&mut f.store), 0);
    assert!(f.store.read().memories().unwrap().is_empty());

    // Otherwise both commit together.
    let p = publication(
        &f.store,
        vec![MemoryOp::Create(new_memory(
            "officina/a",
            MemoryKind::Procedure,
            &[e],
        ))],
    );
    f.store
        .publish_with_then(p, &mut |_| false, &mut |conn, _| mark(conn))
        .unwrap();
    assert_eq!(f.store.version().unwrap(), 1);
    assert_eq!(written(&mut f.store), 1);
}
