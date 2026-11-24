//! The write side: one [`Store`] per project, owned by the project's writer
//! thread ([`crate::Writer`]). Every method is one transaction.
//!
//! The invariants that must never depend on a caller are checked here:
//! sources on every memory and annotation, no memory in the entrance, unique
//! addresses, ids of this project only, `verificato` and the stronger
//! standings only on independent evidence (`connecto_core::memory::confirm`).

use std::path::{Path, PathBuf};

use connecto_core::Redacted;
use connecto_core::memory::{
    Anchor, Annotation, AnnotationId, AnnotationKind, AnnotationOrigin, AnnotationTarget, Evidence,
    EvidenceId, EvidenceKind, LogPointer, MemoryError, MemoryId, MemoryKind, MemoryStatus, Scope,
    Standing, Subject, Verb, Verification, may_verify, supports_standing,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::codec::{
    MemoryRow, anchors_to_value, entropy, scopes_to_json, subjects_to_json, to_i64, word,
};
use crate::error::{Result, StoreError};
use crate::read::Read;
use crate::schema;

/// Where a project's memory lives: `<project data>/memory/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPaths {
    pub dir: PathBuf,
}

impl MemoryPaths {
    /// `project_data` is `<data>/projects/<id>`.
    #[must_use]
    pub fn under(project_data: &Path) -> Self {
        Self {
            dir: project_data.join("memory"),
        }
    }

    #[must_use]
    pub fn db(&self) -> PathBuf {
        self.dir.join("memory.db")
    }

    #[must_use]
    pub fn exports(&self) -> PathBuf {
        self.dir.join("exports")
    }
}

/// A new piece of evidence. `exposed_to` is not given: the store computes it
/// from `served` for the same session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvidence {
    /// The event it comes from (its id becomes `e-<seq>`); `None` takes a new
    /// number from the same counter (evidence without an event, such as a
    /// project document read by the initial build).
    pub event_seq: Option<u64>,
    pub kind: EvidenceKind,
    pub created_ms: u64,
    pub terminal: Option<String>,
    pub tool_session: Option<String>,
    pub turn: Option<String>,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub log: Option<LogPointer>,
    pub files: Vec<Anchor>,
    pub excerpt: Redacted<String>,
}

impl NewEvidence {
    /// Evidence of `kind` at `created_ms` with nothing else set.
    #[must_use]
    pub const fn new(kind: EvidenceKind, created_ms: u64, excerpt: Redacted<String>) -> Self {
        Self {
            event_seq: None,
            kind,
            created_ms,
            terminal: None,
            tool_session: None,
            turn: None,
            branch: None,
            head: None,
            log: None,
            files: Vec::new(),
            excerpt,
        }
    }
}

/// A new annotation: from an agent's `propose`/`flag`, from the development
/// cycle, or from the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAnnotation {
    /// `None`: the store makes one.
    pub id: Option<AnnotationId>,
    pub created_ms: u64,
    pub kind: AnnotationKind,
    pub target: AnnotationTarget,
    pub text: Redacted<String>,
    pub scopes: Vec<Scope>,
    /// At least one, all of this project.
    pub sources: Vec<EvidenceId>,
    pub verification: Verification,
    pub origin: AnnotationOrigin,
    pub memory_kind: Option<MemoryKind>,
    pub standing: Option<Standing>,
}

/// The project's store: the write connection.
pub struct Store {
    conn: Connection,
    project_id: String,
    path: PathBuf,
}

impl Store {
    /// Open or create the database at `path` (normally
    /// [`MemoryPaths::db`]) for `project_id`: WAL, migrated forward, and
    /// refused if it belongs to another project or a newer build.
    pub fn open(path: &Path, project_id: &str) -> Result<Self> {
        Ok(Self {
            conn: schema::open_write(path, project_id)?,
            project_id: project_id.to_owned(),
            path: path.to_owned(),
        })
    }

