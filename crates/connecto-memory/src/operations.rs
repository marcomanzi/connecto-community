//! The structured operations a model may propose on the memories, and the
//! host's validator for them (spec §5.5 step 3, §5.7, §4.5, §4.6, §5.10).
//! Shared by the dream ([`crate::dream`]) and the initial build.
//!
//! The model never writes the store. It answers JSON
//! (`{"operations":[...]}`); [`parse`] reads its shape and [`validate`]
//! judges each operation against the store and against the [`Packet`] that
//! run was given. What passes becomes [`MemoryOp`]s, [`Absorb`]s and kept
//! annotations, ready for one [`crate::Publication`]; what does not is
//! listed in [`Verdict::rejected`] with its reason. A reply that cannot be
//! read at all is an `Err` from [`parse`]: the run fails with no effect.
//!
//! The operations are `create`, `update`, `merge`, `supersede`,
//! `set_status`, `move`, `archive`, `link`, `absorb` and `keep`. There is no
//! delete.
//!
//! # What is refused (each a row of §5.7)
//!
//! - unknown operations or fields, missing or mistyped fields, lengths over
//!   the limits ([`parse`] marks these [`ProposedOp::Malformed`]);
//! - ids that are not live memories or annotations of this project, or that
//!   were not in the material given to this run;
//! - missing sources, or sources not in this run's packet: what a session
//!   read on its own is never evidence;
//! - `set_status: verificato` without an independent confirmation
//!   ([`connecto_core::memory::may_verify`] over the memory's lineage), and
//!   any raise of a standing its evidence does not support
//!   ([`connecto_core::memory::supports_standing`]); the store checks again
//!   when it publishes;
//! - unknown rooms, the entrance, addresses already used;
//! - `absorb` or `keep` of an annotation past the cutoff, already absorbed,
//!   or named twice;
//! - text that redaction cannot clean or leaves empty.
//!
//! # What is read in a form that changes nothing meant
//!
//! Each is listed in [`Verdict::adjusted`]: one valid scope string for a
//! one-element list; a summary over the limit cut after its last whole
//! sentence that fits, the rest moved word for word before the details; an
//! anchor without a blob given the hash Connecto recorded for that file in
//! the run's evidence, or left out when the material holds none.
//!
//! # Truth rules (§4.6, §5.10), enforced here and not asked of the model
//!
//! - A proposal is not an accepted decision, a plan not an implementation, a
//!   hypothesis not an established fact: a `create` whose standing (or
//!   `verificato`) its sources do not support is kept with the kind's weakest
//!   standing (or as `candidato`) and listed in [`Verdict::adjusted`]; an
//!   `update` or `set_status` that would raise one without support is
//!   refused. A `merge` needs the same kind and standing on both sides, so a
//!   proposal is never merged into a decision; a weaker memory never
//!   supersedes a confirmed one.
//! - Repeated claims are not independent evidence (the lineage rule).
//! - `superato` comes only from `supersede` with the memory that replaces it;
//!   `conflitto` needs the diverging source, so both stay visible.
//! - A memory scoped on a branch that is gone (deleted, squashed, rebased) is
//!   archived, superseded, merged away or promoted to `project` only with a
//!   user statement, a code observation or a project document among the
//!   operation's sources: the branch being gone is never enough.
//! - When evidence is not enough a memory stays as it is or goes to
//!   `da_verificare` (always allowed).
//!
//! Texts written by the model are redacted ([`crate::ingest::redact`]) before
//! they leave this module; reasons never repeat the model's text.

use std::collections::{BTreeMap, BTreeSet};

use connecto_core::Redacted;
use connecto_core::memory::{
    Address, Anchor, AnnotationId, Cycle, Evidence, EvidenceId, EvidenceKind, MAX_DETAILS_BYTES,
    MAX_SUMMARY_CHARS, MemoryId, MemoryKind, MemoryStatus, RelationKind, Room, Scope, ScopeEntry,
    Standing, Subject, may_verify, supports_standing,
};
use serde_json::{Map, Value, json};

use crate::publish::{Absorb, MemoryOp, MemoryUpdate, NewMemory, Publication};
use crate::read::Read;

/// Longest reply read, in characters: a longer one fails the run.
pub const MAX_REPLY_CHARS: usize = 2_000_000;
/// Operations judged from one reply; the rest are rejected.
pub const MAX_OPERATIONS: usize = 500;
/// Entries in one list field (sources, scopes, anchors, merged memories).
pub const MAX_LIST: usize = 32;
/// Longest `keep` reason, in bytes.
pub const MAX_REASON_BYTES: usize = 1000;
/// Longest `ref` label of a `create`.
pub const MAX_REF: usize = 32;

/// What one run was given: the only ids its operations may name. Memories
/// created earlier in the same reply (by `ref`) may be named too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Packet {
    pub evidence: BTreeSet<EvidenceId>,
    pub memories: BTreeSet<MemoryId>,
    pub annotations: BTreeSet<AnnotationId>,
    /// The highest annotation `seq` this run may absorb or keep.
    pub cutoff: u64,
}

impl Packet {
    #[must_use]
    pub fn new(
        evidence: Vec<EvidenceId>,
        memories: Vec<MemoryId>,
        annotations: Vec<AnnotationId>,
        cutoff: u64,
    ) -> Self {
        Self {
            evidence: evidence.into_iter().collect(),
            memories: memories.into_iter().collect(),
            annotations: annotations.into_iter().collect(),
            cutoff,
        }
    }
}

/// Why a whole reply was not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("the reply is empty")]
    Empty,
    #[error("the reply is too large")]
    TooLarge,
    #[error("the reply is not readable JSON with an operation list")]
    Unreadable,
}

/// One proposed operation, as read from the reply: shape checked, meaning
/// not yet (ids, words and evidence are [`validate`]'s job).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposedOp {
    Create {
        /// A label other operations of the same reply may use as its id.
        reference: Option<String>,
        room: String,
        slug: Option<String>,
        kind: String,
        standing: Option<String>,
        status: Option<String>,
        summary: String,
        details: Option<String>,
        scope: Vec<String>,
        anchors: Vec<(String, String)>,
        sources: Vec<String>,
        /// `scope` was one scope string, read as a one-element list.
        scope_normalised: bool,
    },
    Update {
        id: String,
        summary: Option<String>,
        details: Option<String>,
        scope: Option<Vec<String>>,
        anchors: Option<Vec<(String, String)>>,
        standing: Option<String>,
        sources: Vec<String>,
        /// `scope` was one scope string, read as a one-element list.
        scope_normalised: bool,
    },
    Merge {
        into: String,
        from: Vec<String>,
        sources: Vec<String>,
    },
    Supersede {
        old: String,
        by: String,
        sources: Vec<String>,
    },
    SetStatus {
        id: String,
        status: String,
        sources: Vec<String>,
    },
    Move {
        id: String,
        to: String,
    },
    Archive {
        id: String,
        sources: Vec<String>,
    },
    Link {
        from: String,
        relation: String,
        to: String,
    },
    Absorb {
        annotation: String,
        into: Option<String>,
    },
    Keep {
        annotation: String,
        reason: String,
    },
    /// Not an operation this host can read; the reason says why.
    Malformed {
        reason: String,
    },
}

/// A proposal turned down, by its position in the reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub index: usize,
    pub reason: String,
}

/// The validator's verdict on one reply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// In the reply's order. `create` carries a fresh id the host made.
    pub ops: Vec<MemoryOp>,
    pub absorb: Vec<Absorb>,
    /// Annotations left open, with the model's (redacted) reason.
    pub kept: Vec<(AnnotationId, String)>,
    pub rejected: Vec<Rejected>,
    /// Kept with a weaker claim than proposed (standing or status), and why.
    pub adjusted: Vec<Rejected>,
    /// For each entry of `ops`, the index of the proposal it came from (an
    /// `archive`, `merge` or `supersede` with sources is preceded by an
    /// `update` that adds them, from the same proposal).
    pub origin: Vec<usize>,
    /// For each entry of `absorb`, the index of its proposal.
    pub absorb_origin: Vec<usize>,
}

impl Verdict {
    /// Nothing to publish.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.absorb.is_empty()
    }
}

// ---- parsing

/// The operation list of a reply: the JSON alone, inside a code fence, or
/// with prose around it; `{"operations": [...]}` or a bare list.
pub fn parse(reply: &str) -> Result<Vec<ProposedOp>, ParseError> {
    if reply.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    if reply.chars().count() > MAX_REPLY_CHARS {
        return Err(ParseError::TooLarge);
    }
    let items = [reply.trim(), strip_fence(reply), outer_json(reply)]
        .into_iter()
        .filter(|c| !c.is_empty())
        .find_map(|candidate| match serde_json::from_str::<Value>(candidate) {
            Ok(Value::Array(items)) => Some(Ok(items)),
            Ok(Value::Object(object)) => Some(match object.get("operations") {
                Some(Value::Array(items)) => Ok(items.clone()),
                _ => Err(ParseError::Unreadable),
            }),
            _ => None,
        })
        .unwrap_or(Err(ParseError::Unreadable))?;
    Ok(items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            if index >= MAX_OPERATIONS {
                ProposedOp::Malformed {
                    reason: "more operations than one run may propose".to_owned(),
                }
            } else {
                one(item).unwrap_or_else(|reason| ProposedOp::Malformed { reason })
            }
        })
        .collect())
}

fn strip_fence(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return "";
    };
    let rest = rest.split_once('\n').map_or("", |(_, body)| body);
    rest.trim_end().strip_suffix("```").unwrap_or("").trim()
}

fn outer_json(text: &str) -> &str {
    let start = text.find(['{', '[']);
    let end = text.rfind(['}', ']']);
    match (start, end) {
        (Some(s), Some(e)) if e > s => text.get(s..=e).unwrap_or(""),
        _ => "",
    }
}

/// The fields each operation may carry (`op` included).
/// A `scope` value, shared by the dream's and the development cycle's
/// parsers: a list of at most `max` strings, or one valid scope string read
/// as a one-element list (the only tolerance: it cannot change what is
/// meant). The flag says it was normalised ([`SCOPE_NORMALISED`]).
pub(crate) fn scope_value(
    value: Option<&Value>,
    max: usize,
) -> Result<(Option<Vec<String>>, bool), String> {
    match value {
        None | Some(Value::Null) => Ok((None, false)),
        Some(Value::String(s)) if Scope::parse(s).is_ok() => Ok((Some(vec![s.clone()]), true)),
        Some(Value::Array(items)) if items.len() <= max => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "scope: not a list of strings".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|l| (Some(l), false)),
        Some(Value::Array(_)) => Err("scope: too many entries".to_owned()),
        Some(_) => Err(format!("scope: not a list ({})", list_shape("scope"))),
    }
}

/// What a normalised scope is recorded as, among the adjustments.
pub(crate) const SCOPE_NORMALISED: &str = r#"scope: one scope string was given; read as a one-element list (scope is a list, e.g. ["project"])"#;

/// What a summary split is recorded as, among the adjustments.
pub(crate) const SUMMARY_SPLIT: &str = "summary: over 400 characters; the sentences past the \
     limit were moved, unchanged, to the start of details";

/// A summary over [`MAX_SUMMARY_CHARS`], cut after its last whole sentence
/// that fits: `(head, details)`, the rest of the summary put, word for word,
/// before `details`. Nothing is dropped or rewritten, so nothing meant is
/// lost. `None` when the summary fits, or has no sentence end to cut at
/// (then it is refused as too long).
fn split_summary(summary: &str, details: Option<&str>) -> Option<(String, String)> {
    let summary = summary.trim();
    if summary.chars().count() <= MAX_SUMMARY_CHARS {
        return None;
    }
    let chars: Vec<(usize, char)> = summary.char_indices().collect();
    let cut = chars
        .windows(2)
        .take(MAX_SUMMARY_CHARS)
        .filter(|w| matches!(w[0].1, '.' | '!' | '?') && w[1].1.is_whitespace())
        .map(|w| w[1].0)
        .next_back()?;
    let (head, rest) = summary.split_at(cut);
    let (head, rest) = (head.trim(), rest.trim());
    if head.is_empty() || rest.is_empty() {
        return None;
    }
    let details = match details.map(str::trim) {
        Some(d) if !d.is_empty() => format!("{rest}\n\n{d}"),
        _ => rest.to_owned(),
    };
    Some((head.to_owned(), details))
}

