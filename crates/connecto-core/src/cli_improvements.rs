//! CLI improvements (Settings > CLI improvements): Connecto starts a coding
//! CLI on a task that improves how that CLI is set up for the person. The
//! first is "Claude auto apply insights": a Claude Code terminal on Opus
//! that runs `/insights`, reads its report and applies its suggestions,
//! asking the person whenever it is in doubt and before anything risky.

/// The model the insights terminal runs on (Claude Code's alias for the
/// latest Opus).
pub const INSIGHTS_MODEL: &str = "opus";

/// The insights terminal's name in the project's list.
pub const INSIGHTS_TERMINAL: &str = "claude-insights";

/// Claude Code's config folder as the terminal has it: Connecto sets
/// `CLAUDE_CONFIG_DIR` for a terminal of an isolated profile, and Claude
/// uses `~/.claude` without it. The task names it so, never `~/.claude`
/// alone, so the terminal reads and changes the set-up it really runs on.
pub const CONFIG_DIR: &str = "${CLAUDE_CONFIG_DIR:-$HOME/.claude}";

/// Where, under that folder, Claude Code writes the `/insights` report when
/// it does not say.
pub const INSIGHTS_REPORT: &str = "usage-data/report.html";

/// The insights terminal's first message. `/insights` is a command the
/// person invokes, not one the model can run for itself, so the terminal
/// runs it as a command of its own (`claude -p "/insights"`, which inherits
/// the terminal's config folder), and asks the person to type it when that
/// does not work.
#[must_use]
pub fn insights_prompt() -> String {
    format!(
        "[connecto task] Apply the suggestions of my Claude Code insights report to how Claude Code is set up for me.

0. Find the Claude config folder this terminal runs on: `echo \"{CONFIG_DIR}\"` in the shell. Below, CONFIG means that folder. When CLAUDE_CONFIG_DIR is set, it is the isolated set-up Connecto keeps for this project's terminals, not ~/.claude: tell me so before changing anything in it, and that a change there applies to this project's Connecto terminals only.
1. Make a fresh report: run `claude -p \"/insights\"` with the Bash tool. It reads my past sessions and can take several minutes, so give it a long timeout or run it in the background and wait for it. Note the path of the report it prints; if it prints none, the report is CONFIG/{INSIGHTS_REPORT}. If it fails, or the report did not change, ask me to type /insights in this terminal, end your turn, and go on when I say it is done.
2. Read the report in full (it is HTML: read its text) and list every concrete suggestion it makes: settings, permissions, hooks, CLAUDE.md instructions, skills, commands, agents, MCP servers, habits.
3. Apply them as well as you can, each where it belongs: what is about me and all my projects at user level (CONFIG/settings.json, CONFIG/CLAUDE.md, CONFIG/skills, CONFIG/agents), and what is about this project in this project (CLAUDE.md, .claude/settings.json, .claude/skills). Read a file before changing it, keep what is there, and make each change small.
4. Ask me first, and wait for my answer, whenever you are in doubt about what a suggestion means or where it goes, and before anything risky: allowing more permissions, a hook that runs commands, deleting or rewriting what is there, changing another project, installing anything, or anything that reaches the network.
5. A suggestion about my habits rather than my setup: tell me about it instead of applying it.
6. End with a short summary: what you changed and where, and what you skipped and why."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_runs_insights_reads_the_report_and_asks_before_anything_risky() {
        let prompt = insights_prompt();
        assert!(prompt.contains("claude -p \"/insights\""));
        assert!(prompt.contains(&format!("CONFIG/{INSIGHTS_REPORT}")));
        assert!(prompt.contains("ask me to type /insights"));
        assert!(prompt.contains("before anything risky"));
    }
}
