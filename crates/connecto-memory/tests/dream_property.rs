//! Review round 2: what a dream publishes is exactly the final judgement of
//! its whole ordered list of staged items, and the final state never holds a
//! claim the validator's own rule does not support.
//!
//! - Regressions: a create verified in one session and a note on the same
//!   evidence absorbed into it in another (the replay's downgrade is what is
//!   published); a verified memory whose lineage gains a note on its own
//!   source (lowered by the post-condition, in the same publication).
//! - A property test: a few hundred seeded random sequences of staged
//!   sessions over evidence of mixed kinds and exposures; after publishing,
//!   no live memory is `verificato`, or above its kind's weakest standing,
//!   without independent support by that rule.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::red;
use connecto_core::memory::{
    Address, AnnotationId, AnnotationKind, AnnotationOrigin, AnnotationTarget, Cycle, Evidence,
    EvidenceId, EvidenceKind, MemoryId, MemoryKind, MemoryStatus, ModelChoice, Provider, Room,
    Scope, Standing, Subject, Verb, Verification, is_independent,
};
use connecto_memory::dream;
use connecto_memory::operations::{self, Packet, Staged};
use connecto_memory::{MemoryOp, NewAnnotation, NewEvidence, NewMemory, Publication, Store};

const T0: u64 = 1_794_614_400_000;

fn evidence(s: &mut Store, kind: EvidenceKind, session: &str) -> EvidenceId {
    let mut e = NewEvidence::new(kind, T0, red("excerpt"));
    e.tool_session = Some(session.to_owned());
    e.terminal = Some(format!("term-{session}"));
    s.insert_evidence(e).unwrap().id
}

fn note(s: &mut Store, room: Room, sources: &[EvidenceId]) -> AnnotationId {
    s.create_annotation(NewAnnotation {
        id: None,
        created_ms: T0,
        kind: AnnotationKind::Integration,
        target: AnnotationTarget::New { room },
        text: red("A note."),
        scopes: vec![Scope::Project],
        sources: sources.to_vec(),
        verification: Verification::Unverified,
        origin: AnnotationOrigin::DevelopmentCycle,
        memory_kind: None,
        standing: None,
    })
    .unwrap()
    .id
}

fn create(
    s: &mut Store,
    address: &str,
    kind: MemoryKind,
    standing: Standing,
    status: MemoryStatus,
    sources: &[EvidenceId],
) -> MemoryId {
    let cutoff = s.read().annotation_seq().unwrap();
    s.publish(Publication {
        project_id: "proj-a".to_owned(),
        cycle: Cycle::Bootstrap,
        base_version: None,
        cutoff,
        ops: vec![MemoryOp::Create(NewMemory {
            id: None,
            address: Address::parse(address).unwrap(),
            kind,
            standing: Some(standing),
            summary: red(&format!("About {address}.")),
            details: red(""),
            scopes: vec![Scope::Project],
            status,
            anchors: Vec::new(),
            sources: sources.to_vec(),
            relations: Vec::new(),
        })],
        absorb: Vec::new(),
        run_id: None,
        now_ms: T0,
    })
    .unwrap()
    .created[0]
        .clone()
}

/// Stage sessions as a dream does (each validated after the earlier ones),
/// then publish through `publish_staged`. Returns the publishing result.
fn dream_sessions(
    s: &mut Store,
    packet: &Packet,
    sessions: &[String],
) -> connecto_memory::Result<dream::Publishing> {
    let choice = ModelChoice::new(Provider::Claude, "sonnet", "medium");
    let run = s.start_run(Cycle::Dream, &choice, T0, 0)?;
    let mut prior: Vec<Staged> = Vec::new();
    for reply in sessions {
        let Ok(proposed) = operations::parse(reply) else {
            continue;
        };
        let verdict = operations::validate_after(&s.read(), packet, &prior, proposed, &[]);
        let mut items: Vec<Staged> = verdict.ops.into_iter().map(Staged::Op).collect();
        items.extend(verdict.absorb.into_iter().map(Staged::Absorb));
        items.extend(verdict.kept.into_iter().map(|(a, r)| Staged::Keep(a, r)));
        for item in &items {
            s.stage(run, None, &operations::encode(item), T0)?;
        }
        prior.extend(items);
    }
    let base = s.version()?;
    dream::publish_staged(s, run, base, packet.cutoff, T0 + 1, &[], &mut |_| false)
}

