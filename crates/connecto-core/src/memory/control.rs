//! The contract between the UI and the memory engine (spec §5.2, §5.9).
//!
//! Same rules as [`crate::AutomationsControl`] and [`crate::PackagesControl`]:
//! the UI reads [`MemoryControl::view`] each frame (a cheap `Arc` clone, never
//! blocking) and calls the commands below. Every command returns promptly
//! and touches no disk: the work runs on the project's memory thread and its
//! result shows in a later view. Argument mistakes come back as `Err` at
//! once; later failures show in [`ProjectMemoryView::last_error`].
//!
//! One control serves every open project; each command names the project
//! (`ProjectView::id`), as [`crate::PackagesControl`] does.
//!
//! Heavy reads (search, one memory with its annotations) are requests: the
//! command returns a request id at once and the answer appears in
//! [`ProjectMemoryView::reads`].

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;

use super::MemoryError;
use super::ids::{Address, Lookup, MemoryId, Room, Subject};
use super::record::{
    Annotation, AnnotationTarget, Evidence, EvidenceKind, Freshness, Memory, MemoryKind,
    MemoryStatus, Scope, ScopeEntry, Standing, check_note_text,
};
use super::settings::{LocalDate, MemorySettings};
use super::{Capability, Cycle, ModelChoice, Provider};

/// How many runs the view carries, newest first.
pub const MAX_RUNS: usize = 100;
/// How many read answers a project's view keeps, newest first.
pub const MAX_READS: usize = 8;
/// Bytes the engine keeps in front of a statement's words for the fixed
/// sentence naming the subject ("The user accepted m-… in the Connecto
/// window:"), within an annotation's [`MAX_NOTE_BYTES`](super::MAX_NOTE_BYTES).
pub const STATEMENT_SENTENCE_BYTES: usize = 96;
/// Longest text of a [`UserStatement`], in bytes: an annotation's limit less
/// the room of the fixed sentence. A longer text is refused, never cut.
pub const MAX_STATEMENT_BYTES: usize = super::MAX_NOTE_BYTES - STATEMENT_SENTENCE_BYTES;

/// How many statement results a project's view keeps, newest first. A
/// pending one is never dropped; the oldest finished ones go first.
pub const MAX_STATEMENTS: usize = 16;

/// Memories per status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusCounts {
    pub candidate: u32,
    pub verified: u32,
    pub to_verify: u32,
    pub superseded: u32,
    pub conflict: u32,
}

impl StatusCounts {
    #[must_use]
    pub const fn get(&self, status: MemoryStatus) -> u32 {
        match status {
            MemoryStatus::Candidate => self.candidate,
            MemoryStatus::Verified => self.verified,
            MemoryStatus::ToVerify => self.to_verify,
            MemoryStatus::Superseded => self.superseded,
            MemoryStatus::Conflict => self.conflict,
        }
    }

    pub fn add(&mut self, status: MemoryStatus, n: u32) {
        let slot = match status {
            MemoryStatus::Candidate => &mut self.candidate,
            MemoryStatus::Verified => &mut self.verified,
            MemoryStatus::ToVerify => &mut self.to_verify,
            MemoryStatus::Superseded => &mut self.superseded,
            MemoryStatus::Conflict => &mut self.conflict,
        };
        *slot = slot.saturating_add(n);
    }

    #[must_use]
    pub const fn total(&self) -> u32 {
        self.candidate
            .saturating_add(self.verified)
            .saturating_add(self.to_verify)
            .saturating_add(self.superseded)
            .saturating_add(self.conflict)
    }
}

/// One room's memories, by status. The entrance is never listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomCount {
    pub room: Room,
    pub counts: StatusCounts,
}

/// What waits to be considered. Distinct from the memories' status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Backlog {
    /// Events in the queue.
    pub queued: u32,
    /// Unix milliseconds of the oldest queued event.
    pub oldest_queued_ms: Option<u64>,
    /// Batches that used up their attempts (`fallito`).
    pub failed_batches: u32,
    /// Events lost because the channel from the terminals was full (since the
    /// app started).
    pub dropped_on_full: u64,
    /// Why the latest batch failed, if one did.
    pub last_failure: Option<String>,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Running,
    /// Development: annotations written (zero is a valid result) and model
    /// proposals the validator turned down.
    Annotated {
        written: u32,
        rejected: u32,
    },
    /// Dream or initial build: a new memory version.
    Published {
        version: u64,
        applied: u32,
        rejected: u32,
    },
    /// Finished with nothing to change.
    NoChange,
    /// The model proposed operations and none was kept: every one was
    /// refused by the validator or at publication. Not "nothing to change":
    /// `reason` says "N proposed, 0 kept" and why, already safe to show.
    Refused {
        proposed: u32,
        reason: String,
    },
    /// A pause came during the run: nothing was published; it runs again
    /// after Resume.
    HeldByPause,
    /// Put on hold for a cause other than a pause, shown to the user as is
    /// (for example: the CLI has not been trusted for the project folder).
    /// It used no attempt and resumes once the cause is gone. Not a failure.
    Held {
        reason: String,
    },
    /// Stop Everything, the user, or the app closing.
    Cancelled,
    /// Found running at startup (the app had stopped).
    Interrupted,
    Failed {
        reason: String,
    },
}

impl RunOutcome {
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// One run, as the run list shows it. Only measurable quantities: no cost in
/// money (spec §8.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEntry {
    /// Unique within the project.
    pub id: u64,
    pub cycle: Cycle,
    pub choice: ModelChoice,
    pub started_ms: u64,
    /// `None` while running.
    pub duration_ms: Option<u64>,
    pub chars_in: u64,
    pub chars_out: u64,
    pub outcome: RunOutcome,
    /// Dream: the scheduled dates this run also stands for (catch-up).
    pub recovers: Vec<LocalDate>,
}

/// A cycle's present and latest run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleState {
    pub cycle: Cycle,
    pub running: bool,
    pub last: Option<RunEntry>,
}

/// A scheduled dream date with no run of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkippedRun {
    pub date: LocalDate,
    /// The catch-up run that stands for it, once there is one.
    pub recovered_by: Option<u64>,
}

