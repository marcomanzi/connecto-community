//! The initial build's reconciliation and truth rules (spec §5.9, §5.10),
//! applied by the host to the operations each packet's run proposed (already
//! judged by [`crate::operations::validate`]) before the one publication.
//!
//! **Reconciliation, never a reset.**
//! - Creations about the same subject in different packets (same address,
//!   or same kind and same summary) become one memory with every source.
//! - A creation about a memory that already exists (same address, or same
//!   kind and summary) only adds its sources to it.
//! - Existing memories are not reorganised here (no merge, supersede, move or
//!   archive: that is the dream's work), a memory the user accepted (an
//!   accepted or discarded decision or convention) or a verified one keeps
//!   its text and standing. Reconciliation itself never lowers a standing;
//!   it may set `da_verificare` or `conflitto` where the new material says
//!   so, and the store-wide check at publication lowers a verification or a
//!   standing its evidence no longer supports (§5.5, point 7).
//! - Duplicate links and updates that change nothing are dropped.
//!
//! **Truth rules, enforced here whatever the model said.** A "user
//! statement" here is only one made in the Connecto window (already in the
//! store, [`crate::user_statement`]): no turn of a past conversation is one
//! ([`crate::bootstrap_packet`]), so old conversations never make anything
//! `accettata`, `accertato`, `verificato`, `eseguita` or `implementato`.
//! - `accettata` / `scartata` need a user statement or a recorded decision
//!   (an ADR, a decision log, the project's instruction files): a README, a
//!   manifest or code is not the user's decision. Otherwise `proposta`.
//! - `accertato` needs a user statement or something the host observed
//!   itself (paths, blobs, branch, HEAD, the paths a commit changed): the
//!   text of any file (code included), commit messages and claims repeated
//!   across old conversations stay `ipotesi`.
//! - `verificato` needs a user statement or a host observation among its
//!   sources, and a standing above the kind's weakest that they allow:
//!   a documented procedure (tests only written down) or a hypothesis never
//!   becomes verified. Otherwise `candidato`.
//! - `eseguita` needs a user statement or a reported outcome; `implementato`
//!   a user statement or a git fact Connecto observed (a file's text, a
//!   commit message or a TODO is not one).

use std::collections::{BTreeMap, BTreeSet};

use connecto_core::memory::{
    Address, EvidenceId, EvidenceKind, Memory, MemoryId, MemoryKind, MemoryStatus, SourceRef,
    Standing,
};

use crate::bootstrap_inventory::decision_record;
use crate::error::Result;
use crate::publish::{MemoryOp, MemoryUpdate, NewMemory};
use crate::read::Read;

/// What the host knows of one piece of evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub kind: EvidenceKind,
    /// The first file it names, if any.
    pub path: Option<String>,
}

/// The facts behind `ids`, read from the store. A `dichiarazione_utente`
/// that was not made in the Connecto window
/// ([`crate::user_statement::is_window_statement`]: a terminal, a session or
/// a log names it) is taken as what live capture makes of a prompt of
/// unknown author, `affermazione_agente`: in the build only the window's
/// statements count as the user's (defence in depth; no current path writes
/// another).
pub fn facts(read: &Read<'_>, ids: &BTreeSet<EvidenceId>) -> Result<BTreeMap<EvidenceId, Fact>> {
    let mut out = BTreeMap::new();
    for id in ids {
        if let Some(e) = read.evidence(*id)? {
            let kind = if e.kind == EvidenceKind::UserStatement
                && !crate::user_statement::is_window_statement(&e)
            {
                EvidenceKind::AgentClaim
            } else {
                e.kind
            };
            out.insert(
                *id,
                Fact {
                    kind,
                    path: e.files.first().map(|a| a.path.clone()),
                },
            );
        }
    }
    Ok(out)
}

/// Every evidence id the operations cite.
#[must_use]
pub fn cited(ops: &[MemoryOp]) -> BTreeSet<EvidenceId> {
    let mut out = BTreeSet::new();
    for op in ops {
        match op {
            MemoryOp::Create(m) => out.extend(m.sources.iter().copied()),
            MemoryOp::Update(u) => out.extend(u.add_sources.iter().copied()),
            MemoryOp::SetStatus { sources, .. } => out.extend(sources.iter().copied()),
            _ => {}
        }
    }
    out
}

