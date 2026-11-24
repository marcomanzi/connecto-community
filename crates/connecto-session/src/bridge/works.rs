//! `connecto works` on the bridge: an agent sets up its own project's
//! Agentic Works.
//!
//! The app holds one [`WorksBridge`], set once by `connecto-app` with
//! [`set_works_bridge`] (until then [`NoWorksBridge`]). The caller is the
//! bridge's, from the process tree: a terminal reaches only its own project.

use std::sync::{Arc, PoisonError, RwLock};

use connecto_core::AskReply;
use connecto_core::agentic_works::{NoWorksBridge, WorksBridge, WorksCaller};
use connecto_hook::bridge::Reply;
use serde_json::json;

static WORKS: RwLock<Option<Arc<dyn WorksBridge>>> = RwLock::new(None);

/// Give the bridge the app's Agentic Works. Called once by `connecto-app`
/// at startup.
pub fn set_works_bridge(bridge: Arc<dyn WorksBridge>) {
    *WORKS.write().unwrap_or_else(PoisonError::into_inner) = Some(bridge);
}

/// The app's Agentic Works, or [`NoWorksBridge`].
pub(crate) fn works_bridge() -> Arc<dyn WorksBridge> {
    WORKS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| Arc::new(NoWorksBridge))
}

/// The commands allowed under Pause: they only read what Connecto has.
/// `test-db` is one only for the linked databases (all, or one named with
/// `--db`): with options it would reach a new destination with a credential.
/// `parallel` and `agent` are ones without arguments: with them they change
/// the setting.
fn reads_only(verb: &str, args: &[String]) -> bool {
    match verb {
        "help" | "--help" | "show" | "databases" | "check" | "guide" | "job" => true,
        "parallel" | "agent" => args.is_empty(),
        "test-db" => args.is_empty() || matches!(args, [flag, _] if flag == "--db"),
        _ => false,
    }
}

/// Answer `connecto works <verb> [args]` for a caller the bridge identified.
pub(crate) fn command(
    bridge: &dyn WorksBridge,
    caller: Option<&WorksCaller>,
    paused: bool,
    verb: &str,
    args: &[String],
    text: Option<&str>,
    notify: AskReply,
) -> Reply {
    let Some(caller) = caller else {
        return Reply::refused(
            "no_project",
            "This terminal has no project, so it has no Agentic Works.",
            None,
        );
    };
    if paused && !reads_only(verb, args) {
        return Reply::refused(
            "paused",
            "Connecto is paused by the user. Nothing was done.",
            None,
        );
    }
    match bridge.command(caller, verb, args, text, notify) {
        Ok(mut body) => {
            if !body.ends_with('\n') {
                body.push('\n');
            }
            Reply::ok(json!({})).with_body(body.into_bytes())
        }
        Err(why) => Reply::refused("refused", &why, None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Says who asked and how it was launched, as the app is told.
    struct Echo;

    impl WorksBridge for Echo {
        fn command(
            &self,
            caller: &WorksCaller,
            verb: &str,
            _: &[String],
            _: Option<&str>,
            _: AskReply,
        ) -> Result<String, String> {
            Ok(format!(
                "{} {verb} {:?}",
                caller.project_id, caller.launched
            ))
        }
    }

    fn caller() -> WorksCaller {
        WorksCaller {
            terminal: "claude-1".into(),
            project_id: "00000000000000ab".into(),
            launched: Some(connecto_core::AgentMode::Prompt),
            job: false,
            attempt: 1,
        }
    }

    fn run(caller: Option<&WorksCaller>, paused: bool, verb: &str, args: &[&str]) -> Reply {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        command(&Echo, caller, paused, verb, &args, None, Box::new(|_| {}))
    }

    #[test]
    fn pause_allows_only_reading_and_a_project_is_needed() {
        for verb in ["publish", "link", "setup", "test", "key-import"] {
            assert!(!run(Some(&caller()), true, verb, &[]).is_ok(), "{verb}");
        }
        let show = run(Some(&caller()), true, "show", &[]);
        assert!(show.is_ok());
        assert_eq!(show.body, b"00000000000000ab show Some(Prompt)\n");
        // The linked databases may be tested under Pause; a new one may not.
        assert!(run(Some(&caller()), true, "test-db", &[]).is_ok());
        assert!(run(Some(&caller()), true, "test-db", &["--db", "staging"]).is_ok());
        assert!(run(Some(&caller()), true, "databases", &[]).is_ok());
        assert!(!run(Some(&caller()), true, "test-db", &["--secret", "DB"]).is_ok());
        assert!(
            !run(
                Some(&caller()),
                true,
                "test-db",
                &["--db", "staging", "--secret", "DB"]
            )
            .is_ok()
        );
        for verb in ["unlink", "link"] {
            assert!(!run(Some(&caller()), true, verb, &["--db", "dev"]).is_ok());
        }
        // How many jobs at once may be read under Pause, not changed.
        assert!(run(Some(&caller()), true, "parallel", &[]).is_ok());
        assert!(!run(Some(&caller()), true, "parallel", &["10"]).is_ok());
        // `agent` alone reads the agent for jobs; with a CLI it sets it.
        assert!(run(Some(&caller()), true, "agent", &[]).is_ok());
        assert!(!run(Some(&caller()), true, "agent", &["claude"]).is_ok());
        assert!(!run(Some(&caller()), true, "agent", &["auto"]).is_ok());
        assert!(!run(None, false, "show", &[]).is_ok());
        let none = command(
            &NoWorksBridge,
            Some(&caller()),
            false,
            "show",
            &[],
            None,
            Box::new(|_| {}),
        );
        assert!(!none.is_ok());
    }
}