fn full_packet(s: &Store) -> Packet {
    let read = s.read();
    Packet::new(
        read.all_evidence()
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect(),
        read.memories().unwrap().into_iter().map(|m| m.id).collect(),
        read.open_annotations()
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect(),
        read.annotation_seq().unwrap(),
    )
}

/// Each live memory's claims and sources before a publication.
type Snapshot = BTreeMap<MemoryId, (MemoryStatus, Standing, BTreeSet<EvidenceId>)>;

fn snapshot(s: &Store) -> Snapshot {
    s.read()
        .memories()
        .unwrap()
        .into_iter()
        .map(|m| {
            let sources = m.sources.iter().map(|x| x.evidence).collect();
            (m.id, (m.status, m.standing, sources))
        })
        .collect()
}

/// The validator's rule, checked from scratch on the stored state against
/// the state before the publication (`before`):
/// - evidence counts only if no agent produced it, or its session had been
///   shown nothing from memory, and it is independent of the lineage;
/// - `verificato` needs such a source that is not a source of a note in the
///   lineage, among the sources added by the publication when it was not
///   `verificato` before (re-citing an existing source confirms nothing),
///   among all its sources when it was;
/// - a standing above the weakest needs such a source of a supporting kind,
///   among the added sources and the non-agent ones it had when the
///   standing was raised, among all its sources otherwise.
fn unsupported_claims(s: &Store, before: &Snapshot) -> Vec<String> {
    let read = s.read();
    let counts = |e: &Evidence| !e.kind.from_agent() || e.exposed_to.is_empty();
    let mut out = Vec::new();
    for m in read.memories().unwrap() {
        let lineage = read.lineage(&m.id).unwrap();
        let evidence = read.evidence_for(&Subject::Memory(m.id.clone())).unwrap();
        let mut note_sources: BTreeSet<EvidenceId> = BTreeSet::new();
        for subject in &lineage {
            if let Subject::Annotation(a) = subject
                && let Some(n) = read.annotation(a).unwrap()
            {
                note_sources.extend(n.sources.iter().map(|x| x.evidence));
            }
        }
        let prior = before.get(&m.id);
        let old_sources: BTreeSet<EvidenceId> = prior.map(|p| p.2.clone()).unwrap_or_default();
        let added = |e: &&Evidence| !old_sources.contains(&e.id);
        if m.status == MemoryStatus::Verified {
            let was = prior.is_some_and(|p| p.0 == MemoryStatus::Verified);
            let ok = evidence
                .iter()
                .filter(|e| was || added(e))
                .any(|e| counts(e) && is_independent(e, &lineage) && !note_sources.contains(&e.id));
            if !ok {
                out.push(format!("{} verificato", m.address));
            }
        }
        if let Some(kinds) = m.standing.required_evidence() {
            let same = prior.is_some_and(|p| p.1 == m.standing);
            let ok = evidence
                .iter()
                .filter(|e| same || added(e) || !e.kind.from_agent())
                .any(|e| kinds.contains(&e.kind) && counts(e) && is_independent(e, &lineage));
            if !ok {
                out.push(format!("{} {}", m.address, m.standing.word()));
            }
        }
    }
    out
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("memory.db"), "proj-a").unwrap();
    (dir, s)
}

