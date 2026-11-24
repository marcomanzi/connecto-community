//! Export (spec §8.5, T12): complete JSON and Markdown, no secrets.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Fixture, T0, create, note, publish};
use connecto_core::memory::{
    AnnotationKind, AnnotationTarget, EvidenceKind, ExportFormat, MemoryKind, Room,
};
use connecto_memory::{MemoryOp, MemoryPaths, NewEvidence, export};

const SECRET: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

#[test]
fn json_and_markdown_hold_everything_and_no_secret() {
    let mut f = Fixture::new();
    // Text is redacted before it is stored; the export writes what is stored.
    let raw = format!("I set ANTHROPIC_API_KEY={SECRET} and ran the tests");
    let excerpt = connecto_redact::redact(&raw, &[SECRET.to_owned()]).unwrap();
    assert!(!excerpt.get().contains(SECRET));
    let e = f
        .store
        .insert_evidence(NewEvidence::new(EvidenceKind::UserStatement, T0, excerpt))
        .unwrap()
        .id;
    let a = create(&mut f.store, "officina/test", MemoryKind::Procedure, &[e]);
    let b = create(&mut f.store, "problemi/flaky", MemoryKind::Problem, &[e]);
    publish(
        &mut f.store,
        vec![MemoryOp::Link {
            from: b.clone(),
            kind: connecto_core::memory::RelationKind::SeeAlso,
            to: a.clone(),
        }],
    );
    let against = note(
        &mut f.store,
        AnnotationTarget::Memory(a.clone()),
        AnnotationKind::Contradiction,
        &[e],
    );
    note(
        &mut f.store,
        AnnotationTarget::New {
            room: Room::Decisions,
        },
        AnnotationKind::Task,
        &[e],
    );

    let paths = MemoryPaths::under(f.dir.path());
    let written =
        export::export(&f.store.read(), &paths.exports(), ExportFormat::Both, T0).unwrap();
    assert_eq!(written.len(), 2);
    assert!(written.iter().all(|p| p.starts_with(paths.exports())));

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&written[0]).unwrap()).unwrap();
    assert_eq!(json["project_id"], common::PROJECT);
    assert_eq!(json["memory_version"], 3);
    assert_eq!(json["memories"].as_array().unwrap().len(), 2);
    assert_eq!(json["annotations"].as_array().unwrap().len(), 2);
    assert_eq!(json["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(json["relations"].as_array().unwrap().len(), 1);
    assert_eq!(json["versions"].as_array().unwrap().len(), 3);

    let folder = &written[1];
    let mut files: Vec<String> = std::fs::read_dir(folder)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    let mut expected: Vec<String> = Room::ALL
        .iter()
        .filter(|r| r.holds_memories())
        .map(|r| format!("{}.md", r.word()))
        .collect();
    expected.sort();
    assert_eq!(files, expected);
    let workshop = std::fs::read_to_string(folder.join("officina.md")).unwrap();
    assert!(workshop.contains(a.as_str()));
    assert!(
        workshop.contains(against.id.as_str()),
        "open notes beside the memory"
    );
    assert!(workshop.contains("status: candidato"));
    let decisions = std::fs::read_to_string(folder.join("decisioni.md")).unwrap();
    assert!(decisions.contains("Open notes for this room"));

    // No secret anywhere in what was written.
    let mut all = std::fs::read_to_string(&written[0]).unwrap();
    for file in &files {
        all.push_str(&std::fs::read_to_string(folder.join(file)).unwrap());
    }
    assert!(!all.contains(SECRET));
    assert!(!all.contains("sk-ant-api03"));

    // A second export does not overwrite the first.
    let again = export::export(
        &f.store.read(),
        &paths.exports(),
        ExportFormat::Json,
        T0 + 1,
    )
    .unwrap();
    assert_ne!(again[0], written[0]);
    assert!(written[0].exists());
}
