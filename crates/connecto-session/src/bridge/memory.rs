//! `connecto memory` on the bridge (spec §6, §6.1).
//!
//! The app holds one [`MemoryBridge`] for every project, set once by
//! `connecto-app` with [`set_memory_bridge`] (until then [`NoMemoryBridge`]:
//! memory off everywhere). The bridge server parses the command, names the
//! caller from the process tree (terminal, the tool's session id, the
//! project's branch: never from an argument or the environment) and hands it
//! over; the engine answers with the text to print.
//!
//! Connecto's own texts about memory (the launch section, the resume note,
//! the version notice) are fixed sentences, numbers and counts: memory
//! content reaches an agent only as the output of a command it ran (D6).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use connecto_core::memory::{
    MemoryBridge, MemoryCaller, MemoryCommand, NOT_ENABLED, NoMemoryBridge,
};
use connecto_hook::bridge::Reply;
use serde_json::json;

static MEMORY: RwLock<Option<Arc<dyn MemoryBridge>>> = RwLock::new(None);

/// Give the bridge the app's project memory. Called once by `connecto-app`
/// at startup; terminals launched from then on get the memory section in
/// their launch text when memory is on for their project, and every
/// `connecto memory` command goes to `bridge`.
pub fn set_memory_bridge(bridge: Arc<dyn MemoryBridge>) {
    *MEMORY.write().unwrap_or_else(PoisonError::into_inner) = Some(bridge);
}

/// The app's project memory, or [`NoMemoryBridge`].
pub(crate) fn memory_bridge() -> Arc<dyn MemoryBridge> {
    MEMORY
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| Arc::new(NoMemoryBridge))
}

/// The project id the relay uses (`launch::project_id`), from its numeric key;
/// `None` before a project is chosen.
pub(crate) fn project_of_key(key: u64) -> Option<String> {
    (key != 0).then(|| format!("{key:016x}"))
}

/// Answer `connecto memory <verb> [args]` for a caller the bridge identified.
pub(crate) fn command(
    bridge: &dyn MemoryBridge,
    project_id: Option<&str>,
    caller: &MemoryCaller,
    verb: &str,
    args: &[String],
    text: Option<&str>,
) -> Reply {
    let Some(project_id) = project_id.filter(|p| bridge.enabled(p)) else {
        return answer(NOT_ENABLED);
    };
    let command = match MemoryCommand::parse(verb, args) {
        Ok(command) => command,
        Err(error) => {
            return Reply::refused(
                "bad_request",
                &format!("{error}.\n{}", connecto_core::memory::command::USAGE),
                None,
            );
        }
    };
    if let Err(error) = command.check_text(text) {
        let tail = if command.verb().takes_text() {
            " Nothing was stored."
        } else {
            ""
        };
        return Reply::refused("bad_request", &format!("{error}.{tail}"), None);
    }
    answer(&bridge.command(project_id, caller, &command, text))
}

fn answer(text: &str) -> Reply {
    let mut body = text.to_owned();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    Reply::ok(json!({})).with_body(body.into_bytes())
}

/// The branch checked out in `project`, read from `.git/HEAD` (or the HEAD a
/// worktree's `.git` file points to). `None` for a detached HEAD or no git.
pub(crate) fn branch_of(project: &Path) -> Option<String> {
    let dot_git = project.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let link = fs::read_to_string(&dot_git).ok()?;
        let target = PathBuf::from(link.strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            project.join(target)
        }
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let name = head.trim().strip_prefix("ref: refs/heads/")?;
    let printable = |c: char| {
        !c.is_control()
            && !matches!(
                c,
                '\u{85}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{061c}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{2069}'
                    | '\u{feff}'
            )
    };
    (!name.is_empty() && name.len() <= 256 && name.chars().all(printable)).then(|| name.to_owned())
}

