//! Atomic publication of memory changes, and its rollback (spec §5.5, D10).
//!
//! A [`Publication`] is one transaction: the previous image of every subject
//! it touches goes to `revision`, the operations apply, the annotations named
//! (and only those with `seq` up to the cutoff) are marked absorbed, the
//! invariants are checked on the final state, and the memory version goes up
//! by one. Any failure leaves the database at the previous version.
//!
//! Nothing is deleted on this path: a merged memory stays as a row that
//! points at the one it was merged into, a moved or archived one leaves a
//! redirect from its old address. [`Store::revert`] restores the images of a
//! version as a new version and reopens the annotations it absorbed.

use std::collections::{BTreeMap, BTreeSet};

use connecto_core::Redacted;
use connecto_core::memory::{
    Address, Anchor, AnnotationId, Cycle, EvidenceId, Freshness, Memory, MemoryId, MemoryKind,
    MemoryStatus, Relation, RelationKind, Room, Scope, ScopeEntry, SourceRef, Standing, Subject,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::codec::{
    AnnotationState, MemoryRow, annotation_state_from_value, annotation_state_to_value,
    memory_from_value, memory_to_value, to_i64, to_u64,
};
use crate::error::{Result, StoreError};
use crate::read::Read;
use crate::schema;
use crate::store::{
    Store, Unsupported, fresh_id, id_used, load_evidence, unsupported, verify_round_trip,
    write_memory_core, write_memory_row, write_relations,
};

/// A new memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMemory {
    /// `None`: the store makes one. A given id must never have been used.
    pub id: Option<MemoryId>,
    pub address: Address,
    pub kind: MemoryKind,
    /// `None`: the weakest standing of the kind.
    pub standing: Option<Standing>,
    pub summary: Redacted<String>,
    pub details: Redacted<String>,
    pub scopes: Vec<Scope>,
    pub status: MemoryStatus,
    pub anchors: Vec<Anchor>,
    /// At least one.
    pub sources: Vec<EvidenceId>,
    pub relations: Vec<Relation>,
}

/// Changes to a memory's content; `None` leaves a field as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryUpdate {
    pub id: MemoryId,
    pub summary: Option<Redacted<String>>,
    pub details: Option<Redacted<String>>,
    pub scopes: Option<Vec<Scope>>,
    pub anchors: Option<Vec<Anchor>>,
    pub standing: Option<Standing>,
    pub add_sources: Vec<EvidenceId>,
}

impl MemoryUpdate {
    #[must_use]
    pub const fn new(id: MemoryId) -> Self {
        Self {
            id,
            summary: None,
            details: None,
            scopes: None,
            anchors: None,
            standing: None,
            add_sources: Vec::new(),
        }
    }
}

/// One change to the memories (spec §5.5). There is no delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryOp {
    Create(NewMemory),
    Update(MemoryUpdate),
    /// `from` become part of `into`: their sources join it, their open
    /// annotations move to it, their addresses redirect to it.
    Merge {
        into: MemoryId,
        from: Vec<MemoryId>,
    },
    /// `old` becomes `superato`; `by` gains a `sostituisce` relation to it.
    Supersede {
        old: MemoryId,
        by: MemoryId,
    },
    /// A new status, with the evidence that justifies it (added as sources).
    SetStatus {
        id: MemoryId,
        status: MemoryStatus,
        sources: Vec<EvidenceId>,
    },
    /// A new address; the old one redirects.
    Move {
        id: MemoryId,
        to: Address,
    },
    /// Into the archive, same slug when free; the old address redirects.
    Archive {
        id: MemoryId,
    },
    Link {
        from: MemoryId,
        kind: RelationKind,
        to: MemoryId,
    },
}

/// An annotation a publication absorbs, and the memory it went into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Absorb {
    pub annotation: AnnotationId,
    pub into: Option<MemoryId>,
}

/// One atomic publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    /// Must be the store's project.
    pub project_id: String,
    pub cycle: Cycle,
    /// When set, the memory must still be at this version.
    pub base_version: Option<u64>,
    /// The highest annotation `seq` this publication may absorb.
    pub cutoff: u64,
    pub ops: Vec<MemoryOp>,
    pub absorb: Vec<Absorb>,
    pub run_id: Option<u64>,
    pub now_ms: u64,
}