/// The expected shape of a list field, for a refusal's reason.
pub(crate) fn list_shape(name: &str) -> &'static str {
    match name {
        "scope" => r#"it must be a list of scopes, e.g. ["project"]"#,
        "sources" => r#"it must be a list of evidence ids, e.g. ["e-12"]"#,
        "anchors" => r#"it must be a list, e.g. [{"path":"src/a.rs"}]"#,
        "from" => r#"it must be a list of memory ids, e.g. ["m-abcd2345"]"#,
        _ => "it must be a list",
    }
}

fn fields_of(op: &str) -> Option<&'static [&'static str]> {
    Some(match op {
        "create" => &[
            "op", "ref", "room", "slug", "kind", "standing", "status", "summary", "details",
            "scope", "anchors", "sources",
        ],
        "update" => &[
            "op", "id", "summary", "details", "scope", "anchors", "standing", "sources",
        ],
        "merge" => &["op", "into", "from", "sources"],
        "supersede" => &["op", "old", "by", "sources"],
        "set_status" => &["op", "id", "status", "sources"],
        "move" => &["op", "id", "to"],
        "archive" => &["op", "id", "sources"],
        "link" => &["op", "from", "relation", "to"],
        "absorb" => &["op", "annotation", "into"],
        "keep" => &["op", "annotation", "reason"],
        _ => return None,
    })
}

struct Fields<'a>(&'a Map<String, Value>);

