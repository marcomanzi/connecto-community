//! The host's validator for development-cycle annotations (spec §5.7, the
//! rules that apply to annotations). The model proposes; nothing it says is
//! stored before it passes here, and what passes is redacted again.
//!
//! A reply that cannot be read at all fails the run with no effect. Inside a
//! readable reply each annotation is judged on its own: a bad one is rejected
//! with its reason and the others go on.
//!
//! Rejected: unknown fields, missing or mistyped required fields, lengths over
//! the limits, unknown types, rooms or scopes, ids that are not live memories
//! of this project, no sources or sources that were not in the packet
//! delivered to this run, a standing that does not apply to the kind, text
//! that redaction cannot clean or that redaction leaves empty.
//!
//! **Standing.** A standing the cited evidence does not support
//! ([`connecto_core::memory::supports_standing`]) is *downgraded* to the
//! kind's weakest standing (proposal, plan, hypothesis, documented
//! procedure), not rejected: what the session said is still worth keeping, as
//! what it is. The lineage used is the target memory's lineage *and* every
//! memory or annotation the cited agent sessions had been shown, so a session
//! that read something and repeats it never counts as independent evidence for
//! a stronger standing.

use std::collections::BTreeSet;

use connecto_core::Redacted;
use connecto_core::memory::{
    AnnotationId, AnnotationKind, AnnotationOrigin, AnnotationTarget, EvidenceId, MAX_NOTE_BYTES,
    MemoryId, MemoryKind, Room, Scope, Standing, Subject, Verification, check_note_text,
    supports_standing,
};
use serde_json::Value;

use crate::read::Read;
use crate::store::NewAnnotation;

/// Longest reply read, in characters: a longer one fails the run.
pub const MAX_REPLY_CHARS: usize = 1_000_000;
/// Annotations kept from one run, at most; the rest are rejected.
pub const MAX_ANNOTATIONS: usize = 50;
/// Scopes per annotation, at most.
pub const MAX_SCOPES: usize = 8;
/// Sources per annotation, at most.
pub const MAX_SOURCES: usize = 32;

const FIELDS: &[&str] = &[
    "type",
    "about",
    "room",
    "memory_kind",
    "standing",
    "text",
    "scope",
    "sources",
];

/// Why a whole reply was not usable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplyError {
    #[error("the reply is empty")]
    Empty,
    #[error("the reply is too large")]
    TooLarge,
    #[error("the reply is not readable JSON")]
    Unreadable,
}

/// The annotation list of a reply. Accepts the JSON alone, inside a code
/// fence, or with prose around it; the object form `{"annotations": [...]}`
/// or a bare list.
pub fn parse_reply(text: &str) -> Result<Vec<Value>, ReplyError> {
    if text.trim().is_empty() {
        return Err(ReplyError::Empty);
    }
    if text.chars().count() > MAX_REPLY_CHARS {
        return Err(ReplyError::TooLarge);
    }
    let candidates = [text.trim(), strip_fence(text), outer_json(text)];
    for candidate in candidates.into_iter().filter(|c| !c.is_empty()) {
        match serde_json::from_str::<Value>(candidate) {
            Ok(Value::Array(items)) => return Ok(items),
            Ok(Value::Object(object)) => match object.get("annotations") {
                Some(Value::Array(items)) => return Ok(items.clone()),
                _ => return Err(ReplyError::Unreadable),
            },
            _ => {}
        }
    }
    Err(ReplyError::Unreadable)
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

/// What the validator knows of the run.
pub struct Packet<'a> {
    /// The evidence delivered to this run: the only valid sources.
    pub sources: &'a BTreeSet<EvidenceId>,
    pub secrets: &'a [String],
    pub now_ms: u64,
}

/// The validator's verdict on one run's annotations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    pub accepted: Vec<NewAnnotation>,
    /// One reason per rejected annotation.
    pub rejected: Vec<String>,
    /// Accepted with a weaker standing than proposed.
    pub downgraded: usize,
    /// Accepted after a normalisation, and which (one per annotation).
    pub adjusted: Vec<String>,
}

