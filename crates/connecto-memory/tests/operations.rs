//! The host's validator for proposed memory operations (spec §5.7): one row
//! per rule, plus the truth rules of §4.5, §4.6 and §5.10.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::red;
use connecto_core::memory::{
    Address, AnnotationId, AnnotationKind, AnnotationOrigin, AnnotationTarget, Cycle, EvidenceId,
    EvidenceKind, MemoryId, MemoryKind, MemoryStatus, Room, Scope, Standing, Subject, Verb,
    Verification,
};
use connecto_memory::operations::{self, Packet, ParseError, Verdict};
use connecto_memory::{MemoryOp, NewAnnotation, NewEvidence, NewMemory, Publication, Store};

const T0: u64 = 1_794_614_400_000;
const SECRET: &str = "sk-live-0123456789abcdefSECRET";

struct World {
    _dir: tempfile::TempDir,
    store: Store,
    user: EvidenceId,
    agent: EvidenceId,
    exposed: EvidenceId,
    proposal: EvidenceId,
    code: EvidenceId,
    outside: EvidenceId,
    fact: MemoryId,
    decision: MemoryId,
    accepted: MemoryId,
    branchy: MemoryId,
    outside_memory: MemoryId,
    note: AnnotationId,
    note2: AnnotationId,
    outside_note: AnnotationId,
    late: AnnotationId,
    cutoff: u64,
}

fn evidence(store: &mut Store, kind: EvidenceKind, session: &str, at: u64) -> EvidenceId {
    let mut e = NewEvidence::new(kind, at, red("an excerpt"));
    e.tool_session = Some(session.to_owned());
    e.terminal = Some(format!("term-{session}"));
    store.insert_evidence(e).unwrap().id
}

fn memory(
    address: &str,
    kind: MemoryKind,
    standing: Option<Standing>,
    sources: &[EvidenceId],
) -> NewMemory {
    NewMemory {
        id: None,
        address: Address::parse(address).unwrap(),
        kind,
        standing,
        summary: red(&format!("Summary of {address}")),
        details: red(""),
        scopes: vec![Scope::Project],
        status: MemoryStatus::Candidate,
        anchors: Vec::new(),
        sources: sources.to_vec(),
        relations: Vec::new(),
    }
}

fn publish(store: &mut Store, ops: Vec<MemoryOp>) -> Vec<MemoryId> {
    let p = Publication {
        project_id: store.project_id().to_owned(),
        cycle: Cycle::Dream,
        base_version: None,
        cutoff: store.read().annotation_seq().unwrap(),
        ops,
        absorb: Vec::new(),
        run_id: None,
        now_ms: T0,
    };
    store.publish(p).unwrap().created
}

fn note(
    store: &mut Store,
    room: Room,
    sources: &[EvidenceId],
    standing: Option<Standing>,
) -> AnnotationId {
    store
        .create_annotation(NewAnnotation {
            id: None,
            created_ms: T0,
            kind: AnnotationKind::Integration,
            target: AnnotationTarget::New { room },
            text: red("We could use SQLite for the store."),
            scopes: vec![Scope::Project],
            sources: sources.to_vec(),
            verification: Verification::Unverified,
            origin: AnnotationOrigin::Agent {
                terminal: "claude-1".to_owned(),
            },
            memory_kind: standing.map(|_| MemoryKind::Decision),
            standing,
        })
        .unwrap()
        .id
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(&dir.path().join("memory.db"), "proj-a").unwrap();
    let user = evidence(&mut store, EvidenceKind::UserStatement, "u", T0);
    let agent = evidence(&mut store, EvidenceKind::AgentClaim, "s1", T0);
    let proposal = evidence(&mut store, EvidenceKind::AgentProposal, "s2", T0);
    let code = evidence(&mut store, EvidenceKind::CodeObservation, "c", T0);
    let outside = evidence(&mut store, EvidenceKind::UserStatement, "u2", T0);
    let created = publish(
        &mut store,
        vec![
            MemoryOp::Create(memory(
                "architettura/store",
                MemoryKind::Fact,
                None,
                &[agent],
            )),
            MemoryOp::Create(memory(
                "decisioni/sqlite",
                MemoryKind::Decision,
                None,
                &[proposal],
            )),
            MemoryOp::Create(memory(
                "decisioni/wal",
                MemoryKind::Decision,
                Some(Standing::Accepted),
                &[user],
            )),
            MemoryOp::Create(memory(
                "lavori-aperti/feature-x",
                MemoryKind::Work,
                None,
                &[agent],
            )),
            MemoryOp::Create(memory(
                "officina/hidden",
                MemoryKind::Procedure,
                None,
                &[agent],
            )),
        ],
    );
    let (fact, decision, accepted, branchy, outside_memory) = (
        created[0].clone(),
        created[1].clone(),
        created[2].clone(),
        created[3].clone(),
        created[4].clone(),
    );
    // `branchy` lives on a branch that is gone.
    {
        let mut ops = connecto_memory::MemoryUpdate::new(branchy.clone());
        ops.scopes = Some(vec![Scope::Branch("feature-x".to_owned())]);
        publish(&mut store, vec![MemoryOp::Update(ops)]);
        assert!(
            store
                .set_scope_gone(&branchy, &Scope::Branch("feature-x".to_owned()), true)
                .unwrap()
        );
    }
    // A session shown `fact`, then repeating it.
    store
        .record_served(
            Some("s9"),
            Some("term-s9"),
            Verb::Read,
            &[Subject::Memory(fact.clone())],
            T0,
        )
        .unwrap();
    let exposed = evidence(&mut store, EvidenceKind::AgentClaim, "s9", T0 + 10);
    let n = note(
        &mut store,
        Room::Decisions,
        &[proposal],
        Some(Standing::Proposed),
    );
    let n2 = note(&mut store, Room::Architecture, &[agent], None);
    let outside_note = note(&mut store, Room::Architecture, &[agent], None);
    let cutoff = store.read().annotation_seq().unwrap();
    let late = note(&mut store, Room::Architecture, &[agent], None);
    World {
        _dir: dir,
        store,
        user,
        agent,
        exposed,
        proposal,
        code,
        outside,
        fact,
        decision,
        accepted,
        branchy,
        outside_memory,
        note: n,
        note2: n2,
        outside_note,
        late,
        cutoff,
    }
}