    #[doc(hidden)]
    pub fn open_with_migrations(
        path: &Path,
        project_id: &str,
        migrations: &[&str],
    ) -> Result<Self> {
        Ok(Self {
            conn: schema::open_write_with(path, project_id, migrations)?,
            project_id: project_id.to_owned(),
            path: path.to_owned(),
        })
    }

    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads over the write connection (they see this store's own writes).
    #[must_use]
    pub const fn read(&self) -> Read<'_> {
        Read::new(&self.conn)
    }

    pub fn version(&self) -> Result<u64> {
        self.read().version()
    }

    pub(crate) fn begin(&mut self) -> Result<Transaction<'_>> {
        Ok(self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    #[doc(hidden)]
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Store a piece of evidence; its `exposed_to` is everything `served`
    /// had shown the same session up to `created_ms`.
    pub fn insert_evidence(&mut self, new: NewEvidence) -> Result<Evidence> {
        let project_id = self.project_id.clone();
        let tx = self.begin()?;
        let evidence = insert_evidence_in(&tx, &project_id, new)?;
        tx.commit()?;
        Ok(evidence)
    }

    /// Record that `subjects` were shown to a session by `verb`.
    pub fn record_served(
        &mut self,
        tool_session: Option<&str>,
        terminal: Option<&str>,
        verb: Verb,
        subjects: &[Subject],
        at_ms: u64,
    ) -> Result<()> {
        let project_id = self.project_id.clone();
        let tx = self.begin()?;
        for subject in subjects {
            tx.execute(
                "INSERT INTO served (project_id, tool_session, terminal, subject, verb, at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    project_id,
                    tool_session,
                    terminal,
                    subject.to_string(),
                    verb.word(),
                    to_i64(at_ms)
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Create an annotation, visible at once. Its `seq` is the next of the
    /// project.
    pub fn create_annotation(&mut self, new: NewAnnotation) -> Result<Annotation> {
        let project_id = self.project_id.clone();
        let tx = self.begin()?;
        let annotation = create_annotation_in(&tx, &project_id, new)?;
        tx.commit()?;
        Ok(annotation)
    }

    /// Set a memory's freshness. Never touches its status (spec D9) and is
    /// not a new memory version.
    pub fn set_freshness(
        &mut self,
        id: &MemoryId,
        last_confirmed_ms: Option<u64>,
        anchors_changed: bool,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE memory SET last_confirmed_ms = ?2, anchors_changed = ?3 WHERE id = ?1 AND live = 1",
            params![id.as_str(), last_confirmed_ms.map(to_i64), anchors_changed],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownMemory(id.clone()));
        }
        refresh_memory_fts(&self.conn, id)?;
        Ok(())
    }

    /// Mark a scope of a memory as gone (its branch or worktree no longer
    /// exists) or back. The memory stays as it is (spec §4.6).
    pub fn set_scope_gone(&mut self, id: &MemoryId, scope: &Scope, gone: bool) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE memory_scope SET gone = ?3 WHERE memory_id = ?1 AND scope = ?2",
            params![id.as_str(), scope.to_string(), gone],
        )?;
        Ok(changed > 0)
    }
}