/// Judge every annotation of a reply.
#[must_use]
pub fn validate(items: &[Value], packet: &Packet<'_>, read: &Read<'_>) -> Verdict {
    let mut verdict = Verdict::default();
    for (index, item) in items.iter().enumerate() {
        if index >= MAX_ANNOTATIONS {
            verdict
                .rejected
                .push("more annotations than one run may write".to_owned());
            continue;
        }
        match one(item, packet, read) {
            Ok((annotation, downgraded, normalised)) => {
                verdict.downgraded += usize::from(downgraded);
                verdict.accepted.push(annotation);
                if normalised {
                    verdict
                        .adjusted
                        .push(crate::operations::SCOPE_NORMALISED.to_owned());
                }
            }
            Err(reason) => verdict.rejected.push(reason),
        }
    }
    verdict
}

fn words<'v>(v: Option<&'v Value>, field: &str) -> Result<Option<&'v str>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("{field}: not a string (it must be one string)")),
    }
}

fn list<'v>(v: Option<&'v Value>, field: &str, max: usize) -> Result<Vec<&'v str>, String> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) if items.len() <= max => items
            .iter()
            .map(|i| {
                i.as_str()
                    .ok_or_else(|| format!("{field}: not a list of strings"))
            })
            .collect(),
        Some(Value::Array(_)) => Err(format!("{field}: too many entries")),
        Some(_) => Err(format!(
            "{field}: not a list ({})",
            crate::operations::list_shape(field)
        )),
    }
}

const fn room_for(kind: Option<MemoryKind>) -> Room {
    match kind {
        Some(MemoryKind::Decision) => Room::Decisions,
        Some(MemoryKind::Procedure) => Room::Workshop,
        Some(MemoryKind::Problem) => Room::Problems,
        Some(MemoryKind::Work) => Room::OpenWork,
        Some(MemoryKind::Convention) => Room::Conventions,
        Some(MemoryKind::Fact) | None => Room::Architecture,
    }
}

fn entropy() -> [u8; 5] {
    let id = crate::prompt::nonce();
    let mut out = [0u8; 5];
    for (slot, pair) in out.iter_mut().zip(id.as_bytes().chunks(2)) {
        *slot = std::str::from_utf8(pair)
            .ok()
            .and_then(|h| u8::from_str_radix(h, 16).ok())
            .unwrap_or_default();
    }
    out
}