/// One published memory version, for "undo version".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionEntry {
    pub version: u64,
    pub published_ms: u64,
    pub cycle: Cycle,
    /// Operations applied.
    pub operations: u32,
    /// The version this one undid, when it is a revert.
    pub reverts: Option<u64>,
}

/// What one provider can do here, and which choices the UI offers for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapability {
    pub provider: Provider,
    pub capability: Capability,
    /// Model words to offer (empty when unavailable).
    pub models: Vec<String>,
    /// Effort words to offer.
    pub efforts: Vec<String>,
}

word_enum! {
    /// The kinds of source the initial build reads (spec §5.9).
    pub enum SourceClass {
        Documentation => "documentation", "Documentation and instructions";
        Manifests => "manifests", "Manifests";
        Structure => "structure", "Component structure";
        Code => "code", "Relevant code";
        TestConfig => "test_config", "Test configuration";
        GitState => "git_state", "Git state and local changes";
        GitHistory => "git_history", "Recent history";
        Conversations => "conversations", "Connecto conversations";
    }
}

/// Coverage of one kind of source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCoverage {
    pub class: SourceClass,
    pub analysed: u32,
    pub excluded: u32,
    pub remaining: u32,
}

word_enum! {
    /// A known limit of what the initial build could see. Shown so a
    /// finished build is not read as complete knowledge of the project.
    pub enum CoverageGap {
        NotAGitRepository => "no_git", "Not a git repository: no history or local changes";
        NoConversations => "no_conversations", "No Connecto conversations for this project";
        HistoryWindow => "history_window", "History limited to the chosen window";
        InputLimit => "input_limit", "Input limit reached: some sources were not analysed";
        TimeLimit => "time_limit", "Time limit reached: some sources were not analysed";
        UserExclusions => "user_exclusions", "Sources excluded by the user";
        ChangedSinceStart => "changed_since_start", "Sources changed since the build started: to check again";
        RolesUnknown => "roles_unknown", "Some conversations had no readable session log: read from the screen transcript, every line as an agent's claim";
        GlobalGitSettings => "global_git_settings", "Some of your global git settings were not taken (a value not spelled as git writes it, a global file that cannot be read or is a link, a global include, which is not followed, or git's home folder could not be found): git may compare files differently here";
        NestedRepositories => "nested_repositories", "Nested repositories and submodules inside the project are not read";
        GitNotUsable => "git_not_usable", "Git was not used, or not fully: this repository's configuration or layout could make git run a program or read elsewhere, or a git command failed";
        FilesNotRead => "files_not_read", "File contents are not read on this system (links cannot be refused safely here): only names, git state and history";
        PastUserProvenance => "past_user_provenance", "Past conversations: messages typed there are not taken as your statements (nothing proves who typed them); to accept or confirm something, use the Memories tab";
    }
}

/// Where the initial build stands (spec §5.9).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BootstrapState {
    #[default]
    NotStarted,
    Queued,
    Running,
    /// Stopped by a restart, Stop Everything or a pause; it can resume from
    /// its last checkpoint.
    Interrupted,
    /// Nothing partial was published as complete.
    Failed {
        reason: String,
    },
    Completed,
}

/// What the initial build would read, before it starts: shown so the user
/// sees sources and limits (no promise of a cost in money).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapPlan {
    /// The options it was computed for.
    pub options: BootstrapOptions,
    /// `remaining` is what would be read; `excluded` what would not.
    pub coverage: Vec<SourceCoverage>,
    pub gaps: Vec<CoverageGap>,
    /// Characters the selected sources hold.
    pub chars: u64,
    /// How many model runs that makes, at the input limit.
    pub runs: u32,
}

/// The initial build, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BootstrapView {
    /// Persistent id, once one was started.
    pub id: Option<String>,
    pub state: BootstrapState,
    pub packets_done: u32,
    pub packets_total: u32,
    pub coverage: Vec<SourceCoverage>,
    pub gaps: Vec<CoverageGap>,
    pub started_ms: Option<u64>,
    pub updated_ms: Option<u64>,
    /// The latest plan asked for with [`MemoryControl::plan_bootstrap`].
    pub plan: Option<BootstrapPlan>,
    /// Computing a plan right now.
    pub planning: bool,
}

/// The smallest input limit of an initial build, in characters. Its fixed
/// prompt part alone is about 7,000 characters and one usable run needs about
/// 11,250, above the cycles' [`MIN_INPUT_CHARS`](super::settings::MIN_INPUT_CHARS).
pub const MIN_BOOTSTRAP_INPUT_CHARS: u64 = 12_000;

/// Options of an initial build. The defaults read every kind of source over
/// the last 90 days with the dream's model and limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapOptions {
    /// `None`: the dream cycle's model.
    pub choice: Option<ModelChoice>,
    pub sources: Vec<SourceClass>,
    /// Paths or globs relative to the project, never read.
    pub exclude: Vec<String>,
    /// Days of git history and conversations to read.
    pub history_days: u32,
    /// Total characters read across all runs.
    pub max_input_chars: u64,
    pub max_minutes: u32,
}

impl BootstrapOptions {
    pub const HISTORY_DAYS: RangeInclusive<u32> = 1..=3650;
    /// From [`MIN_BOOTSTRAP_INPUT_CHARS`].
    pub const MAX_INPUT_CHARS: RangeInclusive<u64> = MIN_BOOTSTRAP_INPUT_CHARS..=100_000_000;
    pub const MAX_MINUTES: RangeInclusive<u32> = 1..=1440;

    /// Every number brought into its range; `choice`, `sources` and
    /// `exclude` are left as they are.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        self.history_days = self
            .history_days
            .clamp(*Self::HISTORY_DAYS.start(), *Self::HISTORY_DAYS.end());
        self.max_input_chars = self
            .max_input_chars
            .clamp(*Self::MAX_INPUT_CHARS.start(), *Self::MAX_INPUT_CHARS.end());
        self.max_minutes = self
            .max_minutes
            .clamp(*Self::MAX_MINUTES.start(), *Self::MAX_MINUTES.end());
        self
    }
}

