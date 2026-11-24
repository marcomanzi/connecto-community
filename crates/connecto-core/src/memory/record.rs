//! Memories, annotations and evidence (spec §4.3-§4.5, §5.10).

use std::fmt;
use std::str::FromStr;

use crate::Redacted;

use super::MemoryError;
use super::ids::{Address, AnnotationId, EvidenceId, MemoryId, Room, Subject};

/// Longest summary, in characters. It is what the brief and search show.
pub const MAX_SUMMARY_CHARS: usize = 400;
/// Longest memory details, in bytes.
pub const MAX_DETAILS_BYTES: usize = 16 * 1024;
/// Longest annotation text, in bytes; also the `propose` / `flag` limit (a
/// longer text is refused, never cut).
pub const MAX_NOTE_BYTES: usize = 8 * 1024;
/// Longest path or branch name in a scope, in bytes.
pub const MAX_SCOPE_BYTES: usize = 512;

word_enum! {
    /// What a memory is.
    pub enum MemoryKind {
        Fact => "fatto", "Fact";
        Procedure => "procedura", "Procedure";
        Decision => "decisione", "Decision";
        Problem => "problema", "Problem";
        Work => "lavoro", "Work";
        Convention => "convenzione", "Convention";
    }
}

word_enum! {
    /// How reliable a memory is. Independent of [`Freshness`] and of
    /// [`Standing`].
    #[derive(Default)]
    pub enum MemoryStatus {
        /// Recorded with sources, not yet independently confirmed.
        #[default]
        Candidate => "candidato", "Candidate";
        /// Confirmed by at least one independent piece of evidence
        /// ([`super::may_verify`]).
        Verified => "verificato", "Verified";
        /// Something it rests on changed, or the evidence was not enough: it
        /// needs checking. Not false.
        ToVerify => "da_verificare", "To verify";
        /// Replaced by another memory (see [`RelationKind::Supersedes`]).
        Superseded => "superato", "Superseded";
        /// Sources disagree; both stay visible.
        Conflict => "conflitto", "Conflict";
    }
}

word_enum! {
    /// What a memory or annotation claims, as opposed to how reliable that
    /// claim is ([`MemoryStatus`]). Keeps apart what the spec says must never
    /// be merged (§5.10): a proposal from an accepted decision, a plan from a
    /// finished implementation, a hypothesis from an established cause, and a
    /// documented procedure from one seen working.
    ///
    /// Each [`MemoryKind`] has its own set ([`Standing::allowed_for`]); the
    /// first of each set is the default and needs no confirmation. The others
    /// need independent evidence of a given kind ([`super::supports_standing`]):
    /// an agent's proposal is never the user's approval.
    pub enum Standing {
        /// A decision or convention someone proposed. Agents' proposals stay
        /// here until the user approves them.
        Proposed => "proposta", "Proposed";
        /// Approved: needs a user statement or a recorded decision (a project
        /// document such as an ADR).
        Accepted => "accettata", "Accepted";
        /// An alternative turned down; same requirement as `Accepted`.
        Discarded => "scartata", "Discarded";
        /// Work that is planned, not started.
        Planned => "pianificato", "Planned";
        /// Work started, not finished.
        InProgress => "in_corso", "In progress";
        /// Work done: needs a code observation or a user statement.
        Implemented => "implementato", "Implemented";
        /// A possible fact or cause, such as a reason inferred from code.
        Hypothesis => "ipotesi", "Hypothesis";
        /// A fact or cause confirmed by any independent evidence.
        Established => "accertato", "Established";
        /// A procedure as written down; not proof it works.
        Documented => "documentata", "Documented";
        /// A procedure seen to work: needs a user statement or an outcome
        /// reported by a session that had not been shown this memory.
        Exercised => "eseguita", "Exercised";
    }
}