impl Fields<'_> {
    fn text(&self, name: &str) -> Result<Option<String>, String> {
        match self.0.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("{name}: not a string (it must be one string)")),
        }
    }

    /// `scope`, tolerating one valid scope string for a one-element list
    /// (the only normalisation: it cannot change what is meant). The flag
    /// says it was normalised.
    fn scope(&self) -> Result<(Option<Vec<String>>, bool), String> {
        scope_value(self.0.get("scope"), MAX_LIST)
    }

    fn required(&self, name: &str) -> Result<String, String> {
        self.text(name)?.ok_or_else(|| format!("{name}: missing"))
    }

    fn list(&self, name: &str) -> Result<Option<Vec<String>>, String> {
        match self.0.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) if items.len() <= MAX_LIST => items
                .iter()
                .map(|i| {
                    i.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("{name}: not a list of strings"))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(Value::Array(_)) => Err(format!("{name}: too many entries")),
            Some(_) => Err(format!("{name}: not a list ({})", list_shape(name))),
        }
    }

    /// `anchors`: each `{"path"}`, with an optional `"blob"`. A blob left
    /// out, `null` or empty is read as `""`: the validator fills it in from
    /// the material ([`State::anchors`]).
    fn anchors(&self) -> Result<Option<Vec<(String, String)>>, String> {
        match self.0.get("anchors") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) if items.len() <= MAX_LIST => items
                .iter()
                .map(|i| {
                    let pair = i
                        .as_object()
                        .filter(|o| o.keys().all(|k| k == "path" || k == "blob"))
                        .and_then(|o| {
                            let blob = match o.get("blob") {
                                None | Some(Value::Null) => String::new(),
                                Some(b) => b.as_str()?.to_owned(),
                            };
                            Some((o.get("path")?.as_str()?.to_owned(), blob))
                        });
                    pair.ok_or_else(|| {
                        "anchors: each must be {\"path\"} (\"blob\" optional)".to_owned()
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(Value::Array(_)) => Err("anchors: too many entries".to_owned()),
            Some(_) => Err(format!("anchors: not a list ({})", list_shape("anchors"))),
        }
    }
}

fn one(item: &Value) -> Result<ProposedOp, String> {
    let Value::Object(map) = item else {
        return Err("not an object".to_owned());
    };
    let f = Fields(map);
    let op = f.required("op")?;
    if matches!(op.as_str(), "delete" | "remove" | "drop" | "erase") {
        return Err("there is no delete operation".to_owned());
    }
    let allowed = fields_of(&op).ok_or("op: unknown operation")?;
    if map.keys().any(|k| !allowed.contains(&k.as_str())) {
        // The field name is the model's text: not repeated.
        return Err("an unknown field".to_owned());
    }
    let sources = || f.list("sources").map(Option::unwrap_or_default);
    Ok(match op.as_str() {
        "create" => {
            let (scope, scope_normalised) = f.scope()?;
            ProposedOp::Create {
                scope: scope.unwrap_or_default(),
                scope_normalised,
                reference: f.text("ref")?,
                room: f.required("room")?,
                slug: f.text("slug")?,
                kind: f.required("kind")?,
                standing: f.text("standing")?,
                status: f.text("status")?,
                summary: f.required("summary")?,
                details: f.text("details")?,
                anchors: f.anchors()?.unwrap_or_default(),
                sources: sources()?,
            }
        }
        "update" => {
            let (scope, scope_normalised) = f.scope()?;
            ProposedOp::Update {
                id: f.required("id")?,
                summary: f.text("summary")?,
                details: f.text("details")?,
                scope,
                anchors: f.anchors()?,
                standing: f.text("standing")?,
                sources: sources()?,
                scope_normalised,
            }
        }
        "merge" => ProposedOp::Merge {
            into: f.required("into")?,
            from: f.list("from")?.ok_or("from: missing")?,
            sources: sources()?,
        },
        "supersede" => ProposedOp::Supersede {
            old: f.required("old")?,
            by: f.required("by")?,
            sources: sources()?,
        },
        "set_status" => ProposedOp::SetStatus {
            id: f.required("id")?,
            status: f.required("status")?,
            sources: sources()?,
        },
        "move" => ProposedOp::Move {
            id: f.required("id")?,
            to: f.required("to")?,
        },
        "archive" => ProposedOp::Archive {
            id: f.required("id")?,
            sources: sources()?,
        },
        "link" => ProposedOp::Link {
            from: f.required("from")?,
            relation: f.required("relation")?,
            to: f.required("to")?,
        },
        "absorb" => ProposedOp::Absorb {
            annotation: f.required("annotation")?,
            into: f.text("into")?,
        },
        _ => ProposedOp::Keep {
            annotation: f.required("annotation")?,
            reason: f.required("reason")?,
        },
    })
}

// ---- validation

/// A memory as this reply has left it so far.
#[derive(Debug, Clone)]
struct Sim {
    kind: MemoryKind,
    standing: Standing,
    status: MemoryStatus,
    address: Address,
    scopes: Vec<ScopeEntry>,
    live: bool,
    /// Evidence it lists (stored and added by this reply).
    sources: BTreeSet<EvidenceId>,
    /// Memories merged into it by this reply.
    merged: Vec<MemoryId>,
    /// Memories it derives from (`deriva_da`) by this reply.
    derived: Vec<MemoryId>,
    /// Its claims and sources as the store had them; `None` when created.
    before: Option<(MemoryStatus, Standing, BTreeSet<EvidenceId>)>,
}

struct State<'r, 'c> {
    read: &'r Read<'c>,
    packet: &'r Packet,
    secrets: &'r [String],
    memories: BTreeMap<MemoryId, Sim>,
    refs: BTreeMap<String, MemoryId>,
    /// Annotations the whole reply means to absorb, into which memory (by
    /// the text the reply used): they join that memory's lineage from the
    /// start, so a claim is judged as the store will judge it.
    absorbs_planned: BTreeMap<String, Vec<String>>,
    absorbed: BTreeSet<AnnotationId>,
    /// Absorptions accepted so far, by the memory they went into.
    absorbed_into: BTreeMap<MemoryId, Vec<AnnotationId>>,
    kept: BTreeSet<AnnotationId>,
    evidence: BTreeMap<EvidenceId, Option<Evidence>>,
    taken_ids: BTreeSet<MemoryId>,
    /// Memories created by this validation (earlier sessions included).
    created: BTreeSet<MemoryId>,
    /// Replaying operations already validated (earlier sessions of the same
    /// run, or the final ordered recheck): packet membership was checked
    /// when they were first judged; every rule on the memories is checked
    /// again on the state they now meet.
    replay: bool,
}

type Step<T> = Result<T, String>;

/// Judge every proposed operation of one reply, in order: each later
/// operation sees the state the accepted earlier ones leave.
#[must_use]
pub fn validate(
    read: &Read,
    packet: &Packet,
    proposed: Vec<ProposedOp>,
    known_secrets: &[String],
) -> Verdict {
    validate_after(read, packet, &[], proposed, known_secrets)
}

/// As [`validate`], on the state `prior` leaves: what earlier sessions of
/// the same run staged (in order) is applied first, so a later session is
/// judged against the memories as they will be, not as they were.
#[must_use]
pub fn validate_after(
    read: &Read,
    packet: &Packet,
    prior: &[Staged],
    proposed: Vec<ProposedOp>,
    known_secrets: &[String],
) -> Verdict {
    let prior: Vec<ProposedOp> = prior.iter().filter_map(to_proposed).collect();
    let mut verdict = Verdict::default();
    let mut all = prior.clone();
    all.extend(proposed.iter().cloned());
    let mut state = match State::new(read, packet, known_secrets, &all) {
        Ok(mut s) => {
            s.replay = true;
            for op in prior {
                let _ = s.judge(op);
            }
            s.replay = false;
            s
        }
        Err(reason) => {
            verdict.rejected = (0..proposed.len())
                .map(|index| Rejected {
                    index,
                    reason: reason.clone(),
                })
                .collect();
            return verdict;
        }
    };
    for (index, op) in proposed.into_iter().enumerate() {
        match state.judge(op) {
            Ok(Judged::Ops(ops, adjusted)) => {
                verdict.origin.extend(std::iter::repeat_n(index, ops.len()));
                verdict.ops.extend(ops);
                verdict.adjusted.extend(
                    adjusted
                        .into_iter()
                        .map(|reason| Rejected { index, reason }),
                );
            }
            Ok(Judged::Absorb(a)) => {
                verdict.absorb.push(a);
                verdict.absorb_origin.push(index);
            }
            Ok(Judged::Keep(id, reason)) => verdict.kept.push((id, reason)),
            Err(reason) => verdict.rejected.push(Rejected { index, reason }),
        }
    }
    verdict
}

/// `judged`, with the scope normalisation recorded among its adjustments.
fn normalised(judged: Step<Judged>, scope_normalised: bool) -> Step<Judged> {
    match judged {
        Ok(Judged::Ops(ops, mut adjusted)) if scope_normalised => {
            adjusted.push(SCOPE_NORMALISED.to_owned());
            Ok(Judged::Ops(ops, adjusted))
        }
        other => other,
    }
}

enum Judged {
    Ops(Vec<MemoryOp>, Vec<String>),
    Absorb(Absorb),
    Keep(AnnotationId, String),
}

fn fresh_memory_id(taken: &BTreeSet<MemoryId>) -> MemoryId {
    loop {
        let hex = crate::prompt::nonce();
        let mut bytes = [0u8; 5];
        for (slot, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
            *slot = std::str::from_utf8(pair)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .unwrap_or_default();
        }
        let id = MemoryId::from_entropy(bytes);
        if !taken.contains(&id) {
            return id;
        }
    }
}

fn valid_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= MAX_REF
        && !r.starts_with(MemoryId::PREFIX)
        && r.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Evidence kinds that can show more than a branch being gone (§4.6).
const STRONG: &[EvidenceKind] = &[
    EvidenceKind::UserStatement,
    EvidenceKind::CodeObservation,
    EvidenceKind::ProjectDocument,
];

impl<'r, 'c> State<'r, 'c> {
    fn new(
        read: &'r Read<'c>,
        packet: &'r Packet,
        secrets: &'r [String],
        proposed: &[ProposedOp],
    ) -> Step<Self> {
        let mut memories = BTreeMap::new();
        let rows = read
            .memories()
            .map_err(|_| "the memories could not be read".to_owned())?;
        for m in rows {
            memories.insert(
                m.id.clone(),
                Sim {
                    kind: m.kind,
                    standing: m.standing,
                    status: m.status,
                    address: m.address.clone(),
                    scopes: m.scopes.clone(),
                    live: true,
                    sources: m.sources.iter().map(|s| s.evidence).collect(),
                    merged: Vec::new(),
                    derived: Vec::new(),
                    before: Some((
                        m.status,
                        m.standing,
                        m.sources.iter().map(|s| s.evidence).collect(),
                    )),
                },
            );
        }
        let mut absorbs_planned: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for op in proposed {
            if let ProposedOp::Absorb {
                annotation,
                into: Some(into),
            } = op
            {
                absorbs_planned
                    .entry(into.clone())
                    .or_default()
                    .push(annotation.clone());
            }
        }
        let taken_ids = memories.keys().cloned().collect();
        Ok(Self {
            created: BTreeSet::new(),
            replay: false,
            read,
            packet,
            secrets,
            memories,
            refs: BTreeMap::new(),
            absorbs_planned,
            absorbed: BTreeSet::new(),
            absorbed_into: BTreeMap::new(),
            kept: BTreeSet::new(),
            evidence: BTreeMap::new(),
            taken_ids,
        })
    }

    fn judge(&mut self, op: ProposedOp) -> Step<Judged> {
        match op {
            ProposedOp::Malformed { reason } => Err(reason),
            ProposedOp::Create {
                reference,
                room,
                slug,
                kind,
                standing,
                status,
                summary,
                details,
                scope,
                anchors,
                sources,
                scope_normalised,
            } => normalised(
                self.create(Create {
                    reference,
                    room,
                    slug,
                    kind,
                    standing,
                    status,
                    summary,
                    details,
                    scope,
                    anchors,
                    sources,
                }),
                scope_normalised,
            ),
            ProposedOp::Update {
                id,
                summary,
                details,
                scope,
                anchors,
                standing,
                sources,
                scope_normalised,
            } => normalised(
                self.update(
                    &id,
                    summary,
                    details,
                    scope,
                    anchors,
                    standing.as_deref(),
                    &sources,
                ),
                scope_normalised,
            ),
            ProposedOp::Merge {
                into,
                from,
                sources,
            } => self.merge(&into, &from, &sources),
            ProposedOp::Supersede { old, by, sources } => self.supersede(&old, &by, &sources),
            ProposedOp::SetStatus {
                id,
                status,
                sources,
            } => self.set_status(&id, &status, &sources),
            ProposedOp::Move { id, to } => self.move_to(&id, &to),
            ProposedOp::Archive { id, sources } => self.archive(&id, &sources),
            ProposedOp::Link { from, relation, to } => self.link(&from, &relation, &to),
            ProposedOp::Absorb { annotation, into } => self.absorb(&annotation, into.as_deref()),
            ProposedOp::Keep { annotation, reason } => self.keep(&annotation, &reason),
        }
    }

    // ---- lookups

    /// A live memory this run may name: an id of the packet or a `ref` made
    /// earlier in the reply.
    fn memory(&self, text: &str, field: &str) -> Step<MemoryId> {
        if let Some(id) = self.refs.get(text) {
            return self.live(id, field).map(|()| id.clone());
        }
        let id = MemoryId::parse(text).map_err(|_| {
            format!(
                "{field}: not a memory id, nor the ref of a create accepted earlier in this reply \
                 (a create that was refused makes no ref)"
            )
        })?;
        match self.memories.get(&id) {
            Some(sim) if sim.live => {}
            Some(_) => return Err(format!("{field}: {id} is no longer live in this reply")),
            None => {
                return match self.read.memory_row(&id) {
                    Ok(Some(_)) => Err(format!(
                        "{field}: {id} is no longer live (merged or withdrawn)"
                    )),
                    Ok(None) => Err(format!("{field}: no memory {id} in this project")),
                    Err(_) => Err(format!("{field}: could not be checked")),
                };
            }
        }
        if !(self.replay || self.created.contains(&id) || self.packet.memories.contains(&id)) {
            return Err(format!(
                "{field}: {id} was not in the material given to this run"
            ));
        }
        Ok(id)
    }

    fn live(&self, id: &MemoryId, field: &str) -> Step<()> {
        match self.memories.get(id) {
            Some(sim) if sim.live => Ok(()),
            _ => Err(format!("{field}: {id} is no longer live in this reply")),
        }
    }

    fn sim(&self, id: &MemoryId) -> Step<&Sim> {
        self.memories
            .get(id)
            .ok_or_else(|| format!("{id}: not found"))
    }

    fn evidence(&mut self, id: EvidenceId) -> Option<Evidence> {
        if let Some(e) = self.evidence.get(&id) {
            return e.clone();
        }
        let e = self.read.evidence(id).ok().flatten();
        self.evidence.insert(id, e.clone());
        e
    }

    /// The sources named, each in this run's packet and in this project.
    fn sources(&mut self, texts: &[String], required: bool) -> Step<Vec<EvidenceId>> {
        let mut out = Vec::new();
        for text in texts {
            let id = EvidenceId::parse(text).map_err(|_| "sources: not an evidence id")?;
            if !self.replay && !self.packet.evidence.contains(&id) {
                return Err(format!(
                    "sources: {id} was not in the material given to this run"
                ));
            }
            if self.evidence(id).is_none() {
                return Err(format!("sources: no evidence {id} in this project"));
            }
            if !out.contains(&id) {
                out.push(id);
            }
        }
        if required && out.is_empty() {
            return Err("sources: at least one is required".to_owned());
        }
        Ok(out)
    }

    fn has_strong(&mut self, sources: &[EvidenceId]) -> bool {
        sources
            .iter()
            .any(|id| self.evidence(*id).is_some_and(|e| STRONG.contains(&e.kind)))
    }

    /// The lineage the store will judge `id` by once this reply is
    /// published: its own (stored), the annotations the reply absorbs into
    /// it, and those of the memories merged into it.
    fn lineage(&self, id: &MemoryId) -> Vec<Subject> {
        let mut out: BTreeSet<Subject> = BTreeSet::new();
        let mut todo = vec![id.clone()];
        let mut seen = BTreeSet::new();
        while let Some(next) = todo.pop() {
            if !seen.insert(next.clone()) {
                continue;
            }
            out.insert(Subject::Memory(next.clone()));
            if let Ok(stored) = self.read.lineage(&next) {
                // Memories in the stored lineage may gain notes or merges
                // in this reply too: walk them as well.
                for subject in &stored {
                    if let Subject::Memory(m) = subject
                        && m != &next
                    {
                        todo.push(m.clone());
                    }
                }
                out.extend(stored);
            }
            let names: Vec<&String> = self
                .refs
                .iter()
                .filter(|(_, m)| **m == next)
                .map(|(r, _)| r)
                .collect();
            for (into, annotations) in &self.absorbs_planned {
                if into == next.as_str() || names.contains(&into) {
                    out.extend(
                        annotations
                            .iter()
                            .filter_map(|a| AnnotationId::parse(a).ok())
                            .map(Subject::Annotation),
                    );
                }
            }
            if let Some(sim) = self.memories.get(&next) {
                todo.extend(sim.merged.iter().cloned());
                todo.extend(sim.derived.iter().cloned());
            }
        }
        out.into_iter().collect()
    }

    /// The lineage `id` has once what was accepted so far is published:
    /// stored, plus accepted absorptions, merges and derivations (planned
    /// ones that were refused do not count here).
    fn final_lineage(&self, id: &MemoryId) -> Vec<Subject> {
        let mut out: BTreeSet<Subject> = BTreeSet::new();
        let mut todo = vec![id.clone()];
        let mut seen = BTreeSet::new();
        while let Some(next) = todo.pop() {
            if !seen.insert(next.clone()) {
                continue;
            }
            out.insert(Subject::Memory(next.clone()));
            if let Ok(stored) = self.read.lineage(&next) {
                // Memories in the stored lineage may gain notes or merges
                // in this reply too: walk them as well.
                for subject in &stored {
                    if let Subject::Memory(m) = subject
                        && m != &next
                    {
                        todo.push(m.clone());
                    }
                }
                out.extend(stored);
            }
            if let Some(notes) = self.absorbed_into.get(&next) {
                out.extend(notes.iter().cloned().map(Subject::Annotation));
            }
            if let Some(sim) = self.memories.get(&next) {
                todo.extend(sim.merged.iter().cloned());
                todo.extend(sim.derived.iter().cloned());
            }
        }
        out.into_iter().collect()
    }

    /// The post-condition of a publication, over the state every accepted
    /// operation leaves: each live memory it creates, changes, or whose
    /// lineage it changes keeps `verificato` and a standing above its kind's
    /// weakest only under the validator's own rule (see [`Self::counting`]):
    /// - `verificato` needs an independent piece of evidence that counts and
    ///   is not a source of a note in its lineage, taken from the sources
    ///   added now when it was not `verificato` before, from all its sources
    ///   when it was;
    /// - a standing needs counting evidence of a kind that supports it, from
    ///   the sources added now and its own non-agent ones when it was raised
    ///   now, from all its sources otherwise.
    ///
    /// What fails is lowered (to `da_verificare`, to the weakest standing):
    /// returned as operations to append, with the reasons.
    fn postcondition(&mut self) -> (Vec<MemoryOp>, Vec<String>) {
        let ids: Vec<MemoryId> = self
            .memories
            .iter()
            .filter(|(_, m)| {
                m.live
                    && (m.status == MemoryStatus::Verified
                        || m.standing.required_evidence().is_some())
            })
            .map(|(id, _)| id.clone())
            .collect();
        let mut ops = Vec::new();
        let mut notes = Vec::new();
        for id in ids {
            let Some(sim) = self.memories.get(&id).cloned() else {
                continue;
            };
            let lineage = self.final_lineage(&id);
            let stored: BTreeSet<Subject> = self
                .read
                .lineage(&id)
                .map(|l| l.into_iter().collect())
                .unwrap_or_default();
            let lineage_set: BTreeSet<Subject> = lineage.iter().cloned().collect();
            let touched = match &sim.before {
                None => true,
                Some((status, standing, sources)) => {
                    *status != sim.status
                        || *standing != sim.standing
                        || *sources != sim.sources
                        || lineage_set != stored
                }
            };
            if !touched {
                continue;
            }
            let (before_sources, was_verified, same_standing) = match &sim.before {
                None => (BTreeSet::new(), false, false),
                Some((status, standing, sources)) => (
                    sources.clone(),
                    *status == MemoryStatus::Verified,
                    *standing == sim.standing,
                ),
            };
            let added: BTreeSet<EvidenceId> =
                sim.sources.difference(&before_sources).copied().collect();
            let mut note_sources: BTreeSet<EvidenceId> = BTreeSet::new();
            for subject in &lineage {
                if let Subject::Annotation(a) = subject
                    && let Ok(Some(note)) = self.read.annotation(a)
                {
                    note_sources.extend(note.sources.iter().map(|s| s.evidence));
                }
            }
            if sim.status == MemoryStatus::Verified {
                let basis = if was_verified { &sim.sources } else { &added };
                let fresh: BTreeSet<EvidenceId> =
                    basis.difference(&note_sources).copied().collect();
                let evidence = self.counting(&fresh);
                if !may_verify(&evidence, &lineage) {
                    ops.push(MemoryOp::SetStatus {
                        id: id.clone(),
                        status: MemoryStatus::ToVerify,
                        sources: Vec::new(),
                    });
                    notes.push(format!(
                        "{id}: verificato without independent support at the final check: \
                         lowered to da_verificare"
                    ));
                    if let Some(m) = self.memories.get_mut(&id) {
                        m.status = MemoryStatus::ToVerify;
                    }
                }
            }
            if sim.standing.required_evidence().is_some() {
                let basis: BTreeSet<EvidenceId> = if same_standing {
                    sim.sources.clone()
                } else {
                    let mut b = added.clone();
                    for e in &before_sources {
                        if self.evidence(*e).is_some_and(|x| !x.kind.from_agent()) {
                            b.insert(*e);
                        }
                    }
                    b
                };
                let evidence = self.counting(&basis);
                if !supports_standing(sim.standing, &evidence, &lineage) {
                    let weakest = Standing::default_for(sim.kind);
                    let mut u = MemoryUpdate::new(id.clone());
                    u.standing = Some(weakest);
                    ops.push(MemoryOp::Update(u));
                    notes.push(format!(
                        "{id}: standing {} without support at the final check: lowered to {}",
                        sim.standing.word(),
                        weakest.word()
                    ));
                    if let Some(m) = self.memories.get_mut(&id) {
                        m.standing = weakest;
                    }
                }
            }
        }
        (ops, notes)
    }

    /// Whether the cited sources confirm `id`: an independent piece among
    /// them that it does not already rest on (its own sources when `own`,
    /// and those of the annotations it absorbs). Repeating a memory's own
    /// source, or the source of the note it was made from, confirms nothing.
    fn confirms(&mut self, id: &MemoryId, cited: &[EvidenceId], own: bool) -> Step<bool> {
        let lineage = self.lineage(id);
        let mut known: BTreeSet<EvidenceId> = if own {
            self.sim(id)?.sources.clone()
        } else {
            BTreeSet::new()
        };
        for subject in &lineage {
            if let Subject::Annotation(a) = subject
                && let Ok(Some(note)) = self.read.annotation(a)
            {
                known.extend(note.sources.iter().map(|s| s.evidence));
            }
        }
        let fresh: BTreeSet<EvidenceId> = cited
            .iter()
            .copied()
            .filter(|e| !known.contains(e))
            .collect();
        let evidence = self.counting(&fresh);
        Ok(may_verify(&evidence, &lineage))
    }

    /// The evidence that may count toward `verificato` or a stronger
    /// standing: user statements, code observations, project documents, and
    /// agent evidence from a session that had been shown nothing from memory.
    /// A session that was shown memory content cannot confirm what it
    /// repeats, whatever id the claim carries now (a copy of X under a new id
    /// is still X), so exposed agent evidence never counts here.
    fn counting(&mut self, ids: &BTreeSet<EvidenceId>) -> Vec<Evidence> {
        self.evidence_of(ids)
            .into_iter()
            .filter(|e| !e.kind.from_agent() || e.exposed_to.is_empty())
            .collect()
    }

    /// Whether `ids` support `standing` for `id`, counting as [`counting`].
    fn supports(&mut self, id: &MemoryId, ids: &BTreeSet<EvidenceId>, standing: Standing) -> bool {
        let evidence = self.counting(ids);
        supports_standing(standing, &evidence, &self.lineage(id))
    }

    /// A gone-branch change needs strong evidence among the operation's
    /// sources; on replay the sources travelled in the update before it.
    fn strong(&mut self, id: &MemoryId, sources: &[EvidenceId]) -> bool {
        if self.replay {
            let all: Vec<EvidenceId> = self
                .memories
                .get(id)
                .map(|m| m.sources.iter().copied().collect())
                .unwrap_or_default();
            self.has_strong(&all)
        } else {
            self.has_strong(sources)
        }
    }

    /// Whether `id` may be raised to `standing` on the cited sources and the
    /// sources it already has that no agent produced: the agent's claim a
    /// memory was made from does not confirm that memory.
    fn raises(&mut self, id: &MemoryId, cited: &[EvidenceId], standing: Standing) -> Step<bool> {
        let own: Vec<EvidenceId> = self.sim(id)?.sources.iter().copied().collect();
        let mut ids: BTreeSet<EvidenceId> = cited.iter().copied().collect();
        for e in own {
            if self.evidence(e).is_some_and(|x| !x.kind.from_agent()) {
                ids.insert(e);
            }
        }
        Ok(self.supports(id, &ids, standing))
    }

    fn evidence_of(&mut self, ids: &BTreeSet<EvidenceId>) -> Vec<Evidence> {
        ids.iter().filter_map(|id| self.evidence(*id)).collect()
    }

    /// Whether `id`, with `extra` sources added, supports `standing` and
    /// (when asked) `verificato`.
    fn supported(
        &mut self,
        id: &MemoryId,
        extra: &[EvidenceId],
        standing: Standing,
        verified: bool,
    ) -> Step<(bool, bool)> {
        let mut ids = self.sim(id)?.sources.clone();
        ids.extend(extra.iter().copied());
        let evidence = self.evidence_of(&ids);
        let lineage = self.lineage(id);
        Ok((
            supports_standing(standing, &evidence, &lineage),
            !verified || may_verify(&evidence, &lineage),
        ))
    }

    fn clean(&self, text: &str, field: &str) -> Step<String> {
        let clean = crate::ingest::redact(text, self.secrets)
            .ok_or_else(|| format!("{field}: redaction could not run"))?;
        if clean.replace("[REDACTED]", "").trim().is_empty() {
            return Err(format!("{field}: nothing left after redaction"));
        }
        Ok(clean)
    }

    fn summary(&self, text: &str) -> Step<String> {
        if text.chars().count() > MAX_SUMMARY_CHARS {
            return Err(format!(
                "summary: too long (at most {MAX_SUMMARY_CHARS} characters; the rest belongs \
                 in details)"
            ));
        }
        let clean = self.clean(text, "summary")?;
        if clean.chars().count() > MAX_SUMMARY_CHARS {
            return Err("summary: too long after redaction".to_owned());
        }
        Ok(clean)
    }

    /// A summary and its details, redacted and within their limits. A
    /// summary over the limit (when `may_split`) is cut after redaction, so
    /// no cut can separate the parts of a secret and leave them unredacted:
    /// each original field is redacted whole, then the redacted summary is
    /// split and the lengths checked again. `details` `None` stays `None`
    /// unless a split moved text into it. The flag says a split was made.
    fn summary_and_details(
        &self,
        summary: &str,
        details: Option<&str>,
        may_split: bool,
    ) -> Step<(String, Option<String>, bool)> {
        if !may_split || summary.trim().chars().count() <= MAX_SUMMARY_CHARS {
            let summary = self.summary(summary)?;
            let details = details.map(|d| self.details(d)).transpose()?;
            return Ok((summary, details, false));
        }
        let clean = self.clean(summary, "summary")?;
        let clean_details = details.map(|d| self.details(d)).transpose()?;
        let Some((head, rest)) = split_summary(&clean, clean_details.as_deref()) else {
            // No sentence end to cut at: refused with the limit named.
            let summary = self.summary(&clean)?;
            return Ok((summary, clean_details, false));
        };
        if head.chars().count() > MAX_SUMMARY_CHARS {
            return Err("summary: too long after redaction".to_owned());
        }
        if rest.len() > MAX_DETAILS_BYTES {
            return Err("details: too long once the summary's tail was moved into them".to_owned());
        }
        Ok((head, Some(rest), true))
    }

    fn details(&self, text: &str) -> Step<String> {
        if text.len() > MAX_DETAILS_BYTES {
            return Err("details: too long".to_owned());
        }
        if text.trim().is_empty() {
            return Ok(String::new());
        }
        let clean = self.clean(text, "details")?;
        if clean.len() > MAX_DETAILS_BYTES {
            return Err("details: too long after redaction".to_owned());
        }
        Ok(clean)
    }

    /// A structural value the model chose (slug, scope, path, address):
    /// stored as it is, so it must hold no known secret and be left alone by
    /// redaction; otherwise the operation is refused, never stored redacted
    /// into a value that would no longer mean the same.
    fn plain(&self, text: &str, field: &str) -> Step<()> {
        let known = self
            .secrets
            .iter()
            .any(|s| s.len() >= 4 && text.contains(s.as_str()));
        if known || crate::ingest::redact(text, self.secrets).as_deref() != Some(text) {
            return Err(format!(
                "{field}: withheld, it holds what looks like a secret"
            ));
        }
        Ok(())
    }

    fn scopes(&self, texts: &[String]) -> Step<Vec<Scope>> {
        let mut out = Vec::new();
        for t in texts {
            self.plain(t, "scope")?;
            let s = Scope::parse(t).map_err(|_| "scope: not a scope")?;
            if !out.contains(&s) {
                out.push(s);
            }
        }
        Ok(out)
    }

    /// The anchors named, with what was adjusted. An anchor given without a
    /// blob takes the one Connecto recorded for that file in this run's
    /// material (the cited sources first): the version the memory was made
    /// from, observed by Connecto, never the model's word. A file the
    /// material holds no hash for is left out of the anchors rather than the
    /// memory refused: an anchor only marks the memory for review when the
    /// file changes, it is not part of the claim.
    fn anchors(
        &mut self,
        pairs: &[(String, String)],
        cited: &[EvidenceId],
    ) -> Step<(Vec<Anchor>, Vec<String>)> {
        let mut out: Vec<Anchor> = Vec::new();
        let mut adjusted = Vec::new();
        for (path, blob) in pairs {
            self.plain(path, "anchors")?;
            if !connecto_core::memory::is_project_relative(path) {
                return Err(format!(
                    "anchors: {}",
                    connecto_core::memory::MemoryError::BadPath
                ));
            }
            if out.iter().any(|o| &o.path == path) {
                return Err("anchors: one path twice".to_owned());
            }
            let blob = if blob.trim().is_empty() {
                match self.recorded_blob(path, cited) {
                    Some(b) => {
                        adjusted.push(format!(
                            "anchors: {path}: no blob given; the hash Connecto recorded \
                             when it read the file was used"
                        ));
                        b
                    }
                    None => {
                        adjusted.push(format!(
                            "anchors: {path}: no blob given and none in the material; \
                             the anchor was left out"
                        ));
                        continue;
                    }
                }
            } else {
                self.plain(blob, "anchors")?;
                blob.clone()
            };
            out.push(Anchor::new(path, &blob).map_err(|e| format!("anchors: {e}"))?);
        }
        Ok((out, adjusted))
    }

    /// The blob Connecto recorded for `path` in this run's evidence: the
    /// cited sources first, then the rest of the packet.
    fn recorded_blob(&mut self, path: &str, cited: &[EvidenceId]) -> Option<String> {
        let rest: Vec<EvidenceId> = self
            .packet
            .evidence
            .iter()
            .filter(|e| !cited.contains(e))
            .copied()
            .collect();
        cited.iter().chain(rest.iter()).find_map(|id| {
            self.evidence(*id)?
                .files
                .into_iter()
                .find(|a| a.path == path)
                .map(|a| a.blob)
        })
    }

    fn address_held(&self, address: &Address, except: Option<&MemoryId>) -> bool {
        self.memories
            .iter()
            .any(|(id, m)| m.live && &m.address == address && Some(id) != except)
    }

    /// A gone branch or worktree scope: changing it needs strong evidence.
    fn gone_scope(&self, id: &MemoryId) -> bool {
        self.memories
            .get(id)
            .is_some_and(|m| m.scopes.iter().any(|s| s.gone))
    }

    /// An update adding `sources` before an op that cannot carry them.
    fn with_sources(id: &MemoryId, sources: Vec<EvidenceId>, op: MemoryOp) -> Vec<MemoryOp> {
        let mut ops = Vec::new();
        if !sources.is_empty() {
            let mut u = MemoryUpdate::new(id.clone());
            u.add_sources = sources;
            ops.push(MemoryOp::Update(u));
        }
        ops.push(op);
        ops
    }

    fn add_sources(&mut self, id: &MemoryId, sources: &[EvidenceId]) {
        if let Some(m) = self.memories.get_mut(id) {
            m.sources.extend(sources.iter().copied());
        }
    }

    // ---- operations

    fn create(&mut self, c: Create) -> Step<Judged> {
        let room = Room::for_memories(&c.room).map_err(|_| "room: unknown or the entrance")?;
        if room == Room::Archive {
            return Err("room: a memory is not created in the archive".to_owned());
        }
        let kind = MemoryKind::from_word(&c.kind).ok_or("kind: unknown")?;
        let mut adjusted = Vec::new();
        let (summary, details, split) =
            self.summary_and_details(&c.summary, Some(c.details.as_deref().unwrap_or("")), true)?;
        if split {
            adjusted.push(SUMMARY_SPLIT.to_owned());
        }
        let details = details.unwrap_or_default();
        if let Some(s) = &c.slug {
            self.plain(s, "slug")?;
        }
        let slug = match &c.slug {
            Some(s) if Address::is_valid_slug(s) => s.clone(),
            Some(_) => return Err("slug: not a valid slug".to_owned()),
            None => Address::slugify(&summary).ok_or("slug: none could be made")?,
        };
        self.plain(&slug, "slug")?;
        let address = Address::new(room, &slug).map_err(|e| format!("address: {e}"))?;
        if self.address_held(&address, None) {
            return Err(format!("address: {address} is already used"));
        }
        let replayed_id = c
            .reference
            .as_deref()
            .filter(|_| self.replay)
            .and_then(|r| MemoryId::parse(r).ok());
        let reference = match c.reference {
            Some(_) if replayed_id.is_some() => None,
            Some(r) if !valid_ref(&r) => return Err("ref: not a valid label".to_owned()),
            Some(r) if self.refs.contains_key(&r) => {
                return Err("ref: already used in this reply".to_owned());
            }
            r => r,
        };
        let standing = match c.standing.as_deref() {
            None => Standing::default_for(kind),
            Some(w) => {
                let s = Standing::from_word(w).ok_or("standing: unknown")?;
                if !s.applies_to(kind) {
                    return Err("standing: does not apply to this kind".to_owned());
                }
                s
            }
        };
        let status = match c.status.as_deref() {
            None => MemoryStatus::Candidate,
            Some(w) => match MemoryStatus::from_word(w).ok_or("status: unknown")? {
                MemoryStatus::Superseded => {
                    return Err(
                        "status: superato comes only from supersede with its replacement"
                            .to_owned(),
                    );
                }
                s => s,
            },
        };
        let mut scopes = self.scopes(&c.scope)?;
        if scopes.is_empty() {
            scopes.push(Scope::Project);
        }
        let sources = self.sources(&c.sources, true)?;
        let (anchors, filled) = self.anchors(&c.anchors, &sources)?;
        adjusted.extend(filled);
        if status == MemoryStatus::Conflict && sources.len() < 2 {
            return Err("status: conflitto needs both diverging sources".to_owned());
        }
        let id = match replayed_id {
            Some(id) if !self.taken_ids.contains(&id) => id,
            Some(_) => return Err("create: the id is already used".to_owned()),
            None => fresh_memory_id(&self.taken_ids),
        };
        self.taken_ids.insert(id.clone());
        self.created.insert(id.clone());
        self.memories.insert(
            id.clone(),
            Sim {
                kind,
                standing,
                status,
                address: address.clone(),
                scopes: scopes.iter().cloned().map(ScopeEntry::new).collect(),
                live: true,
                sources: sources.iter().copied().collect(),
                merged: Vec::new(),
                derived: Vec::new(),
                before: None,
            },
        );
        if let Some(r) = &reference {
            self.refs.insert(r.clone(), id.clone());
        }
        let cited: BTreeSet<EvidenceId> = sources.iter().copied().collect();
        let standing_ok = self.supports(&id, &cited, standing);
        let verified_ok =
            status != MemoryStatus::Verified || self.confirms(&id, &sources, false)?;
        let standing = if standing_ok {
            standing
        } else {
            adjusted.push(format!(
                "standing {} is not supported by its sources: kept as {}",
                standing.word(),
                Standing::default_for(kind).word()
            ));
            Standing::default_for(kind)
        };
        let status = if verified_ok {
            status
        } else {
            adjusted
                .push("verificato needs an independent confirmation: kept as candidato".to_owned());
            MemoryStatus::Candidate
        };
        if let Some(m) = self.memories.get_mut(&id) {
            m.standing = standing;
            m.status = status;
        }
        Ok(Judged::Ops(
            vec![MemoryOp::Create(NewMemory {
                id: Some(id),
                address,
                kind,
                standing: Some(standing),
                summary: Redacted::assume(summary),
                details: Redacted::assume(details),
                scopes,
                status,
                anchors,
                sources,
                relations: Vec::new(),
            })],
            adjusted,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn update(
        &mut self,
        id: &str,
        summary: Option<String>,
        details: Option<String>,
        scope: Option<Vec<String>>,
        anchors: Option<Vec<(String, String)>>,
        standing: Option<&str>,
        sources: &[String],
    ) -> Step<Judged> {
        let id = self.memory(id, "id")?;
        let sources = self.sources(sources, false)?;
        let mut adjusted = Vec::new();
        let mut u = MemoryUpdate::new(id.clone());
        match &summary {
            // A long summary moves its tail into details only when the
            // update replaces the details too: otherwise the tail would
            // replace them.
            Some(s) => {
                let (s, d, split) =
                    self.summary_and_details(s, details.as_deref(), details.is_some())?;
                if split {
                    adjusted.push(SUMMARY_SPLIT.to_owned());
                }
                u.summary = Some(Redacted::assume(s));
                u.details = d.map(Redacted::assume);
            }
            None => {
                u.details = details
                    .map(|d| self.details(&d).map(Redacted::assume))
                    .transpose()?;
            }
        }
        if let Some(a) = anchors {
            let (list, filled) = self.anchors(&a, &sources)?;
            adjusted.extend(filled);
            // Every anchor given was left out: the memory keeps its own,
            // rather than losing them to a list nobody asked to empty.
            if list.is_empty() == a.is_empty() {
                u.anchors = Some(list);
            }
        }
        let sim = self.sim(&id)?.clone();
        if let Some(scope) = scope {
            let scopes = self.scopes(&scope)?;
            if scopes.is_empty() {
                return Err("scope: at least one is required".to_owned());
            }
            let drops_gone = sim
                .scopes
                .iter()
                .any(|s| s.gone && !scopes.contains(&s.scope));
            let promotes = scopes.contains(&Scope::Project)
                && !sim.scopes.iter().any(|s| s.scope == Scope::Project)
                && sim
                    .scopes
                    .iter()
                    .any(|s| matches!(s.scope, Scope::Branch(_)));
            if (drops_gone || promotes) && !self.has_strong(&sources) {
                return Err(
                    "scope: promoting a branch memory or dropping a gone branch needs a user \
                     statement, a code observation or a project document among the sources"
                        .to_owned(),
                );
            }
            u.scopes = Some(scopes);
        }
        if let Some(w) = standing {
            let s = Standing::from_word(w).ok_or("standing: unknown")?;
            if !s.applies_to(sim.kind) {
                return Err("standing: does not apply to this kind".to_owned());
            }
            if s != sim.standing && !self.raises(&id, &sources, s)? {
                return Err(format!(
                    "standing: {} is not supported by the memory's evidence",
                    s.word()
                ));
            }
            u.standing = Some(s);
        }
        // The rest of the memory must still hold once its lineage grows.
        let (standing_ok, verified_ok) = self.supported(
            &id,
            &sources,
            u.standing.unwrap_or(sim.standing),
            sim.status == MemoryStatus::Verified,
        )?;
        if !(standing_ok && verified_ok) {
            return Err("the memory's claims would lose their support".to_owned());
        }
        if u.summary.is_none()
            && u.details.is_none()
            && u.scopes.is_none()
            && u.anchors.is_none()
            && u.standing.is_none()
            && sources.is_empty()
        {
            return Err("nothing to change".to_owned());
        }
        u.add_sources.clone_from(&sources);
        self.add_sources(&id, &sources);
        if let (Some(m), Some(s)) = (self.memories.get_mut(&id), u.standing) {
            m.standing = s;
        }
        if let (Some(m), Some(scopes)) = (self.memories.get_mut(&id), &u.scopes) {
            m.scopes = scopes
                .iter()
                .map(|s| ScopeEntry {
                    scope: s.clone(),
                    gone: m.scopes.iter().any(|e| &e.scope == s && e.gone),
                })
                .collect();
        }
        Ok(Judged::Ops(vec![MemoryOp::Update(u)], adjusted))
    }

    fn merge(&mut self, into: &str, from: &[String], sources: &[String]) -> Step<Judged> {
        let into = self.memory(into, "into")?;
        if from.is_empty() {
            return Err("from: at least one memory".to_owned());
        }
        let sources = self.sources(sources, false)?;
        let target = self.sim(&into)?.clone();
        let mut ids = Vec::new();
        for f in from {
            let id = self.memory(f, "from")?;
            if id == into || ids.contains(&id) {
                return Err("from: memories must be distinct and other than into".to_owned());
            }
            let m = self.sim(&id)?;
            if m.kind != target.kind {
                return Err("merge: the memories are of different kinds".to_owned());
            }
            if m.standing != target.standing {
                return Err(format!(
                    "merge: a {} would be merged into a {}; keep them apart",
                    m.standing.word(),
                    target.standing.word()
                ));
            }
            if self.gone_scope(&id) && !self.strong(&id, &sources) {
                return Err(
                    "merge: a memory of a gone branch is merged away only with a user statement, \
                     a code observation or a project document among the sources"
                        .to_owned(),
                );
            }
            ids.push(id);
        }
        let gained: Vec<EvidenceId> = ids
            .iter()
            .filter_map(|id| self.memories.get(id))
            .flat_map(|m| m.sources.iter().copied())
            .collect();
        if let Some(m) = self.memories.get_mut(&into) {
            m.merged.extend(ids.iter().cloned());
        }
        let mut all = gained;
        all.extend(sources.iter().copied());
        let (standing_ok, verified_ok) = self.supported(
            &into,
            &all,
            target.standing,
            target.status == MemoryStatus::Verified,
        )?;
        if !(standing_ok && verified_ok) {
            if let Some(m) = self.memories.get_mut(&into) {
                m.merged.retain(|x| !ids.contains(x));
            }
            return Err("merge: the kept memory's claims would lose their support".to_owned());
        }
        for id in &ids {
            if let Some(m) = self.memories.get_mut(id) {
                m.live = false;
            }
        }
        self.add_sources(&into, &all);
        Ok(Judged::Ops(
            Self::with_sources(
                &into,
                sources,
                MemoryOp::Merge {
                    into: into.clone(),
                    from: ids,
                },
            ),
            Vec::new(),
        ))
    }

    fn supersede(&mut self, old: &str, by: &str, sources: &[String]) -> Step<Judged> {
        let old = self.memory(old, "old")?;
        let by = self.memory(by, "by")?;
        if old == by {
            return Err("supersede: a memory cannot replace itself".to_owned());
        }
        let sources = self.sources(sources, false)?;
        let (o, b) = (self.sim(&old)?.clone(), self.sim(&by)?.clone());
        if o.standing.required_evidence().is_some() && b.standing.required_evidence().is_none() {
            return Err(format!(
                "supersede: a {} cannot replace a {}",
                b.standing.word(),
                o.standing.word()
            ));
        }
        if o.status == MemoryStatus::Verified
            && matches!(b.status, MemoryStatus::Candidate | MemoryStatus::ToVerify)
            && sources.is_empty()
            && !self.replay
        {
            return Err(
                "supersede: replacing a verified memory needs sources that show the change"
                    .to_owned(),
            );
        }
        if self.gone_scope(&old) && !self.strong(&old, &sources) {
            return Err(
                "supersede: a memory of a gone branch is superseded only with a user statement, \
                 a code observation or a project document among the sources"
                    .to_owned(),
            );
        }
        self.add_sources(&old, &sources);
        if let Some(m) = self.memories.get_mut(&old) {
            m.status = MemoryStatus::Superseded;
        }
        Ok(Judged::Ops(
            Self::with_sources(
                &old,
                sources,
                MemoryOp::Supersede {
                    old: old.clone(),
                    by,
                },
            ),
            Vec::new(),
        ))
    }

    fn set_status(&mut self, id: &str, status: &str, sources: &[String]) -> Step<Judged> {
        let id = self.memory(id, "id")?;
        let status = MemoryStatus::from_word(status).ok_or("status: unknown")?;
        let sources = self.sources(sources, false)?;
        let sim = self.sim(&id)?.clone();
        match status {
            MemoryStatus::Superseded => {
                return Err(
                    "status: superato comes only from supersede with its replacement".to_owned(),
                );
            }
            MemoryStatus::ToVerify => {}
            MemoryStatus::Conflict if sources.is_empty() => {
                return Err(
                    "status: conflitto needs the diverging source, so both stay visible".to_owned(),
                );
            }
            MemoryStatus::Candidate if sources.is_empty() && sim.status != status => {
                return Err("status: candidato needs the evidence that justifies it".to_owned());
            }
            MemoryStatus::Verified => {
                if sources.is_empty() {
                    return Err("status: verificato needs the evidence that confirms it".to_owned());
                }
                if !self.confirms(&id, &sources, true)? {
                    return Err(
                        "status: verificato needs an independent confirmation (a user statement, \
                         a code observation, a project document, or a session that had not been \
                         shown this memory)"
                            .to_owned(),
                    );
                }
            }
            _ => {}
        }
        if sim.status == status && sources.is_empty() {
            return Err("nothing to change".to_owned());
        }
        self.add_sources(&id, &sources);
        if let Some(m) = self.memories.get_mut(&id) {
            m.status = status;
        }
        Ok(Judged::Ops(
            vec![MemoryOp::SetStatus {
                id,
                status,
                sources,
            }],
            Vec::new(),
        ))
    }

    fn move_to(&mut self, id: &str, to: &str) -> Step<Judged> {
        let id = self.memory(id, "id")?;
        self.plain(to, "to")?;
        let to =
            Address::parse(to).map_err(|_| "to: not an address of a room that holds memories")?;
        if to.room() == Room::Archive {
            return Err("to: use archive to move a memory into the archive".to_owned());
        }
        if self.sim(&id)?.address == to {
            return Err("nothing to change".to_owned());
        }
        if self.address_held(&to, Some(&id)) {
            return Err(format!("to: {to} is already used"));
        }
        if let Some(m) = self.memories.get_mut(&id) {
            m.address = to.clone();
        }
        Ok(Judged::Ops(vec![MemoryOp::Move { id, to }], Vec::new()))
    }

    fn archive(&mut self, id: &str, sources: &[String]) -> Step<Judged> {
        let id = self.memory(id, "id")?;
        let sources = self.sources(sources, !self.replay)?;
        let sim = self.sim(&id)?.clone();
        if sim.address.room() == Room::Archive {
            return Err("already in the archive".to_owned());
        }
        if self.gone_scope(&id) && !self.strong(&id, &sources) {
            return Err(
                "archive: a memory of a gone branch is archived only with a user statement, a \
                 code observation or a project document among the sources"
                    .to_owned(),
            );
        }
        self.add_sources(&id, &sources);
        if let Some(m) = self.memories.get_mut(&id)
            && let Ok(a) = Address::new(Room::Archive, sim.address.slug())
        {
            m.address = a;
        }
        Ok(Judged::Ops(
            Self::with_sources(&id, sources, MemoryOp::Archive { id: id.clone() }),
            Vec::new(),
        ))
    }

    fn link(&mut self, from: &str, relation: &str, to: &str) -> Step<Judged> {
        let from = self.memory(from, "from")?;
        let to = self.memory(to, "to")?;
        let kind = RelationKind::from_word(relation).ok_or("relation: unknown")?;
        if kind == RelationKind::Supersedes {
            return Err("relation: use supersede".to_owned());
        }
        if from == to {
            return Err("link: a memory cannot relate to itself".to_owned());
        }
        if kind == RelationKind::DerivedFrom {
            // Deriving changes the lineage; the claims must still hold.
            if let Some(m) = self.memories.get_mut(&from) {
                m.derived.push(to.clone());
            }
            let sim = self.sim(&from)?.clone();
            let (a, b) = self.supported(
                &from,
                &[],
                sim.standing,
                sim.status == MemoryStatus::Verified,
            )?;
            if !(a && b) {
                if let Some(m) = self.memories.get_mut(&from) {
                    m.derived.retain(|x| x != &to);
                }
                return Err("link: the memory's claims would lose their support".to_owned());
            }
        }
        Ok(Judged::Ops(
            vec![MemoryOp::Link { from, kind, to }],
            Vec::new(),
        ))
    }

    /// An open annotation of this run, within the cutoff.
    fn annotation(&self, text: &str) -> Step<AnnotationId> {
        let id = AnnotationId::parse(text).map_err(|_| "annotation: not an annotation id")?;
        let note = match self.read.annotation(&id) {
            Ok(Some(n)) => n,
            Ok(None) => return Err(format!("annotation: no {id} in this project")),
            Err(_) => return Err("annotation: could not be checked".to_owned()),
        };
        if note.seq > self.packet.cutoff {
            return Err(format!(
                "annotation: {id} arrived after this run's cutoff; it is for the next dream"
            ));
        }
        if !self.replay && !self.packet.annotations.contains(&id) {
            return Err(format!(
                "annotation: {id} was not in the material given to this run"
            ));
        }
        if !note.is_open() {
            return Err(format!("annotation: {id} is already absorbed"));
        }
        if self.absorbed.contains(&id) || self.kept.contains(&id) {
            return Err(format!("annotation: {id} is named twice in this reply"));
        }
        Ok(id)
    }

    fn absorb(&mut self, annotation: &str, into: Option<&str>) -> Step<Judged> {
        let id = self.annotation(annotation)?;
        let into = into.map(|m| self.memory(m, "into")).transpose()?;
        self.absorbed.insert(id.clone());
        if let Some(m) = &into {
            self.absorbed_into
                .entry(m.clone())
                .or_default()
                .push(id.clone());
        }
        Ok(Judged::Absorb(Absorb {
            annotation: id,
            into,
        }))
    }

    fn keep(&mut self, annotation: &str, reason: &str) -> Step<Judged> {
        let id = self.annotation(annotation)?;
        if reason.len() > MAX_REASON_BYTES {
            return Err("reason: too long".to_owned());
        }
        let reason = self.clean(reason, "reason")?;
        self.kept.insert(id.clone());
        Ok(Judged::Keep(id, reason))
    }
}

struct Create {
    reference: Option<String>,
    room: String,
    slug: Option<String>,
    kind: String,
    standing: Option<String>,
    status: Option<String>,
    summary: String,
    details: Option<String>,
    scope: Vec<String>,
    anchors: Vec<(String, String)>,
    sources: Vec<String>,
}

/// The outcome of the final judgement of a run's whole ordered list of
/// staged items. Its operations are what [`recheck`] produced (rewritten
/// and downgraded where the replay said so, plus the post-condition's
/// lowerings), never the staged originals; they are private, and the only
/// way to publish them is [`Final::publication`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Final {
    ops: Vec<MemoryOp>,
    absorb: Vec<Absorb>,
    /// For each op: the staged item it came from; `None` for a lowering the
    /// post-condition added.
    origin: Vec<Option<usize>>,
    absorb_origin: Vec<usize>,
    /// Annotations left open, with their reason.
    pub kept: Vec<(AnnotationId, String)>,
    /// Staged items refused now (index into the staged list) and why.
    pub rejected: Vec<Rejected>,
    /// Staged items kept with a weaker claim (index into the staged list),
    /// and the post-condition's lowerings (index `usize::MAX`).
    pub adjusted: Vec<Rejected>,
}

impl Final {
    /// Nothing to publish.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.absorb.is_empty()
    }

    /// Operations and absorptions it would publish.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.len() + self.absorb.len()
    }

    /// The operations, to look at (not to publish: see
    /// [`Final::publication`]).
    #[must_use]
    pub fn ops(&self) -> &[MemoryOp] {
        &self.ops
    }

    #[must_use]
    pub fn absorbs(&self) -> &[Absorb] {
        &self.absorb
    }

    /// The staged item the operation at `index` came from (`None`: a
    /// lowering of the post-condition).
    #[must_use]
    pub fn op_origin(&self, index: usize) -> Option<usize> {
        self.origin.get(index).copied().flatten()
    }

    /// The staged item the absorption at `index` came from.
    #[must_use]
    pub fn absorb_origin(&self, index: usize) -> Option<usize> {
        self.absorb_origin.get(index).copied()
    }

    /// The one publication of this judgement.
    #[must_use]
    pub fn publication(
        &self,
        project_id: &str,
        cycle: Cycle,
        base_version: Option<u64>,
        cutoff: u64,
        run_id: Option<u64>,
        now_ms: u64,
    ) -> Publication {
        Publication {
            project_id: project_id.to_owned(),
            cycle,
            base_version,
            cutoff,
            ops: self.ops.clone(),
            absorb: self.absorb.clone(),
            run_id,
            now_ms,
        }
    }
}

/// The final judgement of a run's whole ordered list of staged items,
/// against the store as it is now: every rule on the memories (kinds and
/// standings of a merge, who may supersede whom, what may be verified or
/// raised, gone branches, addresses, absorptions within the cutoff and
/// once) is applied again in order, each item as the replay rewrites it,
/// then the post-condition lowers any claim the final state does not
/// support. What a run publishes is exactly [`Final::publication`].
#[must_use]
pub fn recheck(read: &Read, staged: &[Staged], cutoff: u64, known_secrets: &[String]) -> Final {
    let packet = Packet {
        cutoff,
        ..Packet::default()
    };
    let indexed: Vec<(usize, ProposedOp)> = staged
        .iter()
        .enumerate()
        .filter_map(|(i, s)| to_proposed(s).map(|p| (i, p)))
        .collect();
    let proposed: Vec<ProposedOp> = indexed.iter().map(|(_, p)| p.clone()).collect();
    let mut out = Final::default();
    let mut state = match State::new(read, &packet, known_secrets, &proposed) {
        Ok(s) => s,
        Err(reason) => {
            out.rejected
                .extend(indexed.iter().map(|(index, _)| Rejected {
                    index: *index,
                    reason: reason.clone(),
                }));
            return out;
        }
    };
    state.replay = true;
    for (index, op) in indexed {
        match state.judge(op) {
            Ok(Judged::Ops(ops, adjusted)) => {
                out.origin
                    .extend(std::iter::repeat_n(Some(index), ops.len()));
                out.ops.extend(ops);
                out.adjusted.extend(
                    adjusted
                        .into_iter()
                        .map(|reason| Rejected { index, reason }),
                );
            }
            Ok(Judged::Absorb(a)) => {
                out.absorb.push(a);
                out.absorb_origin.push(index);
            }
            Ok(Judged::Keep(id, reason)) => out.kept.push((id, reason)),
            Err(reason) => out.rejected.push(Rejected { index, reason }),
        }
    }
    let (lowerings, notes) = state.postcondition();
    out.origin
        .extend(std::iter::repeat_n(None, lowerings.len()));
    out.ops.extend(lowerings);
    out.adjusted
        .extend(notes.into_iter().map(|reason| Rejected {
            index: usize::MAX,
            reason,
        }));
    out
}

/// A validated item as the proposal it came from, to judge it again.
fn to_proposed(item: &Staged) -> Option<ProposedOp> {
    let words = |list: &[Scope]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    let pairs = |list: &[Anchor]| {
        list.iter()
            .map(|a| (a.path.clone(), a.blob.clone()))
            .collect::<Vec<_>>()
    };
    let evs = |list: &[EvidenceId]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    Some(match item {
        Staged::Op(MemoryOp::Create(n)) => ProposedOp::Create {
            reference: n.id.as_ref().map(ToString::to_string),
            room: n.address.room().word().to_owned(),
            slug: Some(n.address.slug().to_owned()),
            kind: n.kind.word().to_owned(),
            standing: n.standing.map(|s| s.word().to_owned()),
            status: Some(n.status.word().to_owned()),
            summary: n.summary.get().clone(),
            details: Some(n.details.get().clone()),
            scope: words(&n.scopes),
            anchors: pairs(&n.anchors),
            sources: evs(&n.sources),
            scope_normalised: false,
        },
        Staged::Op(MemoryOp::Update(u)) => ProposedOp::Update {
            id: u.id.to_string(),
            summary: u.summary.as_ref().map(|s| s.get().clone()),
            details: u.details.as_ref().map(|s| s.get().clone()),
            scope: u.scopes.as_deref().map(words),
            anchors: u.anchors.as_deref().map(pairs),
            standing: u.standing.map(|s| s.word().to_owned()),
            sources: evs(&u.add_sources),
            scope_normalised: false,
        },
        Staged::Op(MemoryOp::Merge { into, from }) => ProposedOp::Merge {
            into: into.to_string(),
            from: from.iter().map(ToString::to_string).collect(),
            sources: Vec::new(),
        },
        Staged::Op(MemoryOp::Supersede { old, by }) => ProposedOp::Supersede {
            old: old.to_string(),
            by: by.to_string(),
            sources: Vec::new(),
        },
        Staged::Op(MemoryOp::SetStatus {
            id,
            status,
            sources,
        }) => ProposedOp::SetStatus {
            id: id.to_string(),
            status: status.word().to_owned(),
            sources: evs(sources),
        },
        Staged::Op(MemoryOp::Move { id, to }) => ProposedOp::Move {
            id: id.to_string(),
            to: to.to_string(),
        },
        Staged::Op(MemoryOp::Archive { id }) => ProposedOp::Archive {
            id: id.to_string(),
            sources: Vec::new(),
        },
        Staged::Op(MemoryOp::Link { from, kind, to }) => ProposedOp::Link {
            from: from.to_string(),
            relation: kind.word().to_owned(),
            to: to.to_string(),
        },
        Staged::Absorb(a) => ProposedOp::Absorb {
            annotation: a.annotation.to_string(),
            into: a.into.as_ref().map(ToString::to_string),
        },
        Staged::Keep(id, reason) => ProposedOp::Keep {
            annotation: id.to_string(),
            reason: reason.clone(),
        },
        Staged::Rejected(_) => return None,
    })
}

// ---- staging encoding

/// One validated item, as staged between the runs of one dream (or the
/// packets of an initial build) and published together at the end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staged {
    Op(MemoryOp),
    Absorb(Absorb),
    Keep(AnnotationId, String),
    /// A proposal turned down, kept for the run's report.
    Rejected(String),
}