pub(crate) fn insert_evidence_in(
    tx: &Transaction<'_>,
    project_id: &str,
    new: NewEvidence,
) -> Result<Evidence> {
    for anchor in &new.files {
        check_anchor(anchor)?;
    }
    let seq = match new.event_seq {
        Some(seq) => {
            if seq == 0 || seq > schema::meta_u64(tx, "event_seq")? {
                return Err(StoreError::UnknownEvent(seq));
            }
            let exists: bool = tx.query_row(
                "SELECT count(*) > 0 FROM evidence WHERE seq = ?1",
                [to_i64(seq)],
                |r| r.get(0),
            )?;
            if exists {
                return Err(StoreError::EvidenceExists(EvidenceId::new(seq)));
            }
            seq
        }
        None => schema::next_counter(tx, "event_seq")?,
    };
    let exposed_to = Read::new(tx).served_to(
        new.tool_session.as_deref(),
        new.terminal.as_deref(),
        new.created_ms,
    )?;
    tx.execute(
        "INSERT INTO evidence (seq, project_id, kind, created_ms, terminal, tool_session, turn,
             branch, head, log_path, log_offset, files, excerpt, exposed_to)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            to_i64(seq),
            project_id,
            new.kind.word(),
            to_i64(new.created_ms),
            new.terminal,
            new.tool_session,
            new.turn,
            new.branch,
            new.head,
            new.log.as_ref().map(|l| l.path.clone()),
            new.log.as_ref().map(|l| to_i64(l.offset)),
            anchors_to_value(&new.files).to_string(),
            new.excerpt.get(),
            subjects_to_json(&exposed_to),
        ],
    )?;
    let evidence = Evidence {
        id: EvidenceId::new(seq),
        kind: new.kind,
        project_id: project_id.to_owned(),
        created_ms: new.created_ms,
        terminal: new.terminal,
        tool_session: new.tool_session,
        turn: new.turn,
        branch: new.branch,
        head: new.head,
        log: new.log,
        files: new.files,
        excerpt: new.excerpt,
        exposed_to,
    };
    // What was written must read back as it was given.
    if Read::new(tx).evidence(evidence.id)?.as_ref() != Some(&evidence) {
        return Err(StoreError::Corrupt("evidence round trip"));
    }
    Ok(evidence)
}

/// Load the evidence named, refusing ids this project does not have.
pub(crate) fn load_evidence(conn: &Connection, ids: &[EvidenceId]) -> Result<Vec<Evidence>> {
    let read = Read::new(conn);
    ids.iter()
        .map(|id| read.evidence(*id)?.ok_or(StoreError::UnknownEvidence(*id)))
        .collect()
}

pub(crate) fn link_sources(
    conn: &Connection,
    project_id: &str,
    subject: &Subject,
    sources: &[EvidenceId],
) -> Result<()> {
    load_evidence(conn, sources)?;
    for id in sources {
        conn.execute(
            "INSERT OR IGNORE INTO source_link (project_id, subject, evidence_seq) VALUES (?1, ?2, ?3)",
            params![project_id, subject.to_string(), to_i64(id.seq())],
        )?;
    }
    Ok(())
}

/// A fresh id that no row has ever used.
pub(crate) fn fresh_id<T>(
    conn: &Connection,
    make: impl Fn([u8; 5]) -> T,
    text: impl Fn(&T) -> String,
    table: &str,
) -> Result<T> {
    for _ in 0..64 {
        let id = make(entropy());
        if !id_used(conn, table, &text(&id))? {
            return Ok(id);
        }
    }
    Err(StoreError::Corrupt("no free id"))
}

pub(crate) fn id_used(conn: &Connection, table: &str, id: &str) -> Result<bool> {
    let sql = format!("SELECT count(*) > 0 FROM {table} WHERE id = ?1");
    Ok(conn.query_row(&sql, [id], |r| r.get(0))?)
}

/// The lineage an annotation's claim is judged against: the annotation, and
/// the lineage of the memory it is about (followed through merges).
fn annotation_lineage(conn: &Connection, annotation: &Annotation) -> Result<Vec<Subject>> {
    let read = Read::new(conn);
    let mut lineage = vec![Subject::Annotation(annotation.id.clone())];
    if let Some(target) = annotation.target_memory() {
        lineage.extend(read.lineage(&read.merge_end(target)?)?);
        let direct = Subject::Memory(target.clone());
        if !lineage.contains(&direct) {
            lineage.push(direct);
        }
    }
    Ok(lineage)
}

