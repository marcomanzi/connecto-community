//! Rows to values and back: the stored words of the core enums, and the JSON
//! images used by `revision` and by the export.
//!
//! The workspace has no serde derive; as in `connecto-session`, JSON is built
//! and read as `serde_json::Value` by hand.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use connecto_core::Redacted;
use connecto_core::memory::{
    Address, Anchor, Annotation, AnnotationOrigin, AnnotationTarget, Evidence, EvidenceId,
    EvidenceKind, Freshness, LogPointer, Memory, MemoryId, MemoryKind, MemoryStatus, Relation,
    RelationKind, Room, Scope, ScopeEntry, SourceRef, Standing, Subject, Verification,
};
use serde_json::{Map, Value, json};

use crate::error::{Result, StoreError};

/// Read a stored word back into its enum.
pub(crate) fn word<T>(value: Option<T>, what: &'static str) -> Result<T> {
    value.ok_or(StoreError::Corrupt(what))
}

pub(crate) fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub(crate) fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// 40 bits for a new id. Not cryptographic: ids only need to be unlikely to
/// collide, and the store checks for a collision anyway.
pub(crate) fn entropy() -> [u8; 5] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    hasher.write_u128(nanos);
    let bits = hasher.finish().to_le_bytes();
    [bits[0], bits[1], bits[2], bits[3], bits[4]]
}

pub(crate) fn subjects_to_json(subjects: &[Subject]) -> String {
    Value::from(
        subjects
            .iter()
            .map(|s| Value::from(s.to_string()))
            .collect::<Vec<_>>(),
    )
    .to_string()
}

pub(crate) fn subjects_from_json(text: &str) -> Result<Vec<Subject>> {
    strings_from_json(text)?
        .iter()
        .map(|s| Subject::parse(s).map_err(|_| StoreError::Corrupt("subject")))
        .collect()
}

pub(crate) fn strings_to_json<S: AsRef<str>>(items: &[S]) -> String {
    Value::from(
        items
            .iter()
            .map(|s| Value::from(s.as_ref()))
            .collect::<Vec<_>>(),
    )
    .to_string()
}

pub(crate) fn strings_from_json(text: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(text).map_err(|_| StoreError::Corrupt("json list"))?;
    value
        .as_array()
        .ok_or(StoreError::Corrupt("json list"))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or(StoreError::Corrupt("json list"))
        })
        .collect()
}

pub(crate) fn anchors_to_value(anchors: &[Anchor]) -> Value {
    Value::from(
        anchors
            .iter()
            .map(|a| json!({ "path": a.path, "blob": a.blob }))
            .collect::<Vec<_>>(),
    )
}

pub(crate) fn anchors_from_value(value: &Value) -> Result<Vec<Anchor>> {
    value
        .as_array()
        .ok_or(StoreError::Corrupt("anchors"))?
        .iter()
        .map(|a| {
            let path = a.get("path").and_then(Value::as_str);
            let blob = a.get("blob").and_then(Value::as_str);
            match (path, blob) {
                (Some(path), Some(blob)) => {
                    Anchor::new(path, blob).map_err(|_| StoreError::Corrupt("anchor"))
                }
                _ => Err(StoreError::Corrupt("anchor")),
            }
        })
        .collect()
}

pub(crate) fn scopes_to_json(scopes: &[Scope]) -> String {
    strings_to_json(&scopes.iter().map(ToString::to_string).collect::<Vec<_>>())
}

pub(crate) fn scopes_from_json(text: &str) -> Result<Vec<Scope>> {
    strings_from_json(text)?
        .iter()
        .map(|s| Scope::parse(s).map_err(|_| StoreError::Corrupt("scope")))
        .collect()
}

/// A memory as stored, with the two columns the core type has no field for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRow {
    pub memory: Memory,
    /// False when merged into another memory or withdrawn by a revert.
    pub live: bool,
    pub merged_into: Option<MemoryId>,
}

fn opt_u64(value: Option<u64>) -> Value {
    value.map_or(Value::Null, Value::from)
}

/// The JSON image of a memory row: what `revision` keeps and the export
/// writes. `open_annotations` is derived, so not kept.
pub(crate) fn memory_to_value(row: &MemoryRow) -> Value {
    let m = &row.memory;
    json!({
        "id": m.id.as_str(),
        "address": m.address.to_string(),
        "room": m.room().word(),
        "kind": m.kind.word(),
        "standing": m.standing.word(),
        "summary": m.summary,
        "details": m.details,
        "scopes": m.scopes.iter().map(|s| json!({ "scope": s.scope.to_string(), "gone": s.gone })).collect::<Vec<_>>(),
        "status": m.status.word(),
        "freshness": {
            "last_confirmed_ms": opt_u64(m.freshness.last_confirmed_ms),
            "anchors_changed": m.freshness.anchors_changed,
        },
        "anchors": anchors_to_value(&m.anchors),
        "sources": m.sources.iter().map(|s| json!({ "evidence": s.evidence.to_string(), "kind": s.kind.word() })).collect::<Vec<_>>(),
        "relations": m.relations.iter().map(|r| json!({ "kind": r.kind.word(), "target": r.target.as_str() })).collect::<Vec<_>>(),
        "created_ms": m.created_ms,
        "updated_ms": m.updated_ms,
        "version": m.version,
        "live": row.live,
        "merged_into": row.merged_into.as_ref().map_or(Value::Null, |id| Value::from(id.as_str())),
    })
}