#[test]
fn a_create_verified_in_one_session_then_fed_its_own_source_publishes_downgraded() {
    let (_d, mut s) = store();
    let e = evidence(&mut s, EvidenceKind::UserStatement, "u1");
    let n = note(&mut s, Room::Problems, &[e]);
    let packet = full_packet(&s);
    // Session 1 creates a verified memory from E; nothing absorbed yet, so
    // it passes. Session 2 absorbs the note made from E into it.
    let session1 = format!(
        r#"[{{"op":"create","room":"problemi","slug":"x","kind":"problema","status":"verificato","summary":"X breaks.","sources":["{e}"]}}]"#
    );
    let created = {
        let v = operations::validate(
            &s.read(),
            &packet,
            operations::parse(&session1).unwrap(),
            &[],
        );
        assert!(v.adjusted.is_empty(), "session 1 alone accepts it: {v:?}");
        match &v.ops[0] {
            MemoryOp::Create(c) => c.id.clone().unwrap(),
            _ => panic!(),
        }
    };
    let _ = created;
    // The dream stages session 1 (fresh id) and session 2 refers to it by
    // the id staged, as a later session would see it.
    let choice = ModelChoice::new(Provider::Claude, "sonnet", "medium");
    let run = s.start_run(Cycle::Dream, &choice, T0, 0).unwrap();
    let v1 = operations::validate(
        &s.read(),
        &packet,
        operations::parse(&session1).unwrap(),
        &[],
    );
    let id = match &v1.ops[0] {
        MemoryOp::Create(c) => c.id.clone().unwrap(),
        _ => panic!(),
    };
    let prior: Vec<Staged> = v1.ops.into_iter().map(Staged::Op).collect();
    let session2 = format!(r#"[{{"op":"absorb","annotation":"{n}","into":"{id}"}}]"#);
    let v2 = operations::validate_after(
        &s.read(),
        &packet,
        &prior,
        operations::parse(&session2).unwrap(),
        &[],
    );
    assert_eq!(v2.absorb.len(), 1, "{v2:?}");
    let mut all = prior;
    all.extend(v2.absorb.into_iter().map(Staged::Absorb));
    for item in &all {
        s.stage(run, None, &operations::encode(item), T0).unwrap();
    }
    let before = snapshot(&s);
    let p = dream::publish_staged(&mut s, run, 0, packet.cutoff, T0, &[], &mut |_| false).unwrap();
    assert!(p.published.is_some());
    assert!(!p.adjusted.is_empty(), "{p:?}");
    let m = s.read().memory(&id).unwrap().unwrap();
    assert_eq!(
        m.status,
        MemoryStatus::Candidate,
        "the replay's downgrade is published"
    );
    assert!(unsupported_claims(&s, &before).is_empty());
}

#[test]
fn a_verified_memory_fed_a_note_on_its_own_source_is_lowered_in_the_same_publication() {
    let (_d, mut s) = store();
    let u = evidence(&mut s, EvidenceKind::UserStatement, "u1");
    let x = create(
        &mut s,
        "decisioni/wal",
        MemoryKind::Decision,
        Standing::Accepted,
        MemoryStatus::Verified,
        &[u],
    );
    let n = note(&mut s, Room::Decisions, &[u]);
    let packet = full_packet(&s);
    let p = dream_sessions(
        &mut s,
        &packet,
        &[format!(
            r#"[{{"op":"absorb","annotation":"{n}","into":"{x}"}}]"#
        )],
    )
    .unwrap();
    let published = p.published.clone().unwrap();
    assert_eq!(published.absorbed, [n]);
    let m = s.read().memory(&x).unwrap().unwrap();
    assert_eq!(
        m.status,
        MemoryStatus::ToVerify,
        "lowered, not left verified"
    );
    // The user's statement still supports the accepted decision.
    assert_eq!(m.standing, Standing::Accepted);
    assert!(p.adjusted.iter().any(|a| a.contains("lowered")), "{p:?}");
    assert_eq!(
        s.read().version().unwrap(),
        published.version,
        "one version"
    );
}

// ---- the property

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n as u64)).unwrap()
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

struct World {
    evidence: Vec<EvidenceId>,
    memories: Vec<MemoryId>,
    notes: Vec<AnnotationId>,
}