impl Standing {
    /// The standings a kind of memory can have; the first is the default.
    #[must_use]
    pub const fn allowed_for(kind: MemoryKind) -> &'static [Self] {
        match kind {
            MemoryKind::Decision | MemoryKind::Convention => {
                &[Self::Proposed, Self::Accepted, Self::Discarded]
            }
            MemoryKind::Work => &[Self::Planned, Self::InProgress, Self::Implemented],
            MemoryKind::Fact | MemoryKind::Problem => &[Self::Hypothesis, Self::Established],
            MemoryKind::Procedure => &[Self::Documented, Self::Exercised],
        }
    }

    /// The weakest standing of a kind, which needs no confirmation.
    #[must_use]
    pub const fn default_for(kind: MemoryKind) -> Self {
        match kind {
            MemoryKind::Decision | MemoryKind::Convention => Self::Proposed,
            MemoryKind::Work => Self::Planned,
            MemoryKind::Fact | MemoryKind::Problem => Self::Hypothesis,
            MemoryKind::Procedure => Self::Documented,
        }
    }

    #[must_use]
    pub fn applies_to(self, kind: MemoryKind) -> bool {
        Self::allowed_for(kind).contains(&self)
    }

    /// The evidence kinds that can support this standing, of which at least
    /// one independent piece is needed. `None`: no support needed.
    #[must_use]
    pub const fn required_evidence(self) -> Option<&'static [EvidenceKind]> {
        match self {
            Self::Proposed
            | Self::Planned
            | Self::InProgress
            | Self::Hypothesis
            | Self::Documented => None,
            Self::Accepted | Self::Discarded => {
                Some(&[EvidenceKind::UserStatement, EvidenceKind::ProjectDocument])
            }
            Self::Implemented => {
                Some(&[EvidenceKind::CodeObservation, EvidenceKind::UserStatement])
            }
            Self::Established => Some(EvidenceKind::ALL),
            Self::Exercised => Some(&[EvidenceKind::UserStatement, EvidenceKind::ReportedOutcome]),
        }
    }
}

/// Where a memory or annotation applies. A memory may have several.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// The whole project.
    Project,
    /// A path relative to the project root ([`is_project_relative`]).
    Component(String),
    /// A git branch.
    Branch(String),
    /// A worktree path.
    Worktree(String),
}

impl Scope {
    /// Read `project`, `component:<path>`, `branch:<name>` or
    /// `worktree:<path>`. The value is 1 to [`MAX_SCOPE_BYTES`] bytes with no
    /// control or hidden characters ([`is_plain_line`]).
    pub fn parse(text: &str) -> Result<Self, MemoryError> {
        if text == "project" {
            return Ok(Self::Project);
        }
        let (word, value) = text.split_once(':').ok_or(MemoryError::BadScope)?;
        let value_ok = !value.is_empty()
            && value.len() <= MAX_SCOPE_BYTES
            && is_plain_line(value)
            && value.trim() == value;
        if !value_ok {
            return Err(MemoryError::BadScope);
        }
        let value = value.to_owned();
        match word {
            "component" if is_project_relative(&value) => Ok(Self::Component(value)),
            "component" => Err(MemoryError::BadScope),
            "branch" => Ok(Self::Branch(value)),
            "worktree" => Ok(Self::Worktree(value)),
            _ => Err(MemoryError::BadScope),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project => f.write_str("project"),
            Self::Component(path) => write!(f, "component:{path}"),
            Self::Branch(name) => write!(f, "branch:{name}"),
            Self::Worktree(path) => write!(f, "worktree:{path}"),
        }
    }
}

impl FromStr for Scope {
    type Err = MemoryError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// One scope of a memory, with what the host has seen of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScopeEntry {
    pub scope: Scope,
    /// The branch (or worktree) is no longer present: deleted, squashed or
    /// rebased away. The memory stays; promotion to `project` or archiving
    /// needs evidence (spec §4.6).
    pub gone: bool,
}