impl Default for BootstrapOptions {
    fn default() -> Self {
        Self {
            choice: None,
            sources: SourceClass::ALL.to_vec(),
            exclude: Vec::new(),
            history_days: 90,
            max_input_chars: 2_000_000,
            max_minutes: 60,
        }
    }
}

word_enum! {
    /// What [`MemoryControl::export`] writes, under
    /// `<data>/projects/<id>/memory/exports/`.
    #[derive(Default)]
    pub enum ExportFormat {
        /// One complete JSON file.
        Json => "json", "JSON";
        /// A folder with one Markdown file per room.
        Markdown => "markdown", "Markdown";
        /// Both.
        #[default]
        Both => "both", "JSON and Markdown";
    }
}

/// The latest export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutcome {
    pub format: ExportFormat,
    pub at_ms: u64,
    /// The files or folder written, or why it failed.
    pub result: Result<Vec<PathBuf>, String>,
}

/// A memory as a list shows it: summary, status and freshness apart, sources
/// by kind, and its open annotations beside it (contradictions first). No
/// view hides an open contradiction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCard {
    pub id: MemoryId,
    pub address: Address,
    pub kind: MemoryKind,
    pub standing: Standing,
    pub summary: String,
    pub status: MemoryStatus,
    pub freshness: Freshness,
    /// With `gone` set for a branch or worktree that no longer exists.
    pub scopes: Vec<ScopeEntry>,
    /// How many sources of each kind.
    pub sources: Vec<(EvidenceKind, u32)>,
    pub open: Vec<Annotation>,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchHit {
    Memory(MemoryCard),
    /// An open annotation that matched on its own.
    Annotation(Annotation),
}

/// One memory, whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDetail {
    pub memory: Memory,
    /// Open annotations first (contradictions first), then absorbed ones.
    pub annotations: Vec<Annotation>,
    pub evidence: Vec<Evidence>,
    /// The address asked for, when it was a redirect to this memory.
    pub redirected_from: Option<Address>,
    /// The live memories that replace this one: each memory with a
    /// `sostituisce` relation to it (the relation is stored on the new
    /// memory), followed through merges to the live memory it was merged
    /// into when it was merged away since. Empty when none. A memory that
    /// was itself merged away is never the subject of a detail (the read
    /// follows the merge and says so in `redirected_from`), so its own
    /// merge is not listed here.
    pub superseded_by: Vec<MemoryId>,
}

/// A heavy read the UI asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadRequest {
    Search {
        query: String,
        room: Option<Room>,
        scope: Option<Scope>,
        /// Include the archive.
        all: bool,
    },
    /// A memory (by id or address) or an annotation.
    Open(Lookup),
    /// A room's index.
    Room(Room),
    /// The palace entrance: the whole project's brief, as `connecto memory
    /// brief` renders it (same budget and cleaning), for the opening
    /// overview. A window read: nothing is recorded as served.
    Brief,
}

/// The answer to a [`ReadRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadAnswer {
    Search(Vec<SearchHit>),
    Memory(Box<MemoryDetail>),
    Annotation(Box<Annotation>),
    Room(Vec<MemoryCard>),
    /// The brief's text: plain lines, already cleaned and within budget.
    Brief(String),
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadState {
    Pending,
    Ready(ReadAnswer),
    Failed(String),
}

/// One read and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    /// What [`MemoryControl::request_read`] returned.
    pub id: u64,
    pub request: ReadRequest,
    pub state: ReadState,
}

word_enum! {
    /// What the user says about the memory from the Connecto window itself.
    pub enum UserStatementKind {
        /// Accept a proposal, or confirm a fact, hypothesis or plan (a memory
        /// or an open annotation).
        Accept => "accetta", "Accept";
        /// Turn a proposal down, or deny a fact, hypothesis or plan.
        Reject => "rifiuta", "Reject";
        /// Replacement or corrective text about a memory.
        Correct => "correggi", "Correct";
        /// A free statement, optionally about a memory or for a room.
        Note => "nota", "Note";
    }
}

/// Exactly what the user had on screen when making a statement, so the
/// engine can refuse it when the subject changed in between (its text, its
/// standing or status, a merge, a revert). Equal only for the same subject,
/// in the same version, with the same content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// The memory or annotation as shown: its own id, no redirect or merge
    /// followed.
    pub subject: Subject,
    /// A memory: the memory version in which it last changed. An
    /// annotation: that of the memory it is about (0 for a note for a new
    /// memory).
    pub version: u64,
    /// [`fingerprint`] of what the user reads: for a memory its id, address,
    /// kind, standing, status, summary and details; for an annotation its
    /// id, kind, target, text, memory kind, standing and verification, plus
    /// the fingerprint of the memory it is about.
    pub fingerprint: String,
}

impl Memory {
    /// What the user sees of this memory (see [`Seen`]).
    #[must_use]
    pub fn seen(&self) -> Seen {
        let address = self.address.to_string();
        let version = self.version.to_string();
        Seen {
            subject: Subject::Memory(self.id.clone()),
            version: self.version,
            fingerprint: fingerprint(&[
                "memory",
                self.id.as_str(),
                &version,
                &address,
                self.kind.word(),
                self.standing.word(),
                self.status.word(),
                &self.summary,
                &self.details,
            ]),
        }
    }
}

impl Annotation {
    /// What the user sees of this annotation, shown under (or about)
    /// `target`, the memory it is about as it is now (see [`Seen`]).
    #[must_use]
    pub fn seen(&self, target: Option<&Memory>) -> Seen {
        let about = match &self.target {
            AnnotationTarget::Memory(id) => format!("memory:{id}"),
            AnnotationTarget::New { room } => format!("new:{}", room.word()),
        };
        let shown = target.map(Memory::seen);
        Seen {
            subject: Subject::Annotation(self.id.clone()),
            version: shown.as_ref().map_or(0, |s| s.version),
            fingerprint: fingerprint(&[
                "annotation",
                self.id.as_str(),
                self.kind.word(),
                &about,
                &self.text,
                self.memory_kind.map_or("", MemoryKind::word),
                self.standing.map_or("", Standing::word),
                self.verification.word(),
                shown.as_ref().map_or("", |s| s.fingerprint.as_str()),
            ]),
        }
    }
}