/// Whether an annotation's verification and standing hold over `sources`,
/// as (verification, standing).
fn annotation_support(
    conn: &Connection,
    annotation: &Annotation,
    sources: &[EvidenceId],
) -> Result<(Result<()>, Result<()>)> {
    let evidence = load_evidence(conn, sources)?;
    let lineage = annotation_lineage(conn, annotation)?;
    let verification = match annotation.verification {
        Verification::Unverified => Ok(()),
        Verification::VerifiedByEvidence if may_verify(&evidence, &lineage) => Ok(()),
        Verification::VerifiedByEvidence => {
            Err(StoreError::NotVerifiable(annotation.id.to_string()))
        }
        Verification::UserStated
            if evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::UserStatement) =>
        {
            Ok(())
        }
        Verification::UserStated => Err(StoreError::NotUserStated(annotation.id.clone())),
    };
    let standing = match annotation.standing {
        Some(standing) if !supports_standing(standing, &evidence, &lineage) => {
            Err(StoreError::StandingUnsupported(annotation.id.to_string()))
        }
        _ => Ok(()),
    };
    Ok((verification, standing))
}

fn check_annotation_evidence(
    conn: &Connection,
    annotation: &Annotation,
    sources: &[EvidenceId],
) -> Result<()> {
    let (verification, standing) = annotation_support(conn, annotation, sources)?;
    verification?;
    standing
}

/// A claim that its evidence no longer supports.
#[derive(Debug)]
pub(crate) enum Unsupported {
    /// A `verificato` memory without an independent confirmation.
    MemoryStatus(MemoryId),
    /// A memory standing its evidence does not support.
    MemoryStanding(MemoryId),
    /// An open annotation's verification.
    AnnotationVerification(AnnotationId, StoreError),
    AnnotationStanding(AnnotationId),
}

impl Unsupported {
    pub(crate) fn into_error(self) -> StoreError {
        match self {
            Self::MemoryStatus(id) => StoreError::NotVerifiable(id.to_string()),
            Self::MemoryStanding(id) => StoreError::StandingUnsupported(id.to_string()),
            Self::AnnotationVerification(_, error) => error,
            Self::AnnotationStanding(id) => StoreError::StandingUnsupported(id.to_string()),
        }
    }
}

/// Every claim in the store, as it stands, that its evidence and lineage do
/// not support: live memories that are `verificato` or hold a standing above
/// the weakest, and open annotations that claim a verification or standing.
///
/// Lineage and sources change on more paths than the rows an operation
/// names (an absorbed annotation, a merge, a derivation, a revert all reach
/// every memory downstream), so publication and revert check the whole
/// store, not only what they touched.
pub(crate) fn unsupported(conn: &Connection) -> Result<Vec<Unsupported>> {
    let read = Read::new(conn);
    let mut out = Vec::new();
    let claims: Vec<(String, String, String)> = conn
        .prepare("SELECT id, status, standing FROM memory WHERE live = 1 ORDER BY rowid")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, status, standing) in claims {
        let needs_status = status == MemoryStatus::Verified.word();
        let needs_standing =
            Standing::from_word(&standing).is_some_and(|s| s.required_evidence().is_some());
        if !(needs_status || needs_standing) {
            continue;
        }
        let id = MemoryId::parse(&id).map_err(|_| StoreError::Corrupt("memory id"))?;
        let standing = word(Standing::from_word(&standing), "standing")?;
        // Only what the rule needs: the sources' evidence (one query) and the
        // lineage (indexed walk), not the whole row.
        let evidence = read.evidence_for(&Subject::Memory(id.clone()))?;
        let lineage = read.lineage(&id)?;
        if needs_status && !may_verify(&evidence, &lineage) {
            out.push(Unsupported::MemoryStatus(id.clone()));
        }
        if !supports_standing(standing, &evidence, &lineage) {
            out.push(Unsupported::MemoryStanding(id));
        }
    }
    let notes: Vec<String> = conn
        .prepare(
            "SELECT id FROM annotation WHERE absorbed_in IS NULL
               AND (verification <> 'non_verificata' OR standing IS NOT NULL) ORDER BY seq",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in notes {
        let id = AnnotationId::parse(&id).map_err(|_| StoreError::Corrupt("annotation id"))?;
        let Some(annotation) = read.annotation(&id)? else {
            continue;
        };
        let sources: Vec<EvidenceId> = annotation.sources.iter().map(|s| s.evidence).collect();
        let (verification, standing) = annotation_support(conn, &annotation, &sources)?;
        if let Err(error) = verification {
            out.push(Unsupported::AnnotationVerification(id.clone(), error));
        }
        if standing.is_err() {
            out.push(Unsupported::AnnotationStanding(id));
        }
    }
    Ok(out)
}

