//! `connecto agents [--roles]` on the bridge (ADR 0045).
//!
//! The app gives one [`RosterBridge`] for every project with
//! [`set_roster_bridge`]: what it knows of each CLI (installed, offered,
//! signed in). Whether the project allows a CLI is decided here, from the
//! saved Tools setting, by the same check every `connecto add` passes
//! ([`crate::project_settings::tool_refusal`]), so the roster never offers
//! a CLI that `add` would refuse.

use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};

use connecto_core::ToolKind;
use connecto_core::packages::ProjectTools;
use connecto_core::roster::{AgentState, RosterAgent, RosterBridge, render};
use connecto_hook::bridge::Reply;
use serde_json::json;

static ROSTER: RwLock<Option<Arc<dyn RosterBridge>>> = RwLock::new(None);

/// Give the bridge the app's view of the agents. Called once by
/// `connecto-app` at startup.
pub fn set_roster_bridge(bridge: Arc<dyn RosterBridge>) {
    *ROSTER.write().unwrap_or_else(PoisonError::into_inner) = Some(bridge);
}

fn roster_bridge() -> Option<Arc<dyn RosterBridge>> {
    ROSTER
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The roster of the project `project_id` in `folder`: the app's answer for
/// each CLI, with `allowed` set from `refusal` (the project's Tools) and a
/// left-out CLI never ready. Without the app's answer, a CLI is still being
/// checked.
pub(crate) fn agents(
    bridge: Option<&dyn RosterBridge>,
    project_id: &str,
    folder: &Path,
    refusal: impl Fn(ToolKind) -> Option<String>,
) -> Vec<RosterAgent> {
    let known = bridge
        .map(|b| b.agents(project_id, folder))
        .unwrap_or_default();
    ProjectTools::CHOICES
        .into_iter()
        .map(|tool| {
            let allowed = refusal(tool).is_none();
            let found = known.iter().find(|a| a.tool == tool);
            let state = if allowed {
                found.map_or_else(
                    || AgentState::Checking("Connecto has not checked it yet".into()),
                    |a| a.state.clone(),
                )
            } else {
                AgentState::Unusable("left out by the project's Tools setting".into())
            };
            RosterAgent {
                tool,
                allowed,
                state,
                version: found.and_then(|a| a.version.clone()),
                via: found.and_then(|a| a.via.clone()),
            }
        })
        .collect()
}

/// Answer `connecto agents`: the text in the body.
pub(crate) fn command(
    data_root: &Path,
    project_id: &str,
    folder: &Path,
    asker: ToolKind,
    roles: bool,
) -> Reply {
    let bridge = roster_bridge();
    let list = agents(bridge.as_deref(), project_id, folder, |tool| {
        crate::project_settings::tool_refusal(data_root, project_id, tool)
    });
    let asker = asker.is_agent().then_some(asker);
    Reply::ok(json!({})).with_body(render(&list, asker, roles).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AllReady;

    impl RosterBridge for AllReady {
        fn agents(&self, _: &str, _: &Path) -> Vec<RosterAgent> {
            ProjectTools::CHOICES
                .into_iter()
                .map(|tool| RosterAgent {
                    tool,
                    allowed: true,
                    state: AgentState::Ready,
                    version: Some("1".into()),
                    via: None,
                })
                .collect()
        }
    }

    /// The app may say a CLI is ready; the project's Tools still leave it
    /// out, as `connecto add` would.
    #[test]
    fn a_cli_the_tools_setting_leaves_out_is_never_ready() {
        let list = agents(Some(&AllReady), "p", Path::new("/p"), |tool| {
            (tool != ToolKind::Codex).then(|| "no".to_owned())
        });
        let ready: Vec<ToolKind> = list
            .iter()
            .filter(|a| a.allowed && a.state == AgentState::Ready)
            .map(|a| a.tool)
            .collect();
        assert_eq!(ready, [ToolKind::Codex]);
        let text = render(&list, Some(ToolKind::Codex), true);
        assert!(text.contains("codex (same CLI)"), "{text}");
        assert!(!text.contains("connecto add claude"), "{text}");
    }

    #[test]
    fn without_the_app_every_allowed_cli_is_still_being_checked() {
        let list = agents(None, "p", Path::new("/p"), |_| None);
        assert!(
            list.iter()
                .all(|a| a.allowed && matches!(a.state, AgentState::Checking(_)))
        );
    }
}