impl World {
    fn packet(&self) -> Packet {
        Packet::new(
            vec![
                self.user,
                self.agent,
                self.exposed,
                self.proposal,
                self.code,
            ],
            vec![
                self.fact.clone(),
                self.decision.clone(),
                self.accepted.clone(),
                self.branchy.clone(),
            ],
            vec![self.note.clone(), self.note2.clone(), self.late.clone()],
            self.cutoff,
        )
    }

    fn judge(&self, reply: &str) -> Verdict {
        self.judge_with(reply, &[])
    }

    fn judge_with(&self, reply: &str, secrets: &[String]) -> Verdict {
        let proposed = operations::parse(reply).unwrap();
        operations::validate(&self.store.read(), &self.packet(), proposed, secrets)
    }

    /// Fill `{fact}` and the other names into a reply.
    fn fill(&self, reply: &str) -> String {
        reply
            .replace("{fact}", self.fact.as_str())
            .replace("{decision}", self.decision.as_str())
            .replace("{accepted}", self.accepted.as_str())
            .replace("{branchy}", self.branchy.as_str())
            .replace("{outside_memory}", self.outside_memory.as_str())
            .replace("{note}", self.note.as_str())
            .replace("{note2}", self.note2.as_str())
            .replace("{outside_note}", self.outside_note.as_str())
            .replace("{late}", self.late.as_str())
            .replace("{user}", &self.user.to_string())
            .replace("{agent}", &self.agent.to_string())
            .replace("{exposed}", &self.exposed.to_string())
            .replace("{proposal}", &self.proposal.to_string())
            .replace("{code}", &self.code.to_string())
            .replace("{outside}", &self.outside.to_string())
    }
}

/// One operation: `Ok(())` when accepted, else the reason.
fn one(w: &World, op: &str) -> Result<(), String> {
    let v = w.judge(&w.fill(&format!("{{\"operations\":[{op}]}}")));
    match v.rejected.first() {
        Some(r) => Err(r.reason.clone()),
        None => Ok(()),
    }
}

#[test]
fn unreadable_replies_fail_the_run() {
    assert_eq!(
        operations::parse("{\"operations\":[{"),
        Err(ParseError::Unreadable)
    );
    assert_eq!(operations::parse(""), Err(ParseError::Empty));
    assert_eq!(operations::parse("\n  "), Err(ParseError::Empty));
    assert_eq!(
        operations::parse("I would rather not."),
        Err(ParseError::Unreadable)
    );
    let huge = "x".repeat(operations::MAX_REPLY_CHARS + 1);
    assert_eq!(operations::parse(&huge), Err(ParseError::TooLarge));
}

