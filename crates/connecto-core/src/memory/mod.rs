//! Project memory (the "memory palace"): the shared vocabulary of the store,
//! the two cycles, the `connecto memory` command and the UI.
//!
//! Everything here is plain data and pure rules: no SQLite, no PTY, no disk.
//! `connecto-memory` owns the store and the cycles, `connecto-hook` the bridge
//! verb, `connecto-session::project_settings` the `memory` key of a project's
//! `settings.json`, and the UI reads [`MemoryControl::view`].
//!
//! Every enum that is stored or crosses a process boundary has a stable
//! [`word`](Room::word) (the Italian names of the spec for memory content, the
//! English keys of `settings.json` for configuration) and a `from_word` that
//! reads it back. As elsewhere in this crate there is no serde derive: the
//! word is the wire form.
//!
//! Two axes are never collapsed into one:
//! - [`MemoryStatus`] says how reliable a record is (candidate, verified ...);
//! - [`Freshness`] says how recently it was confirmed and whether what it
//!   points at changed since.
//!
//! And [`Standing`] says what a record claims (a proposal or an accepted
//! decision, a plan or a finished implementation, a hypothesis or an
//! established cause), independent of both. See [`confirm`] for the rules
//! that decide which evidence may raise status or standing.

use std::fmt;

/// An enum with a stable stored word and a label for the UI.
macro_rules! word_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $word:literal, $label:literal; )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// Every value, in display order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// The stable word stored on disk and used on the wire.
            #[must_use]
            pub const fn word(self) -> &'static str {
                match self {
                    $( Self::$variant => $word, )+
                }
            }

            /// The value whose [`word`](Self::word) is `word`.
            #[must_use]
            pub fn from_word(word: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|v| v.word() == word)
            }

            /// What the UI shows.
            #[must_use]
            pub const fn label(self) -> &'static str {
                match self {
                    $( Self::$variant => $label, )+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.word())
            }
        }
    };
}

pub mod bridge;
pub mod command;
pub mod confirm;
pub mod control;
pub mod ids;
pub mod record;
pub mod settings;

pub use bridge::{MemoryBridge, MemoryCaller, NOT_ENABLED, NoMemoryBridge};
pub use command::{CommandError, MemoryCommand, Verb};
pub use confirm::{independent_confirmations, is_independent, may_verify, supports_standing};
pub use control::{
    AskEntry, AskRequest, AskState, Backlog, BootstrapOptions, BootstrapPlan, BootstrapState,
    BootstrapView, CoverageGap, CycleState, DreamReport, ExportFormat, ExportOutcome,
    MAX_ASK_BYTES, MAX_ASKS, MAX_PENDING_ASKS, MAX_READS, MAX_RUNS, MAX_STATEMENT_BYTES,
    MAX_STATEMENTS, MIN_BOOTSTRAP_INPUT_CHARS, MemoryCard, MemoryControl, MemoryDetail, MemoryView,
    NoMemory, ProjectMemoryView, ProviderCapability, ReadAnswer, ReadRequest, ReadResult,
    ReadState, RoomCount, RunEntry, RunOutcome, STATEMENT_SENTENCE_BYTES, SearchHit, Seen,
    SkippedRun, SourceClass, SourceCoverage, StatementRefusal, StatementResult, StatementState,
    StatusCounts, UserStatement, UserStatementKind, VersionEntry, ask_choices, fingerprint,
    trim_statements,
};
pub use ids::{Address, AnnotationId, EvidenceId, Lookup, MemoryId, Room, Subject};
pub use record::{
    Anchor, Annotation, AnnotationKind, AnnotationOrigin, AnnotationTarget, Evidence, EvidenceKind,
    Freshness, LogPointer, MAX_DETAILS_BYTES, MAX_NOTE_BYTES, MAX_SCOPE_BYTES, MAX_SUMMARY_CHARS,
    Memory, MemoryKind, MemoryStatus, Relation, RelationKind, Scope, ScopeEntry, SourceRef,
    Standing, Verification, check_note_text, is_hidden_or_breaking, is_plain_line,
    is_project_relative,
};
pub use settings::{
    DevelopmentSettings, DreamSettings, LocalDate, LocalTime, MIN_INPUT_CHARS, MemorySettings,
    Weekday,
};

word_enum! {
    /// The two cycles, and the one-off construction from an existing project
    /// (spec §5.9). Also the keys of the `memory` object in `settings.json`.
    pub enum Cycle {
        Development => "development", "Development";
        Dream => "dream", "Dream";
        Bootstrap => "bootstrap", "Initial build";
    }
}

word_enum! {
    /// Which CLI runs a memory job.
    pub enum Provider {
        Claude => "claude", "Claude";
        Codex => "codex", "Codex";
    }
}