/// Why a named annotation was left open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Its `seq` is past the cutoff: it is for the next dream.
    AfterCutoff,
    AlreadyAbsorbed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub version: u64,
    pub created: Vec<MemoryId>,
    /// Every memory the version changed, created ones included.
    pub changed: Vec<MemoryId>,
    pub absorbed: Vec<AnnotationId>,
    pub skipped: Vec<(AnnotationId, SkipReason)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reverted {
    /// The new version.
    pub version: u64,
    pub reverted: u64,
    pub reopened: Vec<AnnotationId>,
    /// Claims the restored state no longer supports, lowered in the same
    /// version: a memory from `verificato` to `da_verificare` and/or to its
    /// kind's weakest standing; an open annotation to `non_verificata`
    /// and/or no standing. Reverting this version restores them.
    pub lowered: Vec<Subject>,
}

/// Points inside a publication's transaction, for tests that stop it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStep {
    Begin,
    /// Before the operation with this index.
    Op(usize),
    Write,
    Absorb,
    Check,
    Version,
    Commit,
}

impl PublishStep {
    /// Every step of a publication with `ops` operations, in order.
    #[must_use]
    pub fn all(ops: usize) -> Vec<Self> {
        let mut steps = vec![Self::Begin];
        steps.extend((0..ops).map(Self::Op));
        steps.extend([
            Self::Write,
            Self::Absorb,
            Self::Check,
            Self::Version,
            Self::Commit,
        ]);
        steps
    }
}

fn scope_entries(scopes: &[Scope], before: &[ScopeEntry]) -> Vec<ScopeEntry> {
    let mut out: Vec<ScopeEntry> = Vec::new();
    for scope in scopes {
        if out.iter().any(|e| &e.scope == scope) {
            continue;
        }
        let gone = before.iter().any(|e| &e.scope == scope && e.gone);
        out.push(ScopeEntry {
            scope: scope.clone(),
            gone,
        });
    }
    out
}

fn add_sources(memory: &mut Memory, conn: &Connection, ids: &[EvidenceId]) -> Result<()> {
    for e in load_evidence(conn, ids)? {
        let source = SourceRef {
            evidence: e.id,
            kind: e.kind,
        };
        if !memory.sources.contains(&source) {
            memory.sources.push(source);
        }
    }
    memory.sources.sort();
    Ok(())
}

/// Annotations absorbed, and those named but left open.
type Absorbed = (Vec<AnnotationId>, Vec<(AnnotationId, SkipReason)>);

/// The memories one publication works on, each loaded once with its
/// previous image saved to `revision` before any change.
struct WorkingSet<'t> {
    tx: &'t Connection,
    project_id: String,
    version: u64,
    rows: BTreeMap<MemoryId, MemoryRow>,
    created: Vec<MemoryId>,
    saved: BTreeSet<String>,
    redirects: BTreeMap<String, MemoryId>,
}

impl<'t> WorkingSet<'t> {
    fn save_image(&mut self, subject: String, image: Option<Value>) -> Result<()> {
        if self.saved.insert(subject.clone()) {
            self.tx.execute(
                "INSERT INTO revision (project_id, version, subject, image) VALUES (?1, ?2, ?3, ?4)",
                params![
                    self.project_id,
                    to_i64(self.version),
                    subject,
                    image.map(|v| v.to_string())
                ],
            )?;
        }
        Ok(())
    }

    /// A live memory, for changing.
    fn live(&mut self, id: &MemoryId) -> Result<&mut MemoryRow> {
        if !self.rows.contains_key(id) {
            let row = Read::new(self.tx)
                .memory_row(id)?
                .ok_or_else(|| StoreError::UnknownMemory(id.clone()))?;
            self.save_image(id.to_string(), Some(memory_to_value(&row)))?;
            self.rows.insert(id.clone(), row);
        }
        match self.rows.get_mut(id) {
            Some(row) if row.live => Ok(row),
            _ => Err(StoreError::UnknownMemory(id.clone())),
        }
    }

    fn is_live(&mut self, id: &MemoryId) -> Result<()> {
        self.live(id).map(|_| ())
    }