fn ids(list: &[MemoryId]) -> Value {
    Value::from(list.iter().map(ToString::to_string).collect::<Vec<_>>())
}

fn ev_ids(list: &[EvidenceId]) -> Value {
    Value::from(list.iter().map(ToString::to_string).collect::<Vec<_>>())
}

fn scope_texts(list: &[Scope]) -> Value {
    Value::from(list.iter().map(ToString::to_string).collect::<Vec<_>>())
}

fn anchor_values(list: &[Anchor]) -> Value {
    Value::from(
        list.iter()
            .map(|a| json!({ "path": a.path, "blob": a.blob }))
            .collect::<Vec<_>>(),
    )
}

/// The stored form of a staged item. Texts in it were redacted by
/// [`validate`].
#[must_use]
pub fn encode(item: &Staged) -> String {
    let v = match item {
        Staged::Op(MemoryOp::Create(n)) => json!({
            "t": "create",
            "id": n.id.as_ref().map(ToString::to_string),
            "address": n.address.to_string(),
            "kind": n.kind.word(),
            "standing": n.standing.map(Standing::word),
            "summary": n.summary.get(),
            "details": n.details.get(),
            "scopes": scope_texts(&n.scopes),
            "status": n.status.word(),
            "anchors": anchor_values(&n.anchors),
            "sources": ev_ids(&n.sources),
            "relations": n.relations.iter().map(|r| json!([r.kind.word(), r.target.to_string()])).collect::<Vec<_>>(),
        }),
        Staged::Op(MemoryOp::Update(u)) => json!({
            "t": "update",
            "id": u.id.to_string(),
            "summary": u.summary.as_ref().map(|s| s.get().clone()),
            "details": u.details.as_ref().map(|s| s.get().clone()),
            "scopes": u.scopes.as_deref().map(scope_texts),
            "anchors": u.anchors.as_deref().map(anchor_values),
            "standing": u.standing.map(Standing::word),
            "add_sources": ev_ids(&u.add_sources),
        }),
        Staged::Op(MemoryOp::Merge { into, from }) => {
            json!({ "t": "merge", "into": into.to_string(), "from": ids(from) })
        }
        Staged::Op(MemoryOp::Supersede { old, by }) => {
            json!({ "t": "supersede", "old": old.to_string(), "by": by.to_string() })
        }
        Staged::Op(MemoryOp::SetStatus {
            id,
            status,
            sources,
        }) => json!({
            "t": "set_status", "id": id.to_string(), "status": status.word(), "sources": ev_ids(sources)
        }),
        Staged::Op(MemoryOp::Move { id, to }) => {
            json!({ "t": "move", "id": id.to_string(), "to": to.to_string() })
        }
        Staged::Op(MemoryOp::Archive { id }) => json!({ "t": "archive", "id": id.to_string() }),
        Staged::Op(MemoryOp::Link { from, kind, to }) => json!({
            "t": "link", "from": from.to_string(), "kind": kind.word(), "to": to.to_string()
        }),
        Staged::Absorb(a) => json!({
            "t": "absorb", "annotation": a.annotation.to_string(),
            "into": a.into.as_ref().map(ToString::to_string)
        }),
        Staged::Keep(id, reason) => {
            json!({ "t": "keep", "annotation": id.to_string(), "reason": reason })
        }
        Staged::Rejected(reason) => json!({ "t": "rejected", "reason": reason }),
    };
    v.to_string()
}

