//! `connecto op` on the bridge, and the prompt hook's use of operations
//! (ADR 0031).
//!
//! The app holds one [`OpsBridge`] for every project, set once by
//! `connecto-app` with [`set_ops_bridge`] (until then [`NoOpsBridge`]). The
//! bridge server names the caller from the process tree: its terminal, tool,
//! project and folder, never from an argument or the environment.

use std::sync::{Arc, PoisonError, RwLock};

use connecto_core::ops::{NoOpsBridge, OpsAnswer, OpsBridge, OpsCaller};
use connecto_hook::bridge::Reply;
use serde_json::json;

static OPS: RwLock<Option<Arc<dyn OpsBridge>>> = RwLock::new(None);

/// Give the bridge the app's project operations. Called once by
/// `connecto-app` at startup.
pub fn set_ops_bridge(bridge: Arc<dyn OpsBridge>) {
    *OPS.write().unwrap_or_else(PoisonError::into_inner) = Some(bridge);
}

/// The app's operations, or [`NoOpsBridge`].
pub(crate) fn ops_bridge() -> Arc<dyn OpsBridge> {
    OPS.read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| Arc::new(NoOpsBridge))
}

/// The verbs that only read, allowed under Pause.
fn reads_only(verb: &str) -> bool {
    matches!(verb, "list" | "show" | "help" | "done")
}

/// Answer `connecto op <verb> [args]` for a caller the bridge identified.
pub(crate) fn command(
    bridge: &dyn OpsBridge,
    caller: Option<&OpsCaller>,
    paused: bool,
    verb: &str,
    args: &[String],
    text: Option<&str>,
) -> Reply {
    let Some(caller) = caller else {
        return Reply::refused(
            "no_project",
            "This terminal has no project folder, so it has no operations.",
            None,
        );
    };
    if paused && !reads_only(verb) {
        return Reply::refused(
            "paused",
            "Connecto is paused by the user. Nothing was done.",
            None,
        );
    }
    reply(bridge.command(caller, verb, args, text))
}

/// The wire form of an answer.
pub(crate) fn reply(answer: OpsAnswer) -> Reply {
    match answer {
        OpsAnswer::Text(text) => {
            let mut body = text;
            if !body.ends_with('\n') {
                body.push('\n');
            }
            Reply::ok(json!({})).with_body(body.into_bytes())
        }
        OpsAnswer::Refused { code, reason } => Reply::refused(&code, &reason, None),
        OpsAnswer::Exec(exec) => {
            let env: Vec<[String; 2]> = exec
                .env
                .iter()
                .map(|(k, v)| [k.clone(), v.clone()])
                .collect();
            let slug = exec
                .env
                .iter()
                .find(|(k, _)| k == "CONNECTO_OP")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            Reply::ok(json!({
                "exec": {
                    "run": exec.run,
                    "slug": slug,
                    "program": exec.program,
                    "args": exec.args,
                    "cwd": exec.cwd.to_string_lossy(),
                    "env": env,
                    "timeout_s": exec.timeout_s,
                }
            }))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::PathBuf;

    use connecto_core::ToolKind;
    use connecto_core::ops::{OpsExec, PromptAdvice};
    use connecto_hook::client::OpExec;

    use super::*;

    struct Fixed(OpsAnswer);

    impl OpsBridge for Fixed {
        fn command(&self, _: &OpsCaller, _: &str, _: &[String], _: Option<&str>) -> OpsAnswer {
            self.0.clone()
        }
        fn has_ops(&self, _: &str, _: &std::path::Path) -> bool {
            true
        }
        fn on_prompt(&self, _: &OpsCaller, _: &str) -> PromptAdvice {
            PromptAdvice::Nothing
        }
    }

    fn caller() -> OpsCaller {
        OpsCaller {
            terminal: "claude-1".into(),
            tool: ToolKind::Claude,
            project_id: "00000000000000ab".into(),
            folder: PathBuf::from("/p"),
            holds: false,
        }
    }

    #[test]
    fn pause_allows_only_reading() {
        let bridge = Fixed(OpsAnswer::Text("ok".into()));
        let paused = command(&bridge, Some(&caller()), true, "run", &[], None);
        assert!(!paused.is_ok());
        let list = command(&bridge, Some(&caller()), true, "list", &[], None);
        assert!(list.is_ok());
        assert_eq!(list.body, b"ok\n");
        assert!(!command(&bridge, None, false, "list", &[], None).is_ok());
    }

    #[test]
    fn an_exec_reads_back_in_the_command() {
        let bridge = Fixed(OpsAnswer::Exec(OpsExec {
            run: "r1".into(),
            program: "/bin/bash".into(),
            args: vec!["-c".into(), "echo hi".into(), "tests".into()],
            cwd: PathBuf::from("/p"),
            env: vec![("CONNECTO_OP".into(), "tests".into())],
            timeout_s: 30,
        }));
        let reply = command(&bridge, Some(&caller()), false, "run", &[], None);
        let exec = OpExec::from_header(&reply.header).unwrap();
        assert_eq!(exec.slug, "tests");
        assert_eq!(exec.args[1], "echo hi");
        assert_eq!(exec.env, [("CONNECTO_OP".to_owned(), "tests".to_owned())]);
        assert_eq!(exec.timeout.as_secs(), 30);
    }
}
