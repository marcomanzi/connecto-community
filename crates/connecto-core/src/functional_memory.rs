//! Functional memory (ADR 0033): a project's `.functionalMemory/` folder, a
//! few Markdown files that say what the project does, kept up to date by
//! Connecto when the main branch advances. Read by a peer's voice call as
//! context; updated by hand from Project Settings or a peer.

/// The folder at the repository root.
pub const FOLDER: &str = ".functionalMemory";
/// The file Connecto writes itself: the commit the memory describes.
pub const STATE_FILE: &str = "state.json";
/// The index, listed first.
pub const INDEX_FILE: &str = "README.md";
/// The most bytes of one file handed out.
pub const MAX_FILE_BYTES: usize = 32 * 1024;
/// The most bytes of all files handed out together.
pub const MAX_TOTAL_BYTES: usize = 64 * 1024;
/// The most files handed out.
pub const MAX_FILES: usize = 16;

/// Where a project's functional memory stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FmState {
    /// Never built here, and nothing on disk.
    #[default]
    None,
    /// Up to date with what was last seen of main.
    Idle,
    /// Main moved: an update starts once it has been still for a while.
    Waiting,
    Running,
    /// An update is due but cannot run now (main not checked out, a merge
    /// or rebase in progress, Pause); tried again later.
    Deferred,
    /// The last update failed; tried again when main moves again.
    Failed,
    /// Automatic updates are off for the project.
    Off,
    /// Not known here yet: another machine's project before that machine
    /// answered. Never sent by a host.
    Unknown,
}

impl FmState {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Idle => "idle",
            Self::Waiting => "waiting",
            Self::Running => "running",
            Self::Deferred => "deferred",
            Self::Failed => "failed",
            Self::Off => "off",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "none" => Self::None,
            "idle" => Self::Idle,
            "waiting" => Self::Waiting,
            "running" => Self::Running,
            "deferred" => Self::Deferred,
            "failed" => Self::Failed,
            "off" => Self::Off,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

/// One project's status, for the window and a peer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FmStatus {
    pub state: FmState,
    /// The commit the memory on disk describes (from `state.json`).
    pub commit: Option<String>,
    /// When it was last updated, ms since the epoch.
    pub updated_ms: Option<u64>,
    /// A short human text: why deferred or failed, or what ran last.
    pub message: Option<String>,
    /// Whether it updates by itself when main advances.
    pub auto: bool,
    /// The model an update runs on: the one chosen for the project, or the
    /// default ([`default_model`]). `None` while it is not known which CLIs
    /// can run here, or when none can.
    pub model: Option<FmModel>,
    /// The model chosen for the project, whether its CLI can run here now
    /// or not (then `model` is the default); `None`: the default.
    pub chosen: Option<FmModel>,
    /// What can be chosen: the models of the CLIs that can run here.
    pub models: Vec<FmModel>,
    /// Whether the CLIs were asked yet: an empty `models` then means none
    /// can run here.
    pub models_known: bool,
    /// Whether the model can be chosen at all: `false` for a host from
    /// before the choice (it says nothing of models).
    pub model_choice: bool,
}

/// A model an update can run on: the CLI and the model word it takes
/// (`claude --model sonnet`, `codex -m gpt-6.2-sol`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FmModel {
    pub provider: crate::memory::Provider,
    pub model: String,
}

impl FmModel {
    #[must_use]
    pub fn new(provider: crate::memory::Provider, model: &str) -> Self {
        Self {
            provider,
            model: model.to_owned(),
        }
    }

    /// Whether it may be passed to its CLI (a plain word, never a flag).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        crate::memory::ModelChoice::is_valid_word(&self.model)
    }

    /// What a menu shows: `Claude · sonnet`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} · {}", self.provider.label(), self.model)
    }
}

/// The reasoning effort an update runs with.
pub const EFFORT: &str = "medium";

/// The model an update runs on by default, from what can run here, in this
/// order: Claude Sonnet, else Codex on GPT-6.2 Sol (or, when its list does
/// not name it, the first Codex model offered).
#[must_use]
pub fn default_model(offered: &[FmModel]) -> Option<FmModel> {
    use crate::memory::Provider;
    let find = |provider: Provider, model: &str| {
        offered
            .iter()
            .find(|m| m.provider == provider && m.model == model)
            .cloned()
    };
    find(Provider::Claude, "sonnet")
        .or_else(|| find(Provider::Codex, "gpt-6.2-sol"))
        .or_else(|| {
            offered
                .iter()
                .find(|m| m.provider == Provider::Codex)
                .cloned()
        })
        .or_else(|| offered.first().cloned())
}

