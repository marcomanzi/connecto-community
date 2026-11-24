//! The rule against self-confirmation (spec §4.5), applied by the host's
//! validator and never asked of a model.
//!
//! The functions take the subject's *lineage*: the memory itself and every
//! memory or annotation it was made from (absorbed annotations, merged or
//! derived memories). A session that was shown any of them has seen the
//! claim, so what it says next is not independent of it.

use std::collections::BTreeSet;

use super::ids::Subject;
use super::record::{Evidence, EvidenceKind, Standing};

/// Whether one piece of evidence may count as an independent confirmation of
/// a subject with this lineage.
///
/// - A user statement, a code observation and a project document always may:
///   they do not come from a session that could have read the memory.
/// - A reply claim, a reported outcome or a proposal may only if the session
///   had been shown nothing in the lineage before producing it.
#[must_use]
pub fn is_independent(evidence: &Evidence, lineage: &[Subject]) -> bool {
    !evidence.kind.from_agent() || !evidence.exposed_to.iter().any(|s| lineage.contains(s))
}

/// Who an independent piece of evidence really comes from. Evidence with the
/// same origin counts once, however many times it repeats.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Origin<'a> {
    /// The user, in one tool session (or terminal). Unknown sessions share one
    /// origin.
    User(Option<&'a str>),
    /// One agent session (or terminal). Unknown sessions share one origin.
    Agent(Option<&'a str>),
    /// The same observed file contents, or the same HEAD when no file is named.
    Code(Vec<(&'a str, &'a str)>, Option<&'a str>),
    Document(Vec<(&'a str, &'a str)>),
}

fn origin(evidence: &Evidence) -> Origin<'_> {
    let session = evidence
        .tool_session
        .as_deref()
        .or(evidence.terminal.as_deref());
    let mut files: Vec<_> = evidence
        .files
        .iter()
        .map(|a| (a.path.as_str(), a.blob.as_str()))
        .collect();
    files.sort_unstable();
    files.dedup();
    match evidence.kind {
        EvidenceKind::UserStatement => Origin::User(session),
        EvidenceKind::ReportedOutcome | EvidenceKind::AgentClaim | EvidenceKind::AgentProposal => {
            Origin::Agent(session)
        }
        EvidenceKind::CodeObservation => {
            let head = if files.is_empty() {
                evidence.head.as_deref()
            } else {
                None
            };
            Origin::Code(files, head)
        }
        EvidenceKind::ProjectDocument => Origin::Document(files),
    }
}

/// How many independent confirmations the evidence gives a subject with this
/// lineage: the evidence that [`is_independent`], counted once per origin
/// (one session repeating itself is one confirmation; two observations of the
/// same file contents are one).
#[must_use]
pub fn independent_confirmations(evidence: &[Evidence], lineage: &[Subject]) -> usize {
    evidence
        .iter()
        .filter(|e| is_independent(e, lineage))
        .map(origin)
        .collect::<BTreeSet<_>>()
        .len()
}

/// Whether the evidence lets a memory become `verificato`: at least one
/// independent confirmation.
#[must_use]
pub fn may_verify(evidence: &[Evidence], lineage: &[Subject]) -> bool {
    independent_confirmations(evidence, lineage) > 0
}

/// Whether the evidence supports claiming `standing`: the weakest standings
/// need nothing; the others need one independent piece of a kind in
/// [`Standing::required_evidence`]. So an agent's proposal, however often
/// repeated, never makes a decision accepted.
#[must_use]
pub fn supports_standing(standing: Standing, evidence: &[Evidence], lineage: &[Subject]) -> bool {
    match standing.required_evidence() {
        None => true,
        Some(kinds) => evidence
            .iter()
            .any(|e| kinds.contains(&e.kind) && is_independent(e, lineage)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Redacted;
    use crate::memory::ids::{AnnotationId, EvidenceId, MemoryId};
    use crate::memory::record::Anchor;

    const BLOB_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BLOB_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn x() -> Subject {
        Subject::Memory(MemoryId::from_entropy([1; 5]))
    }
    fn other() -> Subject {
        Subject::Memory(MemoryId::from_entropy([9; 5]))
    }
    fn absorbed() -> Subject {
        Subject::Annotation(AnnotationId::from_entropy([3; 5]))
    }

    fn ev(kind: EvidenceKind, session: Option<&str>, exposed: &[Subject]) -> Evidence {
        Evidence {
            id: EvidenceId::new(1),
            kind,
            project_id: "p".to_owned(),
            created_ms: 0,
            terminal: None,
            tool_session: session.map(str::to_owned),
            turn: None,
            branch: None,
            head: None,
            log: None,
            files: Vec::new(),
            excerpt: Redacted::assume(String::new()),
            exposed_to: exposed.to_vec(),
        }
    }

    fn code(files: &[(&str, &str)], head: Option<&str>) -> Evidence {
        let mut e = ev(EvidenceKind::CodeObservation, None, &[]);
        e.files = files
            .iter()
            .map(|(p, b)| Anchor::new(p, b).unwrap())
            .collect();
        e.head = head.map(str::to_owned);
        e
    }

    #[test]
    fn independence_table() {
        use EvidenceKind as K;
        let lineage = [x(), absorbed()];
        // (kind, exposed to, counts?)
        let table: &[(K, &[Subject], bool)] = &[
            (K::UserStatement, &[], true),
            (K::UserStatement, &[x()], true),
            (K::CodeObservation, &[x()], true),
            (K::ProjectDocument, &[x()], true),
            (K::AgentClaim, &[], true),
            (K::AgentClaim, &[other()], true),
            (K::AgentClaim, &[x()], false),
            (K::AgentClaim, &[other(), x()], false),
            (K::ReportedOutcome, &[x()], false),
            (K::ReportedOutcome, &[], true),
            (K::AgentProposal, &[x()], false),
            (K::AgentProposal, &[other()], true),
            // Shown the annotation the memory was made from: still its own claim.
            (K::AgentClaim, &[absorbed()], false),
            (K::AgentProposal, &[absorbed()], false),
        ];
        for (kind, exposed, counts) in table {
            assert_eq!(
                is_independent(&ev(*kind, Some("s"), exposed), &lineage),
                *counts,
                "{kind:?} exposed to {exposed:?}"
            );
        }
    }

    #[test]
    fn counting_table() {
        use EvidenceKind as K;
        let lineage = [x()];
        let cases: Vec<(&str, Vec<Evidence>, usize)> = vec![
            ("nothing", vec![], 0),
            (
                "one exposed agent",
                vec![ev(K::AgentClaim, Some("a"), &[x()])],
                0,
            ),
            (
                "an agent that read X repeats it three times",
                vec![
                    ev(K::AgentClaim, Some("a"), &[x()]),
                    ev(K::ReportedOutcome, Some("a"), &[x()]),
                    ev(K::AgentProposal, Some("a"), &[x()]),
                ],
                0,
            ),
            (
                "several agents, all exposed",
                vec![
                    ev(K::AgentClaim, Some("a"), &[x()]),
                    ev(K::AgentClaim, Some("b"), &[x()]),
                    ev(K::AgentClaim, Some("c"), &[x()]),
                ],
                0,
            ),
            (
                "one unexposed session repeating itself",
                vec![
                    ev(K::AgentClaim, Some("a"), &[]),
                    ev(K::AgentClaim, Some("a"), &[]),
                    ev(K::ReportedOutcome, Some("a"), &[]),
                ],
                1,
            ),
            (
                "two unexposed sessions",
                vec![
                    ev(K::AgentClaim, Some("a"), &[]),
                    ev(K::AgentClaim, Some("b"), &[]),
                ],
                2,
            ),
            (
                "agents with no known session count once",
                vec![ev(K::AgentClaim, None, &[]), ev(K::AgentClaim, None, &[])],
                1,
            ),
            (
                "the user twice in one session",
                vec![
                    ev(K::UserStatement, Some("a"), &[x()]),
                    ev(K::UserStatement, Some("a"), &[x()]),
                ],
                1,
            ),
            (
                "user and agent in one session are two parties",
                vec![
                    ev(K::UserStatement, Some("a"), &[]),
                    ev(K::AgentClaim, Some("a"), &[]),
                ],
                2,
            ),
            (
                "user plus an exposed agent",
                vec![
                    ev(K::UserStatement, Some("a"), &[x()]),
                    ev(K::AgentClaim, Some("b"), &[x()]),
                ],
                1,
            ),
            (
                "the same file contents observed twice",
                vec![
                    code(&[("a.rs", BLOB_A)], Some("h1")),
                    code(&[("a.rs", BLOB_A)], Some("h2")),
                ],
                1,
            ),
            (
                "different file contents",
                vec![
                    code(&[("a.rs", BLOB_A)], None),
                    code(&[("a.rs", BLOB_B)], None),
                ],
                2,
            ),
            (
                "no files: one per HEAD",
                vec![
                    code(&[], Some("h1")),
                    code(&[], Some("h1")),
                    code(&[], Some("h2")),
                ],
                2,
            ),
        ];
        for (name, evidence, expected) in cases {
            assert_eq!(
                independent_confirmations(&evidence, &lineage),
                expected,
                "{name}"
            );
            assert_eq!(may_verify(&evidence, &lineage), expected > 0, "{name}");
        }
    }

    #[test]
    fn standing_table() {
        use EvidenceKind as K;
        use Standing as S;
        let lineage = [x()];
        let user = ev(K::UserStatement, Some("u"), &[x()]);
        let doc = ev(K::ProjectDocument, None, &[]);
        let code_seen = code(&[("a.rs", BLOB_A)], None);
        let proposal_exposed = ev(K::AgentProposal, Some("a"), &[x()]);
        let proposal_fresh = ev(K::AgentProposal, Some("b"), &[]);
        let claim_fresh = ev(K::AgentClaim, Some("b"), &[]);
        let outcome_fresh = ev(K::ReportedOutcome, Some("b"), &[]);
        let outcome_exposed = ev(K::ReportedOutcome, Some("a"), &[x()]);
        // (standing, evidence, supported?)
        let table: Vec<(S, Vec<&Evidence>, bool)> = vec![
            (S::Proposed, vec![], true),
            (S::Planned, vec![], true),
            (S::Hypothesis, vec![], true),
            (S::Documented, vec![], true),
            (S::InProgress, vec![], true),
            // A proposal is not approval, from any session.
            (
                S::Accepted,
                vec![&proposal_exposed, &proposal_fresh, &claim_fresh],
                false,
            ),
            (S::Accepted, vec![&user], true),
            (S::Accepted, vec![&doc], true),
            (S::Discarded, vec![&claim_fresh], false),
            (S::Discarded, vec![&user], true),
            // A plan is not an implementation until code or the user shows it.
            (S::Implemented, vec![&claim_fresh, &outcome_fresh], false),
            (S::Implemented, vec![&code_seen], true),
            (S::Implemented, vec![&user], true),
            // A hypothesis needs any independent confirmation.
            (S::Established, vec![&proposal_exposed], false),
            (S::Established, vec![&claim_fresh], true),
            (S::Established, vec![&doc], true),
            // A documented procedure is not an executed one.
            (S::Exercised, vec![&doc], false),
            (S::Exercised, vec![&outcome_exposed], false),
            (S::Exercised, vec![&outcome_fresh], true),
            (S::Exercised, vec![&user], true),
        ];
        for (standing, evidence, expected) in table {
            let evidence: Vec<Evidence> = evidence.into_iter().cloned().collect();
            assert_eq!(
                supports_standing(standing, &evidence, &lineage),
                expected,
                "{standing:?}"
            );
        }
    }
}
