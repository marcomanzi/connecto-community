//! Utility sessions: a hidden sub-run of an agent CLI that gives a terminal a
//! title of two to five words. Started only by the user (the robot icon on a
//! terminal row). The UI reads runs with [`crate::TerminalsControl::utility_runs`]
//! and the settings with [`crate::WorkspaceControl::utility_settings`].
//!
//! A model names its provider ([`UtilityModel::provider`]), so the one menu
//! picks both. The values stored before Codex was a choice read back unchanged.

use crate::memory::{ModelChoice, Provider};

/// The model word that leaves a CLI on its own default model: nothing is
/// passed for the model.
pub const DEFAULT_MODEL: &str = "default";

/// The model to pass to the CLI for `choice`, or `None` for its default.
#[must_use]
pub fn cli_model(choice: &ModelChoice) -> Option<&str> {
    (choice.model != DEFAULT_MODEL).then_some(choice.model.as_str())
}

/// The model a utility run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UtilityModel {
    #[default]
    Haiku,
    Sonnet,
    Opus,
    /// Codex, on the model its own configuration names.
    Codex,
}

impl UtilityModel {
    /// The choices, in menu order.
    pub const ALL: [UtilityModel; 4] = [Self::Haiku, Self::Sonnet, Self::Opus, Self::Codex];

    /// The value stored in `settings.json` (and, for Claude, passed to
    /// `claude --model`).
    #[must_use]
    pub fn cli_value(self) -> &'static str {
        match self {
            Self::Haiku => "haiku",
            Self::Sonnet => "sonnet",
            Self::Opus => "opus",
            Self::Codex => "codex",
        }
    }

    /// The CLI that runs it.
    #[must_use]
    pub const fn provider(self) -> Provider {
        match self {
            Self::Haiku | Self::Sonnet | Self::Opus => Provider::Claude,
            Self::Codex => Provider::Codex,
        }
    }

    /// The model word given to the CLI; [`DEFAULT_MODEL`] leaves the CLI's
    /// own default.
    #[must_use]
    pub fn model_word(self) -> &'static str {
        match self {
            Self::Codex => DEFAULT_MODEL,
            other => other.cli_value(),
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
            Self::Codex => "Codex",
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

impl UtilitySettings {
    /// Provider, model and effort, as every utility run takes them.
    #[must_use]
    pub fn choice(self) -> ModelChoice {
        ModelChoice::new(
            self.model.provider(),
            self.model.model_word(),
            self.effort.cli_value(),
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_models_read_back_and_name_their_provider() {
        for (word, model) in [
            ("haiku", UtilityModel::Haiku),
            ("sonnet", UtilityModel::Sonnet),
            ("opus", UtilityModel::Opus),
            ("codex", UtilityModel::Codex),
        ] {
            assert_eq!(UtilityModel::from_cli(word), Some(model));
            assert_eq!(model.cli_value(), word);
        }
        assert_eq!(UtilityModel::Haiku.provider(), Provider::Claude);
        assert_eq!(UtilityModel::Codex.provider(), Provider::Codex);
        let choice = UtilitySettings::default().choice();
        assert_eq!(choice, ModelChoice::new(Provider::Claude, "haiku", "low"));
        let codex = UtilitySettings {
            model: UtilityModel::Codex,
            effort: UtilityEffort::High,
        }
        .choice();
        assert_eq!(codex.provider, Provider::Codex);
        assert_eq!(cli_model(&codex), None, "Codex's own default model");
        assert!(codex.is_valid());
    }
}