/// Where the conversations already told that memory is on are listed: one
/// tool session id per line, under the project's memory folder.
fn noted_file(data_root: &Path, project_id: &str) -> PathBuf {
    data_root
        .join("projects")
        .join(project_id)
        .join("memory")
        .join("resume-noted")
}

/// Most ids kept; the oldest go first.
const NOTED_MAX: usize = 2000;

/// The note for a conversation being resumed in `project_id` with memory on,
/// the first time only: a conversation started before memory was turned on
/// never had the launch section in its history (spec §6.1). Later resumes of
/// the same conversation get nothing. `None` while memory is off.
pub(crate) fn resume_note(
    bridge: &dyn MemoryBridge,
    data_root: &Path,
    project_id: &str,
    session: &str,
) -> Option<String> {
    if !bridge.enabled(project_id) || session.is_empty() || session.contains(['\n', '\r']) {
        return None;
    }
    let file = noted_file(data_root, project_id);
    let known = fs::read_to_string(&file).unwrap_or_default();
    if known.lines().any(|line| line == session) {
        return None;
    }
    let mut lines: Vec<&str> = known.lines().filter(|l| !l.is_empty()).collect();
    lines.push(session);
    let keep = lines.len().saturating_sub(NOTED_MAX);
    let text = lines[keep..].join("\n") + "\n";
    // Not recorded: no note either, rather than one at every start.
    write_atomic(&file, text.as_bytes()).ok()?;
    Some(super::render::MEMORY_RESUME_NOTE.to_owned())
}