fn get_str<'v>(value: &'v Value, key: &str) -> Result<&'v str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(StoreError::Corrupt("memory image"))
}

fn get_u64(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or(StoreError::Corrupt("memory image"))
}

fn get_array<'v>(value: &'v Value, key: &str) -> Result<&'v Vec<Value>> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or(StoreError::Corrupt("memory image"))
}

pub(crate) fn memory_from_value(value: &Value) -> Result<MemoryRow> {
    let bad = |_| StoreError::Corrupt("memory image");
    let id = MemoryId::parse(get_str(value, "id")?).map_err(bad)?;
    let address = Address::parse(get_str(value, "address")?).map_err(bad)?;
    let kind = word(
        MemoryKind::from_word(get_str(value, "kind")?),
        "memory kind",
    )?;
    let standing = word(Standing::from_word(get_str(value, "standing")?), "standing")?;
    let status = word(MemoryStatus::from_word(get_str(value, "status")?), "status")?;
    let scopes = get_array(value, "scopes")?
        .iter()
        .map(|s| {
            let scope = Scope::parse(get_str(s, "scope")?).map_err(bad)?;
            let gone = s.get("gone").and_then(Value::as_bool).unwrap_or(false);
            Ok(ScopeEntry { scope, gone })
        })
        .collect::<Result<Vec<_>>>()?;
    let freshness = value.get("freshness").unwrap_or(&Value::Null);
    let sources = get_array(value, "sources")?
        .iter()
        .map(|s| {
            Ok(SourceRef {
                evidence: EvidenceId::parse(get_str(s, "evidence")?).map_err(bad)?,
                kind: word(
                    EvidenceKind::from_word(get_str(s, "kind")?),
                    "evidence kind",
                )?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let relations = get_array(value, "relations")?
        .iter()
        .map(|r| {
            Ok(Relation {
                kind: word(RelationKind::from_word(get_str(r, "kind")?), "relation")?,
                target: MemoryId::parse(get_str(r, "target")?).map_err(bad)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let merged_into = match value.get("merged_into").and_then(Value::as_str) {
        Some(id) => Some(MemoryId::parse(id).map_err(bad)?),
        None => None,
    };
    Ok(MemoryRow {
        memory: Memory {
            id,
            address,
            kind,
            standing,
            summary: get_str(value, "summary")?.to_owned(),
            details: get_str(value, "details")?.to_owned(),
            scopes,
            status,
            freshness: Freshness {
                last_confirmed_ms: freshness.get("last_confirmed_ms").and_then(Value::as_u64),
                anchors_changed: freshness
                    .get("anchors_changed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                open_annotations: 0,
            },
            anchors: anchors_from_value(value.get("anchors").unwrap_or(&Value::Null))?,
            sources,
            relations,
            created_ms: get_u64(value, "created_ms")?,
            updated_ms: get_u64(value, "updated_ms")?,
            version: get_u64(value, "version")?,
        },
        live: value.get("live").and_then(Value::as_bool).unwrap_or(true),
        merged_into,
    })
}

/// The part of an annotation a publication can change: what `revision` keeps
/// for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnnotationState {
    pub target_memory: Option<MemoryId>,
    pub absorbed_in: Option<u64>,
    pub absorbed_into: Option<MemoryId>,
}

pub(crate) fn annotation_state_to_value(state: &AnnotationState) -> Value {
    let id = |m: &Option<MemoryId>| m.as_ref().map_or(Value::Null, |m| Value::from(m.as_str()));
    json!({
        "target_memory": id(&state.target_memory),
        "absorbed_in": opt_u64(state.absorbed_in),
        "absorbed_into": id(&state.absorbed_into),
    })
}

pub(crate) fn annotation_state_from_value(value: &Value) -> Result<AnnotationState> {
    let id = |key: &str| -> Result<Option<MemoryId>> {
        match value.get(key).and_then(Value::as_str) {
            Some(text) => MemoryId::parse(text)
                .map(Some)
                .map_err(|_| StoreError::Corrupt("annotation image")),
            None => Ok(None),
        }
    };
    Ok(AnnotationState {
        target_memory: id("target_memory")?,
        absorbed_in: value.get("absorbed_in").and_then(Value::as_u64),
        absorbed_into: id("absorbed_into")?,
    })
}

/// An annotation for the export.
pub(crate) fn annotation_to_value(a: &Annotation) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), a.id.as_str().into());
    map.insert("seq".into(), a.seq.into());
    map.insert("created_ms".into(), a.created_ms.into());
    map.insert("type".into(), a.kind.word().into());
    match &a.target {
        AnnotationTarget::Memory(id) => {
            map.insert("target".into(), id.as_str().into());
        }
        AnnotationTarget::New { room } => {
            map.insert("target".into(), Value::Null);
            map.insert("room".into(), room.word().into());
        }
    }
    map.insert("text".into(), a.text.clone().into());
    map.insert(
        "scopes".into(),
        a.scopes
            .iter()
            .map(|s| Value::from(s.to_string()))
            .collect::<Vec<_>>()
            .into(),
    );
    map.insert(
        "sources".into(),
        a.sources
            .iter()
            .map(|s| json!({ "evidence": s.evidence.to_string(), "kind": s.kind.word() }))
            .collect::<Vec<_>>()
            .into(),
    );
    map.insert("verification".into(), a.verification.word().into());
    map.insert("origin".into(), a.origin.to_string().into());
    map.insert(
        "memory_kind".into(),
        a.memory_kind.map_or(Value::Null, |k| k.word().into()),
    );
    map.insert(
        "standing".into(),
        a.standing.map_or(Value::Null, |s| s.word().into()),
    );
    map.insert("absorbed_in".into(), opt_u64(a.absorbed_in));
    Value::Object(map)
}

/// A piece of evidence for the export: its redacted excerpt only.
pub(crate) fn evidence_to_value(e: &Evidence) -> Value {
    json!({
        "id": e.id.to_string(),
        "kind": e.kind.word(),
        "created_ms": e.created_ms,
        "terminal": e.terminal,
        "tool_session": e.tool_session,
        "turn": e.turn,
        "branch": e.branch,
        "head": e.head,
        "log": e.log.as_ref().map(|l| json!({ "path": l.path, "offset": l.offset })),
        "files": anchors_to_value(&e.files),
        "excerpt": e.excerpt.get(),
        "exposed_to": e.exposed_to.iter().map(ToString::to_string).collect::<Vec<_>>(),
    })
}

/// Build an [`Evidence`] from its row's columns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evidence_from_columns(
    project_id: String,
    seq: i64,
    kind: &str,
    created_ms: i64,
    terminal: Option<String>,
    tool_session: Option<String>,
    turn: Option<String>,
    branch: Option<String>,
    head: Option<String>,
    log: Option<(String, i64)>,
    files: &str,
    excerpt: String,
    exposed_to: &str,
) -> Result<Evidence> {
    let files: Value = serde_json::from_str(files).map_err(|_| StoreError::Corrupt("files"))?;
    Ok(Evidence {
        id: EvidenceId::new(to_u64(seq)),
        kind: word(EvidenceKind::from_word(kind), "evidence kind")?,
        project_id,
        created_ms: to_u64(created_ms),
        terminal,
        tool_session,
        turn,
        branch,
        head,
        log: log.map(|(path, offset)| LogPointer {
            path,
            offset: to_u64(offset),
        }),
        files: anchors_from_value(&files)?,
        excerpt: Redacted::assume(excerpt),
        exposed_to: subjects_from_json(exposed_to)?,
    })
}

/// Build an [`Annotation`] from its row's columns (sources added by the
/// caller).
#[allow(clippy::too_many_arguments)]
pub(crate) fn annotation_from_columns(
    id: &str,
    seq: i64,
    created_ms: i64,
    kind: &str,
    target_memory: Option<String>,
    new_room: Option<String>,
    text: String,
    scopes: &str,
    verification: &str,
    origin: &str,
    memory_kind: Option<String>,
    standing: Option<String>,
    absorbed_in: Option<i64>,
) -> Result<Annotation> {
    let bad = |_| StoreError::Corrupt("annotation");
    let target = match (target_memory, new_room) {
        (Some(id), _) => AnnotationTarget::Memory(MemoryId::parse(&id).map_err(bad)?),
        (None, Some(room)) => AnnotationTarget::New {
            room: word(Room::from_word(&room), "room")?,
        },
        (None, None) => return Err(StoreError::Corrupt("annotation target")),
    };
    Ok(Annotation {
        id: connecto_core::memory::AnnotationId::parse(id).map_err(bad)?,
        seq: to_u64(seq),
        created_ms: to_u64(created_ms),
        kind: word(
            connecto_core::memory::AnnotationKind::from_word(kind),
            "annotation kind",
        )?,
        target,
        text,
        scopes: scopes_from_json(scopes)?,
        sources: Vec::new(),
        verification: word(Verification::from_word(verification), "verification")?,
        origin: AnnotationOrigin::parse(origin).map_err(bad)?,
        memory_kind: match memory_kind {
            Some(k) => Some(word(MemoryKind::from_word(&k), "memory kind")?),
            None => None,
        },
        standing: match standing {
            Some(s) => Some(word(Standing::from_word(&s), "standing")?),
            None => None,
        },
        absorbed_in: absorbed_in.map(to_u64),
    })
}