impl ScopeEntry {
    #[must_use]
    pub const fn new(scope: Scope) -> Self {
        Self { scope, gone: false }
    }
}

/// How fresh a memory is. Never derived from, and never changes,
/// [`MemoryStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Freshness {
    /// Unix milliseconds of the latest independent confirmation.
    pub last_confirmed_ms: Option<u64>,
    /// An anchored file's blob hash changed since it was observed.
    pub anchors_changed: bool,
    /// Open annotations on it.
    pub open_annotations: u32,
}

/// A file a memory refers to, as it was when observed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Anchor {
    /// Relative to the project root, `/`-separated.
    pub path: String,
    /// Git blob hash (40 or 64 lowercase hex characters).
    pub blob: String,
}

impl Anchor {
    pub fn new(path: &str, blob: &str) -> Result<Self, MemoryError> {
        if !is_project_relative(path) {
            return Err(MemoryError::BadPath);
        }
        let blob_ok = matches!(blob.len(), 40 | 64)
            && blob
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !blob_ok {
            return Err(MemoryError::BadBlob);
        }
        Ok(Self {
            path: path.to_owned(),
            blob: blob.to_owned(),
        })
    }
}

/// Whether `c` is a character a single-line identifier (a scope value, a
/// path, a branch or terminal name) must not hold: one that breaks a line or
/// changes how a line reads without being seen. Every control character
/// (U+0085 included), the line and paragraph separators (U+2028, U+2029),
/// zero-width characters and direction marks (U+200B to U+200F, U+2060 to
/// U+2064), direction embeddings, overrides and isolates (U+202A to U+202E,
/// U+2066 to U+2069), the Arabic letter mark (U+061C) and the byte order
/// mark (U+FEFF).
#[must_use]
pub fn is_hidden_or_breaking(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}'
                | '\u{2029}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2060}'..='\u{2064}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{061C}'
                | '\u{FEFF}'
        )
}

/// Whether `text` holds no [`is_hidden_or_breaking`] character, so it shows
/// as the one line it looks like. Free text (summaries, details, notes) is not
/// held to this: the renderer quotes it.
#[must_use]
pub fn is_plain_line(text: &str) -> bool {
    !text.chars().any(is_hidden_or_breaking)
}

/// Whether `path` is a path inside the project, relative to its root, in the
/// form git prints: `/`-separated, 1 to [`MAX_SCOPE_BYTES`] bytes, a
/// [`is_plain_line`]. Refused on every platform, so it cannot leave the project
/// when joined to the root anywhere: a leading `/`, any backslash (a Windows
/// separator), any `:` (a drive prefix such as `C:` or an alternate data
/// stream), and empty, `.` or `..` segments.
#[must_use]
pub fn is_project_relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_SCOPE_BYTES
        && !path.starts_with('/')
        && !path.contains(['\\', ':'])
        && is_plain_line(path)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

