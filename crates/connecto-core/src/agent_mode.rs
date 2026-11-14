//! How an agent CLI is launched with respect to its own permission prompts.
//!
//! Setup lets the user choose, per tool. The choice applies to every launch
//! and restart from then on. A terminal that is already running keeps what it
//! was launched with.

use crate::ToolKind;

/// What an agent may do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AgentMode {
    /// Nothing asks: Claude `--dangerously-skip-permissions`, Codex
    /// `--dangerously-bypass-approvals-and-sandbox` (`--yolo`). The behaviour
    /// before this choice existed, and the default.
    #[default]
    Bypass,
    /// The tool's own automatic mode: Claude `--permission-mode auto` (a model
    /// classifier reviews actions), Codex `--approve-for-me` (automatic review,
    /// on-request approvals, workspace-write sandbox).
    Auto,
    /// No flag: the tool's normal permission prompts.
    Prompt,
}

impl AgentMode {
    pub const ALL: [Self; 3] = [Self::Bypass, Self::Auto, Self::Prompt];

    /// The word saved in the settings file.
    #[must_use]
    pub const fn cli_value(self) -> &'static str {
        match self {
            Self::Bypass => "bypass",
            Self::Auto => "auto",
            Self::Prompt => "prompt",
        }
    }

    #[must_use]
    pub fn from_cli(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.cli_value() == value)
    }
}

/// The choice for each tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaunchModes {
    pub claude: AgentMode,
    pub codex: AgentMode,
}

impl LaunchModes {
    #[must_use]
    pub const fn for_tool(self, tool: ToolKind) -> AgentMode {
        match tool {
            ToolKind::Claude => self.claude,
            ToolKind::Codex => self.codex,
            // A shell has no permissions to ask about; nothing reads this.
            ToolKind::Shell => AgentMode::Prompt,
        }
    }

    #[must_use]
    pub const fn with(mut self, tool: ToolKind, mode: AgentMode) -> Self {
        match tool {
            ToolKind::Claude => self.claude = mode,
            ToolKind::Codex => self.codex = mode,
            ToolKind::Shell => {}
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_todays_behaviour_for_both_tools() {
        let modes = LaunchModes::default();
        assert_eq!(modes.for_tool(ToolKind::Claude), AgentMode::Bypass);
        assert_eq!(modes.for_tool(ToolKind::Codex), AgentMode::Bypass);
    }

    #[test]
    fn a_mode_round_trips_through_its_saved_word_and_unknown_words_are_refused() {
        for mode in AgentMode::ALL {
            assert_eq!(AgentMode::from_cli(mode.cli_value()), Some(mode));
        }
        assert_eq!(AgentMode::from_cli("yolo"), None);
    }

    #[test]
    fn with_changes_one_tool_only() {
        let modes = LaunchModes::default().with(ToolKind::Codex, AgentMode::Auto);
        assert_eq!(modes.codex, AgentMode::Auto);
        assert_eq!(modes.claude, AgentMode::Bypass);
    }
}