fn kinds(sources: &[EvidenceId], facts: &BTreeMap<EvidenceId, Fact>) -> Vec<Fact> {
    sources
        .iter()
        .filter_map(|s| facts.get(s).cloned())
        .collect()
}

/// Whether a piece of evidence can establish a fact or verify a memory in
/// an initial build: only a user statement, or something the host observed
/// itself (a path exists, a blob hash, the branch or HEAD, a commit changed
/// a path). The text of a file of any kind (code with its comments and
/// strings, a manifest, a README, an ADR) proves only that the file says
/// so; a commit message or a conversation is an author's word. Spec §5.9:
/// reasons inferred from code stay hypotheses.
fn establishes(f: &Fact) -> bool {
    matches!(
        f.kind,
        EvidenceKind::UserStatement | EvidenceKind::CodeObservation
    )
}

/// Whether the facts allow `standing` under the initial build's rules (on
/// top of the core rules the store applies).
#[must_use]
pub fn allows(standing: Standing, facts: &[Fact]) -> bool {
    match standing {
        Standing::Accepted | Standing::Discarded => facts.iter().any(|f| {
            f.kind == EvidenceKind::UserStatement
                || (f.kind == EvidenceKind::ProjectDocument
                    && f.path.as_deref().is_some_and(decision_record))
        }),
        Standing::Established => facts.iter().any(establishes),
        Standing::Exercised => facts.iter().any(|f| {
            matches!(
                f.kind,
                EvidenceKind::UserStatement | EvidenceKind::ReportedOutcome
            )
        }),
        Standing::Implemented => facts.iter().any(|f| {
            matches!(
                f.kind,
                EvidenceKind::UserStatement | EvidenceKind::CodeObservation
            )
        }),
        _ => true,
    }
}

/// Whether `verificato` is allowed for a memory of this kind and standing.
#[must_use]
pub fn allows_verified(kind: MemoryKind, standing: Standing, facts: &[Fact]) -> bool {
    standing != Standing::default_for(kind)
        && allows(standing, facts)
        && facts.iter().any(establishes)
}

/// A memory the user stands behind, or a verified one: its text and
/// standing are kept.
fn explicit(m: &Memory) -> bool {
    matches!(m.standing, Standing::Accepted | Standing::Discarded)
        || m.status == MemoryStatus::Verified
}

fn rank(standing: Standing, kind: MemoryKind) -> usize {
    Standing::allowed_for(kind)
        .iter()
        .position(|s| *s == standing)
        .unwrap_or(0)
}

fn status_rank(status: MemoryStatus) -> u8 {
    match status {
        MemoryStatus::Verified => 3,
        MemoryStatus::Conflict => 2,
        MemoryStatus::Candidate => 1,
        MemoryStatus::ToVerify | MemoryStatus::Superseded => 0,
    }
}

/// Words of a summary, to tell the same subject apart from a rewording of
/// punctuation or case.
fn norm(summary: &str) -> String {
    summary
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// What reconciling did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub ops: Vec<MemoryOp>,
    /// For each of `ops`, the input operations (by position) it carries:
    /// one for most, several for the sources gained by an existing memory
    /// or a creation others were joined into.
    pub origins: Vec<Vec<usize>>,
    /// Input operations (by position) refused in whole or in part, with
    /// why. Every refusal goes through here (`dropped` is derived from it).
    pub refused: Vec<(usize, String)>,
    /// Input operations (by position) kept in another form (joined to a
    /// memory of the same subject, a claim lowered, a duplicate), with why.
    pub adjusted: Vec<(usize, String)>,
    /// The reasons of `refused`, in order (for the run report; no model
    /// text).
    pub dropped: Vec<String>,
    /// Claims lowered by the truth rules.
    pub lowered: Vec<String>,
}

fn source_ids(sources: &[SourceRef]) -> Vec<EvidenceId> {
    sources.iter().map(|s| s.evidence).collect()
}