word_enum! {
    /// How one memory relates to another.
    pub enum RelationKind {
        Supersedes => "sostituisce", "Supersedes";
        Contradicts => "contraddice", "Contradicts";
        DependsOn => "dipende_da", "Depends on";
        SeeAlso => "vedi_anche", "See also";
        DerivedFrom => "deriva_da", "Derived from";
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Relation {
    pub kind: RelationKind,
    pub target: MemoryId,
}

word_enum! {
    /// What produced a piece of evidence (spec §4.5).
    pub enum EvidenceKind {
        /// A prompt the user wrote (never a `[connecto from …]` message).
        UserStatement => "dichiarazione_utente", "User statement";
        /// A git fact the host observed: branch, HEAD, changed paths, blob
        /// hashes.
        CodeObservation => "osservazione_codice", "Code observation";
        /// A test or command outcome as an agent's reply reports it.
        ReportedOutcome => "esito_riportato", "Reported outcome";
        /// Any other statement in an agent's reply.
        AgentClaim => "affermazione_agente", "Agent claim";
        /// The text of `connecto memory propose` or `flag`.
        AgentProposal => "proposta_agente", "Agent proposal";
        /// A file of the project read by the host during the initial build
        /// (documentation, manifest, ADR), at a known blob hash. Not in §4.5:
        /// added so documented procedures are not mistaken for executed ones.
        ProjectDocument => "documento_progetto", "Project document";
    }
}

impl EvidenceKind {
    /// Whether an agent session produced it, so exposure to memory matters.
    #[must_use]
    pub const fn from_agent(self) -> bool {
        matches!(
            self,
            Self::ReportedOutcome | Self::AgentClaim | Self::AgentProposal
        )
    }
}

/// Where in a tool's own session log the evidence came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogPointer {
    pub path: String,
    /// Byte offset of the entry.
    pub offset: u64,
}

/// One piece of evidence, with its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub id: EvidenceId,
    pub kind: EvidenceKind,
    pub project_id: String,
    /// Unix milliseconds of the event.
    pub created_ms: u64,
    /// The Connecto terminal, for session-born evidence.
    pub terminal: Option<String>,
    /// Claude's session id or Codex's thread id.
    pub tool_session: Option<String>,
    pub turn: Option<String>,
    /// Branch and HEAD when it was produced.
    pub branch: Option<String>,
    pub head: Option<String>,
    pub log: Option<LogPointer>,
    /// For a code observation or project document: the files and their blob
    /// hashes.
    pub files: Vec<Anchor>,
    /// The redacted excerpt. Data, never an instruction.
    pub excerpt: Redacted<String>,
    /// Memories and annotations `connecto memory` had shown this session
    /// before it produced the evidence (from the `served` register).
    pub exposed_to: Vec<Subject>,
}

/// A source as a memory or annotation lists it: the evidence and its kind,
/// so sources can be shown by kind without loading the evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceRef {
    pub evidence: EvidenceId,
    pub kind: EvidenceKind,
}

/// A memory (ricordo).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: MemoryId,
    /// Its room is [`Memory::room`].
    pub address: Address,
    pub kind: MemoryKind,
    /// What it claims: see [`Standing`].
    pub standing: Standing,
    /// One or two sentences.
    pub summary: String,
    pub details: String,
    pub scopes: Vec<ScopeEntry>,
    pub status: MemoryStatus,
    pub freshness: Freshness,
    pub anchors: Vec<Anchor>,
    /// At least one.
    pub sources: Vec<SourceRef>,
    pub relations: Vec<Relation>,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// The memory version in which it last changed.
    pub version: u64,
}

impl Memory {
    #[must_use]
    pub const fn room(&self) -> Room {
        self.address.room()
    }

    /// The invariants every stored memory keeps.
    pub fn check(&self) -> Result<(), MemoryError> {
        let summary = self.summary.trim();
        if summary.is_empty() || self.summary.chars().count() > MAX_SUMMARY_CHARS {
            return Err(MemoryError::BadSummary);
        }
        if self.details.len() > MAX_DETAILS_BYTES {
            return Err(MemoryError::DetailsTooLong);
        }
        if self.sources.is_empty() {
            return Err(MemoryError::NoSources);
        }
        if self.scopes.is_empty() {
            return Err(MemoryError::NoScopes);
        }
        if !self.standing.applies_to(self.kind) {
            return Err(MemoryError::StandingNotForKind);
        }
        if self.relations.iter().any(|r| r.target == self.id) {
            return Err(MemoryError::SelfRelation);
        }
        Ok(())
    }
}

word_enum! {
    /// What an annotation adds.
    #[derive(Default)]
    pub enum AnnotationKind {
        #[default]
        Integration => "integrazione", "Addition";
        Correction => "correzione", "Correction";
        /// Shown right under the memory it contradicts, in every view.
        Contradiction => "contraddizione", "Contradiction";
        Task => "task", "Task";
    }
}