/// The structural invariants of every memory row the store writes, live or
/// not (a merged or withdrawn row is kept and must read back too): the core
/// checks, a room that holds memories, scopes and anchors that parse back,
/// one anchor per path.
pub(crate) fn check_memory_row(row: &MemoryRow) -> Result<()> {
    let m = &row.memory;
    m.check()?;
    if !m.room().holds_memories() {
        return Err(MemoryError::EntranceHoldsNothing.into());
    }
    for scope in &m.scopes {
        check_scope(&scope.scope)?;
    }
    let mut paths = std::collections::BTreeSet::new();
    for anchor in &m.anchors {
        check_anchor(anchor)?;
        if !paths.insert(anchor.path.as_str()) {
            return Err(MemoryError::BadPath.into());
        }
    }
    Ok(())
}

/// A memory row in the order and form the store reads it back.
fn normalized(row: &MemoryRow) -> MemoryRow {
    let mut row = row.clone();
    let m = &mut row.memory;
    m.scopes.sort_by_key(|s| s.scope.to_string());
    m.scopes.dedup_by(|a, b| a.scope == b.scope);
    m.anchors.sort_by(|a, b| a.path.cmp(&b.path));
    m.sources.sort();
    m.sources.dedup();
    m.relations
        .sort_by(|a, b| (a.kind.word(), &a.target).cmp(&(b.kind.word(), &b.target)));
    m.relations.dedup();
    m.freshness.open_annotations = 0;
    row
}

/// Fail closed unless `row`, as written, reads back the same: nothing the
/// store writes may later read as `Corrupt`.
pub(crate) fn verify_round_trip(conn: &Connection, row: &MemoryRow) -> Result<()> {
    match Read::new(conn).memory_row(&row.memory.id) {
        Ok(Some(back)) if normalized(&back) == normalized(row) => Ok(()),
        _ => Err(StoreError::Corrupt("memory round trip")),
    }
}

/// Refuse a value whose public fields were set past their parsing
/// constructors: what is stored must read back.
pub(crate) fn check_anchor(anchor: &Anchor) -> Result<()> {
    Anchor::new(&anchor.path, &anchor.blob)?;
    Ok(())
}

pub(crate) fn check_scope(scope: &Scope) -> Result<()> {
    if Scope::parse(&scope.to_string()).as_ref() == Ok(scope) {
        Ok(())
    } else {
        Err(MemoryError::BadScope.into())
    }
}

fn check_origin(origin: &AnnotationOrigin) -> Result<()> {
    if AnnotationOrigin::parse(&origin.to_string()).as_ref() == Ok(origin) {
        Ok(())
    } else {
        Err(MemoryError::BadOrigin.into())
    }
}