/// Reconcile the packets' operations, in order, with the memory as it is
/// now, and apply the truth rules. `facts` must cover every cited evidence
/// and every source of the existing memories touched.
#[allow(clippy::too_many_lines)]
pub fn reconcile(
    existing: &[Memory],
    ops: Vec<MemoryOp>,
    facts: &BTreeMap<EvidenceId, Fact>,
) -> Reconciled {
    let live: BTreeMap<&MemoryId, &Memory> = existing.iter().map(|m| (&m.id, m)).collect();
    let by_address: BTreeMap<&Address, &MemoryId> =
        existing.iter().map(|m| (&m.address, &m.id)).collect();
    let by_summary: BTreeMap<(MemoryKind, String), &MemoryId> = existing
        .iter()
        .filter(|m| m.status != MemoryStatus::Superseded)
        .map(|m| ((m.kind, norm(&m.summary)), &m.id))
        .collect();

    let mut out = Reconciled::default();
    // New memories, by id, with their position in `ops`.
    let mut created: BTreeMap<MemoryId, usize> = BTreeMap::new();
    let mut new_by_address: BTreeMap<Address, MemoryId> = BTreeMap::new();
    let mut new_by_summary: BTreeMap<(MemoryKind, String), MemoryId> = BTreeMap::new();
    // Ids replaced by the memory they are joined to.
    let mut alias: BTreeMap<MemoryId, MemoryId> = BTreeMap::new();
    let mut result: Vec<Option<MemoryOp>> = Vec::new();
    let mut links: BTreeSet<(MemoryId, String, MemoryId)> = BTreeSet::new();
    // Sources each existing memory gains.
    let mut gains: BTreeMap<MemoryId, BTreeSet<EvidenceId>> = BTreeMap::new();

    let resolve = |alias: &BTreeMap<MemoryId, MemoryId>, id: &MemoryId| -> MemoryId {
        alias.get(id).cloned().unwrap_or_else(|| id.clone())
    };

    // Which inputs each `result` entry carries, and each gaining memory.
    let mut result_origin: Vec<Vec<usize>> = Vec::new();
    let mut gain_origin: BTreeMap<MemoryId, Vec<usize>> = BTreeMap::new();
    // Every input ends in an output's origins, in `refused` or in
    // `adjusted`: the only two ways to record what happened to it.
    let refuse = |out: &mut Reconciled, at: usize, why: String| {
        out.refused.push((at, why));
    };
    let adjust = |out: &mut Reconciled, at: usize, why: String| {
        out.adjusted.push((at, why));
    };
    for (at, op) in ops.into_iter().enumerate() {
        match op {
            MemoryOp::Create(mut m) => {
                let Some(id) = m.id.clone() else {
                    refuse(&mut out, at, "a creation without an id".to_owned());
                    continue;
                };
                let key = (m.kind, norm(m.summary.get()));
                // Same subject as an existing memory: add the sources only.
                if let Some(target) = by_address
                    .get(&m.address)
                    .or_else(|| by_summary.get(&key))
                    .map(|t| (*t).clone())
                {
                    gains.entry(target.clone()).or_default().extend(&m.sources);
                    gain_origin.entry(target.clone()).or_default().push(at);
                    adjust(
                        &mut out,
                        at,
                        format!("the same subject as {target}: its sources join that memory"),
                    );
                    alias.insert(id, target);
                    continue;
                }
                // Same subject as a creation of an earlier packet: join.
                if let Some(first) = new_by_address
                    .get(&m.address)
                    .or_else(|| new_by_summary.get(&key))
                    .cloned()
                {
                    if let Some(i) = created.get(&first)
                        && let Some(o) = result_origin.get_mut(*i)
                    {
                        o.push(at);
                    }
                    adjust(
                        &mut out,
                        at,
                        "the same subject as another creation of this build: joined to it"
                            .to_owned(),
                    );
                    if let Some(Some(MemoryOp::Create(keep))) =
                        created.get(&first).and_then(|i| result.get_mut(*i))
                    {
                        for s in m.sources {
                            if !keep.sources.contains(&s) {
                                keep.sources.push(s);
                            }
                        }
                        for s in m.scopes {
                            if !keep.scopes.contains(&s) {
                                keep.scopes.push(s);
                            }
                        }
                        for a in m.anchors {
                            if !keep.anchors.iter().any(|k| k.path == a.path) {
                                keep.anchors.push(a);
                            }
                        }
                    }
                    alias.insert(id, first);
                    continue;
                }
                // The truth rules, on the creation's own sources.
                let f = kinds(&m.sources, facts);
                let standing = m.standing.unwrap_or_else(|| Standing::default_for(m.kind));
                if !allows(standing, &f) {
                    let why = format!("{id}: {} to the weakest", standing.word());
                    out.lowered.push(why.clone());
                    adjust(&mut out, at, why);
                    m.standing = None;
                }
                let standing = m.standing.unwrap_or_else(|| Standing::default_for(m.kind));
                if m.status == MemoryStatus::Verified && !allows_verified(m.kind, standing, &f) {
                    let why = format!("{id}: verificato to candidato");
                    out.lowered.push(why.clone());
                    adjust(&mut out, at, why);
                    m.status = MemoryStatus::Candidate;
                }
                m.relations
                    .iter_mut()
                    .for_each(|r| r.target = resolve(&alias, &r.target));
                new_by_address.insert(m.address.clone(), id.clone());
                new_by_summary.insert(key, id.clone());
                created.insert(id, result.len());
                result.push(Some(MemoryOp::Create(m)));
                result_origin.push(vec![at]);
            }
            MemoryOp::Update(mut u) => {
                u.id = resolve(&alias, &u.id);
                if let Some(m) = live.get(&u.id) {
                    gains
                        .entry(u.id.clone())
                        .or_default()
                        .extend(&u.add_sources);
                    gain_origin.entry(u.id.clone()).or_default().push(at);
                    let mut all = source_ids(&m.sources);
                    all.extend(gains.get(&u.id).into_iter().flatten().copied());
                    let f = kinds(&all, facts);
                    if explicit(m)
                        && (u.summary.is_some()
                            || u.details.is_some()
                            || u.scopes.is_some()
                            || u.standing.is_some())
                    {
                        // Its text and standing are refused (its sources may
                        // still join the memory: the proposal is refused).
                        refuse(
                            &mut out,
                            at,
                            format!(
                                "{}: an accepted or verified memory keeps its text and standing",
                                u.id
                            ),
                        );
                        u.summary = None;
                        u.details = None;
                        u.scopes = None;
                        u.standing = None;
                    }
                    if let Some(s) = u.standing
                        && (rank(s, m.kind) < rank(m.standing, m.kind) || !allows(s, &f))
                    {
                        let why = format!("{}: standing kept", u.id);
                        out.lowered.push(why.clone());
                        adjust(&mut out, at, why);
                        u.standing = None;
                    }
                    let fresh: Vec<EvidenceId> = u
                        .add_sources
                        .iter()
                        .copied()
                        .filter(|e| !m.sources.iter().any(|s| s.evidence == *e))
                        .collect();
                    u.add_sources = fresh;
                    let nothing = u.summary.is_none()
                        && u.details.is_none()
                        && u.scopes.is_none()
                        && u.anchors.is_none()
                        && u.standing.is_none();
                    if nothing {
                        // Only sources: gathered below, once per memory.
                        if u.add_sources.is_empty() && !out.refused.iter().any(|(i, _)| *i == at) {
                            adjust(
                                &mut out,
                                at,
                                format!("{}: nothing new, it already has these sources", u.id),
                            );
                        }
                        continue;
                    }
                    u.add_sources.clear();
                    result.push(Some(MemoryOp::Update(u)));
                    result_origin.push(vec![at]);
                } else if let Some(Some(MemoryOp::Create(keep))) =
                    created.get(&u.id).and_then(|i| result.get_mut(*i))
                {
                    if let Some(o) = created.get(&u.id).and_then(|i| result_origin.get_mut(*i)) {
                        o.push(at);
                    }
                    if u.summary.is_some()
                        || u.details.is_some()
                        || u.scopes.is_some()
                        || u.anchors.is_some()
                        || u.standing.is_some()
                    {
                        adjust(
                            &mut out,
                            at,
                            format!(
                                "{}: created in this build: only the update's sources were kept",
                                u.id
                            ),
                        );
                    }
                    // An update of a memory created in this build: fold it in.
                    for s in u.add_sources {
                        if !keep.sources.contains(&s) {
                            keep.sources.push(s);
                        }
                    }
                } else {
                    refuse(&mut out, at, format!("{}: not a memory to update", u.id));
                }
            }
            MemoryOp::SetStatus {
                id,
                status,
                sources,
            } => {
                let id = resolve(&alias, &id);
                if let Some(m) = live.get(&id) {
                    gains.entry(id.clone()).or_default().extend(&sources);
                    gain_origin.entry(id.clone()).or_default().push(at);
                    let mut all = source_ids(&m.sources);
                    all.extend(gains.get(&id).into_iter().flatten().copied());
                    let f = kinds(&all, facts);
                    let lowers = status_rank(status) < status_rank(m.status)
                        && !matches!(status, MemoryStatus::ToVerify | MemoryStatus::Conflict);
                    let unsupported = status == MemoryStatus::Verified
                        && !allows_verified(m.kind, m.standing, &f);
                    if lowers || unsupported || (explicit(m) && status == MemoryStatus::Candidate) {
                        let why = format!("{id}: status kept");
                        out.lowered.push(why.clone());
                        refuse(&mut out, at, why);
                        continue;
                    }
                    result.push(Some(MemoryOp::SetStatus {
                        id,
                        status,
                        sources,
                    }));
                    result_origin.push(vec![at]);
                } else if let Some(Some(MemoryOp::Create(keep))) =
                    created.get(&id).and_then(|i| result.get_mut(*i))
                {
                    if let Some(o) = created.get(&id).and_then(|i| result_origin.get_mut(*i)) {
                        o.push(at);
                    }
                    for s in sources {
                        if !keep.sources.contains(&s) {
                            keep.sources.push(s);
                        }
                    }
                    let f = kinds(&keep.sources, facts);
                    let standing = keep
                        .standing
                        .unwrap_or_else(|| Standing::default_for(keep.kind));
                    if status != MemoryStatus::Verified || allows_verified(keep.kind, standing, &f)
                    {
                        keep.status = status;
                    } else {
                        let why = format!("{id}: verificato to candidato");
                        out.lowered.push(why.clone());
                        adjust(&mut out, at, why);
                    }
                } else {
                    refuse(&mut out, at, format!("{id}: not a memory"));
                }
            }
            MemoryOp::Link { from, kind, to } => {
                let (from, to) = (resolve(&alias, &from), resolve(&alias, &to));
                if from == to {
                    adjust(
                        &mut out,
                        at,
                        "link: both ends are the same memory once joined".to_owned(),
                    );
                    continue;
                }
                if !links.insert((from.clone(), kind.word().to_owned(), to.clone())) {
                    adjust(
                        &mut out,
                        at,
                        "link: the same link is already proposed".to_owned(),
                    );
                    continue;
                }
                let known = |id: &MemoryId| live.contains_key(id) || created.contains_key(id);
                if known(&from) && known(&to) {
                    result.push(Some(MemoryOp::Link { from, kind, to }));
                    result_origin.push(vec![at]);
                } else {
                    refuse(&mut out, at, "link: an end is not a memory".to_owned());
                }
            }
            MemoryOp::Merge { .. }
            | MemoryOp::Supersede { .. }
            | MemoryOp::Move { .. }
            | MemoryOp::Archive { .. } => {
                refuse(
                    &mut out,
                    at,
                    "reorganising memories is left to the dream, not the initial build".to_owned(),
                );
            }
        }
    }
    // Sources gained by existing memories, once per memory, first.
    let mut ops: Vec<MemoryOp> = Vec::new();
    let mut origins: Vec<Vec<usize>> = Vec::new();
    for (id, sources) in gains {
        let Some(m) = live.get(&id) else { continue };
        let fresh: Vec<EvidenceId> = sources
            .into_iter()
            .filter(|e| !m.sources.iter().any(|s| s.evidence == *e))
            .collect();
        if !fresh.is_empty() {
            origins.push(gain_origin.remove(&id).unwrap_or_default());
            let mut u = MemoryUpdate::new(id);
            u.add_sources = fresh;
            ops.push(MemoryOp::Update(u));
        }
    }
    for (op, origin) in result.into_iter().zip(result_origin) {
        if let Some(op) = op {
            ops.push(op);
            origins.push(origin);
        }
    }
    out.ops = ops;
    out.origins = origins;
    out.dropped = out.refused.iter().map(|(_, why)| why.clone()).collect();
    out
}

