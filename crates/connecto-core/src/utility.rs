//! Utility sessions: a hidden sub-run of the Claude CLI that gives a terminal a
//! title of two to five words. Started only by the user (the robot icon on a
//! terminal row). The UI reads runs with [`crate::TerminalsControl::utility_runs`]
//! and the settings with [`crate::WorkspaceControl::utility_settings`].

/// The model a utility run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UtilityModel {
    #[default]
    Haiku,
    Sonnet,
    Opus,
}

impl UtilityModel {
    /// The choices, in menu order.
    pub const ALL: [UtilityModel; 3] = [Self::Haiku, Self::Sonnet, Self::Opus];

    /// The value passed to `claude --model`, and stored in `settings.json`.
    #[must_use]
    pub fn cli_value(self) -> &'static str {
        match self {
            Self::Haiku => "haiku",
            Self::Sonnet => "sonnet",
            Self::Opus => "opus",
        }
    }

    /// The choice whose [`UtilityModel::cli_value`] is `value`.
    #[must_use]
    pub fn from_cli(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.cli_value() == value)
    }

    /// What the menu shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Haiku => "Haiku",
            Self::Sonnet => "Sonnet",
            Self::Opus => "Opus",
        }
    }
}

/// The effort a utility run uses (`claude --effort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UtilityEffort {
    #[default]
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl UtilityEffort {
    /// The choices, in menu order.
    pub const ALL: [UtilityEffort; 5] =
        [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    /// The value passed to `claude --effort`, and stored in `settings.json`.
    #[must_use]
    pub fn cli_value(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The choice whose [`UtilityEffort::cli_value`] is `value`.
    #[must_use]
    pub fn from_cli(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.cli_value() == value)
    }

    /// What the menu shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Xhigh => "Extra high",
            Self::Max => "Max",
        }
    }
}

/// App-wide utility settings. Default: Haiku, low.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UtilitySettings {
    pub model: UtilityModel,
    pub effort: UtilityEffort,
}

/// Where a run stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UtilityStatus {
    Running,
    /// The terminal was renamed to this title.
    Done {
        title: String,
    },
    /// The name was left unchanged. A short reason for the list.
    Failed {
        reason: String,
    },
}

/// One titling run, as View > Utility Sessions lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtilityRun {
    /// Unique within the app run.
    pub id: u64,
    pub project: String,
    pub terminal_id: String,
    /// The terminal's name (or its id when unnamed) when the run started.
    pub terminal_label: String,
    /// Milliseconds since the Unix epoch.
    pub started_ms: u64,
    /// `None` while running.
    pub finished_ms: Option<u64>,
    pub model: UtilityModel,
    pub effort: UtilityEffort,
    pub status: UtilityStatus,
}

/// How many runs are kept (newest first).
pub const MAX_UTILITY_RUNS: usize = 50;