fn world(s: &mut Store) -> World {
    use EvidenceKind as K;
    let u1 = evidence(s, K::UserStatement, "u1");
    let u2 = evidence(s, K::UserStatement, "u2");
    let c1 = evidence(s, K::CodeObservation, "c1");
    let d1 = evidence(s, K::ProjectDocument, "d1");
    let a1 = evidence(s, K::AgentClaim, "s1");
    let a2 = evidence(s, K::AgentClaim, "s2");
    let p1 = evidence(s, K::AgentProposal, "s3");
    let r1 = evidence(s, K::ReportedOutcome, "s4");
    let m0 = create(
        s,
        "architettura/m0",
        MemoryKind::Fact,
        Standing::Hypothesis,
        MemoryStatus::Candidate,
        &[a1],
    );
    let m1 = create(
        s,
        "decisioni/m1",
        MemoryKind::Decision,
        Standing::Accepted,
        MemoryStatus::Verified,
        &[u1],
    );
    let m2 = create(
        s,
        "lavori-aperti/m2",
        MemoryKind::Work,
        Standing::Planned,
        MemoryStatus::Candidate,
        &[a2],
    );
    let m3 = create(
        s,
        "officina/m3",
        MemoryKind::Procedure,
        Standing::Documented,
        MemoryStatus::Candidate,
        &[d1],
    );
    let m4 = create(
        s,
        "problemi/m4",
        MemoryKind::Problem,
        Standing::Established,
        MemoryStatus::Verified,
        &[c1],
    );
    let m5 = create(
        s,
        "decisioni/m5",
        MemoryKind::Decision,
        Standing::Proposed,
        MemoryStatus::Candidate,
        &[p1],
    );
    // Sessions shown memories, then speaking.
    s.record_served(
        Some("s9"),
        Some("term-s9"),
        Verb::Read,
        &[Subject::Memory(m0.clone())],
        T0,
    )
    .unwrap();
    s.record_served(
        Some("s8"),
        Some("term-s8"),
        Verb::Brief,
        &[Subject::Memory(m1.clone()), Subject::Memory(m4.clone())],
        T0,
    )
    .unwrap();
    let mut e9 = NewEvidence::new(K::AgentClaim, T0 + 1, red("echo"));
    e9.tool_session = Some("s9".to_owned());
    let x1 = s.insert_evidence(e9).unwrap().id;
    let mut e8 = NewEvidence::new(K::ReportedOutcome, T0 + 1, red("tests pass"));
    e8.tool_session = Some("s8".to_owned());
    let x2 = s.insert_evidence(e8).unwrap().id;
    let mut e7 = NewEvidence::new(K::AgentProposal, T0 + 1, red("let's"));
    e7.tool_session = Some("s9".to_owned());
    let x3 = s.insert_evidence(e7).unwrap().id;
    let notes = vec![
        note(s, Room::Architecture, &[u1]),
        note(s, Room::Problems, &[x1]),
        note(s, Room::Workshop, &[a1]),
        note(s, Room::Problems, &[c1]),
        note(s, Room::Decisions, &[u2]),
        note(s, Room::Decisions, &[x2, p1]),
        note(s, Room::OpenWork, &[r1, x3]),
    ];
    World {
        evidence: vec![u1, u2, c1, d1, a1, a2, p1, r1, x1, x2, x3],
        memories: vec![m0, m1, m2, m3, m4, m5],
        notes,
    }
}

const KINDS: &[(&str, &str, &[&str])] = &[
    ("fatto", "architettura", &["ipotesi", "accertato"]),
    ("problema", "problemi", &["ipotesi", "accertato"]),
    (
        "decisione",
        "decisioni",
        &["proposta", "accettata", "scartata"],
    ),
    ("convenzione", "convenzioni", &["proposta", "accettata"]),
    (
        "lavoro",
        "lavori-aperti",
        &["pianificato", "in_corso", "implementato"],
    ),
    ("procedura", "officina", &["documentata", "eseguita"]),
];
const STATUSES: &[&str] = &["candidato", "verificato", "da_verificare", "conflitto"];
const RELATIONS: &[&str] = &["vedi_anche", "deriva_da", "dipende_da", "contraddice"];