/// The content fingerprint of [`Seen`]: SHA-256 (in lowercase hex) of the
/// parts, each prefixed with its length so no two lists of parts share an
/// encoding. Pure, the same on every platform and build.
#[must_use]
pub fn fingerprint(parts: &[&str]) -> String {
    let mut canonical = Vec::new();
    for part in parts {
        canonical.extend_from_slice(part.len().to_string().as_bytes());
        canonical.push(b':');
        canonical.extend_from_slice(part.as_bytes());
        canonical.push(b';');
    }
    sha256::hex(&canonical)
}

/// SHA-256 (FIPS 180-4), small and dependency-free: `connecto-core` has no
/// hash crate and adding one for a fingerprint is not worth it.
mod sha256 {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    /// The digest of `data`, as 64 lowercase hex characters.
    pub fn hex(data: &[u8]) -> String {
        let mut h: [u32; 8] = [
            0x6a09_e667,
            0xbb67_ae85,
            0x3c6e_f372,
            0xa54f_f53a,
            0x510e_527f,
            0x9b05_688c,
            0x1f83_d9ab,
            0x5be0_cd19,
        ];
        let mut message = data.to_vec();
        let bits = (data.len() as u64).wrapping_mul(8);
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bits.to_be_bytes());
        for block in message.as_chunks::<64>().0 {
            let mut w = [0u32; 64];
            for (slot, word) in w.iter_mut().zip(block.as_chunks::<4>().0) {
                *slot = u32::from_be_bytes(*word);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
            for (k, wi) in K.iter().zip(w.iter()) {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(*k)
                    .wrapping_add(*wi);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
                *slot = slot.wrapping_add(v);
            }
        }
        h.iter().map(|v| format!("{v:08x}")).collect()
    }
}

/// A statement the user made in the Connecto window. Its provenance is
/// certain: unlike a terminal prompt, it cannot have been typed by an agent.
/// The engine records it as `dichiarazione_utente` evidence originating from
/// the UI, plus an annotation with origin `utente` and verification
/// `dichiarata_dall_utente`. It is the only path to a user statement whose
/// provenance does not depend on terminal input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserStatement {
    pub kind: UserStatementKind,
    /// The memory or open annotation it is about. Required for `Accept`,
    /// `Reject` and `Correct`.
    pub about: Option<Subject>,
    /// The room suggested for a note about no memory. Never with `about`,
    /// never the entrance.
    pub room: Option<Room>,
    /// Required for `Correct` and `Note`, at most [`MAX_STATEMENT_BYTES`];
    /// optional for `Accept` and `Reject` (a reason), with the same limit
    /// when given.
    pub text: String,
    /// What was on screen ([`Memory::seen`], [`Annotation::seen`]): required
    /// with `about` (so for `Accept`, `Reject`, `Correct` and a `Note` about
    /// something), never without it, and for that same subject. The engine
    /// refuses the statement when the subject no longer matches it.
    pub seen: Option<Seen>,
}

impl UserStatement {
    /// The rules above.
    pub fn check(&self) -> Result<(), MemoryError> {
        let needs_about = !matches!(self.kind, UserStatementKind::Note);
        if needs_about && self.about.is_none() {
            return Err(MemoryError::TargetRequired);
        }
        match self.kind {
            UserStatementKind::Correct | UserStatementKind::Note => check_note_text(&self.text)?,
            UserStatementKind::Accept | UserStatementKind::Reject if !self.text.is_empty() => {
                check_note_text(&self.text)?;
            }
            UserStatementKind::Accept | UserStatementKind::Reject => {}
        }
        if self.text.len() > MAX_STATEMENT_BYTES {
            return Err(MemoryError::TextTooLong);
        }
        if let Some(room) = self.room {
            if self.about.is_some() {
                return Err(MemoryError::RoomWithTarget);
            }
            if !room.holds_memories() {
                return Err(MemoryError::EntranceHoldsNothing);
            }
        }
        match (&self.about, &self.seen) {
            (None, None) => Ok(()),
            (Some(_), None) => Err(MemoryError::SeenRequired),
            (Some(about), Some(seen)) if *about == seen.subject => Ok(()),
            _ => Err(MemoryError::SeenMismatch),
        }
    }
}

/// Why a statement from the Connecto window was not recorded. Nothing of it
/// was written; the UI keeps the draft and the user may act again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementRefusal {
    /// The memory or note is no longer what was on screen ([`Seen`]): its
    /// text, standing or status changed, it was merged, superseded,
    /// absorbed or restored to another version. Review it and confirm again.
    Changed,
    /// The same statement is already recorded as an open note.
    AlreadyStated,
    /// A dream, a revert or an initial build is publishing now.
    Busy,
    /// No such memory or note in this project.
    NotFound,
    /// Memory was turned off, deleted, or the app closed before it was
    /// recorded.
    Disabled,
    /// The statement itself was refused (empty or too long text...).
    Invalid(String),
    /// Anything else (a database error), as a sentence safe to show.
    Other(String),
}

impl StatementRefusal {
    /// A stable word, for matching and logs.
    #[must_use]
    pub const fn word(&self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::AlreadyStated => "already_stated",
            Self::Busy => "busy",
            Self::NotFound => "not_found",
            Self::Disabled => "disabled",
            Self::Invalid(_) => "invalid",
            Self::Other(_) => "other",
        }
    }

    /// A short label.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Changed => "Changed since you saw it",
            Self::AlreadyStated => "Already stated",
            Self::Busy => "Memory is publishing",
            Self::NotFound => "Not found",
            Self::Disabled => "Memory is off",
            Self::Invalid(_) => "Not valid",
            Self::Other(_) => "Not recorded",
        }
    }

    /// What the user is told.
    #[must_use]
    pub fn sentence(&self) -> String {
        match self {
            Self::Changed => {
                "This changed since you saw it: review it and confirm again.".to_owned()
            }
            Self::AlreadyStated => {
                "You already stated this; it is recorded as an open note.".to_owned()
            }
            Self::Busy => "A dream, a revert or an initial build is running; try again when the \
                           current run finishes."
                .to_owned(),
            Self::NotFound => "This memory or note is not in the project any more.".to_owned(),
            Self::Disabled => {
                "Memory was turned off before your statement was recorded.".to_owned()
            }
            Self::Invalid(why) => format!("The statement was not valid: {why}."),
            Self::Other(why) => format!("Your statement was not recorded: {why}."),
        }
    }
}

