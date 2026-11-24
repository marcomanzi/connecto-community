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
    /// before this choice existed, and the default. Pi never asks before a
    /// tool call; for it this mode also trusts the project's own `.pi`
    /// resources (`--approve`), and the other two leave Pi's trust question.
    /// Antigravity: `--dangerously-skip-permissions`. OpenCode: `--auto`
    /// (every permission not explicitly denied is granted).
    #[default]
    Bypass,
    /// The tool's own automatic mode: Claude `--permission-mode auto` (a model
    /// classifier reviews actions), Codex `--approve-for-me` (automatic review,
    /// on-request approvals, workspace-write sandbox), Antigravity `--mode
    /// auto` (where the account offers it; else its normal prompts).
    /// OpenCode has no reviewing mode: its normal prompts.
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
    pub pi: AgentMode,
    pub antigravity: AgentMode,
    pub opencode: AgentMode,
}

impl LaunchModes {
    #[must_use]
    pub const fn for_tool(self, tool: ToolKind) -> AgentMode {
        match tool {
            ToolKind::Claude => self.claude,
            ToolKind::Codex => self.codex,
            ToolKind::Pi => self.pi,
            ToolKind::Antigravity => self.antigravity,
            ToolKind::OpenCode => self.opencode,
            // A shell has no permissions to ask about; nothing reads this.
            ToolKind::Shell => AgentMode::Prompt,
        }
    }

    #[must_use]
    pub const fn with(mut self, tool: ToolKind, mode: AgentMode) -> Self {
        match tool {
            ToolKind::Claude => self.claude = mode,
            ToolKind::Codex => self.codex = mode,
            ToolKind::Pi => self.pi = mode,
            ToolKind::Antigravity => self.antigravity = mode,
            ToolKind::OpenCode => self.opencode = mode,
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
        assert_eq!(modes.for_tool(ToolKind::Antigravity), AgentMode::Bypass);
        assert_eq!(modes.for_tool(ToolKind::OpenCode), AgentMode::Bypass);
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
        let modes = modes.with(ToolKind::Antigravity, AgentMode::Prompt);
        assert_eq!(modes.antigravity, AgentMode::Prompt);
        assert_eq!(modes.codex, AgentMode::Auto);
    }
}
