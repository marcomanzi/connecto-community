//! Reads: search, room index, the single view, counts, and reading while the
//! writer writes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::{Fixture, PROJECT, create, new_memory, note, publication, publish, red, user};
use connecto_core::memory::{
    AnnotationKind, AnnotationTarget, Lookup, MemoryKind, MemoryStatus, ReadAnswer, Room, Scope,
    SearchHit,
};
use connecto_memory::{MemoryOp, MemoryUpdate, Reader, SearchQuery, Writer};

fn ids(hits: &[SearchHit]) -> Vec<String> {
    hits.iter()
        .map(|h| match h {
            SearchHit::Memory(card) => card.id.to_string(),
            SearchHit::Annotation(a) => a.id.to_string(),
        })
        .collect()
}

#[test]
fn search_finds_memories_and_notes_with_filters_and_without_the_archive() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let mut release = new_memory("officina/rilascio-macos", MemoryKind::Procedure, &[e]);
    release.summary = red("Notarize the macOS release after signing.");
    let mut windows = new_memory("problemi/windows-build", MemoryKind::Problem, &[e]);
    windows.summary = red("The Windows build needs MSVC.");
    windows.scopes = vec![Scope::Branch("win".to_owned())];
    let mut old = new_memory("officina/rilascio-vecchio", MemoryKind::Procedure, &[e]);
    old.summary = red("Old macOS release steps without notarization.");
    let created = publish(
        &mut f.store,
        vec![
            MemoryOp::Create(release),
            MemoryOp::Create(windows),
            MemoryOp::Create(old),
        ],
    )
    .created;
    let (release, windows, old) = (created[0].clone(), created[1].clone(), created[2].clone());
    publish(&mut f.store, vec![MemoryOp::Archive { id: old.clone() }]);
    let n = note(
        &mut f.store,
        AnnotationTarget::New {
            room: Room::Workshop,
        },
        AnnotationKind::Integration,
        &[e],
    );
    let read = f.store.read();
    // Accents and case do not matter; query syntax in the text is inert.
    let hits = read
        .search(&SearchQuery::new("MACOS \"release\" OR ("))
        .unwrap();
    assert_eq!(ids(&hits), [release.to_string()]);
    let mut all = SearchQuery::new("macos");
    all.all = true;
    let hits = read.search(&all).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(ids(&hits).contains(&old.to_string()));
    let mut in_room = SearchQuery::new("archivio macos");
    in_room.room = Some(Room::Archive);
    assert_eq!(ids(&read.search(&in_room).unwrap()), [old.to_string()]);
    let mut scoped = SearchQuery::new("build windows");
    scoped.scope = Some(Scope::Branch("main".to_owned()));
    assert!(read.search(&scoped).unwrap().is_empty());
    scoped.scope = Some(Scope::Branch("win".to_owned()));
    assert_eq!(ids(&read.search(&scoped).unwrap()), [windows.to_string()]);
    // Notes are found too.
    assert_eq!(
        ids(&read.search(&SearchQuery::new("note")).unwrap()),
        [n.id.to_string()]
    );
    assert!(read.search(&SearchQuery::new("   ")).unwrap().is_empty());
    // Search follows edits.
    let mut update = MemoryUpdate::new(release.clone());
    update.summary = Some(red("Staple the ticket."));
    publish(&mut f.store, vec![MemoryOp::Update(update)]);
    let read = f.store.read();
    assert!(
        read.search(&SearchQuery::new("notarize"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ids(&read.search(&SearchQuery::new("staple")).unwrap()),
        [release.to_string()]
    );
}

#[test]
fn the_single_view_puts_open_notes_beside_each_memory_contradictions_first() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let m = create(&mut f.store, "decisioni/sqlite", MemoryKind::Decision, &[e]);
    let other = create(&mut f.store, "decisioni/json", MemoryKind::Decision, &[e]);
    let plus = note(
        &mut f.store,
        AnnotationTarget::Memory(m.clone()),
        AnnotationKind::Integration,
        &[e],
    );
    let against = note(
        &mut f.store,
        AnnotationTarget::Memory(m.clone()),
        AnnotationKind::Contradiction,
        &[e],
    );
    let read = f.store.read();
    let room = read.room(Room::Decisions).unwrap();
    assert_eq!(room.len(), 2);
    let card = room.iter().find(|c| c.id == m).unwrap();
    assert_eq!(
        card.open.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
        [against.id.clone(), plus.id.clone()]
    );
    assert_eq!(card.freshness.open_annotations, 2);
    assert_eq!(card.status, MemoryStatus::Candidate);
    assert_eq!(card.sources.len(), 1);
    assert!(room.iter().find(|c| c.id == other).unwrap().open.is_empty());
    // Search shows the same card.
    match &read.search(&SearchQuery::new("sqlite")).unwrap()[0] {
        SearchHit::Memory(c) => assert_eq!(c, card),
        other => panic!("{other:?}"),
    }
    // `read` lists every note, open ones first, contradictions first.
    match read
        .detail(&Lookup::parse("decisioni/sqlite").unwrap())
        .unwrap()
    {
        ReadAnswer::Memory(detail) => {
            assert_eq!(detail.annotations[0].id, against.id);
            assert_eq!(detail.evidence.len(), 1);
            assert_eq!(detail.redirected_from, None);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        read.detail(&Lookup::parse("decisioni/none").unwrap())
            .unwrap(),
        ReadAnswer::NotFound
    );
    assert!(matches!(
        read.detail(&Lookup::Annotation(plus.id)).unwrap(),
        ReadAnswer::Annotation(_)
    ));
    assert!(read.room(Room::Entrance).unwrap().is_empty());
}

#[test]
fn counts_by_room_and_status() {
    let mut f = Fixture::new();
    let e = user(&mut f.store);
    let a = create(&mut f.store, "officina/a", MemoryKind::Procedure, &[e]);
    create(&mut f.store, "officina/b", MemoryKind::Procedure, &[e]);
    create(&mut f.store, "problemi/c", MemoryKind::Problem, &[e]);
    publish(
        &mut f.store,
        vec![MemoryOp::SetStatus {
            id: a.clone(),
            status: MemoryStatus::Verified,
            sources: vec![],
        }],
    );
    note(
        &mut f.store,
        AnnotationTarget::Memory(a),
        AnnotationKind::Contradiction,
        &[e],
    );
    let counts = f.store.read().counts().unwrap();
    assert_eq!(counts.rooms.len(), Room::ALL.len() - 1);
    let workshop = counts
        .rooms
        .iter()
        .find(|r| r.room == Room::Workshop)
        .unwrap();
    assert_eq!(workshop.counts.verified, 1);
    assert_eq!(workshop.counts.candidate, 1);
    assert_eq!(counts.totals.total(), 3);
    assert_eq!(counts.open_annotations, 1);
    assert_eq!(counts.open_contradictions, 1);
    let _ = publication;
}

#[test]
fn a_reader_reads_while_the_writer_holds_a_write_transaction() {
    let f = Fixture::new();
    let db = f.db();
    drop(f.store);
    let writer = Writer::spawn(&db, PROJECT, 8).unwrap();
    let e = writer.call(common::user).unwrap();
    let id = writer
        .call(move |s| common::create(s, "officina/a", MemoryKind::Procedure, &[e]))
        .unwrap();
    let reader = Reader::open(&db, PROJECT).unwrap();
    // A snapshot opened now keeps seeing version 1 throughout.
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(snapshot.read().version().unwrap(), 1);

    let (held_tx, held_rx) = crossbeam_channel::bounded::<()>(0);
    let (go_tx, go_rx) = crossbeam_channel::bounded::<()>(0);
    let handle = writer.handle();
    let id2 = id.clone();
    let job = std::thread::spawn(move || {
        handle
            .call(move |store| {
                let conn = store.connection_mut();
                let tx = conn.transaction().unwrap();
                tx.execute(
                    "UPDATE memory SET summary = 'uncommitted' WHERE id = ?1",
                    [id2.as_str()],
                )
                .unwrap();
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                tx.commit().unwrap();
            })
            .unwrap();
    });
    held_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    // The writer is mid-transaction; the reader neither waits nor sees it.
    let other = Reader::open(&db, PROJECT).unwrap();
    let started = std::time::Instant::now();
    let summary = other.read().memory(&id).unwrap().unwrap().summary;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(summary, "Summary of officina/a");
    go_tx.send(()).unwrap();
    job.join().unwrap();
    assert_eq!(
        other.read().memory(&id).unwrap().unwrap().summary,
        "uncommitted"
    );

    // A publication while the snapshot is open: it still sees version 1.
    let id3 = id.clone();
    writer
        .call(move |s| publish(s, vec![MemoryOp::Archive { id: id3 }]))
        .unwrap();
    assert_eq!(snapshot.read().version().unwrap(), 1);
    assert_eq!(
        snapshot.read().memory(&id).unwrap().unwrap().address.room(),
        Room::Workshop
    );
    drop(snapshot);
    let fresh = reader.snapshot().unwrap();
    assert_eq!(fresh.read().version().unwrap(), 2);
    assert_eq!(
        fresh.read().memory(&id).unwrap().unwrap().address.room(),
        Room::Archive
    );
}
