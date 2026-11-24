//! Reads. [`Read`] works over any connection: the writer's own, or a
//! [`Reader`]'s read-only one on a bridge or UI-worker thread. A [`Reader`]
//! answers inside one read transaction ([`Reader::snapshot`]), so a read made
//! of several queries sees a single version even while the writer publishes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use connecto_core::memory::{
    Address, Annotation, AnnotationId, AnnotationKind, Cycle, Evidence, EvidenceId, EvidenceKind,
    Freshness, Lookup, Memory, MemoryCard, MemoryDetail, MemoryId, MemoryKind, MemoryStatus,
    ReadAnswer, Relation, RelationKind, Room, RoomCount, Scope, ScopeEntry, SearchHit, SourceRef,
    Standing, StatusCounts, Subject, VersionEntry,
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::codec::{
    MemoryRow, anchors_from_value, annotation_from_columns, evidence_from_columns, to_i64, to_u64,
    word,
};
use crate::error::{Result, StoreError};
use crate::schema;

/// A memory found by id or address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Memory {
        memory: Memory,
        /// The address asked for, when it was an old one (a redirect) or the
        /// id of a memory merged into this one.
        redirected_from: Option<Address>,
    },
    Annotation(Annotation),
}

/// What `search` looks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub room: Option<Room>,
    /// Memories with this scope, or with `project` scope.
    pub scope: Option<Scope>,
    /// Include the archive and absorbed annotations.
    pub all: bool,
    pub limit: usize,
}

impl SearchQuery {
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            room: None,
            scope: None,
            all: false,
            limit: 50,
        }
    }
}

/// Counts for `status` and the UI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Counts {
    /// Every room that holds memories, in display order, archive included.
    pub rooms: Vec<RoomCount>,
    /// Over every room.
    pub totals: StatusCounts,
    pub open_annotations: u32,
    pub open_contradictions: u32,
}

/// Reads over one connection.
#[derive(Clone, Copy)]
pub struct Read<'c> {
    conn: &'c Connection,
}

const MEMORY_COLUMNS: &str = "id, room, slug, kind, standing, summary, details, status,
    last_confirmed_ms, anchors_changed, created_ms, updated_ms, version, live, merged_into";

/// `merged`: memory `?1` and every memory merged into it, transitively.
/// Annotations keep the target they were written for; a merge is followed
/// when reading, so a later annotation is never rewritten by a publication.
const MERGED_INTO: &str = "WITH RECURSIVE merged(id) AS (
    SELECT ?1 UNION SELECT m.id FROM memory m JOIN merged ON m.merged_into = merged.id)";

/// `final_room(start, room)`: for every memory, the room of the live memory
/// it is or was merged into.
/// The walk is complete, however long the chain: `UNION` drops repeated
/// (start, cur) pairs, so even a cycle ends.
const FINAL_ROOM: &str = "WITH RECURSIVE follow(start, cur) AS (
    SELECT id, id FROM memory
    UNION
    SELECT f.start, m.merged_into FROM follow f JOIN memory m ON m.id = f.cur
    WHERE m.live = 0 AND m.merged_into IS NOT NULL),
  final_room(start, room) AS (
    SELECT f.start, m.room FROM follow f JOIN memory m ON m.id = f.cur WHERE m.live = 1)";

const ANNOTATION_COLUMNS: &str = "id, seq, created_ms, kind, target_memory, new_room, text,
    scopes, verification, origin, memory_kind, standing, absorbed_in";

const EVIDENCE_COLUMNS: &str = "project_id, seq, kind, created_ms, terminal, tool_session, turn,
    branch, head, log_path, log_offset, files, excerpt, exposed_to";