/// Where a statement stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementState {
    Pending,
    Recorded {
        /// `true`: applied at once in `version`. `false`: stored as an open
        /// note for the next dream (always so for Correct and Note).
        applied: bool,
        version: Option<u64>,
        /// Why an Accept or Reject was not applied at once, safe to show.
        note: Option<String>,
    },
    Refused(StatementRefusal),
}

impl StatementState {
    /// No more change will come.
    #[must_use]
    pub const fn is_final(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// One statement and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementResult {
    /// What [`MemoryControl::user_statement`] returned.
    pub id: u64,
    pub subject: Option<Subject>,
    pub kind: UserStatementKind,
    pub state: StatementState,
}

/// Keep at most [`MAX_STATEMENTS`] results, newest first: the oldest
/// finished ones go, a pending one never does.
pub fn trim_statements(list: &mut Vec<StatementResult>) {
    while list.len() > MAX_STATEMENTS {
        match list.iter().rposition(|r| r.state.is_final()) {
            Some(i) => {
                list.remove(i);
            }
            None => break,
        }
    }
}

/// What the latest dream (or revert) refused, weakened or lowered. Each
/// sentence is short and already safe to show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DreamReport {
    /// The run it reports on.
    pub run: u64,
    /// The version it published, if any.
    pub version: Option<u64>,
    pub at_ms: u64,
    /// Proposed operations the validator refused, and why.
    pub rejected: Vec<String>,
    /// Operations applied in a weaker form (a status or standing the evidence
    /// did not support), and why.
    pub adjusted: Vec<String>,
    /// Memories and annotations whose status or standing was lowered.
    pub lowered: Vec<Subject>,
}

/// One project's memory, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectMemoryView {
    /// The project id (`ProjectView::id`).
    pub project_id: String,
    pub settings: MemorySettings,
    /// Same as `settings.enabled`.
    pub enabled: bool,
    /// The published memory version (0: nothing published yet).
    pub version: u64,
    /// Rooms that hold memories, in [`Room::ALL`] order.
    pub rooms: Vec<RoomCount>,
    pub totals: StatusCounts,
    pub open_annotations: u32,
    pub open_contradictions: u32,
    /// Freshness of the memory as a whole: Unix milliseconds of the latest
    /// event the development cycle has considered.
    pub considered_through_ms: Option<u64>,
    pub backlog: Backlog,
    /// One per cycle, in [`Cycle::ALL`] order.
    pub cycles: Vec<CycleState>,
    /// Unix milliseconds of the next scheduled dream (only while the app is
    /// open).
    pub next_dream_ms: Option<u64>,
    pub skipped: Vec<SkippedRun>,
    pub bootstrap: BootstrapView,
    /// Newest first, at most [`MAX_RUNS`].
    pub runs: Arc<Vec<RunEntry>>,
    /// Newest first.
    pub versions: Arc<Vec<VersionEntry>>,
    /// Newest first, at most [`MAX_READS`].
    pub reads: Vec<ReadResult>,
    /// The user's statements from this window, newest first, at most
    /// [`MAX_STATEMENTS`] (pending ones always kept).
    pub statements: Vec<StatementResult>,
    /// The user's questions to the memory ("Ask the memory"), oldest first,
    /// newest last, at most [`MAX_ASKS`]; kept in memory only.
    pub asks: Vec<AskEntry>,
    pub last_export: Option<ExportOutcome>,
    /// The latest dream's or revert's report.
    pub last_report: Option<Arc<DreamReport>>,
    /// Why git is not used, or only partly used, for this project (the host's
    /// reason and limits); shown next to coverage. `None`: git is used.
    pub git_note: Option<String>,
    /// The latest failure of a background step, cleared by the next success.
    pub last_error: Option<String>,
}

impl ProjectMemoryView {
    /// A cycle's state, if listed.
    #[must_use]
    pub fn cycle(&self, cycle: Cycle) -> Option<&CycleState> {
        self.cycles.iter().find(|c| c.cycle == cycle)
    }

    /// A read's state, if still kept.
    #[must_use]
    pub fn read(&self, id: u64) -> Option<&ReadResult> {
        self.reads.iter().find(|r| r.id == id)
    }

    /// A statement's result, if still kept.
    #[must_use]
    pub fn statement(&self, id: u64) -> Option<&StatementResult> {
        self.statements.iter().find(|r| r.id == id)
    }

    /// Agents see "initial build in progress" rather than a brief that looks
    /// complete.
    #[must_use]
    pub const fn initializing(&self) -> bool {
        matches!(
            self.bootstrap.state,
            BootstrapState::Queued | BootstrapState::Running | BootstrapState::Interrupted
        )
    }
}

/// Everything the memory screens show.
#[derive(Debug, Clone, Default)]
pub struct MemoryView {
    /// Bumped on every change; compare to skip work.
    pub revision: u64,
    /// One entry per open project.
    pub projects: Vec<ProjectMemoryView>,
    /// One per provider, in [`Provider::ALL`] order.
    pub capabilities: Vec<ProviderCapability>,
}

impl MemoryView {
    /// The entry for a project, if it is open.
    #[must_use]
    pub fn project(&self, project_id: &str) -> Option<&ProjectMemoryView> {
        self.projects.iter().find(|p| p.project_id == project_id)
    }

    /// What a provider can do, if known.
    #[must_use]
    pub fn capability(&self, provider: Provider) -> Option<&ProviderCapability> {
        self.capabilities.iter().find(|c| c.provider == provider)
    }
}

const UNAVAILABLE: &str = "project memory is not available";

pub trait MemoryControl: Send + Sync {
    /// Never blocks.
    fn view(&self) -> Arc<MemoryView>;

    /// Save a project's memory settings (numbers clamped, model words
    /// checked). Turning memory on creates the rooms and starts capture;
    /// turning it off stops capture and both cycles and keeps the data.
    fn set_settings(&self, project_id: &str, settings: MemorySettings) -> Result<(), String> {
        let _ = (project_id, settings);
        Err(UNAVAILABLE.to_owned())
    }