/// One file of the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FmFile {
    /// Relative to the folder (top level only).
    pub path: String,
    pub text: String,
}

/// The folder's files, capped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FmFiles {
    /// The index first, then by name.
    pub files: Vec<FmFile>,
    /// Something was cut or left out by the caps.
    pub truncated: bool,
}

const UNAVAILABLE: &str = "functional memory is not available on this machine";

/// What the window and a peer can ask of functional memory.
pub trait FunctionalMemoryControl: Send + Sync {
    /// Never blocks.
    fn status(&self, project_id: &str) -> FmStatus {
        let _ = project_id;
        FmStatus::default()
    }

    /// The folder's files. Reads a few small files.
    ///
    /// # Errors
    /// No such open project, or unavailable.
    fn files(&self, project_id: &str) -> Result<FmFiles, String> {
        let _ = project_id;
        Err(UNAVAILABLE.to_owned())
    }

    /// Update now (a full build when there is none yet). Returns at once.
    ///
    /// # Errors
    /// Why it cannot start.
    fn update_now(&self, project_id: &str) -> Result<(), String> {
        let _ = project_id;
        Err(UNAVAILABLE.to_owned())
    }

    /// Turn automatic updates on or off for the project.
    ///
    /// # Errors
    /// Why it was not saved.
    fn set_auto(&self, project_id: &str, on: bool) -> Result<(), String> {
        let _ = (project_id, on);
        Err(UNAVAILABLE.to_owned())
    }

    /// Run the project's updates on `model` (one of the status's `models`),
    /// or on the default with `None`.
    ///
    /// # Errors
    /// A model not offered here, or why it was not saved.
    fn set_model(&self, project_id: &str, model: Option<FmModel>) -> Result<(), String> {
        let _ = (project_id, model);
        Err(UNAVAILABLE.to_owned())
    }
}

/// No functional memory (tests, builds without the host).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoFunctionalMemory;

impl FunctionalMemoryControl for NoFunctionalMemory {}

/// Whether `name` may be a file of the folder written from a model's reply:
/// a plain Markdown file name at the top level, never `state.json`.
#[must_use]
pub fn valid_doc_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    !stem.is_empty()
        && name.len() <= 64
        && stem
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !stem.contains("..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_names_are_plain_markdown_files() {
        for ok in ["README.md", "overview.md", "key-flows.md", "a_b.v2.md"] {
            assert!(valid_doc_name(ok), "{ok}");
        }
        for bad in [
            "state.json",
            ".md",
            "../x.md",
            "a/b.md",
            ".hidden.md",
            "x.txt",
            "a..b.md",
            "a b.md",
            "-x.md",
        ] {
            assert!(!valid_doc_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_default_model_is_sonnet_then_codex_sol() {
        use crate::memory::Provider;
        let claude = |m: &str| FmModel::new(Provider::Claude, m);
        let codex = |m: &str| FmModel::new(Provider::Codex, m);
        assert_eq!(default_model(&[]), None);
        assert_eq!(
            default_model(&[claude("opus"), claude("sonnet"), codex("gpt-6.2-sol")]),
            Some(claude("sonnet"))
        );
        assert_eq!(
            default_model(&[codex("gpt-6.2-astra"), codex("gpt-6.2-sol")]),
            Some(codex("gpt-6.2-sol"))
        );
        assert_eq!(
            default_model(&[codex("gpt-6.2-astra")]),
            Some(codex("gpt-6.2-astra"))
        );
        assert!(!FmModel::new(Provider::Claude, "--danger").is_valid());
    }

    #[test]
    fn state_names_round_trip() {
        for s in [
            FmState::None,
            FmState::Idle,
            FmState::Waiting,
            FmState::Running,
            FmState::Deferred,
            FmState::Failed,
            FmState::Off,
            FmState::Unknown,
        ] {
            assert_eq!(FmState::from_name(s.name()), Some(s));
        }
    }
}