#[allow(clippy::too_many_lines)]
fn one(
    item: &Value,
    packet: &Packet<'_>,
    read: &Read<'_>,
) -> Result<(NewAnnotation, bool, bool), String> {
    let Value::Object(fields) = item else {
        return Err("not an object".to_owned());
    };
    if let Some(unknown) = fields.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        // The field name is the model's text: not repeated.
        let _ = unknown;
        return Err("an unknown field".to_owned());
    }
    let kind = words(fields.get("type"), "type")?
        .and_then(AnnotationKind::from_word)
        .ok_or("type: missing or unknown")?;
    let text = words(fields.get("text"), "text")?.ok_or("text: missing")?;
    if text.len() > MAX_NOTE_BYTES {
        return Err("text: too long".to_owned());
    }
    let clean =
        crate::ingest::redact(text, packet.secrets).ok_or("text: redaction could not run")?;
    if clean.replace("[REDACTED]", "").trim().is_empty() {
        return Err("text: nothing left after redaction".to_owned());
    }
    check_note_text(&clean).map_err(|e| format!("text: {e}"))?;

    let memory_kind = match words(fields.get("memory_kind"), "memory_kind")? {
        None => None,
        Some(w) => Some(MemoryKind::from_word(w).ok_or("memory_kind: unknown")?),
    };
    let standing = match words(fields.get("standing"), "standing")? {
        None => None,
        Some(w) => Some(Standing::from_word(w).ok_or("standing: unknown")?),
    };
    let standing = match (memory_kind, standing) {
        (None, Some(_)) => return Err("standing given without memory_kind".to_owned()),
        (Some(k), Some(s)) if !s.applies_to(k) => {
            return Err("standing does not apply to this memory_kind".to_owned());
        }
        (Some(k), None) => Some(Standing::default_for(k)),
        (_, s) => s,
    };

    let about = match words(fields.get("about"), "about")? {
        None => None,
        Some(w) => {
            let id = MemoryId::parse(w).map_err(|_| "about: not a memory id")?;
            match read.memory(&id) {
                Ok(Some(_)) => Some(id),
                Ok(None) => return Err("about: no such live memory in this project".to_owned()),
                Err(_) => return Err("about: could not be checked".to_owned()),
            }
        }
    };
    if kind.needs_target() && about.is_none() {
        return Err("a correction or contradiction needs about".to_owned());
    }
    let target = match &about {
        Some(id) => AnnotationTarget::Memory(id.clone()),
        None => {
            let room = match words(fields.get("room"), "room")? {
                None => room_for(memory_kind),
                Some(w) => Room::for_memories(w).map_err(|_| "room: unknown or the entrance")?,
            };
            AnnotationTarget::New { room }
        }
    };

    let mut scopes = Vec::new();
    let (scope, normalised) = crate::operations::scope_value(fields.get("scope"), MAX_SCOPES)?;
    for s in scope.unwrap_or_default() {
        scopes.push(Scope::parse(&s).map_err(|_| "scope: not a scope")?);
    }
    if scopes.is_empty() {
        scopes.push(Scope::Project);
    }

    let mut sources = Vec::new();
    for s in list(fields.get("sources"), "sources", MAX_SOURCES)? {
        let id = EvidenceId::parse(s).map_err(|_| "sources: not an evidence id")?;
        if !packet.sources.contains(&id) {
            return Err("sources: not in the material given to this run".to_owned());
        }
        if !sources.contains(&id) {
            sources.push(id);
        }
    }
    if sources.is_empty() {
        return Err("sources: at least one is required".to_owned());
    }
    let mut evidence = Vec::new();
    for id in &sources {
        match read.evidence(*id) {
            Ok(Some(e)) => evidence.push(e),
            _ => return Err("sources: evidence not found in this project".to_owned()),
        }
    }

    let id = AnnotationId::from_entropy(entropy());
    let mut lineage: BTreeSet<Subject> = BTreeSet::new();
    lineage.insert(Subject::Annotation(id.clone()));
    if let Some(target) = &about {
        lineage.extend(read.lineage(target).unwrap_or_default());
    }
    for e in evidence.iter().filter(|e| e.kind.from_agent()) {
        lineage.extend(e.exposed_to.iter().cloned());
    }
    let lineage: Vec<Subject> = lineage.into_iter().collect();
    let mut downgraded = false;
    let standing = match (memory_kind, standing) {
        (Some(k), Some(s)) if !supports_standing(s, &evidence, &lineage) => {
            downgraded = true;
            Some(Standing::default_for(k))
        }
        (_, s) => s,
    };

    Ok((
        NewAnnotation {
            id: Some(id),
            created_ms: packet.now_ms,
            kind,
            target,
            text: Redacted::assume(clean),
            scopes,
            sources,
            verification: Verification::Unverified,
            origin: AnnotationOrigin::DevelopmentCycle,
            memory_kind,
            standing,
        },
        downgraded,
        normalised,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_found_in_their_usual_wrappings() {
        assert_eq!(parse_reply(r#"{"annotations":[]}"#), Ok(Vec::new()));
        assert_eq!(parse_reply("[]"), Ok(Vec::new()));
        assert_eq!(
            parse_reply("```json\n{\"annotations\":[{\"a\":1}]}\n```")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            parse_reply("Here you go: {\"annotations\":[]} done").unwrap(),
            Vec::<Value>::new()
        );
        assert_eq!(parse_reply("  "), Err(ReplyError::Empty));
        assert_eq!(
            parse_reply("{\"annotations\": [}"),
            Err(ReplyError::Unreadable)
        );
        assert_eq!(parse_reply("{\"notes\": []}"), Err(ReplyError::Unreadable));
        assert_eq!(parse_reply("no json here"), Err(ReplyError::Unreadable));
        let huge = format!("[\"{}\"]", "x".repeat(MAX_REPLY_CHARS));
        assert_eq!(parse_reply(&huge), Err(ReplyError::TooLarge));
    }
}