pub(crate) fn create_annotation_in(
    tx: &Transaction<'_>,
    project_id: &str,
    new: NewAnnotation,
) -> Result<Annotation> {
    let id = match new.id {
        Some(id) => {
            if id_used(tx, "annotation", id.as_str())? {
                return Err(StoreError::IdTaken(id.to_string()));
            }
            id
        }
        None => fresh_id(
            tx,
            AnnotationId::from_entropy,
            ToString::to_string,
            "annotation",
        )?,
    };
    if let AnnotationTarget::Memory(target) = &new.target {
        let live: Option<bool> = tx
            .query_row(
                "SELECT live FROM memory WHERE id = ?1",
                [target.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if live != Some(true) {
            return Err(StoreError::UnknownMemory(target.clone()));
        }
    }
    let mut annotation = Annotation {
        id,
        seq: 0,
        created_ms: new.created_ms,
        kind: new.kind,
        target: new.target,
        text: new.text.get().clone(),
        scopes: new.scopes,
        sources: Vec::new(),
        verification: new.verification,
        origin: new.origin,
        memory_kind: new.memory_kind,
        standing: new.standing,
        absorbed_in: None,
    };
    // `check` wants the sources present; their kinds are filled in below.
    annotation.sources = load_evidence(tx, &new.sources)?
        .iter()
        .map(|e| connecto_core::memory::SourceRef {
            evidence: e.id,
            kind: e.kind,
        })
        .collect();
    annotation.check()?;
    for scope in &annotation.scopes {
        check_scope(scope)?;
    }
    check_origin(&annotation.origin)?;
    check_annotation_evidence(tx, &annotation, &new.sources)?;
    annotation.seq = schema::next_counter(tx, "annotation_seq")?;
    let (target_memory, new_room) = match &annotation.target {
        AnnotationTarget::Memory(id) => (Some(id.as_str()), None),
        AnnotationTarget::New { room } => (None, Some(room.word())),
    };
    tx.execute(
        "INSERT INTO annotation (id, project_id, seq, created_ms, kind, target_memory, new_room,
             text, scopes, verification, origin, memory_kind, standing)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            annotation.id.as_str(),
            project_id,
            to_i64(annotation.seq),
            to_i64(annotation.created_ms),
            annotation.kind.word(),
            target_memory,
            new_room,
            annotation.text,
            scopes_to_json(&annotation.scopes),
            annotation.verification.word(),
            annotation.origin.to_string(),
            annotation.memory_kind.map(MemoryKind::word),
            annotation.standing.map(Standing::word),
        ],
    )?;
    let rowid = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO annotation_fts (rowid, text) VALUES (?1, ?2)",
        params![rowid, annotation.text],
    )?;
    link_sources(
        tx,
        project_id,
        &Subject::Annotation(annotation.id.clone()),
        &new.sources,
    )?;
    annotation.sources.sort();
    annotation.sources.dedup();
    if Read::new(tx).annotation(&annotation.id)?.as_ref() != Some(&annotation) {
        return Err(StoreError::Corrupt("annotation round trip"));
    }
    Ok(annotation)
}

/// Write a memory row and its children as `row` says, replacing what was
/// there, and bring its search entry in step.
/// Write a memory row and its children (relations included), replacing
/// what was there, and bring its search entry in step. When several rows are
/// written together, use [`write_memory_core`] for all of them first and
/// [`write_relations`] after, so a relation can name a row created alongside.
pub(crate) fn write_memory_row(conn: &Connection, project_id: &str, row: &MemoryRow) -> Result<()> {
    write_memory_core(conn, project_id, row)?;
    write_relations(conn, project_id, row)
}

/// Replace a memory's outgoing relations with those of `row`.
pub(crate) fn write_relations(conn: &Connection, project_id: &str, row: &MemoryRow) -> Result<()> {
    let id = row.memory.id.as_str();
    conn.execute("DELETE FROM relation WHERE from_id = ?1", [id])?;
    for relation in &row.memory.relations {
        conn.execute(
            "INSERT OR IGNORE INTO relation (project_id, from_id, kind, to_id) VALUES (?1, ?2, ?3, ?4)",
            params![project_id, id, relation.kind.word(), relation.target.as_str()],
        )?;
    }
    Ok(())
}