fn sources(rng: &mut Rng, w: &World) -> String {
    let n = 1 + rng.below(3);
    (0..n)
        .map(|_| format!("\"{}\"", rng.pick(&w.evidence)))
        .collect::<Vec<_>>()
        .join(",")
}

/// One random operation. `pool` holds the fixture's memories and those
/// earlier sessions of the same dream created; `refs` this reply's own.
fn op(
    rng: &mut Rng,
    w: &World,
    pool: &[MemoryId],
    refs: &mut Vec<String>,
    case: usize,
    at: usize,
) -> String {
    let mem = |rng: &mut Rng, refs: &Vec<String>| -> String {
        if !refs.is_empty() && rng.below(3) == 0 {
            rng.pick(refs).clone()
        } else if pool.len() > w.memories.len() && rng.below(2) == 0 {
            // Prefer what earlier sessions created.
            rng.pick(&pool[w.memories.len()..]).to_string()
        } else {
            rng.pick(pool).to_string()
        }
    };
    match rng.below(12) {
        0 => {
            let (kind, room, standings) = rng.pick(KINDS);
            let r = format!("r{at}");
            refs.push(r.clone());
            format!(
                r#"{{"op":"create","ref":"{r}","room":"{room}","slug":"g{case}-{at}","kind":"{kind}","standing":"{}","status":"{}","summary":"Generated {at}.","sources":[{}]}}"#,
                rng.pick(standings),
                rng.pick(STATUSES),
                sources(rng, w)
            )
        }
        1 => {
            let (_, _, standings) = rng.pick(KINDS);
            format!(
                r#"{{"op":"update","id":"{}","standing":"{}","sources":[{}]}}"#,
                mem(rng, refs),
                rng.pick(standings),
                sources(rng, w)
            )
        }
        2 => format!(
            r#"{{"op":"merge","into":"{}","from":["{}"]}}"#,
            mem(rng, refs),
            mem(rng, refs)
        ),
        3 => format!(
            r#"{{"op":"supersede","old":"{}","by":"{}","sources":[{}]}}"#,
            mem(rng, refs),
            mem(rng, refs),
            sources(rng, w)
        ),
        4 => format!(
            r#"{{"op":"set_status","id":"{}","status":"{}","sources":[{}]}}"#,
            mem(rng, refs),
            rng.pick(STATUSES),
            sources(rng, w)
        ),
        5 => format!(
            r#"{{"op":"move","id":"{}","to":"{}/moved-{case}-{at}"}}"#,
            mem(rng, refs),
            rng.pick(KINDS).1
        ),
        6 | 7 => format!(
            r#"{{"op":"absorb","annotation":"{}","into":"{}"}}"#,
            rng.pick(&w.notes),
            mem(rng, refs)
        ),
        10 => format!(
            r#"{{"op":"archive","id":"{}","sources":[{}]}}"#,
            mem(rng, refs),
            sources(rng, w)
        ),
        11 => {
            // The same evidence again: a re-cited source.
            format!(
                r#"{{"op":"set_status","id":"{}","status":"verificato","sources":[{}]}}"#,
                mem(rng, refs),
                sources(rng, w)
            )
        }
        8 => format!(
            r#"{{"op":"link","from":"{}","relation":"{}","to":"{}"}}"#,
            mem(rng, refs),
            rng.pick(RELATIONS),
            mem(rng, refs)
        ),
        _ => format!(
            r#"{{"op":"keep","annotation":"{}","reason":"later"}}"#,
            rng.pick(&w.notes)
        ),
    }
}

/// The kinds of operation a verdict accepted, by name.
fn accepted_kinds(v: &operations::Verdict, seen: &mut BTreeSet<&'static str>) {
    for op in &v.ops {
        seen.insert(match op {
            MemoryOp::Create(_) => "create",
            MemoryOp::Update(_) => "update",
            MemoryOp::Merge { .. } => "merge",
            MemoryOp::Supersede { .. } => "supersede",
            MemoryOp::SetStatus { .. } => "set_status",
            MemoryOp::Move { .. } => "move",
            MemoryOp::Archive { .. } => "archive",
            MemoryOp::Link { .. } => "link",
        });
    }
    if !v.absorb.is_empty() {
        seen.insert("absorb");
    }
    if !v.kept.is_empty() {
        seen.insert("keep");
    }
}