impl AnnotationKind {
    /// Whether it must name the memory it is about.
    #[must_use]
    pub const fn needs_target(self) -> bool {
        matches!(self, Self::Correction | Self::Contradiction)
    }
}

word_enum! {
    pub enum Verification {
        Unverified => "non_verificata", "Unverified";
        VerifiedByEvidence => "verificata_da_evidenza", "Verified by evidence";
        UserStated => "dichiarata_dall_utente", "Stated by the user";
    }
}

/// Who wrote an annotation. Stored as `ciclo_sviluppo`, `agente:<terminal>`
/// or `utente`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AnnotationOrigin {
    DevelopmentCycle,
    /// An agent, through `propose` or `flag`; the terminal comes from the
    /// bridge, never from the agent.
    Agent {
        terminal: String,
    },
    User,
}

impl AnnotationOrigin {
    pub fn parse(text: &str) -> Result<Self, MemoryError> {
        match text {
            "ciclo_sviluppo" => Ok(Self::DevelopmentCycle),
            "utente" => Ok(Self::User),
            _ => match text.strip_prefix("agente:") {
                Some(terminal) if !terminal.is_empty() && is_plain_line(terminal) => {
                    Ok(Self::Agent {
                        terminal: terminal.to_owned(),
                    })
                }
                _ => Err(MemoryError::BadOrigin),
            },
        }
    }
}

impl fmt::Display for AnnotationOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DevelopmentCycle => f.write_str("ciclo_sviluppo"),
            Self::Agent { terminal } => write!(f, "agente:{terminal}"),
            Self::User => f.write_str("utente"),
        }
    }
}

/// What an annotation is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AnnotationTarget {
    Memory(MemoryId),
    /// A new note, with the room suggested for it.
    New {
        room: Room,
    },
}

/// An annotation (postilla): a short sourced note, visible at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub id: AnnotationId,
    /// Increasing per project.
    pub seq: u64,
    pub created_ms: u64,
    pub kind: AnnotationKind,
    pub target: AnnotationTarget,
    pub text: String,
    pub scopes: Vec<Scope>,
    /// At least one.
    pub sources: Vec<SourceRef>,
    pub verification: Verification,
    pub origin: AnnotationOrigin,
    /// What kind of memory the note would become, when known. Not in §4.4:
    /// carries the §5.10 distinctions from the development cycle to the dream.
    pub memory_kind: Option<MemoryKind>,
    /// What the note claims, when known (an agent's proposal is
    /// [`Standing::Proposed`]). Same reason as `memory_kind`.
    pub standing: Option<Standing>,
    /// `None` until a dream absorbs it; then that memory version.
    pub absorbed_in: Option<u64>,
}

impl Annotation {
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.absorbed_in.is_none()
    }

    /// The memory it is about, if any.
    #[must_use]
    pub const fn target_memory(&self) -> Option<&MemoryId> {
        match &self.target {
            AnnotationTarget::Memory(id) => Some(id),
            AnnotationTarget::New { .. } => None,
        }
    }

    /// The invariants every stored annotation keeps.
    pub fn check(&self) -> Result<(), MemoryError> {
        check_note_text(&self.text)?;
        if self.sources.is_empty() {
            return Err(MemoryError::NoSources);
        }
        match &self.target {
            AnnotationTarget::New { room } if !room.holds_memories() => {
                return Err(MemoryError::EntranceHoldsNothing);
            }
            AnnotationTarget::New { .. } if self.kind.needs_target() => {
                return Err(MemoryError::TargetRequired);
            }
            _ => {}
        }
        if let (Some(kind), Some(standing)) = (self.memory_kind, self.standing)
            && !standing.applies_to(kind)
        {
            return Err(MemoryError::StandingNotForKind);
        }
        Ok(())
    }
}