    /// Compute what an initial build would read with these options; the
    /// answer appears in `bootstrap.plan`.
    fn plan_bootstrap(&self, project_id: &str, options: BootstrapOptions) -> Result<(), String> {
        let _ = (project_id, options);
        Err(UNAVAILABLE.to_owned())
    }

    /// Start (or queue) an initial build. On a populated memory it is an
    /// incremental reconciliation, never a reset. Refused while one runs.
    fn bootstrap(&self, project_id: &str, options: BootstrapOptions) -> Result<(), String> {
        let _ = (project_id, options);
        Err(UNAVAILABLE.to_owned())
    }

    /// Stop the initial build; it becomes `Interrupted` and can resume.
    fn cancel_bootstrap(&self, project_id: &str) {
        let _ = project_id;
    }

    /// Resume an interrupted or failed build from its last checkpoint.
    fn resume_bootstrap(&self, project_id: &str) -> Result<(), String> {
        let _ = project_id;
        Err(UNAVAILABLE.to_owned())
    }

    /// "Dream now". Refused while paused or while a dream or build runs.
    fn run_dream_now(&self, project_id: &str) -> Result<(), String> {
        let _ = project_id;
        Err(UNAVAILABLE.to_owned())
    }

    /// Undo a published version, as a new version. Annotations that arrived
    /// after it are untouched. User only: no agent command does this.
    fn revert(&self, project_id: &str, version: u64) -> Result<(), String> {
        let _ = (project_id, version);
        Err(UNAVAILABLE.to_owned())
    }

    /// Write an export; the result shows in `last_export`.
    fn export(&self, project_id: &str, format: ExportFormat) -> Result<(), String> {
        let _ = (project_id, format);
        Err(UNAVAILABLE.to_owned())
    }

    /// Ask for a heavy read. Returns its id at once; the answer appears in
    /// the project's `reads`.
    fn request_read(&self, project_id: &str, request: ReadRequest) -> Result<u64, String> {
        let _ = (project_id, request);
        Err(UNAVAILABLE.to_owned())
    }

    /// Record a statement the user made in the Connecto window (see
    /// [`UserStatement`]). Never blocks. `Err` only for what is known at
    /// once: [`UserStatement::check`] failed, the project is not open, memory
    /// is off, the queue is full. Otherwise the statement's id: its result
    /// appears in the project's `statements`, pending first, then exactly one
    /// final [`StatementState`] (a refusal is typed, [`StatementRefusal`]).
    fn user_statement(&self, project_id: &str, statement: UserStatement) -> Result<u64, String> {
        let _ = (project_id, statement);
        Err(UNAVAILABLE.to_owned())
    }

    /// Ask the project's memory a question: a cheap model answers from the
    /// stored memories only (spec §5.12). Refused while memory is off or
    /// Connecto is paused, for an empty question or one over
    /// [`MAX_ASK_BYTES`], an invalid model choice, or with
    /// [`MAX_PENDING_ASKS`] already pending. Otherwise the ask's id: it
    /// appears in the project's `asks`, pending, then answered or failed.
    /// An answer is not a memory: nothing is written to the store.
    fn ask(&self, project_id: &str, request: AskRequest) -> Result<u64, String> {
        let _ = (project_id, request);
        Err(UNAVAILABLE.to_owned())
    }

    /// Delete the project's memory: store, queue, exports. The UI asks for
    /// confirmation first. Memory is left off.
    fn delete_memory(&self, project_id: &str) -> Result<(), String> {
        let _ = project_id;
        Err(UNAVAILABLE.to_owned())
    }
}

/// The control used when memory is not wired in: an empty view, and every
/// command refused. Lets `connecto-ui` build and run without a backend.
#[derive(Debug, Default)]
pub struct NoMemory;

