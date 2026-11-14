//! The adapter of a plain shell terminal: a terminal that runs no agent.
//!
//! It exists so the code that holds an adapter per terminal needs no special
//! case. A shell sends no hook events, has no login, no turn log, and is never
//! "ready" or "not ready" for a prompt, so every answer here is the empty one.
//! The session layer starts a shell itself; [`ToolAdapter::launch`] is not used.

use std::path::{Path, PathBuf};

use connecto_core::{AuthState, Readiness};

use crate::spec::{
    AdapterError, Capabilities, Continuation, HookCtx, LaunchCtx, LaunchSpec, LogMark, LoginCtx,
    ToolAdapter,
};

/// A plain shell. See the module comment.
pub struct Plain;

fn nothing(cwd: &Path) -> LaunchSpec {
    LaunchSpec {
        program: PathBuf::new(),
        args: Vec::new(),
        cwd: cwd.to_owned(),
        env: Vec::new(),
        files: Vec::new(),
    }
}

impl ToolAdapter for Plain {
    fn launch(&self, ctx: &LaunchCtx) -> LaunchSpec {
        nothing(&ctx.project)
    }

    fn login(&self, ctx: &LoginCtx) -> LaunchSpec {
        nothing(&ctx.run)
    }

    fn status(&self, ctx: &LoginCtx) -> LaunchSpec {
        nothing(&ctx.run)
    }

    fn parse_status(&self, _output: &str) -> AuthState {
        AuthState::Unknown
    }

    fn parse_hook(
        &self,
        _payload: &[u8],
        _ctx: &HookCtx,
    ) -> Result<Vec<connecto_core::ToolEvent>, AdapterError> {
        Ok(Vec::new())
    }

    fn readiness(&self, _screen: &str) -> Readiness {
        Readiness::Unknown
    }

    fn send_anyway_text(&self) -> &'static str {
        "A shell is always ready to be typed into."
    }

    fn ack_form(&self, text: &str) -> String {
        text.to_owned()
    }

    fn capabilities(&self, _version: &str) -> Capabilities {
        Capabilities {
            immediate_ack: false,
        }
    }

    fn turn_log(&self, _reported: &Path, _ctx: &HookCtx) -> Option<PathBuf> {
        None
    }

    fn read_log(&self, _line: &str, _whole: bool) -> LogMark {
        LogMark::Other
    }

    fn continuation(&self, _prompt: &str) -> Option<Continuation> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_reports_nothing_and_is_never_ready_or_blocked() {
        assert_eq!(Plain.readiness("$ "), Readiness::Unknown);
        assert_eq!(Plain.parse_status("anything"), AuthState::Unknown);
        assert_eq!(Plain.ack_form("ls -la"), "ls -la");
        assert!(Plain.continuation("x").is_none());
        assert_eq!(Plain.read_log("{}", true), LogMark::Other);
    }
}