/// Provider, model and reasoning effort of one cycle. `model` and `effort`
/// are free words so a new model needs no new build; they become CLI
/// arguments, so only [`ModelChoice::is_valid_word`] values are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelChoice {
    pub provider: Provider,
    pub model: String,
    pub effort: String,
}

impl ModelChoice {
    /// Longest model or effort word.
    pub const MAX_WORD: usize = 64;

    #[must_use]
    pub fn new(provider: Provider, model: &str, effort: &str) -> Self {
        Self {
            provider,
            model: model.to_owned(),
            effort: effort.to_owned(),
        }
    }

    /// Whether `word` may be a model or effort: 1 to 64 of ASCII letters,
    /// digits and `. _ : - [ ]`, not starting with `-` (it would read as a
    /// flag).
    #[must_use]
    pub fn is_valid_word(word: &str) -> bool {
        !word.is_empty()
            && word.len() <= Self::MAX_WORD
            && !word.starts_with('-')
            && word.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-' | '[' | ']')
            })
    }

    /// Both words are valid.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        Self::is_valid_word(&self.model) && Self::is_valid_word(&self.effort)
    }
}

impl fmt::Display for ModelChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} ({})",
            self.provider.label(),
            self.model,
            self.effort
        )
    }
}

/// Whether a provider (or one model choice) can run memory jobs here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    /// Its CLI can run memory jobs here.
    Available,
    /// It cannot run memory jobs; the reason is shown to the user as is
    /// ("codex not installed", "claude: not logged in", "folder not trusted").
    Unavailable { reason: String },
}

impl Capability {
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// The reason, when unavailable.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Unavailable { reason } => Some(reason),
        }
    }
}

/// Why a memory value was refused. Messages never repeat the refused input:
/// it may come from an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    #[error("not a memory id (m- and 8 base32 characters)")]
    BadMemoryId,
    #[error("not an annotation id (p- and 8 base32 characters)")]
    BadAnnotationId,
    #[error("not an evidence id (e- and a number)")]
    BadEvidenceId,
    #[error("not an id or an address (room/slug)")]
    BadLookup,
    #[error("not a room")]
    BadRoom,
    #[error("the entrance is generated: no memory lives in it")]
    EntranceHoldsNothing,
    #[error("not an address slug (lowercase letters, digits, single hyphens)")]
    BadSlug,
    #[error("not a scope (project, component:<path>, branch:<name>, worktree:<path>)")]
    BadScope,
    #[error("the summary is empty or too long")]
    BadSummary,
    #[error("the details are too long")]
    DetailsTooLong,
    #[error("the text is empty")]
    EmptyText,
    #[error("the text is too long")]
    TextTooLong,
    #[error("at least one source is required")]
    NoSources,
    #[error("at least one scope is required")]
    NoScopes,
    #[error("this standing does not apply to this kind of memory")]
    StandingNotForKind,
    #[error("a memory cannot relate to itself")]
    SelfRelation,
    #[error("a correction or contradiction must name the memory it is about")]
    TargetRequired,
    #[error("not an annotation origin")]
    BadOrigin,
    #[error("not a blob hash")]
    BadBlob,
    #[error("not a file path relative to the project")]
    BadPath,
    #[error("a room is only suggested for a statement about no memory")]
    RoomWithTarget,
    #[error("a statement about a memory or note must say what was on screen")]
    SeenRequired,
    #[error("what was on screen is not what the statement is about")]
    SeenMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_round_trip_for_every_enum() {
        for v in Cycle::ALL {
            assert_eq!(Cycle::from_word(v.word()), Some(*v));
        }
        for v in Provider::ALL {
            assert_eq!(Provider::from_word(v.word()), Some(*v));
        }
        assert_eq!(Cycle::from_word("Dream"), None);
    }

    #[test]
    fn model_words_refuse_flags_and_spaces() {
        assert!(ModelChoice::is_valid_word("haiku"));
        assert!(ModelChoice::is_valid_word("claude-opus-4-1[1m]"));
        assert!(ModelChoice::is_valid_word("gpt-5.1"));
        assert!(!ModelChoice::is_valid_word(""));
        assert!(!ModelChoice::is_valid_word("--tools"));
        assert!(!ModelChoice::is_valid_word("a b"));
        assert!(!ModelChoice::is_valid_word(&"x".repeat(65)));
        assert!(!ModelChoice::new(Provider::Claude, "haiku", "lo w").is_valid());
    }

    #[test]
    fn capability_reports_its_reason() {
        assert!(Capability::Available.is_available());
        let no = Capability::Unavailable {
            reason: "codex not installed".to_owned(),
        };
        assert!(!no.is_available());
        assert_eq!(no.reason(), Some("codex not installed"));
    }
}