impl MemoryControl for NoMemory {
    fn view(&self) -> Arc<MemoryView> {
        Arc::new(MemoryView::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_options_clamp_their_numbers() {
        let d = BootstrapOptions::default();
        assert!(d.max_input_chars >= MIN_BOOTSTRAP_INPUT_CHARS);
        assert_eq!(MIN_BOOTSTRAP_INPUT_CHARS, 12_000);
        let just_under = BootstrapOptions {
            max_input_chars: MIN_BOOTSTRAP_INPUT_CHARS - 1,
            ..BootstrapOptions::default()
        }
        .clamped();
        assert_eq!(just_under.max_input_chars, 12_000);
        assert_eq!(d.clone().clamped(), d);
        let low = BootstrapOptions {
            history_days: 0,
            max_input_chars: 0,
            max_minutes: 0,
            ..BootstrapOptions::default()
        }
        .clamped();
        assert_eq!(
            (low.history_days, low.max_input_chars, low.max_minutes),
            (1, MIN_BOOTSTRAP_INPUT_CHARS, 1)
        );
        let high = BootstrapOptions {
            history_days: u32::MAX,
            max_input_chars: u64::MAX,
            max_minutes: u32::MAX,
            exclude: vec!["target".to_owned()],
            ..BootstrapOptions::default()
        }
        .clamped();
        assert_eq!(
            (high.history_days, high.max_input_chars, high.max_minutes),
            (3650, 100_000_000, 1440)
        );
        assert_eq!(high.exclude, ["target"]);
    }

    #[test]
    fn user_statements_are_checked() {
        use UserStatementKind as K;
        let memory = || Some(Subject::Memory(MemoryId::from_entropy([1; 5])));
        let shown = |subject: &Subject| Seen {
            subject: subject.clone(),
            version: 1,
            fingerprint: fingerprint(&["x"]),
        };
        let s = |kind, about: Option<Subject>, room, text: &str| UserStatement {
            kind,
            seen: about.as_ref().map(shown),
            about,
            room,
            text: text.to_owned(),
        };
        let long = "x".repeat(MAX_STATEMENT_BYTES + 1);
        let longest = "x".repeat(MAX_STATEMENT_BYTES);
        const { assert!(MAX_STATEMENT_BYTES < crate::memory::MAX_NOTE_BYTES) };
        let table: Vec<(UserStatement, Result<(), MemoryError>)> = vec![
            (s(K::Accept, memory(), None, ""), Ok(())),
            (s(K::Reject, memory(), None, "not what we chose"), Ok(())),
            (
                s(K::Accept, None, None, ""),
                Err(MemoryError::TargetRequired),
            ),
            (
                s(K::Reject, None, None, "x"),
                Err(MemoryError::TargetRequired),
            ),
            (
                s(K::Correct, None, None, "x"),
                Err(MemoryError::TargetRequired),
            ),
            (
                s(K::Correct, memory(), None, "  "),
                Err(MemoryError::EmptyText),
            ),
            (
                s(K::Correct, memory(), None, &long),
                Err(MemoryError::TextTooLong),
            ),
            (
                s(K::Accept, memory(), None, &long),
                Err(MemoryError::TextTooLong),
            ),
            (s(K::Correct, memory(), None, "the port is 8080"), Ok(())),
            (s(K::Correct, memory(), None, &longest), Ok(())),
            (s(K::Note, None, None, &longest), Ok(())),
            (s(K::Note, None, None, &long), Err(MemoryError::TextTooLong)),
            (s(K::Note, None, None, "we ship on Fridays"), Ok(())),
            (s(K::Note, None, None, ""), Err(MemoryError::EmptyText)),
            (s(K::Note, memory(), None, "about it"), Ok(())),
            (s(K::Note, None, Some(Room::Conventions), "x"), Ok(())),
            (
                s(K::Note, memory(), Some(Room::Conventions), "x"),
                Err(MemoryError::RoomWithTarget),
            ),
            (
                s(K::Accept, memory(), Some(Room::Decisions), ""),
                Err(MemoryError::RoomWithTarget),
            ),
            (
                s(K::Note, None, Some(Room::Entrance), "x"),
                Err(MemoryError::EntranceHoldsNothing),
            ),
            (
                s(
                    K::Accept,
                    Some(Subject::Annotation(
                        crate::memory::AnnotationId::from_entropy([2; 5]),
                    )),
                    None,
                    "",
                ),
                Ok(()),
            ),
        ];
        let unseen = UserStatement {
            seen: None,
            ..s(K::Note, memory(), None, "about it")
        };
        let seen_alone = UserStatement {
            seen: memory().as_ref().map(shown),
            ..s(K::Note, None, None, "free")
        };
        let other = Subject::Memory(MemoryId::from_entropy([3; 5]));
        let seen_other = UserStatement {
            seen: Some(shown(&other)),
            ..s(K::Accept, memory(), None, "")
        };
        let table = table.into_iter().chain([
            (unseen.clone(), Err(MemoryError::SeenRequired)),
            (
                UserStatement {
                    kind: K::Accept,
                    ..unseen
                },
                Err(MemoryError::SeenRequired),
            ),
            (seen_alone, Err(MemoryError::SeenMismatch)),
            (seen_other, Err(MemoryError::SeenMismatch)),
        ]);
        for (statement, expected) in table {
            assert_eq!(statement.check(), expected, "{statement:?}");
        }
        for kind in UserStatementKind::ALL {
            assert_eq!(UserStatementKind::from_word(kind.word()), Some(*kind));
        }
        assert_eq!(
            UserStatementKind::ALL
                .iter()
                .map(|k| k.word())
                .collect::<Vec<_>>(),
            ["accetta", "rifiuta", "correggi", "nota"]
        );
        assert!(
            NoMemory
                .user_statement("x", s(K::Note, None, None, "x"))
                .is_err()
        );
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            sha256::hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256::hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256::hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256::hex(&[b'a'; 1000]),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn fingerprints_follow_what_the_user_reads() {
        use super::super::ids::AnnotationId;
        use super::super::record::{AnnotationKind, AnnotationOrigin, Verification};
        // Parts are length-prefixed: a boundary moved is another list.
        assert_ne!(fingerprint(&["ab", "c"]), fingerprint(&["a", "bc"]));
        let memory = Memory {
            id: MemoryId::from_entropy([1; 5]),
            address: Address::parse("decisioni/use-sqlite").unwrap_or_else(|_| unreachable!()),
            kind: MemoryKind::Decision,
            standing: Standing::Proposed,
            summary: "Use SQLite.".to_owned(),
            details: String::new(),
            scopes: vec![ScopeEntry::new(Scope::Project)],
            status: MemoryStatus::Candidate,
            freshness: Freshness::default(),
            anchors: Vec::new(),
            sources: Vec::new(),
            relations: Vec::new(),
            created_ms: 0,
            updated_ms: 0,
            version: 3,
        };
        let seen = memory.seen();
        assert_eq!(seen.subject, Subject::Memory(memory.id.clone()));
        assert_eq!((seen.version, seen.fingerprint.len()), (3, 64));
        // Freshness is not read as content.
        let mut fresher = memory.clone();
        fresher.freshness.open_annotations = 4;
        assert_eq!(fresher.seen(), seen);
        let changes: [fn(&mut Memory); 6] = [
            |m| m.summary.push('!'),
            |m| m.details.push('!'),
            |m| m.standing = Standing::Accepted,
            |m| m.status = MemoryStatus::Superseded,
            |m| m.version = 4,
            |m| m.id = MemoryId::from_entropy([2; 5]),
        ];
        for change in changes {
            let mut m = memory.clone();
            change(&mut m);
            assert_ne!(m.seen().fingerprint, seen.fingerprint);
        }
        let note = Annotation {
            id: AnnotationId::from_entropy([5; 5]),
            seq: 1,
            created_ms: 0,
            kind: AnnotationKind::Integration,
            target: AnnotationTarget::Memory(memory.id.clone()),
            text: "Also WAL.".to_owned(),
            scopes: vec![Scope::Project],
            sources: Vec::new(),
            verification: Verification::Unverified,
            origin: AnnotationOrigin::User,
            memory_kind: Some(MemoryKind::Decision),
            standing: None,
            absorbed_in: None,
        };
        let a = note.seen(Some(&memory));
        assert_eq!(a.version, 3);
        let mut changed = memory.clone();
        changed.summary = "Use Postgres.".to_owned();
        assert_ne!(note.seen(Some(&changed)), a, "its memory changed");
        let mut edited = note.clone();
        edited.text.push('!');
        assert_ne!(edited.seen(Some(&memory)), a);
        assert_eq!(note.seen(Some(&memory)), a);
    }

    #[test]
    fn statement_results_keep_the_pending_ones() {
        let r = |id, state| StatementResult {
            id,
            subject: None,
            kind: UserStatementKind::Note,
            state,
        };
        let done = || StatementState::Refused(StatementRefusal::Busy);
        let mut list: Vec<StatementResult> = (0..MAX_STATEMENTS as u64 + 4)
            .rev()
            .map(|id| {
                if id < 3 {
                    r(id, StatementState::Pending)
                } else {
                    r(id, done())
                }
            })
            .collect();
        trim_statements(&mut list);
        assert_eq!(list.len(), MAX_STATEMENTS);
        let ids: Vec<u64> = list.iter().map(|x| x.id).collect();
        assert!(ids.contains(&0) && ids.contains(&1) && ids.contains(&2));
        assert!(!ids.contains(&3) && !ids.contains(&6) && ids.contains(&7));
        let mut all_pending: Vec<StatementResult> = (0..MAX_STATEMENTS as u64 + 2)
            .map(|id| r(id, StatementState::Pending))
            .collect();
        trim_statements(&mut all_pending);
        assert_eq!(all_pending.len(), MAX_STATEMENTS + 2);
        let p = ProjectMemoryView {
            statements: list,
            ..ProjectMemoryView::default()
        };
        assert!(p.statement(1).is_some_and(|x| !x.state.is_final()));
        assert!(p.statement(3).is_none());
        for refusal in [
            StatementRefusal::Changed,
            StatementRefusal::AlreadyStated,
            StatementRefusal::Busy,
            StatementRefusal::NotFound,
            StatementRefusal::Disabled,
            StatementRefusal::Invalid("x".to_owned()),
            StatementRefusal::Other("y".to_owned()),
        ] {
            assert!(!refusal.word().is_empty() && !refusal.label().is_empty());
            assert!(refusal.sentence().ends_with('.'));
        }
    }

    #[test]
    fn status_counts_add_up() {
        let mut c = StatusCounts::default();
        c.add(MemoryStatus::Verified, 2);
        c.add(MemoryStatus::Conflict, 1);
        c.add(MemoryStatus::Verified, 1);
        assert_eq!(c.get(MemoryStatus::Verified), 3);
        assert_eq!(c.total(), 4);
        c.add(MemoryStatus::Candidate, u32::MAX);
        assert_eq!(c.total(), u32::MAX);
    }

    #[test]
    fn the_empty_control_refuses_everything() {
        let control = NoMemory;
        assert!(control.view().projects.is_empty());
        assert!(control.view().project("x").is_none());
        assert!(
            control
                .set_settings("x", MemorySettings::default())
                .is_err()
        );
        assert!(control.run_dream_now("x").is_err());
        assert!(
            control
                .request_read("x", ReadRequest::Room(Room::Workshop))
                .is_err()
        );
        control.cancel_bootstrap("x");
    }

    #[test]
    fn initializing_covers_an_unfinished_build() {
        let mut p = ProjectMemoryView::default();
        assert!(!p.initializing());
        p.bootstrap.state = BootstrapState::Running;
        assert!(p.initializing());
        p.bootstrap.state = BootstrapState::Failed {
            reason: "x".to_owned(),
        };
        assert!(!p.initializing());
    }

    #[test]
    fn words_round_trip() {
        for v in SourceClass::ALL {
            assert_eq!(SourceClass::from_word(v.word()), Some(*v));
        }
        for v in CoverageGap::ALL {
            assert_eq!(CoverageGap::from_word(v.word()), Some(*v));
        }
        for v in ExportFormat::ALL {
            assert_eq!(ExportFormat::from_word(v.word()), Some(*v));
        }
        assert_eq!(
            BootstrapOptions::default().sources.len(),
            SourceClass::ALL.len()
        );
    }
}

// ---- Ask the memory (spec §5.12)

/// Longest question, in bytes.
pub const MAX_ASK_BYTES: usize = 2000;
/// Asks a project keeps in its view (oldest finished ones go first).
pub const MAX_ASKS: usize = 8;
/// Asks waiting for an answer at once, per project.
pub const MAX_PENDING_ASKS: usize = 2;

/// A question to the memory, and the model that answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRequest {
    pub question: String,
    pub choice: ModelChoice,
}

/// One question and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskEntry {
    /// What [`MemoryControl::ask`] returned.
    pub id: u64,
    pub question: String,
    pub choice: ModelChoice,
    /// Unix milliseconds.
    pub asked_ms: u64,
    pub state: AskState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskState {
    Pending,
    Answered {
        /// The model's answer, cleaned (untrusted text: control characters
        /// gone, no line can pass for a Connecto line). Several lines.
        text: String,
        /// The addresses (`room/slug`) of the memories the answer cited,
        /// each one that exists.
        used: Vec<String>,
        /// The memory version the answer was given from.
        version: u64,
    },
    /// Why there is no answer (cancelled, paused, timed out, the model or
    /// the provider failed), safe to show.
    Failed(String),
}

impl AskState {
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

/// The cheap models offered to answer, one per provider, from the utility
/// models (`crate::utility`): Claude Haiku at low effort, and Codex on its
/// own configured model ([`crate::utility::DEFAULT_MODEL`]) at low effort.
#[must_use]
pub fn ask_choices() -> Vec<ModelChoice> {
    use crate::utility::{UtilityEffort, UtilityModel};
    [UtilityModel::Haiku, UtilityModel::Codex]
        .into_iter()
        .map(|m| ModelChoice::new(m.provider(), m.model_word(), UtilityEffort::Low.cli_value()))
        .collect()
}

#[cfg(test)]
mod ask_tests {
    use super::*;

    #[test]
    fn the_ask_choices_are_one_cheap_model_per_provider() {
        let choices = ask_choices();
        assert_eq!(
            choices,
            vec![
                ModelChoice::new(Provider::Claude, "haiku", "low"),
                ModelChoice::new(Provider::Codex, "default", "low"),
            ]
        );
        assert!(choices.iter().all(ModelChoice::is_valid));
    }
}