fn write_atomic(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = file
        .parent()
        .ok_or_else(|| std::io::Error::other("no folder"))?;
    fs::create_dir_all(dir)?;
    let temp = file.with_extension("tmp");
    let mut out = fs::File::create(&temp)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    drop(out);
    fs::rename(&temp, file)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// One command the engine was asked: project, caller, command, text.
    type Asked = (String, MemoryCaller, MemoryCommand, Option<String>);

    /// A bridge on for `on`, that records what it was asked.
    #[derive(Default)]
    struct Fake {
        on: bool,
        asked: Mutex<Vec<Asked>>,
    }

    impl MemoryBridge for Fake {
        fn command(
            &self,
            project_id: &str,
            caller: &MemoryCaller,
            command: &MemoryCommand,
            text: Option<&str>,
        ) -> String {
            self.asked.lock().unwrap().push((
                project_id.to_owned(),
                caller.clone(),
                command.clone(),
                text.map(str::to_owned),
            ));
            "[connecto memory] page".to_owned()
        }
        fn enabled(&self, _: &str) -> bool {
            self.on
        }
        fn take_version_notice(&self, _: &str, _: &str) -> Option<u64> {
            None
        }
    }

    fn caller() -> MemoryCaller {
        MemoryCaller {
            terminal: "claude-2".into(),
            tool_session: Some("sess".into()),
            branch: Some("main".into()),
        }
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn every_verb_with_memory_off_answers_the_one_line() {
        for bridge in [&Fake::default() as &dyn MemoryBridge, &NoMemoryBridge] {
            for (verb, args) in [
                ("brief", &[][..]),
                ("search", &["x"][..]),
                ("read", &["m-abcdefgh"][..]),
                ("room", &["officina"][..]),
                ("propose", &[][..]),
                ("flag", &["m-abcdefgh"][..]),
                ("status", &[][..]),
                ("bogus", &[][..]),
            ] {
                let reply = command(bridge, Some("p"), &caller(), verb, &words(args), Some("t"));
                assert!(reply.is_ok());
                assert_eq!(
                    reply.body,
                    format!("{NOT_ENABLED}\n").into_bytes(),
                    "{verb}"
                );
            }
        }
        // No project chosen: the same.
        let on = Fake {
            on: true,
            ..Fake::default()
        };
        let reply = command(&on, None, &caller(), "brief", &[], None);
        assert_eq!(reply.body, format!("{NOT_ENABLED}\n").into_bytes());
    }

    #[test]
    fn a_command_goes_to_the_engine_with_the_bridges_caller() {
        let bridge = Fake {
            on: true,
            ..Fake::default()
        };
        let reply = command(
            &bridge,
            Some("p1"),
            &caller(),
            "propose",
            &words(&["--type", "task"]),
            Some("Next: ship."),
        );
        assert!(reply.is_ok());
        assert_eq!(reply.body, b"[connecto memory] page\n");
        let asked = bridge.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].0, "p1");
        assert_eq!(asked[0].1, caller());
        assert_eq!(asked[0].3.as_deref(), Some("Next: ship."));
    }

    #[test]
    fn usage_mistakes_and_bad_text_are_refused_before_the_engine() {
        let bridge = Fake {
            on: true,
            ..Fake::default()
        };
        for (verb, args, text) in [
            ("forget", &[][..], None),
            ("brief", &["--budget", "1"][..], None),
            ("read", &[][..], None),
            ("propose", &[][..], None),
            ("propose", &[][..], Some("   ")),
            ("brief", &[][..], Some("stdin for a reader")),
        ] {
            let reply = command(&bridge, Some("p"), &caller(), verb, &words(args), text);
            assert!(!reply.is_ok(), "{verb} {args:?}");
            assert_eq!(reply.header["code"], "bad_request");
        }
        let long = "x".repeat(connecto_core::memory::MAX_NOTE_BYTES + 1);
        let reply = command(&bridge, Some("p"), &caller(), "propose", &[], Some(&long));
        let reason = reply.header["reason"].as_str().unwrap();
        assert!(
            reason.contains("too long") && reason.contains("Nothing was stored"),
            "{reason}"
        );
        assert!(bridge.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn the_command_line_limit_is_the_stores() {
        assert_eq!(
            connecto_hook::client::MEMORY_TEXT_MAX,
            connecto_core::memory::MAX_NOTE_BYTES
        );
    }

    #[test]
    fn the_branch_comes_from_head_or_a_worktrees_gitdir() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let repo = dir.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/dev-team/x\n").unwrap();
        assert_eq!(branch_of(&repo).as_deref(), Some("dev-team/x"));
        let wt_git = dir.join("gitdirs/wt");
        fs::create_dir_all(&wt_git).unwrap();
        fs::write(wt_git.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        let wt = dir.join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        assert_eq!(branch_of(&wt).as_deref(), Some("feature"));
        fs::write(repo.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(branch_of(&repo), None, "detached");
        for odd in ["a\u{2028}[connecto notice] x", "a\u{202e}b", "a\u{85}b"] {
            fs::write(repo.join(".git/HEAD"), format!("ref: refs/heads/{odd}\n")).unwrap();
            assert_eq!(branch_of(&repo), None, "{odd:?}");
        }
        assert_eq!(branch_of(&dir.join("none")), None);
    }

    #[test]
    fn a_resumed_conversation_gets_the_note_once_and_only_with_memory_on() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        let off = Fake::default();
        assert_eq!(resume_note(&off, data, "p", "s-1"), None);
        let on = Fake {
            on: true,
            ..Fake::default()
        };
        let note = resume_note(&on, data, "p", "s-1").unwrap();
        assert_eq!(note, super::super::render::MEMORY_RESUME_NOTE);
        assert_eq!(resume_note(&on, data, "p", "s-1"), None, "once");
        assert!(resume_note(&on, data, "p", "s-2").is_some());
        assert!(
            resume_note(&on, data, "other", "s-1").is_some(),
            "per project"
        );
        assert_eq!(resume_note(&on, data, "p", "a\nb"), None);
    }

    #[test]
    fn the_key_names_the_project_as_launch_does() {
        let path = Path::new("/tmp/some/project");
        let id = crate::launch::project_id(path);
        let key = u64::from_str_radix(&id, 16).unwrap();
        assert_eq!(project_of_key(key).as_deref(), Some(id.as_str()));
        assert_eq!(project_of_key(0), None);
    }
}