/// Read back what [`encode`] wrote; `None` for anything else.
#[must_use]
pub fn decode(text: &str) -> Option<Staged> {
    let v: Value = serde_json::from_str(text).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    let opt = |k: &str| -> Option<Option<String>> {
        match v.get(k) {
            None | Some(Value::Null) => Some(None),
            Some(Value::String(t)) => Some(Some(t.clone())),
            Some(_) => None,
        }
    };
    let strings = |val: &Value| -> Option<Vec<String>> {
        val.as_array()?
            .iter()
            .map(|x| x.as_str().map(str::to_owned))
            .collect()
    };
    let mem = |k: &str| MemoryId::parse(s(k)?).ok();
    let mems = |k: &str| -> Option<Vec<MemoryId>> {
        strings(v.get(k)?)?
            .iter()
            .map(|x| MemoryId::parse(x).ok())
            .collect()
    };
    let evs = |k: &str| -> Option<Vec<EvidenceId>> {
        strings(v.get(k)?)?
            .iter()
            .map(|x| EvidenceId::parse(x).ok())
            .collect()
    };
    let scopes = |val: &Value| -> Option<Vec<Scope>> {
        strings(val)?.iter().map(|x| Scope::parse(x).ok()).collect()
    };
    let anchors = |val: &Value| -> Option<Vec<Anchor>> {
        val.as_array()?
            .iter()
            .map(|a| Anchor::new(a.get("path")?.as_str()?, a.get("blob")?.as_str()?).ok())
            .collect()
    };
    let standing = |k: &str| -> Option<Option<Standing>> {
        match opt(k)? {
            None => Some(None),
            Some(w) => Standing::from_word(&w).map(Some),
        }
    };
    Some(match s("t")? {
        "create" => {
            let relations = v
                .get("relations")?
                .as_array()?
                .iter()
                .map(|r| {
                    let pair = r.as_array()?;
                    Some(connecto_core::memory::Relation {
                        kind: RelationKind::from_word(pair.first()?.as_str()?)?,
                        target: MemoryId::parse(pair.get(1)?.as_str()?).ok()?,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Staged::Op(MemoryOp::Create(NewMemory {
                id: match opt("id")? {
                    None => None,
                    Some(t) => Some(MemoryId::parse(&t).ok()?),
                },
                address: Address::parse(s("address")?).ok()?,
                kind: MemoryKind::from_word(s("kind")?)?,
                standing: standing("standing")?,
                summary: Redacted::assume(s("summary")?.to_owned()),
                details: Redacted::assume(s("details")?.to_owned()),
                scopes: scopes(v.get("scopes")?)?,
                status: MemoryStatus::from_word(s("status")?)?,
                anchors: anchors(v.get("anchors")?)?,
                sources: evs("sources")?,
                relations,
            }))
        }
        "update" => {
            let mut u = MemoryUpdate::new(mem("id")?);
            u.summary = opt("summary")?.map(Redacted::assume);
            u.details = opt("details")?.map(Redacted::assume);
            u.scopes = match v.get("scopes") {
                None | Some(Value::Null) => None,
                Some(val) => Some(scopes(val)?),
            };
            u.anchors = match v.get("anchors") {
                None | Some(Value::Null) => None,
                Some(val) => Some(anchors(val)?),
            };
            u.standing = standing("standing")?;
            u.add_sources = evs("add_sources")?;
            Staged::Op(MemoryOp::Update(u))
        }
        "merge" => Staged::Op(MemoryOp::Merge {
            into: mem("into")?,
            from: mems("from")?,
        }),
        "supersede" => Staged::Op(MemoryOp::Supersede {
            old: mem("old")?,
            by: mem("by")?,
        }),
        "set_status" => Staged::Op(MemoryOp::SetStatus {
            id: mem("id")?,
            status: MemoryStatus::from_word(s("status")?)?,
            sources: evs("sources")?,
        }),
        "move" => Staged::Op(MemoryOp::Move {
            id: mem("id")?,
            to: Address::parse(s("to")?).ok()?,
        }),
        "archive" => Staged::Op(MemoryOp::Archive { id: mem("id")? }),
        "link" => Staged::Op(MemoryOp::Link {
            from: mem("from")?,
            kind: RelationKind::from_word(s("kind")?)?,
            to: mem("to")?,
        }),
        "absorb" => Staged::Absorb(Absorb {
            annotation: AnnotationId::parse(s("annotation")?).ok()?,
            into: match opt("into")? {
                None => None,
                Some(t) => Some(MemoryId::parse(&t).ok()?),
            },
        }),
        "keep" => Staged::Keep(
            AnnotationId::parse(s("annotation")?).ok()?,
            s("reason")?.to_owned(),
        ),
        "rejected" => Staged::Rejected(s("reason")?.to_owned()),
        _ => return None,
    })
}

// ---- the prompt

const PROMPT_SECTION: &str = r#"Reply with JSON only: {"operations":[...]}. An empty list is a valid answer.
Each operation has "op" and only its listed fields. Every value is one string, except the fields shown with [...]: those are always lists, even of one item. Cite only ids shown in the material. A create may carry "ref" (a-z, 0-9, - or _) that later operations of the same reply may use as its id.
- create: ref, room, slug, kind, standing, status, summary, details, scope ["project" or "component:<path>", "branch:<name>", "worktree:<path>"], anchors [{"path":"<path>"}], sources ["e-..."]
- update: id, then only what changes: summary, details, scope [...], anchors [...], standing, sources [...]
- merge: into, from ["m-..."], sources [...] (same kind and standing only)
- supersede: old, by, sources [...]
- set_status: id, status, sources [...]
- move: id, to ("room/slug")
- archive: id, sources [...]
- link: from, relation (contraddice, dipende_da, vedi_anche, deriva_da), to
- absorb: annotation, into (a memory id, a ref, or null)
- keep: annotation, reason
There is no delete: what is no longer true is superseded or archived, and every version can be undone.
summary: one or two sentences, at most 400 characters; everything else goes in details. anchors: the project files the memory is about, by path relative to the project root; Connecto adds the hash of the version in the material, so leave "blob" out unless the material shows the file's hash.
room: architettura, officina, decisioni, problemi, lavori-aperti, convenzioni. kind: fatto, procedura, decisione, problema, lavoro, convenzione. status: candidato, da_verificare, conflitto, verificato. standing by kind: decisione, convenzione: proposta, accettata, scartata; lavoro: pianificato, in_corso, implementato; fatto, problema: ipotesi, accertato; procedura: documentata, eseguita. Leave standing out for the weakest.
Example: {"operations":[{"op":"create","ref":"r1","room":"officina","kind":"procedura","summary":"Release with make release.","scope":["project"],"sources":["e-12"]},{"op":"absorb","annotation":"p-abcd2345","into":"r1"}]}

Truth rules (Connecto enforces them; an operation that breaks one is discarded):
- Sources are e- ids of this material only. A file you read on your own is not evidence.
- A proposal is not an accepted decision: accettata or scartata need a user statement or a project document; an agent agreeing is not approval.
- A plan is not a completed implementation: implementato needs a code observation or a user statement.
- A hypothesis is not an established fact; a documented procedure is not one seen working.
- Repeating a claim does not confirm it: a session that had been shown memory content never confirms it, and verificato needs independent evidence.
- Merge only memories of the same kind and standing.
- Code says how things are, the user how they should be, decisions why: when they diverge, set conflitto citing the diverging source.
- A changed anchored file makes a memory da_verificare at most, never false or superseded by itself.
- A deleted, squashed or rebased branch alone never promotes a memory to project scope or archives it.
- When the evidence is not enough, leave the memory or set it to da_verificare."#;

/// The fixed description of the operation format and of the truth rules,
/// shared by the dream's and the initial build's prompts.
#[must_use]
pub const fn operations_prompt_section() -> &'static str {
    PROMPT_SECTION
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_found_in_their_usual_wrappings() {
        assert_eq!(parse(r#"{"operations":[]}"#), Ok(Vec::new()));
        assert_eq!(parse("[]"), Ok(Vec::new()));
        assert_eq!(
            parse("```json\n{\"operations\":[{\"op\":\"keep\",\"annotation\":\"p-aaaaaaaa\",\"reason\":\"x\"}]}\n```")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(parse("Sure: {\"operations\":[]} done"), Ok(Vec::new()));
        assert_eq!(parse("   "), Err(ParseError::Empty));
        assert_eq!(parse("{\"operations\": [}"), Err(ParseError::Unreadable));
        assert_eq!(parse("{\"annotations\": []}"), Err(ParseError::Unreadable));
        assert_eq!(parse("no json"), Err(ParseError::Unreadable));
        let huge = format!("[\"{}\"]", "x".repeat(MAX_REPLY_CHARS));
        assert_eq!(parse(&huge), Err(ParseError::TooLarge));
    }

    #[test]
    fn shape_errors_are_per_operation() {
        let ops = parse(
            r#"{"operations":[
                {"op":"delete","id":"m-aaaaaaaa"},
                {"op":"frobnicate"},
                {"op":"move","id":"m-aaaaaaaa","to":"officina/x","extra":1},
                {"op":"move","id":3,"to":"officina/x"},
                {"op":"merge","into":"m-aaaaaaaa"},
                "text",
                {"op":"archive","id":"m-aaaaaaaa","sources":["e-1"]}
            ]}"#,
        )
        .unwrap();
        let reasons: Vec<String> = ops
            .iter()
            .map(|o| match o {
                ProposedOp::Malformed { reason } => reason.clone(),
                _ => "ok".to_owned(),
            })
            .collect();
        assert_eq!(
            reasons,
            [
                "there is no delete operation",
                "op: unknown operation",
                "an unknown field",
                "id: not a string (it must be one string)",
                "from: missing",
                "not an object",
                "ok"
            ]
        );
    }

    #[test]
    fn too_many_operations_are_malformed_past_the_limit() {
        let one = r#"{"op":"archive","id":"m-aaaaaaaa"}"#;
        let list = vec![one; MAX_OPERATIONS + 2].join(",");
        let ops = parse(&format!("[{list}]")).unwrap();
        assert_eq!(ops.len(), MAX_OPERATIONS + 2);
        assert!(matches!(ops[MAX_OPERATIONS], ProposedOp::Malformed { .. }));
        assert!(!matches!(ops[0], ProposedOp::Malformed { .. }));
    }

    #[test]
    fn staged_items_round_trip() {
        let id = MemoryId::from_entropy([1; 5]);
        let other = MemoryId::from_entropy([2; 5]);
        let mut u = MemoryUpdate::new(id.clone());
        u.summary = Some(Redacted::assume("s".to_owned()));
        u.scopes = Some(vec![Scope::Branch("b".to_owned())]);
        u.add_sources = vec![EvidenceId::new(3)];
        let items = vec![
            Staged::Op(MemoryOp::Create(NewMemory {
                id: Some(id.clone()),
                address: Address::parse("officina/x").unwrap(),
                kind: MemoryKind::Procedure,
                standing: Some(Standing::Documented),
                summary: Redacted::assume("S".to_owned()),
                details: Redacted::assume(String::new()),
                scopes: vec![Scope::Project],
                status: MemoryStatus::Candidate,
                anchors: vec![Anchor::new("a.rs", &"a".repeat(40)).unwrap()],
                sources: vec![EvidenceId::new(1)],
                relations: Vec::new(),
            })),
            Staged::Op(MemoryOp::Update(u)),
            Staged::Op(MemoryOp::Merge {
                into: id.clone(),
                from: vec![other.clone()],
            }),
            Staged::Op(MemoryOp::Supersede {
                old: other.clone(),
                by: id.clone(),
            }),
            Staged::Op(MemoryOp::SetStatus {
                id: id.clone(),
                status: MemoryStatus::ToVerify,
                sources: Vec::new(),
            }),
            Staged::Op(MemoryOp::Move {
                id: id.clone(),
                to: Address::parse("decisioni/y").unwrap(),
            }),
            Staged::Op(MemoryOp::Archive { id: id.clone() }),
            Staged::Op(MemoryOp::Link {
                from: id.clone(),
                kind: RelationKind::SeeAlso,
                to: other,
            }),
            Staged::Absorb(Absorb {
                annotation: AnnotationId::from_entropy([3; 5]),
                into: Some(id),
            }),
            Staged::Keep(AnnotationId::from_entropy([4; 5]), "why".to_owned()),
            Staged::Rejected("because".to_owned()),
        ];
        for item in items {
            assert_eq!(decode(&encode(&item)), Some(item.clone()), "{item:?}");
        }
        assert_eq!(decode("{\"t\":\"nope\"}"), None);
        assert_eq!(decode("not json"), None);
    }

    /// The operation lines of the prompt section: op, then each field with
    /// whether the prompt shows it as a list.
    fn prompt_shapes() -> Vec<(String, Vec<(String, bool)>)> {
        let mut out = Vec::new();
        for line in PROMPT_SECTION.lines() {
            let Some((op, rest)) = line.strip_prefix("- ").and_then(|l| l.split_once(": ")) else {
                continue;
            };
            if fields_of(op).is_none() {
                continue;
            }
            let rest = rest.replace("then only what changes: ", "");
            // Split at commas outside brackets, parentheses and braces.
            let (mut depth, mut piece, mut pieces) = (0i32, String::new(), Vec::new());
            for c in rest.chars() {
                match c {
                    '[' | '(' | '{' => depth += 1,
                    ']' | ')' | '}' => depth -= 1,
                    ',' if depth == 0 => {
                        pieces.push(std::mem::take(&mut piece));
                        continue;
                    }
                    _ => {}
                }
                piece.push(c);
            }
            pieces.push(piece);
            let fields = pieces
                .iter()
                .map(|p| {
                    let p = p.trim();
                    let name: String = p
                        .chars()
                        .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
                        .collect();
                    let list = p[name.len()..].trim_start().starts_with('[');
                    (name, list)
                })
                .collect();
            out.push((op.to_owned(), fields));
        }
        out
    }

    /// A value of the shape the prompt shows.
    fn shaped(field: &str, list: bool) -> Value {
        match (field, list) {
            // As the prompt shows it: a path, the blob left to Connecto.
            ("anchors", true) => json!([{"path": "src/a.rs"}]),
            ("scope", true) => json!(["project"]),
            (_, true) => json!(["e-1"]),
            _ => json!("x"),
        }
    }

    #[test]
    fn the_prompt_and_the_parser_agree_on_every_field_and_shape() {
        // The fields `one` reads with `list`, `scope` or `anchors`, by op.
        let lists = |op: &str| -> &[&str] {
            match op {
                "create" | "update" => &["scope", "anchors", "sources"],
                "merge" => &["from", "sources"],
                "supersede" | "set_status" | "archive" => &["sources"],
                _ => &[],
            }
        };
        let shapes = prompt_shapes();
        let ops: Vec<&str> = shapes.iter().map(|(op, _)| op.as_str()).collect();
        assert_eq!(
            ops,
            [
                "create",
                "update",
                "merge",
                "supersede",
                "set_status",
                "move",
                "archive",
                "link",
                "absorb",
                "keep"
            ]
        );
        for (op, fields) in &shapes {
            let mut named: Vec<&str> = fields.iter().map(|(f, _)| f.as_str()).collect();
            named.push("op");
            named.sort_unstable();
            let mut parsed: Vec<&str> = fields_of(op).unwrap().to_vec();
            parsed.sort_unstable();
            assert_eq!(
                named, parsed,
                "{op}: the prompt lists the fields the parser reads"
            );
            for (field, list) in fields {
                assert_eq!(
                    *list,
                    lists(op).contains(&field.as_str()),
                    "{op}.{field}: shown as a list iff read as one"
                );
            }
            // Built exactly as the prompt shows it: it parses.
            let mut object = Map::new();
            object.insert("op".to_owned(), json!(op));
            for (field, list) in fields {
                object.insert(field.clone(), shaped(field, *list));
            }
            let reply = json!({ "operations": [Value::Object(object.clone())] }).to_string();
            let parsed = parse(&reply).unwrap();
            assert!(
                !matches!(parsed[0], ProposedOp::Malformed { .. }),
                "{op}: {:?}",
                parsed[0]
            );
            // A list given as one string is refused with the field and the
            // shape named; only one valid scope string is read as a list.
            for (field, list) in fields.iter().filter(|(_, l)| *l) {
                let _ = list;
                let mut bare = object.clone();
                let value = if field == "scope" { "project" } else { "e-1" };
                bare.insert(field.clone(), json!(value));
                let reply = json!({ "operations": [Value::Object(bare)] }).to_string();
                let parsed = parse(&reply).unwrap();
                match (&parsed[0], field.as_str()) {
                    (
                        ProposedOp::Create {
                            scope,
                            scope_normalised,
                            ..
                        },
                        "scope",
                    ) => {
                        assert!(*scope_normalised && scope == &["project"]);
                    }
                    (
                        ProposedOp::Update {
                            scope,
                            scope_normalised,
                            ..
                        },
                        "scope",
                    ) => {
                        assert!(
                            *scope_normalised
                                && scope.as_deref() == Some(&["project".to_owned()][..])
                        );
                    }
                    (ProposedOp::Malformed { reason }, _) => {
                        assert!(
                            reason.starts_with(&format!("{field}: not a list")),
                            "{reason}"
                        );
                        assert!(reason.contains("it must be a list"), "{reason}");
                    }
                    (other, _) => panic!("{op}.{field}: {other:?}"),
                }
            }
        }
        // A scope string that is not one valid scope is refused, shape named.
        let parsed =
            parse(r#"[{"op":"update","id":"m-aaaaaaaa","scope":"project, branch:x"}]"#).unwrap();
        assert!(
            matches!(&parsed[0], ProposedOp::Malformed { reason } if reason.contains(r#"e.g. ["project"]"#)),
            "{parsed:?}"
        );
    }

    /// The prompt states the limits the validator holds the reply to: the
    /// summary length, and anchors as paths whose hash Connecto adds (a
    /// real build lost every memory to `"blob":""` and long summaries).
    #[test]
    fn the_prompt_states_the_summary_limit_and_anchors_without_a_hash() {
        let limit = format!("at most {MAX_SUMMARY_CHARS} characters");
        assert!(PROMPT_SECTION.contains(&limit));
        assert!(SUMMARY_SPLIT.contains(&format!("over {MAX_SUMMARY_CHARS} characters")));
        assert!(PROMPT_SECTION.contains(r#"anchors [{"path":"<path>"}]"#));
        assert!(PROMPT_SECTION.contains(r#"leave "blob" out"#));
        assert!(!PROMPT_SECTION.contains("<hash>"));
        // As the prompt shows it, an anchor parses with an empty blob.
        let parsed = parse(
            r#"[{"op":"create","room":"officina","kind":"fatto","summary":"s","anchors":[{"path":"a.rs"},{"path":"b.rs","blob":null},{"path":"c.rs","blob":""}],"sources":["e-1"]}]"#,
        )
        .unwrap();
        let ProposedOp::Create { anchors, .. } = &parsed[0] else {
            panic!("{parsed:?}")
        };
        assert_eq!(
            anchors,
            &[
                ("a.rs".to_owned(), String::new()),
                ("b.rs".to_owned(), String::new()),
                ("c.rs".to_owned(), String::new())
            ]
        );
    }

    #[test]
    fn a_long_summary_is_cut_after_its_last_whole_sentence_that_fits() {
        let head = "a".repeat(300) + ".";
        let rest = "b".repeat(200) + ". End.";
        let s = format!("{head} {rest}");
        assert_eq!(split_summary(&s, None), Some((head.clone(), rest.clone())));
        assert_eq!(
            split_summary(&s, Some("  Old.  ")),
            Some((head, format!("{rest}\n\nOld.")))
        );
        assert_eq!(split_summary("Short. Enough.", None), None);
        assert_eq!(
            split_summary(&"c".repeat(401), None),
            None,
            "no sentence end"
        );
        // A sentence end past the limit is not a cut.
        let late = format!("{}. tail", "d".repeat(400));
        assert_eq!(split_summary(&late, None), None);
        // Multibyte text is cut at characters, not bytes.
        let wide = format!("{}. {}", "é".repeat(399), "f".repeat(10));
        assert_eq!(
            split_summary(&wide, None).map(|(h, _)| h.chars().count()),
            Some(400)
        );
    }

    #[test]
    fn the_prompt_section_states_the_truth_rules() {
        let p = operations_prompt_section();
        for needle in [
            "There is no delete",
            "A proposal is not an accepted decision",
            "A plan is not a completed implementation",
            "A hypothesis is not an established fact",
            "Repeating a claim does not confirm it",
            "conflitto",
            "da_verificare at most",
            "A file you read on your own is not evidence",
        ] {
            assert!(p.contains(needle), "{needle}");
        }
    }
}
