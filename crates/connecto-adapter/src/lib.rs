//! Everything specific to one tool, behind [`ToolAdapter`].
//!
//! Adapters describe launches and interpret what tools report. They run
//! nothing, so the session layer stays the only place that starts a process.
//! Facts in here were observed on Claude 2.1.318, Codex 0.166.0 and Pi 1.5.2;
//! Antigravity's come from its documentation and its 1.5.11 program, and
//! OpenCode's from running 1.18.8. Versions
//! are not pinned: every version gets the bridge, and the CLIs are expected to
//! stay backward compatible.

mod antigravity;
mod claude;
mod codex;
mod opencode;
mod past_log;
mod pi;
mod plain;
mod platform;
mod quote;
mod spec;
mod statusline;

pub use antigravity::{
    Antigravity, HooksFileError as AntigravityHooksError, antigravity_composer_state,
    antigravity_profile, hooks_file as antigravity_hooks_file,
    hooks_path as antigravity_hooks_path, read_rule as antigravity_read_rule,
    settings_file as antigravity_settings_file, settings_path as antigravity_settings_path,
};
pub use claude::{
    Claude, ClaudeCuts, ClaudeProviders, ProjectSettingsConflicts, SECURE_STORAGE_FILE,
    TOGETHER_API_KEY, TogetherLink, claude_blocked_reason, claude_composer_state,
    claude_utility_ready, project_settings_conflicts,
};
pub use codex::{
    BRIDGE_TEXT_ENV as CODEX_BRIDGE_TEXT_ENV, Codex, RECAP_ENV as CODEX_RECAP_ENV,
    codex_composer_state, expected_hook_keys, hook_config, hook_key_matches, hook_trust_state,
};
pub use connecto_core::Readiness;
pub use opencode::{OpenCode, opencode_composer_state, opencode_models, opencode_state_dir};
pub use past_log::{LoggedExchange, logged_exchanges, session_log};
pub use pi::{Pi, PiProviders, pi_composer_state, pi_extension_for_listing};
pub use plain::Plain;
pub use quote::QuoteError;
pub use spec::{
    AdapterError, BoxRows, BridgeLaunch, Capabilities, ComposerState, ComposerView, Continuation,
    DraftKeys, HookCtx, LaunchCtx, LaunchSpec, LogMark, LoggedTurn, LoginCtx, Screen, ToolAdapter,
    UtilityAccess, UtilityLaunch, UtilityTool, prompt_arrived_whole, prompt_matches_exact,
};

#[cfg(test)]
mod works_tests {
    use super::*;
    use connecto_core::AgentMode;

    #[test]
    fn four_work_providers_receive_prompt_model_effort_and_project_permissions() {
        let mut ctx = spec::tests::launch_ctx();
        ctx.agent_mode = AgentMode::Prompt;
        ctx.model = Some("work-model".into());
        ctx.effort = Some("high".into());
        ctx.initial_prompt = Some("Review {{literal}}".into());
        for (provider, model, effort) in [
            (&Claude as &dyn ToolAdapter, "work-model", "--effort"),
            (
                &Codex,
                r#"model="work-model""#,
                "model_reasoning_effort=\"high\"",
            ),
            (&Pi, "work-model", "--thinking"),
            (&Antigravity, "--model=work-model", "--effort=high"),
        ] {
            let spec = provider.launch(&ctx);
            let args: Vec<_> = spec.args.iter().map(|s| s.to_string_lossy()).collect();
            assert_eq!(spec.cwd, ctx.project);
            assert!(args.iter().any(|a| a == effort), "{args:?}");
            assert!(args.iter().any(|a| a == model), "{args:?}");
            assert!(
                args.iter().any(|a| a.ends_with("Review {{literal}}")),
                "{args:?}"
            );
            assert!(
                !args
                    .iter()
                    .any(|a| a.contains("--dangerously-skip-permissions")
                        || a.contains("--dangerously-bypass-approvals-and-sandbox"))
            );
        }
    }
}
