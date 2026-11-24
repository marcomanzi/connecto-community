//! The bridge side of `connecto memory` (spec §6, §6.1): what the bridge server
//! asks the memory engine. The server identifies the caller from the process
//! tree (ADR 0016) and fills [`MemoryCaller`]; nothing in it comes from the
//! agent's arguments or environment.
//!
//! `connecto-session` holds one [`MemoryBridge`] for the whole app
//! ([`NoMemoryBridge`] until `connecto-app` sets the real one); the engine in
//! `connecto-memory` implements it by calling its `commands::execute`.

use super::MemoryCommand;

/// The one line every command answers while memory is off for the project.
pub const NOT_ENABLED: &str =
    "project memory is not enabled for this project (Project settings > Memory turns it on).";

/// Who runs a command, as the bridge found it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryCaller {
    /// The Connecto terminal id (`claude-2`).
    pub terminal: String,
    /// Claude's session id or Codex's thread id, once the tool reported it.
    pub tool_session: Option<String>,
    /// The project's checked-out branch, when the bridge could read it.
    pub branch: Option<String>,
}

pub trait MemoryBridge: Send + Sync {
    /// Runs one `connecto memory` command for a caller identified by the
    /// bridge; returns the text to print.
    fn command(
        &self,
        project_id: &str,
        caller: &MemoryCaller,
        command: &MemoryCommand,
        text: Option<&str>,
    ) -> String;

    fn enabled(&self, project_id: &str) -> bool;

    /// Latest published version not yet announced to this terminal, if any
    /// (marks it announced).
    fn take_version_notice(&self, project_id: &str, terminal: &str) -> Option<u64>;
}

/// The bridge used when memory is not wired in: off for every project.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoMemoryBridge;

impl MemoryBridge for NoMemoryBridge {
    fn command(
        &self,
        _project_id: &str,
        _caller: &MemoryCaller,
        _command: &MemoryCommand,
        _text: Option<&str>,
    ) -> String {
        NOT_ENABLED.to_owned()
    }

    fn enabled(&self, _project_id: &str) -> bool {
        false
    }

    fn take_version_notice(&self, _project_id: &str, _terminal: &str) -> Option<u64> {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_bridge_is_off_for_every_verb() {
        let bridge = NoMemoryBridge;
        let caller = MemoryCaller {
            terminal: "claude".to_owned(),
            ..MemoryCaller::default()
        };
        for line in [
            "brief",
            "search x",
            "read m-abcdefgh",
            "room officina",
            "propose",
            "flag m-abcdefgh",
            "status",
        ] {
            let mut words = line.split_whitespace();
            let verb = words.next().unwrap();
            let args: Vec<String> = words.map(str::to_owned).collect();
            let command = MemoryCommand::parse(verb, &args).unwrap();
            let out = bridge.command("p", &caller, &command, Some("x"));
            assert_eq!(out, NOT_ENABLED);
            assert_eq!(out.lines().count(), 1);
        }
        assert!(!bridge.enabled("p"));
        assert_eq!(bridge.take_version_notice("p", "claude"), None);
    }
}