    /// Whether a live memory other than `except` holds `address`, in the
    /// state this publication has reached.
    fn address_held(&self, address: &Address, except: Option<&MemoryId>) -> Result<bool> {
        let in_set = self
            .rows
            .values()
            .any(|r| r.live && &r.memory.address == address && Some(&r.memory.id) != except);
        if in_set {
            return Ok(true);
        }
        let holder: Option<String> = self
            .tx
            .query_row(
                "SELECT id FROM memory WHERE room = ?1 AND slug = ?2 AND live = 1",
                params![address.room().word(), address.slug()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(holder.is_some_and(|holder| {
            MemoryId::parse(&holder)
                .is_ok_and(|holder| Some(&holder) != except && !self.rows.contains_key(&holder))
        }))
    }

    fn redirect(&mut self, from: &Address, to: &MemoryId) -> Result<()> {
        let key = from.to_string();
        if !self.saved.contains(&format!("@{key}")) {
            let before: Option<String> = self
                .tx
                .query_row(
                    "SELECT memory_id FROM address_redirect WHERE address = ?1",
                    [&key],
                    |r| r.get(0),
                )
                .optional()?;
            self.save_image(format!("@{key}"), before.map(Value::from))?;
        }
        self.redirects.insert(key, to.clone());
        Ok(())
    }

    fn apply(&mut self, op: MemoryOp, now_ms: u64) -> Result<()> {
        match op {
            MemoryOp::Create(new) => self.create(new, now_ms),
            MemoryOp::Update(update) => {
                let tx = self.tx;
                let row = self.live(&update.id)?;
                let m = &mut row.memory;
                if let Some(summary) = update.summary {
                    m.summary = summary.get().clone();
                }
                if let Some(details) = update.details {
                    m.details = details.get().clone();
                }
                if let Some(scopes) = update.scopes {
                    m.scopes = scope_entries(&scopes, &m.scopes);
                }
                if let Some(anchors) = update.anchors {
                    m.anchors = anchors;
                }
                if let Some(standing) = update.standing {
                    m.standing = standing;
                }
                add_sources(m, tx, &update.add_sources)
            }
            MemoryOp::Merge { into, from } => {
                let distinct: BTreeSet<_> = from.iter().collect();
                if from.is_empty() || distinct.len() != from.len() || distinct.contains(&into) {
                    return Err(StoreError::BadMerge);
                }
                self.is_live(&into)?;
                let mut gained = Vec::new();
                for id in &from {
                    let row = self.live(id)?;
                    row.live = false;
                    row.merged_into = Some(into.clone());
                    gained.extend(row.memory.sources.iter().map(|s| s.evidence));
                    let address = row.memory.address.clone();
                    // Annotations on `id` keep their target (later ones must
                    // stay intact); reads follow the merge to `into`.
                    self.redirect(&address, &into)?;
                }
                let tx = self.tx;
                add_sources(&mut self.live(&into)?.memory, tx, &gained)
            }
            MemoryOp::Supersede { old, by } => {
                if old == by {
                    return Err(StoreError::Invalid(
                        connecto_core::memory::MemoryError::SelfRelation,
                    ));
                }
                self.live(&old)?.memory.status = MemoryStatus::Superseded;
                let relation = Relation {
                    kind: RelationKind::Supersedes,
                    target: old,
                };
                let by = &mut self.live(&by)?.memory;
                if !by.relations.contains(&relation) {
                    by.relations.push(relation);
                }
                Ok(())
            }
            MemoryOp::SetStatus {
                id,
                status,
                sources,
            } => {
                let tx = self.tx;
                let m = &mut self.live(&id)?.memory;
                m.status = status;
                add_sources(m, tx, &sources)
            }
            MemoryOp::Move { id, to } => self.move_to(&id, to),
            MemoryOp::Archive { id } => {
                let address = self.live(&id)?.memory.address.clone();
                if address.room() == Room::Archive {
                    return Ok(());
                }
                let to = self.free_address(Room::Archive, address.slug(), &id)?;
                self.move_to(&id, to)
            }
            MemoryOp::Link { from, kind, to } => {
                self.is_live(&to)?;
                let m = &mut self.live(&from)?.memory;
                let relation = Relation { kind, target: to };
                if !m.relations.contains(&relation) {
                    m.relations.push(relation);
                }
                Ok(())
            }
        }
    }

    fn create(&mut self, new: NewMemory, now_ms: u64) -> Result<()> {
        let id = match new.id {
            Some(id) => {
                if self.rows.contains_key(&id) || id_used(self.tx, "memory", id.as_str())? {
                    return Err(StoreError::IdTaken(id.to_string()));
                }
                id
            }
            None => loop {
                let id = fresh_id(
                    self.tx,
                    MemoryId::from_entropy,
                    ToString::to_string,
                    "memory",
                )?;
                if !self.rows.contains_key(&id) {
                    break id;
                }
            },
        };
        if self.address_held(&new.address, None)? {
            return Err(StoreError::AddressTaken(new.address.to_string()));
        }
        for relation in &new.relations {
            if relation.target != id {
                self.is_live(&relation.target)?;
            }
        }
        let mut memory = Memory {
            id: id.clone(),
            address: new.address,
            kind: new.kind,
            standing: new
                .standing
                .unwrap_or_else(|| Standing::default_for(new.kind)),
            summary: new.summary.get().clone(),
            details: new.details.get().clone(),
            scopes: scope_entries(&new.scopes, &[]),
            status: new.status,
            freshness: Freshness::default(),
            anchors: new.anchors,
            sources: Vec::new(),
            relations: new.relations,
            created_ms: now_ms,
            updated_ms: now_ms,
            version: self.version,
        };
        add_sources(&mut memory, self.tx, &new.sources)?;
        self.save_image(id.to_string(), None)?;
        self.created.push(id.clone());
        self.rows.insert(
            id,
            MemoryRow {
                memory,
                live: true,
                merged_into: None,
            },
        );
        Ok(())
    }

    fn move_to(&mut self, id: &MemoryId, to: Address) -> Result<()> {
        let from = self.live(id)?.memory.address.clone();
        if from == to {
            return Ok(());
        }
        if self.address_held(&to, Some(id))? {
            return Err(StoreError::AddressTaken(to.to_string()));
        }
        self.redirect(&from, id)?;
        self.live(id)?.memory.address = to;
        Ok(())
    }

    /// `room/slug`, or `room/slug-2`, `-3`... the first that is free.
    fn free_address(&self, room: Room, slug: &str, id: &MemoryId) -> Result<Address> {
        for n in 1..1000u32 {
            let candidate = if n == 1 {
                slug.to_owned()
            } else {
                let suffix = format!("-{n}");
                let keep = Address::MAX_SLUG.saturating_sub(suffix.len());
                let base = slug.get(..keep.min(slug.len())).unwrap_or(slug);
                format!("{}{suffix}", base.trim_end_matches('-'))
            };
            let address = Address::new(room, &candidate)?;
            if !self.address_held(&address, Some(id))? {
                return Ok(address);
            }
        }
        Err(StoreError::AddressTaken(format!("{}/{slug}", room.word())))
    }

    fn annotation_state(&self, id: &str) -> Result<AnnotationState> {
        let (target, absorbed_in, into): (Option<String>, Option<i64>, Option<String>) =
            self.tx.query_row(
                "SELECT target_memory, absorbed_in, absorbed_into FROM annotation WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
        let parse = |m: Option<String>| -> Result<Option<MemoryId>> {
            m.map(|m| MemoryId::parse(&m).map_err(|_| StoreError::Corrupt("annotation")))
                .transpose()
        };
        Ok(AnnotationState {
            target_memory: parse(target)?,
            absorbed_in: absorbed_in.map(to_u64),
            absorbed_into: parse(into)?,
        })
    }

    fn save_annotation(&mut self, id: &str) -> Result<AnnotationState> {
        let state = self.annotation_state(id)?;
        self.save_image(id.to_owned(), Some(annotation_state_to_value(&state)))?;
        Ok(state)
    }

    /// Write every changed row. Addresses are released first so two
    /// memories can trade places within one publication, and every row is
    /// in place before any relation, so a relation can name a memory created
    /// in the same publication.
    fn write(&mut self, now_ms: u64) -> Result<()> {
        for id in self.rows.keys() {
            self.tx
                .execute("UPDATE memory SET live = 0 WHERE id = ?1", [id.as_str()])?;
        }
        for row in self.rows.values_mut() {
            row.memory.version = self.version;
            row.memory.updated_ms = now_ms;
            write_memory_core(self.tx, &self.project_id, row)?;
        }
        for row in self.rows.values() {
            write_relations(self.tx, &self.project_id, row)?;
        }
        for (address, id) in &self.redirects {
            self.tx.execute(
                "INSERT INTO address_redirect (project_id, address, memory_id, version)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (project_id, address) DO UPDATE SET memory_id = excluded.memory_id,
                     version = excluded.version",
                params![self.project_id, address, id.as_str(), to_i64(self.version)],
            )?;
        }
        Ok(())
    }

    fn absorb(&mut self, absorb: Vec<Absorb>, cutoff: u64) -> Result<Absorbed> {
        let mut absorbed = Vec::new();
        let mut skipped = Vec::new();
        for Absorb { annotation, into } in absorb {
            let seq: Option<i64> = self
                .tx
                .query_row(
                    "SELECT seq FROM annotation WHERE id = ?1",
                    [annotation.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            let seq = seq.ok_or_else(|| StoreError::UnknownAnnotation(annotation.clone()))?;
            let state = self.annotation_state(annotation.as_str())?;
            if state.absorbed_in.is_some() {
                skipped.push((annotation, SkipReason::AlreadyAbsorbed));
                continue;
            }
            if to_u64(seq) > cutoff {
                skipped.push((annotation, SkipReason::AfterCutoff));
                continue;
            }
            if let Some(into) = &into {
                let live: Option<bool> = self
                    .tx
                    .query_row(
                        "SELECT live FROM memory WHERE id = ?1",
                        [into.as_str()],
                        |r| r.get(0),
                    )
                    .optional()?;
                if live != Some(true) {
                    return Err(StoreError::UnknownMemory(into.clone()));
                }
            }
            self.save_annotation(annotation.as_str())?;
            self.tx.execute(
                "UPDATE annotation SET absorbed_in = ?2, absorbed_into = ?3 WHERE id = ?1",
                params![
                    annotation.as_str(),
                    to_i64(self.version),
                    into.as_ref().map(MemoryId::as_str)
                ],
            )?;
            absorbed.push(annotation);
        }
        Ok((absorbed, skipped))
    }

    /// The invariants on the final state: every live memory touched is
    /// well formed (nested values included), and no claim anywhere in the
    /// store has lost its support, since absorbing, merging or deriving
    /// changes the lineage of memories this publication did not name.
    fn check(&self) -> Result<()> {
        // Every written row, live or merged: structure was checked by
        // `write_memory_core`; here each must also read back as written.
        for row in self.rows.values() {
            verify_round_trip(self.tx, row)?;
        }
        match unsupported(self.tx)?.into_iter().next() {
            Some(claim) => Err(claim.into_error()),
            None => Ok(()),
        }
    }
}

/// The `publication` row of a new version.
struct Record {
    version: u64,
    now_ms: u64,
    cycle: Cycle,
    operations: usize,
    reverts: Option<u64>,
    run_id: Option<u64>,
}

fn record_publication(tx: &Connection, project_id: &str, record: Record) -> Result<()> {
    let Record {
        version,
        now_ms,
        cycle,
        operations,
        reverts,
        run_id,
    } = record;
    tx.execute(
        "INSERT INTO publication (version, project_id, published_ms, cycle, operations, reverts, run_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            to_i64(version),
            project_id,
            to_i64(now_ms),
            cycle.word(),
            to_i64(operations as u64),
            reverts.map(to_i64),
            run_id.map(to_i64)
        ],
    )?;
    schema::set_meta_u64(tx, "memory_version", version)
}

/// The body of [`Store::publish_with`] on an open transaction (or a
/// savepoint inside one): every step up to the commit, which is the
/// caller's. A refusal leaves `conn` as the caller rolls it back.
pub(crate) fn publish_in(
    conn: &Connection,
    project_id: &str,
    publication: Publication,
    stop: &mut dyn FnMut(PublishStep) -> bool,
) -> Result<Published> {
    if publication.project_id != project_id {
        return Err(StoreError::WrongProject);
    }
    if publication.ops.is_empty() && publication.absorb.is_empty() {
        return Err(StoreError::EmptyPublication);
    }
    let project_id = project_id.to_owned();
    let halt = |stop: &mut dyn FnMut(PublishStep) -> bool, step| {
        if stop(step) {
            Err(StoreError::Aborted)
        } else {
            Ok(())
        }
    };
    halt(stop, PublishStep::Begin)?;
    let current = schema::meta_u64(conn, "memory_version")?;
    if let Some(expected) = publication.base_version
        && expected != current
    {
        return Err(StoreError::StaleBase {
            expected,
            found: current,
        });
    }
    let version = current.saturating_add(1);
    let operations = publication.ops.len() + publication.absorb.len();
    let mut set = WorkingSet {
        tx: conn,
        project_id: project_id.clone(),
        version,
        rows: BTreeMap::new(),
        created: Vec::new(),
        saved: BTreeSet::new(),
        redirects: BTreeMap::new(),
    };
    for (index, op) in publication.ops.into_iter().enumerate() {
        halt(stop, PublishStep::Op(index))?;
        set.apply(op, publication.now_ms)?;
    }
    halt(stop, PublishStep::Write)?;
    set.write(publication.now_ms)?;
    halt(stop, PublishStep::Absorb)?;
    let (absorbed, skipped) = set.absorb(publication.absorb, publication.cutoff)?;
    halt(stop, PublishStep::Check)?;
    set.check()?;
    halt(stop, PublishStep::Version)?;
    record_publication(
        conn,
        &project_id,
        Record {
            version,
            now_ms: publication.now_ms,
            cycle: publication.cycle,
            operations,
            reverts: None,
            run_id: publication.run_id,
        },
    )?;
    let published = Published {
        version,
        created: set.created.clone(),
        changed: set.rows.keys().cloned().collect(),
        absorbed,
        skipped,
    };
    halt(stop, PublishStep::Commit)?;
    Ok(published)
}

impl Store {
    /// Publish `publication` as one new memory version.
    pub fn publish(&mut self, publication: Publication) -> Result<Published> {
        self.publish_with(publication, &mut |_| false)
    }

    /// As [`Store::publish`], calling `stop` at every [`PublishStep`]; `true`
    /// abandons the transaction there. For crash tests.
    #[doc(hidden)]
    pub fn publish_with(
        &mut self,
        publication: Publication,
        stop: &mut dyn FnMut(PublishStep) -> bool,
    ) -> Result<Published> {
        self.publish_with_then(publication, stop, &mut |_, _| Ok(()))
    }

    /// [`Store::publish_with`], running `then` inside the same transaction
    /// once the publication is written: what `then` writes commits with the
    /// version or not at all. A failing `then` rolls everything back, and
    /// `stop` is asked again at [`PublishStep::Commit`] after `then`, right
    /// before the commit (a pause during `then` still aborts).
    pub fn publish_with_then(
        &mut self,
        publication: Publication,
        stop: &mut dyn FnMut(PublishStep) -> bool,
        then: &mut dyn FnMut(&rusqlite::Connection, &Published) -> Result<()>,
    ) -> Result<Published> {
        let project_id = self.project_id().to_owned();
        let tx = self.begin()?;
        let published = publish_in(&tx, &project_id, publication, stop)?;
        then(&tx, &published)?;
        if stop(PublishStep::Commit) {
            // Dropping the transaction rolls it back.
            return Err(StoreError::Aborted);
        }
        tx.commit()?;
        Ok(published)
    }

    /// Undo `version` as a new version: every subject it changed goes back to
    /// its image from before, and the annotations it absorbed are open again.
    /// Refused when a later version changed one of the same memories.
    pub fn revert(&mut self, version: u64, now_ms: u64) -> Result<Reverted> {
        let project_id = self.project_id().to_owned();
        let tx = self.begin()?;
        let cycle: Option<String> = tx
            .query_row(
                "SELECT cycle FROM publication WHERE version = ?1",
                [to_i64(version)],
                |r| r.get(0),
            )
            .optional()?;
        let cycle = cycle.ok_or(StoreError::UnknownVersion(version))?;
        let cycle = crate::codec::word(Cycle::from_word(&cycle), "cycle")?;
        let images: Vec<(String, Option<String>)> = tx
            .prepare("SELECT subject, image FROM revision WHERE version = ?1 ORDER BY subject")?
            .query_map([to_i64(version)], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let new_version = schema::meta_u64(&tx, "memory_version")?.saturating_add(1);
        let read = Read::new(&tx);
        let mut memories: Vec<(MemoryRow, Option<MemoryRow>)> = Vec::new();
        let mut annotations = Vec::new();
        let mut redirects = Vec::new();
        let mut claims: Vec<(AnnotationId, Value)> = Vec::new();
        for (subject, image) in &images {
            let parsed: Option<Value> = match image {
                Some(text) => {
                    Some(serde_json::from_str(text).map_err(|_| StoreError::Corrupt("revision"))?)
                }
                None => None,
            };
            if let Some(address) = subject.strip_prefix('@') {
                redirects.push((address.to_owned(), parsed));
            } else if let Some(id) = subject.strip_prefix('~') {
                let id = AnnotationId::parse(id).map_err(|_| StoreError::Corrupt("revision"))?;
                claims.push((id, parsed.ok_or(StoreError::Corrupt("revision"))?));
            } else if subject.starts_with(MemoryId::PREFIX) {
                let id = MemoryId::parse(subject).map_err(|_| StoreError::Corrupt("revision"))?;
                let current = read
                    .memory_row(&id)?
                    .ok_or(StoreError::Corrupt("revision"))?;
                if current.memory.version != version {
                    return Err(StoreError::RevertConflict {
                        version,
                        subject: id.to_string(),
                    });
                }
                let before = parsed.as_ref().map(memory_from_value).transpose()?;
                memories.push((current, before));
            } else {
                let id =
                    AnnotationId::parse(subject).map_err(|_| StoreError::Corrupt("revision"))?;
                let before = annotation_state_from_value(
                    parsed.as_ref().ok_or(StoreError::Corrupt("revision"))?,
                )?;
                annotations.push((id, before));
            }
        }
        let save = |subject: &str, image: Option<Value>| -> Result<()> {
            tx.execute(
                "INSERT INTO revision (project_id, version, subject, image) VALUES (?1, ?2, ?3, ?4)",
                params![project_id, to_i64(new_version), subject, image.map(|v| v.to_string())],
            )?;
            Ok(())
        };
        for (current, _) in &memories {
            save(current.memory.id.as_str(), Some(memory_to_value(current)))?;
            tx.execute(
                "UPDATE memory SET live = 0 WHERE id = ?1",
                [current.memory.id.as_str()],
            )?;
        }
        let mut restored = Vec::new();
        for (current, before) in memories {
            let mut row = match before {
                Some(before) => before,
                // Created by the reverted version: withdrawn, never deleted.
                None => MemoryRow {
                    live: false,
                    merged_into: None,
                    ..current
                },
            };
            row.memory.version = new_version;
            row.memory.updated_ms = now_ms;
            write_memory_core(&tx, &project_id, &row)?;
            restored.push(row);
        }
        for row in &restored {
            write_relations(&tx, &project_id, row)?;
        }
        for row in &restored {
            verify_round_trip(&tx, row)?;
        }
        for (id, before) in &claims {
            save(&format!("~{id}"), Some(claim_image(&tx, id)?))?;
            let verification = before
                .get("verification")
                .and_then(Value::as_str)
                .ok_or(StoreError::Corrupt("revision"))?;
            let standing = before.get("standing").and_then(Value::as_str);
            tx.execute(
                "UPDATE annotation SET verification = ?2, standing = ?3 WHERE id = ?1",
                params![id.as_str(), verification, standing],
            )?;
        }
        let mut reopened = Vec::new();
        for (id, before) in annotations {
            let (target, absorbed_in, into): (Option<String>, Option<i64>, Option<String>) = tx
                .query_row(
                    "SELECT target_memory, absorbed_in, absorbed_into FROM annotation WHERE id = ?1",
                    [id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
            let parse = |m: Option<String>| m.and_then(|m| MemoryId::parse(&m).ok());
            let now = AnnotationState {
                target_memory: parse(target),
                absorbed_in: absorbed_in.map(to_u64),
                absorbed_into: parse(into),
            };
            save(id.as_str(), Some(annotation_state_to_value(&now)))?;
            if now.absorbed_in == Some(version) && before.absorbed_in.is_none() {
                reopened.push(id.clone());
            }
            tx.execute(
                "UPDATE annotation SET target_memory = ?2, absorbed_in = ?3, absorbed_into = ?4 WHERE id = ?1",
                params![
                    id.as_str(),
                    before.target_memory.as_ref().map(MemoryId::as_str),
                    before.absorbed_in.map(to_i64),
                    before.absorbed_into.as_ref().map(MemoryId::as_str)
                ],
            )?;
        }
        for (address, before) in &redirects {
            let now: Option<String> = tx
                .query_row(
                    "SELECT memory_id FROM address_redirect WHERE address = ?1",
                    [address],
                    |r| r.get(0),
                )
                .optional()?;
            save(&format!("@{address}"), now.map(Value::from))?;
            match before.as_ref().and_then(Value::as_str) {
                Some(id) => {
                    tx.execute(
                        "INSERT INTO address_redirect (project_id, address, memory_id, version)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT (project_id, address) DO UPDATE SET memory_id = excluded.memory_id,
                             version = excluded.version",
                        params![project_id, address, id, to_i64(new_version)],
                    )?;
                }
                None => {
                    tx.execute("DELETE FROM address_redirect WHERE address = ?1", [address])?;
                }
            }
        }
        // Claims the restored state no longer supports are lowered, never
        // left standing (spec §4.5): an intervening absorption or merge may
        // have changed a lineage since the images were taken.
        let mut saved: BTreeSet<String> = images.iter().map(|(s, _)| s.clone()).collect();
        let mut lowered: Vec<Subject> = Vec::new();
        for claim in unsupported(&tx)? {
            let subject = match &claim {
                Unsupported::MemoryStatus(id) | Unsupported::MemoryStanding(id) => {
                    let mut row = read
                        .memory_row(id)?
                        .ok_or_else(|| StoreError::UnknownMemory(id.clone()))?;
                    if saved.insert(id.to_string()) {
                        save(id.as_str(), Some(memory_to_value(&row)))?;
                    }
                    if matches!(claim, Unsupported::MemoryStatus(_)) {
                        row.memory.status = MemoryStatus::ToVerify;
                    } else {
                        row.memory.standing = Standing::default_for(row.memory.kind);
                    }
                    row.memory.version = new_version;
                    row.memory.updated_ms = now_ms;
                    write_memory_row(&tx, &project_id, &row)?;
                    verify_round_trip(&tx, &row)?;
                    Subject::Memory(id.clone())
                }
                Unsupported::AnnotationVerification(id, _)
                | Unsupported::AnnotationStanding(id) => {
                    let key = format!("~{id}");
                    if saved.insert(key.clone()) {
                        save(&key, Some(claim_image(&tx, id)?))?;
                    }
                    let sql = if matches!(claim, Unsupported::AnnotationStanding(_)) {
                        "UPDATE annotation SET standing = NULL WHERE id = ?1"
                    } else {
                        "UPDATE annotation SET verification = 'non_verificata' WHERE id = ?1"
                    };
                    tx.execute(sql, [id.as_str()])?;
                    Subject::Annotation(id.clone())
                }
            };
            if !lowered.contains(&subject) {
                lowered.push(subject);
            }
        }
        if let Some(claim) = unsupported(&tx)?.into_iter().next() {
            return Err(claim.into_error());
        }
        record_publication(
            &tx,
            &project_id,
            Record {
                version: new_version,
                now_ms,
                cycle,
                operations: images.len(),
                reverts: Some(version),
                run_id: None,
            },
        )?;
        tx.commit()?;
        Ok(Reverted {
            version: new_version,
            reverted: version,
            reopened,
            lowered,
        })
    }
}

/// An annotation's verification and standing, as a revert keeps them.
fn claim_image(conn: &Connection, id: &AnnotationId) -> Result<Value> {
    let (verification, standing): (String, Option<String>) = conn.query_row(
        "SELECT verification, standing FROM annotation WHERE id = ?1",
        [id.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(serde_json::json!({ "verification": verification, "standing": standing }))
}