#[test]
fn no_published_claim_lacks_independent_support() {
    const CASES: u64 = 300;
    let mut published = 0;
    let mut lowered = 0;
    let mut cross_session = 0;
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    for seed in 1..=CASES {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let (_d, mut s) = store();
        let w = world(&mut s);
        let fixture = snapshot(&s);
        assert!(
            unsupported_claims(&s, &fixture).is_empty(),
            "the fixture itself"
        );
        let packet = full_packet(&s);
        let before = snapshot(&s);
        // Sessions generated one after the other, each able to name what
        // the earlier ones created (as the dream's later sessions see it).
        let choice = ModelChoice::new(Provider::Claude, "sonnet", "medium");
        let run = s.start_run(Cycle::Dream, &choice, T0, 0).unwrap();
        let mut prior: Vec<Staged> = Vec::new();
        let mut pool: Vec<MemoryId> = w.memories.clone();
        let mut replies = Vec::new();
        let mut at = 0;
        for _ in 0..1 + rng.below(3) {
            let mut refs = Vec::new();
            let n = 1 + rng.below(7);
            let ops: Vec<String> = (0..n)
                .map(|_| {
                    at += 1;
                    op(
                        &mut rng,
                        &w,
                        &pool,
                        &mut refs,
                        usize::try_from(seed).unwrap(),
                        at,
                    )
                })
                .collect();
            let reply = format!("[{}]", ops.join(","));
            let proposed = operations::parse(&reply).unwrap();
            let verdict = operations::validate_after(&s.read(), &packet, &prior, proposed, &[]);
            accepted_kinds(&verdict, &mut seen);
            let earlier = pool.len() > w.memories.len();
            if earlier
                && verdict.ops.iter().any(|o| match o {
                    MemoryOp::Create(_) => false,
                    other => pool[w.memories.len()..]
                        .iter()
                        .any(|m| format!("{other:?}").contains(m.as_str())),
                })
            {
                cross_session += 1;
            }
            let mut items: Vec<Staged> = verdict.ops.into_iter().map(Staged::Op).collect();
            items.extend(verdict.absorb.into_iter().map(Staged::Absorb));
            items.extend(verdict.kept.into_iter().map(|(a, r)| Staged::Keep(a, r)));
            for item in &items {
                if let Staged::Op(MemoryOp::Create(c)) = item {
                    pool.extend(c.id.clone());
                }
                s.stage(run, None, &operations::encode(item), T0).unwrap();
            }
            prior.extend(items);
            replies.push(reply);
        }
        let base = s.version().unwrap();
        match dream::publish_staged(&mut s, run, base, packet.cutoff, T0 + 1, &[], &mut |_| {
            false
        }) {
            Ok(p) => {
                if p.published.is_some() {
                    published += 1;
                }
                lowered += p.adjusted.len();
            }
            // A refusal that names nothing publishes nothing: still fine.
            Err(e) => assert_eq!(s.read().version().unwrap(), base, "seed {seed}: {e}"),
        }
        let bad = unsupported_claims(&s, &before);
        assert!(
            bad.is_empty(),
            "seed {seed}: {bad:?}\nreplies: {replies:#?}"
        );
    }
    // The generator exercises publications, the final lowering, sessions
    // acting on earlier sessions' creations, and every operation kind.
    assert!(published > CASES / 3, "{published}");
    assert!(lowered > 0);
    assert!(cross_session > 10, "{cross_session}");
    let all: BTreeSet<&str> = [
        "create",
        "update",
        "merge",
        "supersede",
        "set_status",
        "move",
        "archive",
        "link",
        "absorb",
        "keep",
    ]
    .into_iter()
    .collect();
    assert_eq!(seen, all, "every operation kind accepted at least once");
}