fn annotation_row(row: &Row<'_>) -> rusqlite::Result<Result<Annotation>> {
    Ok(annotation_from_columns(
        &row.get::<_, String>(0)?,
        row.get(1)?,
        row.get(2)?,
        &row.get::<_, String>(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        &row.get::<_, String>(7)?,
        &row.get::<_, String>(8)?,
        &row.get::<_, String>(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
    ))
}

fn evidence_row(row: &Row<'_>) -> rusqlite::Result<Result<Evidence>> {
    let log = match (
        row.get::<_, Option<String>>(9)?,
        row.get::<_, Option<i64>>(10)?,
    ) {
        (Some(path), Some(offset)) => Some((path, offset)),
        _ => None,
    };
    Ok(evidence_from_columns(
        row.get(0)?,
        row.get(1)?,
        &row.get::<_, String>(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        log,
        &row.get::<_, String>(11)?,
        row.get(12)?,
        &row.get::<_, String>(13)?,
    ))
}

/// Run `sql` and collect rows whose mapping can itself fail.
fn collect<T, P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<Result<T>>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, map)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// Contradictions first, then by sequence.
fn contradictions_first(annotations: &mut [Annotation]) {
    annotations.sort_by_key(|a| (a.kind != AnnotationKind::Contradiction, a.seq));
}

/// An FTS5 query matching any word of `text`, each quoted so nothing in it
/// is read as query syntax.
fn fts_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(32)
        .map(|w| format!("\"{w}\""))
        .collect();
    (!words.is_empty()).then(|| words.join(" OR "))
}

impl<'c> Read<'c> {
    #[must_use]
    pub const fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    #[must_use]
    pub const fn connection(&self) -> &'c Connection {
        self.conn
    }

    pub fn project_id(&self) -> Result<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = 'project_id'", [], |r| {
                r.get(0)
            })
            .map_err(Into::into)
    }

    /// The current memory version (0 before the first publication).
    pub fn version(&self) -> Result<u64> {
        schema::meta_u64(self.conn, "memory_version")
    }

    /// The highest annotation `seq` so far: a dream's cutoff.
    pub fn annotation_seq(&self) -> Result<u64> {
        schema::meta_u64(self.conn, "annotation_seq")
    }

    /// The highest event (and evidence) `seq` so far.
    pub fn event_seq(&self) -> Result<u64> {
        schema::meta_u64(self.conn, "event_seq")
    }

    /// A memory row in any state (live, merged, withdrawn), without the open
    /// annotation count.
    pub fn memory_row(&self, id: &MemoryId) -> Result<Option<MemoryRow>> {
        let sql = format!("SELECT {MEMORY_COLUMNS} FROM memory WHERE id = ?1");
        let head = self
            .conn
            .prepare(&sql)?
            .query_row([id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, bool>(9)?,
                    (
                        row.get::<_, i64>(10)?,
                        row.get::<_, i64>(11)?,
                        row.get::<_, i64>(12)?,
                    ),
                    row.get::<_, bool>(13)?,
                    row.get::<_, Option<String>>(14)?,
                ))
            })
            .optional()?;
        let Some((
            room,
            slug,
            kind,
            standing,
            summary,
            details,
            status,
            confirmed,
            anchors_changed,
            (created, updated, version),
            live,
            merged,
        )) = head
        else {
            return Ok(None);
        };
        let room = word(Room::from_word(&room), "room")?;
        let address = Address::new(room, &slug).map_err(|_| StoreError::Corrupt("address"))?;
        let scopes = collect(
            self.conn,
            "SELECT scope, gone FROM memory_scope WHERE memory_id = ?1 ORDER BY scope",
            [id.as_str()],
            |row| {
                let scope: String = row.get(0)?;
                let gone: bool = row.get(1)?;
                Ok(Scope::parse(&scope)
                    .map(|scope| ScopeEntry { scope, gone })
                    .map_err(|_| StoreError::Corrupt("scope")))
            },
        )?;
        let anchors = collect(
            self.conn,
            "SELECT path, blob FROM memory_anchor WHERE memory_id = ?1 ORDER BY path",
            [id.as_str()],
            |row| {
                let value = serde_json::json!([{ "path": row.get::<_, String>(0)?, "blob": row.get::<_, String>(1)? }]);
                Ok(anchors_from_value(&value)
                    .and_then(|mut a| a.pop().ok_or(StoreError::Corrupt("anchor"))))
            },
        )?;
        let relations = collect(
            self.conn,
            "SELECT kind, to_id FROM relation WHERE from_id = ?1 ORDER BY kind, to_id",
            [id.as_str()],
            |row| {
                let kind: String = row.get(0)?;
                let target: String = row.get(1)?;
                Ok((|| {
                    Ok(Relation {
                        kind: word(RelationKind::from_word(&kind), "relation")?,
                        target: MemoryId::parse(&target)
                            .map_err(|_| StoreError::Corrupt("relation"))?,
                    })
                })())
            },
        )?;
        let merged_into = match merged {
            Some(m) => Some(MemoryId::parse(&m).map_err(|_| StoreError::Corrupt("merged"))?),
            None => None,
        };
        Ok(Some(MemoryRow {
            memory: Memory {
                id: id.clone(),
                address,
                kind: word(MemoryKind::from_word(&kind), "memory kind")?,
                standing: word(Standing::from_word(&standing), "standing")?,
                summary,
                details,
                scopes,
                status: word(MemoryStatus::from_word(&status), "status")?,
                freshness: Freshness {
                    last_confirmed_ms: confirmed.map(to_u64),
                    anchors_changed,
                    open_annotations: 0,
                },
                anchors,
                sources: self.sources(&Subject::Memory(id.clone()))?,
                relations,
                created_ms: to_u64(created),
                updated_ms: to_u64(updated),
                version: to_u64(version),
            },
            live,
            merged_into,
        }))
    }

    /// A live memory, with its open annotation count.
    pub fn memory(&self, id: &MemoryId) -> Result<Option<Memory>> {
        let Some(row) = self.memory_row(id)? else {
            return Ok(None);
        };
        if !row.live {
            return Ok(None);
        }
        let mut memory = row.memory;
        memory.freshness.open_annotations = self.open_count(&memory.id)?;
        Ok(Some(memory))
    }

    fn open_count(&self, id: &MemoryId) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            &format!(
                "{MERGED_INTO} SELECT count(*) FROM annotation
                 WHERE target_memory IN merged AND absorbed_in IS NULL"
            ),
            [id.as_str()],
            |r| r.get(0),
        )?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// The live memory at `address`, or the one an old address leads to.
    pub fn memory_at(&self, address: &Address) -> Result<Option<Found>> {
        let live: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM memory WHERE room = ?1 AND slug = ?2 AND live = 1",
                params![address.room().word(), address.slug()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = live {
            let id = MemoryId::parse(&id).map_err(|_| StoreError::Corrupt("memory id"))?;
            return Ok(self.memory(&id)?.map(|memory| Found::Memory {
                memory,
                redirected_from: None,
            }));
        }
        let target: Option<String> = self
            .conn
            .query_row(
                "SELECT memory_id FROM address_redirect WHERE address = ?1",
                [address.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(target) = target else {
            return Ok(None);
        };
        let target = MemoryId::parse(&target).map_err(|_| StoreError::Corrupt("redirect"))?;
        Ok(self.follow(&target)?.map(|memory| Found::Memory {
            memory,
            redirected_from: Some(address.clone()),
        }))
    }

    /// The end of `id`'s merge chain: `id` itself unless it was merged,
    /// else the memory it was merged into, followed to the end however long
    /// the chain. The end is live, withdrawn, or unknown. A cycle (which no
    /// write path makes) fails closed as `Corrupt`, never a partial answer.
    pub fn merge_end(&self, id: &MemoryId) -> Result<MemoryId> {
        let mut seen: BTreeSet<MemoryId> = BTreeSet::new();
        let mut id = id.clone();
        loop {
            if !seen.insert(id.clone()) {
                return Err(StoreError::Corrupt("merge cycle"));
            }
            let next: Option<(bool, Option<String>)> = self
                .conn
                .query_row(
                    "SELECT live, merged_into FROM memory WHERE id = ?1",
                    [id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match next {
                Some((false, Some(next))) => {
                    id = MemoryId::parse(&next).map_err(|_| StoreError::Corrupt("merged"))?;
                }
                _ => return Ok(id),
            }
        }
    }

    /// The live memory `id` is, or was merged into.
    fn follow(&self, id: &MemoryId) -> Result<Option<Memory>> {
        self.memory(&self.merge_end(id)?)
    }

    /// What `read` and the UI open: an id or an address, following redirects
    /// and merges.
    pub fn resolve(&self, lookup: &Lookup) -> Result<Option<Found>> {
        match lookup {
            Lookup::Address(address) => self.memory_at(address),
            Lookup::Annotation(id) => Ok(self.annotation(id)?.map(Found::Annotation)),
            Lookup::Memory(id) => {
                let Some(row) = self.memory_row(id)? else {
                    return Ok(None);
                };
                let from = (!row.live).then(|| row.memory.address.clone());
                Ok(self.follow(id)?.map(|memory| Found::Memory {
                    memory,
                    redirected_from: from,
                }))
            }
        }
    }

    /// The full answer for `read`: the memory, every annotation on it (open
    /// ones first, contradictions first among them) and its evidence.
    pub fn detail(&self, lookup: &Lookup) -> Result<ReadAnswer> {
        match self.resolve(lookup)? {
            None => Ok(ReadAnswer::NotFound),
            Some(Found::Annotation(a)) => Ok(ReadAnswer::Annotation(Box::new(a))),
            Some(Found::Memory {
                memory,
                redirected_from,
            }) => {
                let mut annotations = self.annotations_on(&memory.id, true)?;
                let mut closed: Vec<_> = self
                    .annotations_on(&memory.id, false)?
                    .into_iter()
                    .filter(|a| !a.is_open())
                    .collect();
                closed.sort_by_key(|a| a.seq);
                annotations.extend(closed);
                let evidence = self.evidence_for(&Subject::Memory(memory.id.clone()))?;
                let superseded_by = self.superseded_by(&memory.id)?;
                Ok(ReadAnswer::Memory(Box::new(MemoryDetail {
                    memory,
                    annotations,
                    evidence,
                    redirected_from,
                    superseded_by,
                })))
            }
        }
    }

    /// The live memories that replace `id` (see
    /// [`MemoryDetail::superseded_by`]): those with a `sostituisce`
    /// relation to it, each followed through merges; sorted, once each.
    pub fn superseded_by(&self, id: &MemoryId) -> Result<Vec<MemoryId>> {
        let from: Vec<MemoryId> = collect(
            self.conn,
            "SELECT from_id FROM relation WHERE to_id = ?1 AND kind = 'sostituisce'",
            [id.as_str()],
            |row| {
                let from: String = row.get(0)?;
                Ok(MemoryId::parse(&from).map_err(|_| StoreError::Corrupt("memory id")))
            },
        )?;
        let mut out = BTreeSet::new();
        for m in from {
            let end = self.merge_end(&m)?;
            if &end != id && self.memory(&end)?.is_some() {
                out.insert(end);
            }
        }
        Ok(out.into_iter().collect())
    }

    /// The card shown for a memory in brief, search and room: summary, status
    /// and freshness apart, sources by kind, open annotations beside it with
    /// contradictions first.
    pub fn card(&self, memory: &Memory) -> Result<MemoryCard> {
        let mut by_kind: BTreeMap<EvidenceKind, u32> = BTreeMap::new();
        for source in &memory.sources {
            *by_kind.entry(source.kind).or_default() += 1;
        }
        Ok(MemoryCard {
            id: memory.id.clone(),
            address: memory.address.clone(),
            kind: memory.kind,
            standing: memory.standing,
            summary: memory.summary.clone(),
            status: memory.status,
            freshness: memory.freshness,
            scopes: memory.scopes.clone(),
            sources: by_kind.into_iter().collect(),
            open: self.annotations_on(&memory.id, true)?,
        })
    }

    /// The live memory ids of a room, by address.
    fn room_ids(&self, room: Room) -> Result<Vec<MemoryId>> {
        collect(
            self.conn,
            "SELECT id FROM memory WHERE room = ?1 AND live = 1 ORDER BY slug",
            [room.word()],
            |row| {
                let id: String = row.get(0)?;
                Ok(MemoryId::parse(&id).map_err(|_| StoreError::Corrupt("memory id")))
            },
        )
    }

    /// A room's index (the entrance holds nothing: it is generated).
    pub fn room(&self, room: Room) -> Result<Vec<MemoryCard>> {
        let mut cards = Vec::new();
        for id in self.room_ids(room)? {
            if let Some(memory) = self.memory(&id)? {
                cards.push(self.card(&memory)?);
            }
        }
        Ok(cards)
    }

    /// Every live memory, by room and address.
    pub fn memories(&self) -> Result<Vec<Memory>> {
        let mut out = Vec::new();
        for room in Room::ALL {
            for id in self.room_ids(*room)? {
                if let Some(memory) = self.memory(&id)? {
                    out.push(memory);
                }
            }
        }
        Ok(out)
    }

    /// Every memory row, live or not, by rowid.
    pub fn memory_rows(&self) -> Result<Vec<MemoryRow>> {
        let ids = collect(
            self.conn,
            "SELECT id FROM memory ORDER BY rowid",
            [],
            |row| {
                let id: String = row.get(0)?;
                Ok(MemoryId::parse(&id).map_err(|_| StoreError::Corrupt("memory id")))
            },
        )?;
        let mut out = Vec::new();
        for id in ids {
            if let Some(row) = self.memory_row(&id)? {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Text search over live memories and annotations, best first. The
    /// archive and absorbed annotations only with `all`.
    pub fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let Some(fts) = fts_query(&query.text) else {
            return Ok(Vec::new());
        };
        let room = query.room.map(Room::word);
        let scope = query.scope.as_ref().map(ToString::to_string);
        let limit = to_i64(query.limit as u64);
        let memory_hits = collect(
            self.conn,
            "SELECT m.id, bm25(memory_fts, 2.0, 4.0, 1.0) AS score
             FROM memory_fts JOIN memory m ON m.rowid = memory_fts.rowid
             WHERE memory_fts MATCH ?1 AND m.live = 1
               AND (?2 IS NULL OR m.room = ?2)
               AND (?3 OR m.room <> 'archivio' OR ?2 = 'archivio')
               AND (?4 IS NULL OR EXISTS (SELECT 1 FROM memory_scope s
                    WHERE s.memory_id = m.id AND (s.scope = ?4 OR s.scope = 'project')))
             ORDER BY score LIMIT ?5",
            params![fts, room, query.all, scope, limit],
            |row| {
                let id: String = row.get(0)?;
                let score: f64 = row.get(1)?;
                Ok(MemoryId::parse(&id)
                    .map(|id| (score, id))
                    .map_err(|_| StoreError::Corrupt("memory id")))
            },
        )?;
        let annotation_hits = collect(
            self.conn,
            &format!(
                "{FINAL_ROOM}
             SELECT a.id, bm25(annotation_fts) AS score
             FROM annotation_fts JOIN annotation a ON a.rowid = annotation_fts.rowid
             LEFT JOIN final_room t ON t.start = a.target_memory
             WHERE annotation_fts MATCH ?1
               AND (?3 OR a.absorbed_in IS NULL)
               AND (?2 IS NULL OR coalesce(t.room, a.new_room) = ?2)
               AND (?3 OR ?2 = 'archivio' OR coalesce(t.room, a.new_room, '') <> 'archivio')
               AND (?4 IS NULL OR a.scopes LIKE '%\"project\"%' OR instr(a.scopes, json_quote(?4)) > 0)
             ORDER BY score LIMIT ?5"
            ),
            params![fts, room, query.all, scope, limit],
            |row| {
                let id: String = row.get(0)?;
                let score: f64 = row.get(1)?;
                Ok(AnnotationId::parse(&id)
                    .map(|id| (score, id))
                    .map_err(|_| StoreError::Corrupt("annotation id")))
            },
        )?;
        let mut ranked: Vec<(f64, Subject)> = memory_hits
            .into_iter()
            .map(|(s, id)| (s, Subject::Memory(id)))
            .chain(
                annotation_hits
                    .into_iter()
                    .map(|(s, id)| (s, Subject::Annotation(id))),
            )
            .collect();
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut hits = Vec::new();
        for (_, subject) in ranked.into_iter().take(query.limit) {
            match subject {
                Subject::Memory(id) => {
                    if let Some(memory) = self.memory(&id)? {
                        hits.push(SearchHit::Memory(self.card(&memory)?));
                    }
                }
                Subject::Annotation(id) => {
                    if let Some(a) = self.annotation(&id)? {
                        hits.push(SearchHit::Annotation(a));
                    }
                }
            }
        }
        Ok(hits)
    }

    /// The sources of a memory or annotation, with their kinds.
    pub fn sources(&self, subject: &Subject) -> Result<Vec<SourceRef>> {
        collect(
            self.conn,
            "SELECT e.seq, e.kind FROM source_link l JOIN evidence e ON e.seq = l.evidence_seq
             WHERE l.subject = ?1 ORDER BY e.seq",
            [subject.to_string()],
            |row| {
                let seq: i64 = row.get(0)?;
                let kind: String = row.get(1)?;
                Ok(
                    word(EvidenceKind::from_word(&kind), "evidence kind").map(|kind| SourceRef {
                        evidence: EvidenceId::new(to_u64(seq)),
                        kind,
                    }),
                )
            },
        )
    }

    pub fn annotation(&self, id: &AnnotationId) -> Result<Option<Annotation>> {
        let sql = format!("SELECT {ANNOTATION_COLUMNS} FROM annotation WHERE id = ?1");
        let found = collect(self.conn, &sql, [id.as_str()], annotation_row)?;
        match found.into_iter().next() {
            Some(mut a) => {
                a.sources = self.sources(&Subject::Annotation(a.id.clone()))?;
                Ok(Some(a))
            }
            None => Ok(None),
        }
    }

    fn with_sources(&self, mut list: Vec<Annotation>) -> Result<Vec<Annotation>> {
        for a in &mut list {
            a.sources = self.sources(&Subject::Annotation(a.id.clone()))?;
        }
        Ok(list)
    }

    /// Annotations about a memory: open ones only, or all; contradictions
    /// first, then by sequence.
    pub fn annotations_on(&self, id: &MemoryId, open_only: bool) -> Result<Vec<Annotation>> {
        let sql = format!(
            "{MERGED_INTO} SELECT {ANNOTATION_COLUMNS} FROM annotation
             WHERE target_memory IN merged AND (?2 = 0 OR absorbed_in IS NULL) ORDER BY seq"
        );
        let mut list = self.with_sources(collect(
            self.conn,
            &sql,
            params![id.as_str(), open_only],
            annotation_row,
        )?)?;
        contradictions_first(&mut list);
        Ok(list)
    }

    /// Every open annotation, by sequence.
    pub fn open_annotations(&self) -> Result<Vec<Annotation>> {
        let sql = format!(
            "SELECT {ANNOTATION_COLUMNS} FROM annotation WHERE absorbed_in IS NULL ORDER BY seq"
        );
        self.with_sources(collect(self.conn, &sql, [], annotation_row)?)
    }

    /// Open annotations with `seq` up to `cutoff`: what a dream may absorb.
    pub fn open_annotations_through(&self, cutoff: u64) -> Result<Vec<Annotation>> {
        let sql = format!(
            "SELECT {ANNOTATION_COLUMNS} FROM annotation
             WHERE absorbed_in IS NULL AND seq <= ?1 ORDER BY seq"
        );
        self.with_sources(collect(self.conn, &sql, [to_i64(cutoff)], annotation_row)?)
    }

    /// Every annotation, by sequence.
    pub fn annotations(&self) -> Result<Vec<Annotation>> {
        let sql = format!("SELECT {ANNOTATION_COLUMNS} FROM annotation ORDER BY seq");
        self.with_sources(collect(self.conn, &sql, [], annotation_row)?)
    }

    pub fn evidence(&self, id: EvidenceId) -> Result<Option<Evidence>> {
        let sql = format!("SELECT {EVIDENCE_COLUMNS} FROM evidence WHERE seq = ?1");
        Ok(collect(self.conn, &sql, [to_i64(id.seq())], evidence_row)?
            .into_iter()
            .next())
    }

    /// The evidence a memory or annotation lists as its sources.
    pub fn evidence_for(&self, subject: &Subject) -> Result<Vec<Evidence>> {
        let sql = format!(
            "SELECT {EVIDENCE_COLUMNS} FROM evidence
             WHERE seq IN (SELECT evidence_seq FROM source_link WHERE subject = ?1) ORDER BY seq"
        );
        collect(self.conn, &sql, [subject.to_string()], evidence_row)
    }

    /// Every piece of evidence, by id.
    pub fn all_evidence(&self) -> Result<Vec<Evidence>> {
        let sql = format!("SELECT {EVIDENCE_COLUMNS} FROM evidence ORDER BY seq");
        collect(self.conn, &sql, [], evidence_row)
    }

    /// Every relation, as (from, relation).
    pub fn relations(&self) -> Result<Vec<(MemoryId, Relation)>> {
        collect(
            self.conn,
            "SELECT from_id, kind, to_id FROM relation ORDER BY from_id, kind, to_id",
            [],
            |row| {
                let from: String = row.get(0)?;
                let kind: String = row.get(1)?;
                let to: String = row.get(2)?;
                Ok((|| {
                    let bad = |_| StoreError::Corrupt("relation");
                    Ok((
                        MemoryId::parse(&from).map_err(bad)?,
                        Relation {
                            kind: word(RelationKind::from_word(&kind), "relation")?,
                            target: MemoryId::parse(&to).map_err(bad)?,
                        },
                    ))
                })())
            },
        )
    }

    /// Every redirect: an old address and the memory it leads to.
    pub fn redirects(&self) -> Result<Vec<(String, MemoryId)>> {
        collect(
            self.conn,
            "SELECT address, memory_id FROM address_redirect ORDER BY address",
            [],
            |row| {
                let address: String = row.get(0)?;
                let id: String = row.get(1)?;
                Ok(MemoryId::parse(&id)
                    .map(|id| (address, id))
                    .map_err(|_| StoreError::Corrupt("redirect")))
            },
        )
    }

    /// Counts per room and status, and open annotations.
    pub fn counts(&self) -> Result<Counts> {
        let mut per_room: BTreeMap<Room, StatusCounts> = BTreeMap::new();
        let rows = collect(
            self.conn,
            "SELECT room, status, count(*) FROM memory WHERE live = 1 GROUP BY room, status",
            [],
            |row| {
                let room: String = row.get(0)?;
                let status: String = row.get(1)?;
                let n: i64 = row.get(2)?;
                Ok((|| {
                    Ok((
                        word(Room::from_word(&room), "room")?,
                        word(MemoryStatus::from_word(&status), "status")?,
                        u32::try_from(n).unwrap_or(u32::MAX),
                    ))
                })())
            },
        )?;
        let mut totals = StatusCounts::default();
        for (room, status, n) in rows {
            per_room.entry(room).or_default().add(status, n);
            totals.add(status, n);
        }
        let (open, contradictions): (i64, i64) = self.conn.query_row(
            "SELECT count(*), coalesce(sum(kind = 'contraddizione'), 0)
             FROM annotation WHERE absorbed_in IS NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Counts {
            rooms: Room::ALL
                .iter()
                .filter(|r| r.holds_memories())
                .map(|room| RoomCount {
                    room: *room,
                    counts: per_room.get(room).copied().unwrap_or_default(),
                })
                .collect(),
            totals,
            open_annotations: u32::try_from(open).unwrap_or(u32::MAX),
            open_contradictions: u32::try_from(contradictions).unwrap_or(u32::MAX),
        })
    }

    /// The memories and annotations `connecto memory` showed a session up to
    /// `until_ms`. A session is matched by its tool session id; by terminal
    /// when either side has no tool session (which can only add exposure,
    /// never hide it).
    pub fn served_to(
        &self,
        tool_session: Option<&str>,
        terminal: Option<&str>,
        until_ms: u64,
    ) -> Result<Vec<Subject>> {
        collect(
            self.conn,
            "SELECT DISTINCT subject FROM served WHERE at_ms <= ?3 AND (
                (?1 IS NOT NULL AND tool_session = ?1)
                OR (?2 IS NOT NULL AND terminal = ?2 AND (?1 IS NULL OR tool_session IS NULL)))
             ORDER BY subject",
            params![tool_session, terminal, to_i64(until_ms)],
            |row| {
                let subject: String = row.get(0)?;
                Ok(Subject::parse(&subject).map_err(|_| StoreError::Corrupt("served subject")))
            },
        )
    }

    /// The lineage of a memory for the rule against self-confirmation: the
    /// memory, the annotations absorbed into it, the memories merged into it
    /// and those it derives from, transitively.
    pub fn lineage(&self, id: &MemoryId) -> Result<Vec<Subject>> {
        let mut seen: BTreeSet<Subject> = BTreeSet::new();
        let mut todo = vec![id.clone()];
        while let Some(next) = todo.pop() {
            if !seen.insert(Subject::Memory(next.clone())) {
                continue;
            }
            let absorbed = collect(
                self.conn,
                "SELECT id FROM annotation WHERE absorbed_into = ?1",
                [next.as_str()],
                |row| {
                    let id: String = row.get(0)?;
                    Ok(AnnotationId::parse(&id).map_err(|_| StoreError::Corrupt("annotation id")))
                },
            )?;
            seen.extend(absorbed.into_iter().map(Subject::Annotation));
            let parents = collect(
                self.conn,
                "SELECT id FROM memory WHERE merged_into = ?1
                 UNION SELECT to_id FROM relation WHERE from_id = ?1 AND kind = 'deriva_da'",
                [next.as_str()],
                |row| {
                    let id: String = row.get(0)?;
                    Ok(MemoryId::parse(&id).map_err(|_| StoreError::Corrupt("memory id")))
                },
            )?;
            todo.extend(parents);
        }
        Ok(seen.into_iter().collect())
    }

    /// Published versions, newest first.
    pub fn versions(&self, limit: usize) -> Result<Vec<VersionEntry>> {
        collect(
            self.conn,
            "SELECT version, published_ms, cycle, operations, reverts FROM publication
             ORDER BY version DESC LIMIT ?1",
            [to_i64(limit as u64)],
            |row| {
                let cycle: String = row.get(2)?;
                let version: i64 = row.get(0)?;
                let published: i64 = row.get(1)?;
                let operations: i64 = row.get(3)?;
                let reverts: Option<i64> = row.get(4)?;
                Ok(
                    word(Cycle::from_word(&cycle), "cycle").map(|cycle| VersionEntry {
                        version: to_u64(version),
                        published_ms: to_u64(published),
                        cycle,
                        operations: u32::try_from(operations).unwrap_or(u32::MAX),
                        reverts: reverts.map(to_u64),
                    }),
                )
            },
        )
    }
}

/// A read-only connection for a reader thread (bridge commands, UI reads).
pub struct Reader {
    conn: Connection,
}

impl Reader {
    /// Open the project's database read-only. Refuses a database of another
    /// project, of a newer build, or not yet upgraded by the writer.
    pub fn open(path: &Path, project_id: &str) -> Result<Self> {
        Ok(Self {
            conn: schema::open_read(path, project_id)?,
        })
    }

    /// A consistent view: every query made through it sees the same version.
    pub fn snapshot(&self) -> Result<Snapshot<'_>> {
        Ok(Snapshot {
            tx: Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?,
        })
    }

    /// Reads outside a transaction (each query sees the latest commit).
    #[must_use]
    pub const fn read(&self) -> Read<'_> {
        Read::new(&self.conn)
    }
}

/// One read transaction; ends when dropped.
pub struct Snapshot<'c> {
    tx: Transaction<'c>,
}

impl Snapshot<'_> {
    #[must_use]
    pub fn read(&self) -> Read<'_> {
        Read::new(&self.tx)
    }
}