#[test]
fn every_rule_of_the_validator() {
    let w = world();
    let create = |extra: &str| {
        format!(
            r#"{{"op":"create","room":"architettura","kind":"fatto","summary":"The store uses WAL mode.","sources":["{{user}}"]{extra}}}"#
        )
    };
    // (name, operation, expected: None = accepted, Some(part of the reason))
    let table: Vec<(&str, String, Option<&str>)> = vec![
        ("a valid create", create(""), None),
        ("unknown operation", r#"{"op":"rename","id":"{fact}"}"#.into(), Some("unknown operation")),
        ("delete does not exist", r#"{"op":"delete","id":"{fact}"}"#.into(), Some("no delete")),
        ("unknown field", create(r#","color":"red""#), Some("unknown field")),
        ("missing field", r#"{"op":"create","room":"architettura","kind":"fatto","sources":["{user}"]}"#.into(), Some("summary: missing")),
        ("wrong type", r#"{"op":"move","id":42,"to":"officina/x"}"#.into(), Some("not a string")),
        ("wrong list type", r#"{"op":"archive","id":"{fact}","sources":"{user}"}"#.into(), Some("not a list")),
        ("summary too long", format!(r#"{{"op":"update","id":"{{fact}}","summary":"{}"}}"#, "x".repeat(401)), Some("summary: too long")),
        ("details too long", format!(r#"{{"op":"update","id":"{{fact}}","details":"{}"}}"#, "x".repeat(16 * 1024 + 1)), Some("details: too long")),
        ("too many sources", format!(r#"{{"op":"archive","id":"{{fact}}","sources":[{}]}}"#, vec!["\"e-1\""; 33].join(",")), Some("too many entries")),
        ("invented memory id", r#"{"op":"archive","id":"m-zzzzzzzz","sources":["{user}"]}"#.into(), Some("no memory m-zzzzzzzz in this project")),
        ("not an id", r#"{"op":"archive","id":"store","sources":["{user}"]}"#.into(), Some("not a memory id")),
        ("memory not in this run's packet", r#"{"op":"archive","id":"{outside_memory}","sources":["{user}"]}"#.into(), Some("not in the material")),
        ("invented source", create("").replace("{user}", "e-999"), Some("not in the material")),
        ("source not in this run's packet", create("").replace("{user}", "{outside}"), Some("not in the material")),
        ("no sources", r#"{"op":"create","room":"architettura","kind":"fatto","summary":"x y z","sources":[]}"#.into(), Some("at least one is required")),
        ("unknown room", create("").replace("architettura", "cantina"), Some("room: unknown")),
        ("the entrance", create("").replace("architettura", "ingresso"), Some("room: unknown or the entrance")),
        ("not in the archive", create("").replace("architettura", "archivio"), Some("not created in the archive")),
        ("duplicate address", create(r#","slug":"store""#), Some("is already used")),
        ("bad slug", create(r#","slug":"Not A Slug""#), Some("slug: not a valid slug")),
        ("unknown kind", create("").replace("fatto", "opinione"), Some("kind: unknown")),
        ("standing for another kind", create(r#","standing":"implementato""#), Some("does not apply")),
        ("verificato without independent evidence", r#"{"op":"set_status","id":"{fact}","status":"verificato","sources":["{exposed}"]}"#.into(), Some("independent confirmation")),
        ("verificato without sources", r#"{"op":"set_status","id":"{fact}","status":"verificato"}"#.into(), Some("needs the evidence")),
        ("verificato by the user", r#"{"op":"set_status","id":"{fact}","status":"verificato","sources":["{user}"]}"#.into(), None),
        ("superato only by supersede", r#"{"op":"set_status","id":"{fact}","status":"superato","sources":["{user}"]}"#.into(), Some("only from supersede")),
        ("conflitto needs the diverging source", r#"{"op":"set_status","id":"{fact}","status":"conflitto"}"#.into(), Some("both stay visible")),
        ("conflitto with its source", r#"{"op":"set_status","id":"{fact}","status":"conflitto","sources":["{code}"]}"#.into(), None),
        ("da_verificare always allowed", r#"{"op":"set_status","id":"{fact}","status":"da_verificare"}"#.into(), None),
        ("absorb past the cutoff", r#"{"op":"absorb","annotation":"{late}","into":"{fact}"}"#.into(), Some("after this run's cutoff")),
        ("absorb not in this packet", r#"{"op":"absorb","annotation":"{outside_note}"}"#.into(), Some("not in the material")),
        ("absorb an invented annotation", r#"{"op":"absorb","annotation":"p-zzzzzzzz"}"#.into(), Some("no p-zzzzzzzz in this project")),
        ("absorb into nothing is fine", r#"{"op":"absorb","annotation":"{note2}"}"#.into(), None),
        ("keep with a reason", r#"{"op":"keep","annotation":"{note2}","reason":"not settled yet"}"#.into(), None),
        ("keep without a reason", r#"{"op":"keep","annotation":"{note2}","reason":"  "}"#.into(), Some("nothing left")),
        ("move into a used address", r#"{"op":"move","id":"{fact}","to":"decisioni/sqlite"}"#.into(), Some("is already used")),
        ("move into the archive", r#"{"op":"move","id":"{fact}","to":"archivio/store"}"#.into(), Some("use archive")),
        ("move", r#"{"op":"move","id":"{fact}","to":"architettura/memory-store"}"#.into(), None),
        ("archive needs a source", r#"{"op":"archive","id":"{fact}"}"#.into(), Some("at least one is required")),
        ("link to itself", r#"{"op":"link","from":"{fact}","relation":"vedi_anche","to":"{fact}"}"#.into(), Some("itself")),
        ("link by supersedes", r#"{"op":"link","from":"{fact}","relation":"sostituisce","to":"{decision}"}"#.into(), Some("use supersede")),
        ("link", r#"{"op":"link","from":"{fact}","relation":"vedi_anche","to":"{decision}"}"#.into(), None),
        ("merge across kinds", r#"{"op":"merge","into":"{fact}","from":["{decision}"]}"#.into(), Some("different kinds")),
    ];
    for (name, op, expected) in table {
        let got = one(&w, &op);
        match expected {
            None => assert_eq!(got, Ok(()), "{name}"),
            Some(part) => {
                let reason = got.expect_err(name);
                assert!(reason.contains(part), "{name}: {reason}");
            }
        }
    }
}

#[test]
fn a_foreign_id_is_refused() {
    let w = world();
    let dir = tempfile::tempdir().unwrap();
    let mut other = Store::open(&dir.path().join("memory.db"), "proj-b").unwrap();
    let e = evidence(&mut other, EvidenceKind::UserStatement, "x", T0);
    let foreign = publish(
        &mut other,
        vec![MemoryOp::Create(memory(
            "architettura/other",
            MemoryKind::Fact,
            None,
            &[e],
        ))],
    )
    .remove(0);
    let reason = one(
        &w,
        &format!(r#"{{"op":"archive","id":"{foreign}","sources":["{{user}}"]}}"#),
    )
    .unwrap_err();
    assert!(reason.contains("in this project"), "{reason}");
    // Even when the run's packet names it.
    let mut packet = w.packet();
    packet.memories.insert(foreign.clone());
    let proposed = operations::parse(&w.fill(&format!(
        r#"[{{"op":"update","id":"{foreign}","summary":"taken over"}}]"#
    )))
    .unwrap();
    let v = operations::validate(&w.store.read(), &packet, proposed, &[]);
    assert!(v.ops.is_empty() && v.rejected.len() == 1);
}

#[test]
fn redaction_cleans_model_text_and_refuses_what_it_withdraws() {
    let w = world();
    let secrets = vec![SECRET.to_owned()];
    let v = w.judge_with(
        &w.fill(&format!(
            r#"[{{"op":"create","room":"officina","kind":"procedura","summary":"Deploy with token {SECRET} set.","details":"Uses {SECRET}.","sources":["{{user}}"]}},
               {{"op":"update","id":"{{fact}}","summary":"{SECRET}"}},
               {{"op":"keep","annotation":"{{note2}}","reason":"waiting on {SECRET}"}}]"#
        )),
        &secrets,
    );
    assert_eq!(v.rejected.len(), 1, "{v:?}");
    assert!(
        v.rejected[0]
            .reason
            .contains("nothing left after redaction")
    );
    let MemoryOp::Create(new) = &v.ops[0] else {
        panic!("not a create");
    };
    assert!(!new.summary.get().contains(SECRET) && new.summary.get().contains("[REDACTED]"));
    assert!(!new.details.get().contains(SECRET));
    assert!(!v.kept[0].1.contains(SECRET));
}

#[test]
fn distinctions_are_kept_proposal_plan_hypothesis() {
    let w = world();
    // A decision the sources only propose comes out as a proposal.
    let v = w.judge(&w.fill(
        r#"[{"op":"create","ref":"d","room":"decisioni","kind":"decisione","standing":"accettata","summary":"Use SQLite for memory.","sources":["{proposal}"]},
            {"op":"absorb","annotation":"{note}","into":"d"},
            {"op":"create","room":"lavori-aperti","kind":"lavoro","standing":"implementato","summary":"Export is done.","sources":["{agent}"]}]"#,
    ));
    assert!(v.rejected.is_empty(), "{v:?}");
    let standings: Vec<(Standing, MemoryStatus)> = v
        .ops
        .iter()
        .filter_map(|op| match op {
            MemoryOp::Create(n) => Some((n.standing.unwrap(), n.status)),
            _ => None,
        })
        .collect();
    assert_eq!(
        standings,
        [
            (Standing::Proposed, MemoryStatus::Candidate),
            (Standing::Planned, MemoryStatus::Candidate),
        ]
    );
    assert_eq!(v.adjusted.len(), 2, "{:?}", v.adjusted);
    // A hypothesis is not made established by a session that had read it.
    let reason = one(
        &w,
        r#"{"op":"update","id":"{fact}","standing":"accertato","sources":["{exposed}"]}"#,
    )
    .unwrap_err();
    assert!(reason.contains("not supported"), "{reason}");
    assert_eq!(v.absorb.len(), 1);
    // The user's own statement does make it accepted.
    let v = w.judge(&w.fill(
        r#"[{"op":"create","room":"decisioni","kind":"decisione","standing":"accettata","summary":"Use SQLite for memory.","sources":["{user}"]}]"#,
    ));
    let MemoryOp::Create(n) = &v.ops[0] else {
        panic!()
    };
    assert_eq!(n.standing, Some(Standing::Accepted));
    // Raising a standing without support is refused, not adjusted.
    let reason = one(
        &w,
        r#"{"op":"update","id":"{decision}","standing":"accettata","sources":["{proposal}"]}"#,
    )
    .unwrap_err();
    assert!(reason.contains("not supported"), "{reason}");
    assert_eq!(
        one(
            &w,
            r#"{"op":"update","id":"{decision}","standing":"accettata","sources":["{user}"]}"#
        ),
        Ok(())
    );
    // A proposal is never merged into an accepted decision, nor replaces it.
    let reason = one(
        &w,
        r#"{"op":"merge","into":"{accepted}","from":["{decision}"]}"#,
    )
    .unwrap_err();
    assert!(reason.contains("keep them apart"), "{reason}");
    let reason = one(
        &w,
        r#"{"op":"supersede","old":"{accepted}","by":"{decision}"}"#,
    )
    .unwrap_err();
    assert!(reason.contains("cannot replace"), "{reason}");
    // The other way is allowed.
    assert_eq!(
        one(
            &w,
            r#"{"op":"supersede","old":"{decision}","by":"{accepted}"}"#
        ),
        Ok(())
    );
}

#[test]
fn a_repeated_claim_never_verifies_what_its_session_had_read() {
    let w = world();
    let reason = one(
        &w,
        r#"{"op":"set_status","id":"{fact}","status":"verificato","sources":["{exposed}","{exposed}","{agent}"]}"#,
    )
    .unwrap_err();
    // `agent` is the fact's own source: still the same claim.
    assert!(reason.contains("independent"), "{reason}");
}

#[test]
fn a_gone_branch_promotes_or_archives_nothing_by_itself() {
    let w = world();
    for op in [
        r#"{"op":"archive","id":"{branchy}","sources":["{agent}"]}"#,
        r#"{"op":"update","id":"{branchy}","scope":["project"],"sources":["{agent}"]}"#,
        r#"{"op":"update","id":"{branchy}","scope":["project"]}"#,
        r#"{"op":"supersede","old":"{branchy}","by":"{fact}","sources":["{exposed}"]}"#,
    ] {
        let reason = one(&w, op).unwrap_err();
        assert!(
            reason.contains("needs a user statement")
                || reason.contains("only with a user statement"),
            "{op}: {reason}"
        );
    }
    // With the user's word, or the code on the main branch, it may.
    assert_eq!(
        one(
            &w,
            r#"{"op":"archive","id":"{branchy}","sources":["{user}"]}"#
        ),
        Ok(())
    );
    assert_eq!(
        one(
            &w,
            r#"{"op":"update","id":"{branchy}","scope":["project"],"sources":["{code}"]}"#
        ),
        Ok(())
    );
    // Other edits leave the gone scope as it is.
    assert_eq!(
        one(
            &w,
            r#"{"op":"update","id":"{branchy}","summary":"Feature X, on its branch."}"#
        ),
        Ok(())
    );
}

#[test]
fn later_operations_see_earlier_ones_and_refs_name_new_memories() {
    let mut w = world();
    let v = w.judge(&w.fill(
        r#"[{"op":"create","ref":"wal","room":"architettura","kind":"fatto","summary":"The store runs in WAL mode.","sources":["{user}"]},
            {"op":"link","from":"wal","relation":"vedi_anche","to":"{fact}"},
            {"op":"absorb","annotation":"{note2}","into":"wal"},
            {"op":"create","ref":"wal","room":"architettura","kind":"fatto","summary":"Again.","sources":["{user}"]},
            {"op":"create","room":"architettura","slug":"wal-mode","kind":"fatto","summary":"Same address.","sources":["{user}"]},
            {"op":"create","room":"architettura","slug":"wal-mode","kind":"fatto","summary":"Same address twice.","sources":["{user}"]},
            {"op":"move","id":"{fact}","to":"architettura/moved"},
            {"op":"create","room":"architettura","slug":"moved","kind":"fatto","summary":"Where it moved.","sources":["{user}"]},
            {"op":"absorb","annotation":"{note2}"}]"#,
    ));
    let reasons: Vec<(usize, &str)> = v
        .rejected
        .iter()
        .map(|r| (r.index, r.reason.as_str()))
        .collect();
    assert_eq!(reasons.len(), 4, "{reasons:?}");
    assert_eq!(reasons[0].0, 3);
    assert!(reasons[0].1.contains("ref: already used"));
    assert_eq!(reasons[1].0, 5);
    assert!(reasons[1].1.contains("already used"));
    assert_eq!(reasons[2].0, 7);
    assert!(reasons[2].1.contains("already used"));
    assert_eq!(reasons[3].0, 8);
    assert!(reasons[3].1.contains("named twice"));
    assert_eq!(v.origin.len(), v.ops.len());
    // What passed publishes as one version.
    let published = w
        .store
        .publish(Publication {
            project_id: "proj-a".to_owned(),
            cycle: Cycle::Dream,
            base_version: None,
            cutoff: w.cutoff,
            ops: v.ops.clone(),
            absorb: v.absorb.clone(),
            run_id: None,
            now_ms: T0,
        })
        .unwrap();
    assert_eq!(published.created.len(), 2);
    assert_eq!(published.absorbed, [w.note2.clone()]);
    // After a merge the merged memory is not live for later operations.
    let v = w.judge(&w.fill(
        r#"[{"op":"merge","into":"{decision}","from":["{accepted}"]},
            {"op":"update","id":"{fact}","summary":"ok"}]"#,
    ));
    assert_eq!(v.rejected.len(), 1);
    let v = w.judge(&w.fill(
        r#"[{"op":"supersede","old":"{fact}","by":"{decision}","sources":["{user}"]},
            {"op":"archive","id":"{decision}","sources":["{user}"]},
            {"op":"merge","into":"{decision}","from":["{decision}"]}]"#,
    ));
    // supersede across kinds is allowed (a fact replaced by a decision is
    // odd but not false); the self-merge is refused.
    assert_eq!(v.rejected.len(), 1, "{v:?}");
    assert!(v.rejected[0].reason.contains("distinct"));
    // With sources, archive is preceded by the update that adds them.
    assert!(matches!(v.ops[0], MemoryOp::Update(_)));
    assert!(matches!(v.ops[1], MemoryOp::Supersede { .. }));
    assert!(matches!(v.ops[2], MemoryOp::Update(_)));
    assert!(matches!(v.ops[3], MemoryOp::Archive { .. }));
    assert_eq!(v.origin, [0, 0, 1, 1]);
}

#[test]
fn too_many_operations_are_refused_past_the_limit() {
    let w = world();
    let op = w.fill(r#"{"op":"set_status","id":"{fact}","status":"da_verificare"}"#);
    let list = vec![op.as_str(); operations::MAX_OPERATIONS + 1].join(",");
    let v = w.judge(&format!("[{list}]"));
    let last = v.rejected.last().unwrap();
    assert_eq!(last.index, operations::MAX_OPERATIONS);
    assert!(last.reason.contains("more operations"));
}

/// The JSON example a prompt shows, as written in it.
fn example_in(prompt: &str, start: &str) -> String {
    prompt
        .lines()
        .find_map(|l| l.find(start).map(|i| l[i..].to_owned()))
        .unwrap_or_else(|| panic!("no example starting with {start}"))
}

#[test]
fn the_dream_and_build_example_parses_and_validates_as_written() {
    let w = world();
    // The initial build's prompt carries the same section (the dream's
    // too: `dream::prompt` pushes it), so this one example is theirs.
    assert!(
        connecto_memory::bootstrap_packet::instructions("x")
            .contains(operations::operations_prompt_section())
    );
    let example = example_in(operations::operations_prompt_section(), "Example: ").replacen(
        "Example: ",
        "",
        1,
    );
    // The same text, with this store's ids in place of the example's.
    let reply = example
        .replace("e-12", &w.user.to_string())
        .replace("p-abcd2345", w.note2.as_str());
    let v = w.judge(&reply);
    assert!(v.rejected.is_empty(), "{:?}", v.rejected);
    assert!(v.adjusted.is_empty(), "{:?}", v.adjusted);
    assert_eq!((v.ops.len(), v.absorb.len()), (1, 1));
}

#[test]
fn one_of_every_operation_as_the_prompt_shows_it_validates() {
    let w = world();
    let blob = "a".repeat(40);
    let reply = w.fill(&format!(
        r#"{{"operations":[
        {{"op":"create","ref":"r1","room":"decisioni","slug":"paging","kind":"decisione","standing":"proposta","status":"candidato","summary":"Page with cursors.","details":"Offsets drift.","scope":["project"],"anchors":[{{"path":"src/a.rs","blob":"{blob}"}}],"sources":["{{proposal}}"]}},
        {{"op":"create","ref":"r2","room":"officina","kind":"procedura","summary":"Release with make release.","scope":["component:src"],"sources":["{{user}}"]}},
        {{"op":"update","id":"{{fact}}","summary":"The store uses WAL.","details":"Set at open.","scope":["project"],"anchors":[{{"path":"src/db.rs","blob":"{blob}"}}],"standing":"ipotesi","sources":["{{code}}"]}},
        {{"op":"link","from":"{{fact}}","relation":"vedi_anche","to":"{{decision}}"}},
        {{"op":"set_status","id":"{{fact}}","status":"da_verificare","sources":["{{code}}"]}},
        {{"op":"merge","into":"{{decision}}","from":["r1"],"sources":["{{proposal}}"]}},
        {{"op":"move","id":"r2","to":"officina/release"}},
        {{"op":"absorb","annotation":"{{note2}}","into":"r2"}},
        {{"op":"keep","annotation":"{{note}}","reason":"Waiting for the user."}},
        {{"op":"supersede","old":"{{fact}}","by":"r2","sources":["{{code}}"]}},
        {{"op":"archive","id":"{{decision}}","sources":["{{user}}"]}}
        ]}}"#
    ));
    let v = w.judge(&reply);
    assert!(v.rejected.is_empty(), "{:?}", v.rejected);
    assert!(v.adjusted.is_empty(), "{:?}", v.adjusted);
    // One scope string, the models' usual slip: read as a list, and said so.
    let v = w.judge(&w.fill(
        r#"{"operations":[{"op":"create","ref":"r1","room":"officina","kind":"procedura","summary":"Release with make.","scope":"project","sources":["{user}"]},{"op":"absorb","annotation":"{note2}","into":"r1"}]}"#,
    ));
    assert!(v.rejected.is_empty(), "{:?}", v.rejected);
    assert_eq!(v.adjusted.len(), 1);
    assert!(v.adjusted[0].reason.contains("scope"), "{:?}", v.adjusted);
    // A refused create makes no ref, and the absorb says why.
    let v = w.judge(&w.fill(
        r#"{"operations":[{"op":"create","ref":"r1","room":"officina","kind":"procedura","summary":"Release with make.","sources":"{user}"},{"op":"absorb","annotation":"{note2}","into":"r1"}]}"#,
    ));
    assert_eq!(v.rejected.len(), 2);
    assert!(
        v.rejected[0].reason.contains(r#"e.g. ["e-12"]"#),
        "{:?}",
        v.rejected
    );
    assert!(
        v.rejected[1].reason.contains("refused makes no ref"),
        "{:?}",
        v.rejected
    );
}

#[test]
fn the_development_example_parses_and_validates_as_written() {
    let w = world();
    let prompt = connecto_memory::prompt::instructions("x");
    let example = example_in(&prompt, "{\"annotations\"").replace("e-12", &w.user.to_string());
    let items = connecto_memory::validate::parse_reply(&example).unwrap();
    let sources = [w.user].into_iter().collect();
    let v = connecto_memory::validate::validate(
        &items,
        &connecto_memory::validate::Packet {
            sources: &sources,
            secrets: &[],
            now_ms: T0,
        },
        &w.store.read(),
    );
    assert!(v.rejected.is_empty(), "{:?}", v.rejected);
    assert_eq!((v.accepted.len(), v.downgraded), (1, 0));
    // Every field the prompt lists is a field the validator reads.
    let listed: Vec<&str> = prompt
        .lines()
        .filter_map(|l| l.strip_prefix("- ")?.split_once(':').map(|(f, _)| f))
        .map(|f| f.split(',').next().unwrap_or(f))
        .filter(|f| !f.contains(' '))
        .collect();
    assert_eq!(
        listed,
        [
            "type",
            "about",
            "room",
            "memory_kind",
            "standing",
            "text",
            "scope",
            "sources"
        ]
    );
}

#[test]
fn the_development_cycle_reads_one_scope_string_and_names_shapes() {
    let w = world();
    let sources = [w.user].into_iter().collect();
    let packet = connecto_memory::validate::Packet {
        sources: &sources,
        secrets: &[],
        now_ms: T0,
    };
    let judge = |reply: &str| {
        let items = connecto_memory::validate::parse_reply(&w.fill(reply)).unwrap();
        connecto_memory::validate::validate(&items, &packet, &w.store.read())
    };
    let v = judge(
        r#"[{"type":"integrazione","room":"officina","text":"Release with make.","scope":"project","sources":["{user}"]}]"#,
    );
    assert!(v.rejected.is_empty(), "{:?}", v.rejected);
    assert_eq!(v.accepted[0].scopes, [Scope::Project]);
    assert_eq!(v.adjusted.len(), 1);
    assert!(v.adjusted[0].contains("scope"));
    let v = judge(
        r#"[{"type":"integrazione","text":"x y","sources":"{user}"},{"type":"integrazione","text":"x y","scope":"project, branch:x","sources":["{user}"]},{"type":1,"text":"x y","sources":["{user}"]}]"#,
    );
    assert_eq!(v.rejected.len(), 3);
    assert!(
        v.rejected[0].contains(r#"e.g. ["e-12"]"#),
        "{:?}",
        v.rejected
    );
    assert!(
        v.rejected[1].contains(r#"e.g. ["project"]"#),
        "{:?}",
        v.rejected
    );
    assert!(
        v.rejected[2].contains("it must be one string"),
        "{:?}",
        v.rejected
    );
    // The note limit in the prompt is the real one.
    let prompt = connecto_memory::prompt::instructions("x");
    assert!(prompt.contains(&format!(
        "at most {} bytes",
        connecto_core::memory::MAX_NOTE_BYTES
    )));
}

#[test]
fn the_build_prompt_names_the_decision_records_the_build_accepts() {
    use connecto_memory::bootstrap_inventory::decision_record;
    let prompt = connecto_memory::bootstrap_packet::instructions("x");
    for (word, path) in [
        ("adr", "adr/x.md"),
        ("adrs", "adrs/x.md"),
        ("decisions", "decisions/x.md"),
        ("decision-records", "decision-records/x.md"),
        ("rfc", "rfc/x.md"),
        ("rfcs", "rfcs/x.md"),
        ("adr-", "adr-1.md"),
        ("adr_", "adr_1.md"),
        ("decision-", "decision-1.md"),
        ("rfc-", "rfc-1.md"),
        ("AGENTS.md", "AGENTS.md"),
        ("CLAUDE.md", "CLAUDE.md"),
        ("CONTRIBUTING.md", "CONTRIBUTING.md"),
        ("CONVENTIONS.md", "CONVENTIONS.md"),
    ] {
        assert!(decision_record(path), "{path}");
        assert!(prompt.contains(word), "{word}");
    }
}

/// The trial's failure (all 26 proposals of a real build refused): the
/// prompt showed `"blob":"<hash>"` but the material shows no hash, so the
/// model wrote `"blob":""` and each memory was refused for its anchor. An
/// anchor without a blob takes the hash Connecto recorded for the file in
/// the run's material, or is left out; never refuses the memory, and an
/// update whose anchors are all left out keeps the memory's own.
#[test]
fn an_anchor_without_a_blob_takes_the_recorded_hash_or_is_left_out() {
    let mut w = world();
    let blob = "3".repeat(40);
    let mut e = NewEvidence::new(EvidenceKind::ProjectDocument, T0, red("# Readme"));
    e.files = vec![connecto_core::memory::Anchor::new("README.md", &blob).unwrap()];
    let doc = w.store.insert_evidence(e).unwrap().id;
    let packet = Packet::new(
        vec![doc, w.user],
        vec![w.fact.clone()],
        Vec::new(),
        w.cutoff,
    );
    let reply = w.fill(&format!(
        r#"{{"operations":[
        {{"op":"create","room":"architettura","kind":"fatto","summary":"Docs live in README.","scope":["project"],"anchors":[{{"path":"README.md","blob":""}},{{"path":"config/poc.json"}}],"sources":["{doc}"]}},
        {{"op":"update","id":"{{fact}}","anchors":[{{"path":"nowhere.txt","blob":null}}],"sources":["{{user}}"]}},
        {{"op":"create","room":"architettura","kind":"fatto","summary":"Bad path.","anchors":[{{"path":"../etc/passwd"}}],"sources":["{doc}"]}},
        {{"op":"create","room":"architettura","kind":"fatto","summary":"A made-up hash.","anchors":[{{"path":"README.md","blob":"abc"}}],"sources":["{doc}"]}}
        ]}}"#
    ));
    let v = operations::validate(
        &w.store.read(),
        &packet,
        operations::parse(&reply).unwrap(),
        &[],
    );
    let reasons: Vec<(usize, &str)> = v
        .rejected
        .iter()
        .map(|r| (r.index, r.reason.as_str()))
        .collect();
    // A path outside the project and a malformed hash are still refused.
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert_eq!(reasons[0].0, 2);
    assert!(reasons[0].1.starts_with("anchors:"), "{reasons:?}");
    assert_eq!(reasons[1].0, 3);
    assert!(reasons[1].1.contains("blob"), "{reasons:?}");
    let created = v
        .ops
        .iter()
        .find_map(|op| match op {
            MemoryOp::Create(m) => Some(m),
            _ => None,
        })
        .unwrap();
    assert_eq!(created.anchors.len(), 1);
    assert_eq!(created.anchors[0].path, "README.md");
    assert_eq!(
        created.anchors[0].blob, blob,
        "the recorded hash, not the model's"
    );
    let update = v
        .ops
        .iter()
        .find_map(|op| match op {
            MemoryOp::Update(u) => Some(u),
            _ => None,
        })
        .unwrap();
    assert_eq!(update.anchors, None, "the memory keeps its own anchors");
    let adjusted: Vec<String> = v
        .adjusted
        .iter()
        .map(|a| format!("{}: {}", a.index, a.reason))
        .collect();
    assert!(
        adjusted
            .iter()
            .any(|a| a.starts_with("0: anchors: README.md: no blob given; the hash Connecto")),
        "{adjusted:?}"
    );
    assert!(
        adjusted
            .iter()
            .any(|a| a.starts_with("0: anchors: config/poc.json:") && a.contains("left out")),
        "{adjusted:?}"
    );
    assert!(
        adjusted
            .iter()
            .any(|a| a.starts_with("1: anchors: nowhere.txt:") && a.contains("left out")),
        "{adjusted:?}"
    );
}

/// A summary over the limit keeps its first sentences and moves the rest,
/// word for word, before the details; one long sentence is refused with
/// the limit named. An update moves nothing into details it does not
/// replace.
#[test]
fn a_long_summary_moves_its_tail_to_details_or_is_refused_with_the_limit() {
    let w = world();
    let first = "The store is SQLite in WAL mode.";
    let tail = "x".repeat(380) + ".";
    let long = format!("{first} {tail} Last sentence.");
    let one_sentence = "y".repeat(450);
    let reply = w.fill(&format!(
        r#"{{"operations":[
        {{"op":"create","room":"architettura","kind":"fatto","summary":"{long}","details":"Original details.","sources":["{{user}}"]}},
        {{"op":"create","room":"architettura","slug":"one","kind":"fatto","summary":"{one_sentence}","sources":["{{user}}"]}},
        {{"op":"update","id":"{{fact}}","summary":"{long}","sources":["{{user}}"]}}
        ]}}"#
    ));
    let v = w.judge(&reply);
    let reasons: Vec<(usize, String)> = v
        .rejected
        .iter()
        .map(|r| (r.index, r.reason.clone()))
        .collect();
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert_eq!(reasons[0].0, 1);
    assert!(
        reasons[0].1.contains("at most 400 characters"),
        "{reasons:?}"
    );
    assert_eq!(reasons[1].0, 2, "no details to move the tail into");
    let MemoryOp::Create(m) = &v.ops[0] else {
        panic!("{:?}", v.ops)
    };
    assert_eq!(m.summary.get(), first);
    assert_eq!(
        m.details.get(),
        &format!("{tail} Last sentence.\n\nOriginal details.")
    );
    assert!(
        v.adjusted
            .iter()
            .any(|a| a.index == 0 && a.reason.starts_with("summary: over 400 characters")),
        "{:?}",
        v.adjusted
    );
}

/// Review finding: a summary cut before redaction could split a known
/// secret across the cut ("alpha." in the summary, "beta" in details) so
/// neither part matched it. Each field is redacted whole first, then cut.
#[test]
fn a_secret_across_the_summary_cut_is_redacted_on_create_and_update() {
    let w = world();
    let secret = "alpha. beta".to_owned();
    // "alpha." ends inside the limit: cutting the raw text there would put
    // "alpha." in the summary and "beta" in details.
    let lead = "First sentence.";
    let filler = "w".repeat(400 - lead.len() - " alpha.".len() - 1);
    let summary = format!("{lead} {filler} alpha. beta {} Last.", "z".repeat(60));
    assert!(summary.find("alpha.").unwrap() + "alpha.".len() <= 400);
    let reply = w.fill(&format!(
        r#"{{"operations":[
        {{"op":"create","room":"architettura","kind":"fatto","summary":"{summary}","details":"More.","sources":["{{user}}"]}},
        {{"op":"update","id":"{{fact}}","summary":"{summary}","details":"More.","sources":["{{user}}"]}}
        ]}}"#
    ));
    let v = w.judge_with(&reply, std::slice::from_ref(&secret));
    assert!(v.rejected.is_empty(), "{:?}", v.rejected);
    let texts: Vec<(String, String)> = v
        .ops
        .iter()
        .filter_map(|op| match op {
            MemoryOp::Create(m) => Some((m.summary.get().clone(), m.details.get().clone())),
            MemoryOp::Update(u) => Some((
                u.summary.as_ref()?.get().clone(),
                u.details.as_ref()?.get().clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(texts.len(), 2, "{:?}", v.ops);
    for (summary, details) in &texts {
        for part in ["alpha", "beta"] {
            assert!(!summary.contains(part), "{summary}");
            assert!(!details.contains(part), "{details}");
        }
        assert_eq!(summary, lead, "cut after the sentence that still fits");
        assert!(details.contains("[REDACTED]"), "{details}");
        assert!(details.ends_with("Last.\n\nMore."), "{details}");
    }
}
