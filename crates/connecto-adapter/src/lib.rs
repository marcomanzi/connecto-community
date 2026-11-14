//! Everything specific to one tool, behind [`ToolAdapter`].
//!
//! Adapters describe launches and interpret what tools report. They run
//! nothing, so the session layer stays the only place that starts a process.
//! Facts in here were observed on Claude 2.1.318 and Codex 0.166.0. Versions are
//! not pinned: every version gets the bridge, and the CLIs are expected to stay
//! backward compatible.

mod claude;
mod codex;
mod plain;
mod platform;
mod quote;
mod spec;
mod statusline;

pub use claude::{
    Claude, ProjectSettingsConflicts, SECURE_STORAGE_FILE, claude_composer_state,
    project_settings_conflicts,
};
pub use codex::{
    Codex, codex_composer_state, expected_hook_keys, hook_config, hook_key_matches,
    hook_trust_state,
};
pub use connecto_core::Readiness;
pub use plain::Plain;
pub use quote::QuoteError;
pub use spec::{
    AdapterError, BridgeLaunch, Capabilities, ComposerState, Continuation, HookCtx, LaunchCtx,
    LaunchSpec, LogMark, LoginCtx, ToolAdapter, UtilityLaunch,
};