/// Write a memory row and every child but its relations.
pub(crate) fn write_memory_core(
    conn: &Connection,
    project_id: &str,
    row: &MemoryRow,
) -> Result<()> {
    check_memory_row(row)?;
    let m = &row.memory;
    let id = m.id.as_str();
    conn.execute(
        "INSERT INTO memory (id, project_id, room, slug, kind, standing, summary, details, status,
             last_confirmed_ms, anchors_changed, created_ms, updated_ms, version, live, merged_into)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT (id) DO UPDATE SET room = excluded.room, slug = excluded.slug,
             kind = excluded.kind, standing = excluded.standing, summary = excluded.summary,
             details = excluded.details, status = excluded.status,
             last_confirmed_ms = excluded.last_confirmed_ms,
             anchors_changed = excluded.anchors_changed, created_ms = excluded.created_ms,
             updated_ms = excluded.updated_ms, version = excluded.version,
             live = excluded.live, merged_into = excluded.merged_into",
        params![
            id,
            project_id,
            m.room().word(),
            m.address.slug(),
            m.kind.word(),
            m.standing.word(),
            m.summary,
            m.details,
            m.status.word(),
            m.freshness.last_confirmed_ms.map(to_i64),
            m.freshness.anchors_changed,
            to_i64(m.created_ms),
            to_i64(m.updated_ms),
            to_i64(m.version),
            row.live,
            row.merged_into.as_ref().map(MemoryId::as_str),
        ],
    )
    .map_err(|e| address_conflict(e, &m.address.to_string()))?;
    conn.execute("DELETE FROM memory_scope WHERE memory_id = ?1", [id])?;
    for scope in &m.scopes {
        conn.execute(
            "INSERT OR REPLACE INTO memory_scope (project_id, memory_id, scope, gone) VALUES (?1, ?2, ?3, ?4)",
            params![project_id, id, scope.scope.to_string(), scope.gone],
        )?;
    }
    conn.execute("DELETE FROM memory_anchor WHERE memory_id = ?1", [id])?;
    for anchor in &m.anchors {
        conn.execute(
            "INSERT OR REPLACE INTO memory_anchor (project_id, memory_id, path, blob) VALUES (?1, ?2, ?3, ?4)",
            params![project_id, id, anchor.path, anchor.blob],
        )?;
    }
    let subject = Subject::Memory(m.id.clone()).to_string();
    conn.execute("DELETE FROM source_link WHERE subject = ?1", [&subject])?;
    let sources: Vec<EvidenceId> = m.sources.iter().map(|s| s.evidence).collect();
    link_sources(conn, project_id, &Subject::Memory(m.id.clone()), &sources)?;
    refresh_memory_fts(conn, &m.id)?;
    Ok(())
}

/// A unique-index failure on a memory's address, as `AddressTaken`.
fn address_conflict(error: rusqlite::Error, address: &str) -> StoreError {
    match &error {
        rusqlite::Error::SqliteFailure(e, Some(message))
            if e.code == rusqlite::ErrorCode::ConstraintViolation
                && message.contains("memory.room") =>
        {
            StoreError::AddressTaken(address.to_owned())
        }
        _ => StoreError::Sqlite(error),
    }
}

/// Put a memory's search entry in step with its row: present while live.
pub(crate) fn refresh_memory_fts(conn: &Connection, id: &MemoryId) -> Result<()> {
    let row: Option<(i64, String, String, String, String, bool)> = conn
        .query_row(
            "SELECT rowid, room, slug, summary, details, live FROM memory WHERE id = ?1",
            [id.as_str()],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((rowid, room, slug, summary, details, live)) = row else {
        return Ok(());
    };
    conn.execute("DELETE FROM memory_fts WHERE rowid = ?1", [rowid])?;
    if live {
        conn.execute(
            "INSERT INTO memory_fts (rowid, address, summary, details) VALUES (?1, ?2, ?3, ?4)",
            params![rowid, format!("{room}/{slug}"), summary, details],
        )?;
    }
    Ok(())
}