/// The text of an operation's subject ids and address, to find the one a
/// store refusal names.
#[must_use]
pub fn names(op: &MemoryOp, subject: &str) -> bool {
    let id = |m: &MemoryId| m.as_str() == subject;
    match op {
        MemoryOp::Create(NewMemory { id: m, address, .. }) => {
            m.as_ref().is_some_and(id) || address.to_string() == subject
        }
        MemoryOp::Update(u) => id(&u.id),
        MemoryOp::Merge { into, from } => id(into) || from.iter().any(id),
        MemoryOp::Supersede { old, by } => id(old) || id(by),
        MemoryOp::SetStatus { id: m, .. } | MemoryOp::Move { id: m, .. } => id(m),
        MemoryOp::Archive { id: m } => id(m),
        MemoryOp::Link { from, to, .. } => id(from) || id(to),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use connecto_core::Redacted;
    use connecto_core::memory::{Freshness, RelationKind, Scope, ScopeEntry};

    fn fact(kind: EvidenceKind, path: Option<&str>) -> Fact {
        Fact {
            kind,
            path: path.map(str::to_owned),
        }
    }

    fn create(id: u8, address: &str, kind: MemoryKind, sources: &[u64]) -> NewMemory {
        NewMemory {
            id: Some(MemoryId::from_entropy([id; 5])),
            address: Address::parse(address).unwrap(),
            kind,
            standing: None,
            summary: Redacted::assume(format!("About {address}.")),
            details: Redacted::assume(String::new()),
            scopes: vec![Scope::Project],
            status: MemoryStatus::Candidate,
            anchors: Vec::new(),
            sources: sources.iter().map(|s| EvidenceId::new(*s)).collect(),
            relations: Vec::new(),
        }
    }

    fn facts_of(list: &[(u64, Fact)]) -> BTreeMap<EvidenceId, Fact> {
        list.iter()
            .map(|(i, f)| (EvidenceId::new(*i), f.clone()))
            .collect()
    }

    #[test]
    fn standings_follow_the_build_rules() {
        let readme = fact(EvidenceKind::ProjectDocument, Some("README.md"));
        let adr = fact(EvidenceKind::ProjectDocument, Some("docs/adr/1.md"));
        let user = fact(EvidenceKind::UserStatement, None);
        let agent = fact(EvidenceKind::AgentClaim, None);
        let code = fact(EvidenceKind::CodeObservation, Some("src/a.rs"));
        assert!(!allows(Standing::Accepted, std::slice::from_ref(&readme)));
        assert!(!allows(Standing::Accepted, std::slice::from_ref(&code)));
        assert!(!allows(Standing::Accepted, std::slice::from_ref(&agent)));
        assert!(allows(Standing::Accepted, std::slice::from_ref(&adr)));
        assert!(allows(Standing::Accepted, std::slice::from_ref(&user)));
        assert!(!allows(
            Standing::Established,
            &[agent.clone(), agent.clone()]
        ));
        assert!(allows(Standing::Established, std::slice::from_ref(&code)));
        let doc_code = fact(EvidenceKind::ProjectDocument, Some("src/a.rs"));
        assert!(!allows(
            Standing::Established,
            std::slice::from_ref(&doc_code)
        ));
        assert!(!allows_verified(
            MemoryKind::Decision,
            Standing::Accepted,
            std::slice::from_ref(&adr)
        ));
        assert!(!allows(
            Standing::Established,
            std::slice::from_ref(&readme)
        ));
        assert!(!allows(
            Standing::Implemented,
            std::slice::from_ref(&doc_code)
        ));
        assert!(!allows(Standing::Exercised, std::slice::from_ref(&readme)));
        assert!(!allows_verified(
            MemoryKind::Procedure,
            Standing::Documented,
            std::slice::from_ref(&readme)
        ));
        assert!(!allows_verified(
            MemoryKind::Fact,
            Standing::Hypothesis,
            std::slice::from_ref(&code)
        ));
        assert!(allows_verified(
            MemoryKind::Fact,
            Standing::Established,
            &[code]
        ));
    }

    #[test]
    fn creations_of_the_same_subject_join_and_existing_ones_gain_sources() {
        let facts = facts_of(&[
            (1, fact(EvidenceKind::ProjectDocument, Some("README.md"))),
            (2, fact(EvidenceKind::ProjectDocument, Some("docs/b.md"))),
            (3, fact(EvidenceKind::CodeObservation, Some("src/a.rs"))),
        ]);
        let existing = Memory {
            id: MemoryId::from_entropy([9; 5]),
            address: Address::parse("officina/build").unwrap(),
            kind: MemoryKind::Procedure,
            standing: Standing::Documented,
            summary: "Build with cargo.".to_owned(),
            details: String::new(),
            scopes: vec![ScopeEntry::new(Scope::Project)],
            status: MemoryStatus::Candidate,
            freshness: Freshness::default(),
            anchors: Vec::new(),
            sources: vec![SourceRef {
                evidence: EvidenceId::new(1),
                kind: EvidenceKind::ProjectDocument,
            }],
            relations: Vec::new(),
            created_ms: 0,
            updated_ms: 0,
            version: 1,
        };
        let mut verified = create(1, "officina/test", MemoryKind::Procedure, &[1]);
        verified.status = MemoryStatus::Verified;
        let ops = vec![
            MemoryOp::Create(verified),
            MemoryOp::Create(create(2, "officina/test", MemoryKind::Procedure, &[2])),
            MemoryOp::Create(create(3, "officina/build", MemoryKind::Procedure, &[3])),
        ];
        let r = reconcile(std::slice::from_ref(&existing), ops, &facts);
        assert_eq!(r.ops.len(), 2, "{r:?}");
        let MemoryOp::Update(u) = &r.ops[0] else {
            panic!("{r:?}")
        };
        assert_eq!(u.id, existing.id);
        assert_eq!(u.add_sources, [EvidenceId::new(3)]);
        let MemoryOp::Create(m) = &r.ops[1] else {
            panic!("{r:?}")
        };
        assert_eq!(m.sources, [EvidenceId::new(1), EvidenceId::new(2)]);
        assert_eq!(m.status, MemoryStatus::Candidate);
        // Run again with the same sources: nothing new for the existing one.
        let again = reconcile(
            std::slice::from_ref(&existing),
            vec![MemoryOp::Create(create(
                4,
                "officina/build",
                MemoryKind::Procedure,
                &[1],
            ))],
            &facts,
        );
        assert!(again.ops.is_empty(), "{again:?}");
    }

    fn existing(seed: u8, address: &str, kind: MemoryKind, standing: Standing) -> Memory {
        Memory {
            id: MemoryId::from_entropy([seed; 5]),
            address: Address::parse(address).unwrap(),
            kind,
            standing,
            summary: format!("About {address}."),
            details: String::new(),
            scopes: vec![ScopeEntry::new(Scope::Project)],
            status: MemoryStatus::Candidate,
            freshness: Freshness::default(),
            anchors: Vec::new(),
            sources: vec![SourceRef {
                evidence: EvidenceId::new(1),
                kind: EvidenceKind::ProjectDocument,
            }],
            relations: Vec::new(),
            created_ms: 0,
            updated_ms: 0,
            version: 1,
        }
    }

    /// Every input ends in an output's origins, in `refused` or in
    /// `adjusted`, over one input of each branch; and `dropped` holds
    /// exactly the refusals. A new branch that drops an input without
    /// recording it fails here.
    #[test]
    fn every_input_is_carried_adjusted_or_refused() {
        let facts = facts_of(&[
            (1, fact(EvidenceKind::ProjectDocument, Some("README.md"))),
            (2, fact(EvidenceKind::ProjectDocument, Some("docs/b.md"))),
            (3, fact(EvidenceKind::AgentClaim, None)),
        ]);
        let decision = existing(
            9,
            "decisioni/sqlite",
            MemoryKind::Decision,
            Standing::Accepted,
        );
        let build = existing(
            8,
            "officina/build",
            MemoryKind::Procedure,
            Standing::Documented,
        );
        let fresh = create(2, "officina/test", MemoryKind::Procedure, &[2]);
        let new_id = fresh.id.clone().unwrap();
        let mut no_id = create(1, "officina/x", MemoryKind::Procedure, &[2]);
        no_id.id = None;
        let mut unsupported = create(5, "architettura/y", MemoryKind::Fact, &[3]);
        unsupported.standing = Some(Standing::Established);
        let update = |id: &MemoryId, f: &dyn Fn(&mut MemoryUpdate)| {
            let mut u = MemoryUpdate::new(id.clone());
            f(&mut u);
            MemoryOp::Update(u)
        };
        let text =
            |u: &mut MemoryUpdate| u.summary = Some(Redacted::assume("New text.".to_owned()));
        let ghost = MemoryId::from_entropy([7; 5]);
        let joined = MemoryId::from_entropy([4; 5]);
        let ops = vec![
            /* 0 */ MemoryOp::Create(no_id),
            /* 1 */
            MemoryOp::Create(create(4, "officina/build", MemoryKind::Procedure, &[2])),
            /* 2 */ MemoryOp::Create(fresh),
            /* 3 */
            MemoryOp::Create(create(6, "officina/test", MemoryKind::Procedure, &[1])),
            /* 4 */ update(&decision.id, &text),
            /* 5 */ update(&build.id, &|u| u.add_sources = vec![EvidenceId::new(1)]),
            /* 6 */ update(&build.id, &|u| u.add_sources = vec![EvidenceId::new(2)]),
            /* 7 */ update(&build.id, &text),
            /* 8 */ update(&new_id, &text),
            /* 9 */ update(&ghost, &text),
            /* 10 */
            MemoryOp::SetStatus {
                id: decision.id.clone(),
                status: MemoryStatus::Candidate,
                sources: Vec::new(),
            },
            /* 11 */
            MemoryOp::SetStatus {
                id: build.id.clone(),
                status: MemoryStatus::ToVerify,
                sources: Vec::new(),
            },
            /* 12 */
            MemoryOp::SetStatus {
                id: new_id.clone(),
                status: MemoryStatus::Verified,
                sources: Vec::new(),
            },
            /* 13 */
            MemoryOp::Link {
                from: build.id.clone(),
                kind: RelationKind::SeeAlso,
                to: decision.id.clone(),
            },
            /* 14 */
            MemoryOp::Link {
                from: build.id.clone(),
                kind: RelationKind::SeeAlso,
                to: decision.id.clone(),
            },
            /* 15 */
            MemoryOp::Link {
                from: joined,
                kind: RelationKind::SeeAlso,
                to: build.id.clone(),
            },
            /* 16 */
            MemoryOp::Link {
                from: build.id.clone(),
                kind: RelationKind::SeeAlso,
                to: ghost.clone(),
            },
            /* 17 */
            MemoryOp::Merge {
                into: build.id.clone(),
                from: vec![decision.id.clone()],
            },
            /* 18 */ MemoryOp::Archive {
                id: build.id.clone(),
            },
            /* 19 */
            MemoryOp::Supersede {
                old: build.id.clone(),
                by: decision.id.clone(),
            },
            /* 20 */
            MemoryOp::Move {
                id: build.id.clone(),
                to: Address::parse("officina/moved").unwrap(),
            },
            /* 21 */ MemoryOp::Create(unsupported),
        ];
        let n = ops.len();
        let r = reconcile(&[decision, build], ops, &facts);
        let carried: BTreeSet<usize> = r.origins.iter().flatten().copied().collect();
        let refused: BTreeSet<usize> = r.refused.iter().map(|(i, _)| *i).collect();
        let adjusted: BTreeSet<usize> = r.adjusted.iter().map(|(i, _)| *i).collect();
        assert_eq!(r.origins.len(), r.ops.len());
        for i in 0..n {
            assert!(
                carried.contains(&i) || refused.contains(&i) || adjusted.contains(&i),
                "input {i} is not accounted for: {r:#?}"
            );
        }
        let want_refused: BTreeSet<usize> = [0, 4, 9, 10, 16, 17, 18, 19, 20].into();
        assert_eq!(refused, want_refused, "{r:#?}");
        for i in [1, 3, 5, 8, 12, 14, 15, 21] {
            assert!(adjusted.contains(&i), "input {i} adjusted: {r:#?}");
        }
        let reasons: Vec<String> = r.refused.iter().map(|(_, w)| w.clone()).collect();
        assert_eq!(r.dropped, reasons, "dropped is exactly the refusals");
        assert!(
            r.refused
                .iter()
                .any(|(i, w)| *i == 4 && w.contains("keeps its text and standing"))
        );
    }
}