/// The rule for an annotation's text, and for `propose` / `flag`: not blank,
/// at most [`MAX_NOTE_BYTES`]. Refused, never cut.
pub fn check_note_text(text: &str) -> Result<(), MemoryError> {
    if text.trim().is_empty() {
        Err(MemoryError::EmptyText)
    } else if text.len() > MAX_NOTE_BYTES {
        Err(MemoryError::TextTooLong)
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::memory::ids::Lookup;

    const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";

    fn source() -> SourceRef {
        SourceRef {
            evidence: EvidenceId::new(1),
            kind: EvidenceKind::UserStatement,
        }
    }

    fn memory() -> Memory {
        Memory {
            id: MemoryId::from_entropy([1; 5]),
            address: Address::parse("decisioni/sqlite").unwrap(),
            kind: MemoryKind::Decision,
            standing: Standing::Proposed,
            summary: "Use SQLite for the memory store.".to_owned(),
            details: String::new(),
            scopes: vec![ScopeEntry::new(Scope::Project)],
            status: MemoryStatus::Candidate,
            freshness: Freshness::default(),
            anchors: Vec::new(),
            sources: vec![source()],
            relations: Vec::new(),
            created_ms: 1,
            updated_ms: 1,
            version: 1,
        }
    }

    fn annotation() -> Annotation {
        Annotation {
            id: AnnotationId::from_entropy([2; 5]),
            seq: 1,
            created_ms: 1,
            kind: AnnotationKind::Integration,
            target: AnnotationTarget::New {
                room: Room::Decisions,
            },
            text: "We might use SQLite.".to_owned(),
            scopes: vec![Scope::Project],
            sources: vec![source()],
            verification: Verification::Unverified,
            origin: AnnotationOrigin::Agent {
                terminal: "claude-2".to_owned(),
            },
            memory_kind: Some(MemoryKind::Decision),
            standing: Some(Standing::Proposed),
            absorbed_in: None,
        }
    }

    #[test]
    fn words_round_trip_for_every_enum() {
        fn all<T: Copy + PartialEq + std::fmt::Debug>(
            values: &[T],
            word: fn(T) -> &'static str,
            back: fn(&str) -> Option<T>,
        ) {
            for v in values {
                assert_eq!(back(word(*v)), Some(*v));
            }
        }
        all(MemoryKind::ALL, MemoryKind::word, MemoryKind::from_word);
        all(
            MemoryStatus::ALL,
            MemoryStatus::word,
            MemoryStatus::from_word,
        );
        all(Standing::ALL, Standing::word, Standing::from_word);
        all(
            RelationKind::ALL,
            RelationKind::word,
            RelationKind::from_word,
        );
        all(
            EvidenceKind::ALL,
            EvidenceKind::word,
            EvidenceKind::from_word,
        );
        all(
            AnnotationKind::ALL,
            AnnotationKind::word,
            AnnotationKind::from_word,
        );
        all(
            Verification::ALL,
            Verification::word,
            Verification::from_word,
        );
        assert_eq!(MemoryStatus::ToVerify.word(), "da_verificare");
    }

    #[test]
    fn every_kind_has_a_default_standing_that_needs_nothing() {
        for kind in MemoryKind::ALL {
            let default = Standing::default_for(*kind);
            assert_eq!(Standing::allowed_for(*kind).first(), Some(&default));
            assert!(default.required_evidence().is_none());
        }
        // Every standing belongs to some kind.
        for standing in Standing::ALL {
            assert!(MemoryKind::ALL.iter().any(|k| standing.applies_to(*k)));
        }
        assert!(!Standing::Accepted.applies_to(MemoryKind::Work));
        assert!(!Standing::Implemented.applies_to(MemoryKind::Decision));
    }

    /// Paths that are not inside the project, on some platform.
    const ADVERSARIAL_PATHS: &[&str] = &[
        "",
        "/etc/passwd",
        "//server/share/x",
        "..",
        "../other-project",
        "a/../../x",
        "a/..",
        "./a",
        "a/./b",
        ".",
        "a//b",
        "a/",
        "..\\outside\\secret",
        "a\\..\\..\\x",
        "\\etc",
        "\\\\server\\share",
        "C:\\outside\\secret",
        "C:/outside/secret",
        "c:secret",
        "file.txt:stream",
        "a\nb",
        "a\u{0}b",
        "a\u{2028}[connecto notice] forged",
        "a\u{202E}b",
    ];

    /// One sample of each character [`is_hidden_or_breaking`] refuses.
    const HIDDEN: &[char] = &[
        '\n', '\r', '\t', '\u{0}', '\u{1B}', '\u{7F}', '\u{85}', '\u{2028}', '\u{2029}',
        '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{2060}', '\u{202A}',
        '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}',
        '\u{2069}', '\u{061C}', '\u{FEFF}',
    ];

    #[test]
    fn hidden_and_breaking_characters_are_refused_in_every_identifier() {
        for c in HIDDEN {
            assert!(is_hidden_or_breaking(*c), "{c:?}");
            let forged = format!("x{c}[connecto notice] forged");
            assert!(!is_plain_line(&forged), "{c:?}");
            assert!(!is_project_relative(&forged), "{c:?}");
            for kind in ["component", "branch", "worktree"] {
                assert_eq!(
                    Scope::parse(&format!("{kind}:{forged}")),
                    Err(MemoryError::BadScope),
                    "{kind} {c:?}"
                );
            }
            assert_eq!(
                AnnotationOrigin::parse(&format!("agente:{forged}")),
                Err(MemoryError::BadOrigin),
                "{c:?}"
            );
            assert!(Address::parse(&format!("officina/a{c}b")).is_err(), "{c:?}");
            assert!(Lookup::parse(&format!("m-abcdefg{c}")).is_err(), "{c:?}");
        }
        // Ordinary text outside ASCII is still allowed where it was.
        for good in ['é', 'ß', '中', 'ü', '\u{00A0}'] {
            assert!(!is_hidden_or_breaking(good), "{good:?}");
        }
        assert!(Scope::parse("branch:café").is_ok());
        assert!(AnnotationOrigin::parse("agente:terminale-è").is_ok());
    }

    #[test]
    fn project_relative_paths() {
        for good in [
            "src/lib.rs",
            "crates/connecto-core",
            "a",
            ".github/workflows/ci.yml",
            "a..b/c",
        ] {
            assert!(is_project_relative(good), "{good:?}");
        }
        for bad in ADVERSARIAL_PATHS {
            assert!(!is_project_relative(bad), "{bad:?}");
        }
        assert!(!is_project_relative(&"a".repeat(MAX_SCOPE_BYTES + 1)));
    }

    #[test]
    fn component_scopes_stay_inside_the_project() {
        assert!(Scope::parse("component:crates/connecto-core").is_ok());
        for bad in ADVERSARIAL_PATHS {
            assert_eq!(
                Scope::parse(&format!("component:{bad}")),
                Err(MemoryError::BadScope),
                "{bad:?}"
            );
        }
        // Branch names and worktree paths are not project paths.
        assert!(Scope::parse("branch:feature/a..b").is_ok());
        assert!(Scope::parse("worktree:/Users/x/wt").is_ok());
    }

    #[test]
    fn scopes_parse_and_print() {
        for text in [
            "project",
            "component:crates/connecto-core",
            "branch:dev-team/project-memory",
            "worktree:/Users/x/wt",
        ] {
            assert_eq!(Scope::parse(text).unwrap().to_string(), text);
        }
        for bad in [
            "",
            "Project",
            "component:",
            "branch",
            "repo:x",
            "branch:a\nb",
            "branch: x",
        ] {
            assert_eq!(Scope::parse(bad), Err(MemoryError::BadScope), "{bad:?}");
        }
        assert!(Scope::parse(&format!("component:{}", "a".repeat(513))).is_err());
    }

    #[test]
    fn anchors_need_a_relative_path_and_a_blob_hash() {
        assert!(Anchor::new("src/lib.rs", BLOB).is_ok());
        assert!(Anchor::new("src/lib.rs", &"a".repeat(64)).is_ok());
        assert_eq!(Anchor::new("/etc/passwd", BLOB), Err(MemoryError::BadPath));
        assert_eq!(Anchor::new("../x", BLOB), Err(MemoryError::BadPath));
        assert_eq!(Anchor::new("a//b", BLOB), Err(MemoryError::BadPath));
        assert_eq!(Anchor::new("a", "XYZ"), Err(MemoryError::BadBlob));
        for bad in ADVERSARIAL_PATHS {
            assert_eq!(Anchor::new(bad, BLOB), Err(MemoryError::BadPath), "{bad:?}");
        }
        assert_eq!(
            Anchor::new("a", &BLOB.to_uppercase()),
            Err(MemoryError::BadBlob)
        );
    }

    #[test]
    fn memory_invariants() {
        assert_eq!(memory().check(), Ok(()));
        let mut m = memory();
        m.sources.clear();
        assert_eq!(m.check(), Err(MemoryError::NoSources));
        let mut m = memory();
        m.scopes.clear();
        assert_eq!(m.check(), Err(MemoryError::NoScopes));
        let mut m = memory();
        m.summary = "  ".to_owned();
        assert_eq!(m.check(), Err(MemoryError::BadSummary));
        let mut m = memory();
        m.summary = "é".repeat(MAX_SUMMARY_CHARS);
        assert_eq!(m.check(), Ok(()));
        m.summary.push('x');
        assert_eq!(m.check(), Err(MemoryError::BadSummary));
        let mut m = memory();
        m.details = "x".repeat(MAX_DETAILS_BYTES + 1);
        assert_eq!(m.check(), Err(MemoryError::DetailsTooLong));
        let mut m = memory();
        m.standing = Standing::Implemented;
        assert_eq!(m.check(), Err(MemoryError::StandingNotForKind));
        let mut m = memory();
        m.relations.push(Relation {
            kind: RelationKind::SeeAlso,
            target: m.id.clone(),
        });
        assert_eq!(m.check(), Err(MemoryError::SelfRelation));
        assert_eq!(memory().room(), Room::Decisions);
    }

    #[test]
    fn annotation_invariants() {
        assert_eq!(annotation().check(), Ok(()));
        assert!(annotation().is_open());
        let mut a = annotation();
        a.kind = AnnotationKind::Contradiction;
        assert_eq!(a.check(), Err(MemoryError::TargetRequired));
        a.target = AnnotationTarget::Memory(MemoryId::from_entropy([1; 5]));
        assert_eq!(a.check(), Ok(()));
        let mut a = annotation();
        a.target = AnnotationTarget::New {
            room: Room::Entrance,
        };
        assert_eq!(a.check(), Err(MemoryError::EntranceHoldsNothing));
        let mut a = annotation();
        a.text = "x".repeat(MAX_NOTE_BYTES + 1);
        assert_eq!(a.check(), Err(MemoryError::TextTooLong));
        a.text = " \n".to_owned();
        assert_eq!(a.check(), Err(MemoryError::EmptyText));
        let mut a = annotation();
        a.standing = Some(Standing::Implemented);
        assert_eq!(a.check(), Err(MemoryError::StandingNotForKind));
        let mut a = annotation();
        a.sources.clear();
        assert_eq!(a.check(), Err(MemoryError::NoSources));
    }

    #[test]
    fn origins_parse_and_print() {
        for text in ["ciclo_sviluppo", "utente", "agente:claude-2"] {
            assert_eq!(AnnotationOrigin::parse(text).unwrap().to_string(), text);
        }
        for bad in ["agente:", "agent:x", "", "agente:a\u{7}"] {
            assert_eq!(AnnotationOrigin::parse(bad), Err(MemoryError::BadOrigin));
        }
    }
}
