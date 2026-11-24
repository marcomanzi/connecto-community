//! The built-in package's always-on instructions for the agents other than
//! Claude: the end-of-task status recap. The text is compiled into Connecto
//! from the built-in package's source, never read from the project, and is
//! passed at launch (by each tool's own option) when the project's
//! `.connecto/packages.lock` says the built-in package is installed. The
//! project's AGENTS.md and CLAUDE.md are never written. Claude reads the same
//! text from `.claude/rules`, where the package installs it.

use std::fs;
use std::io::Read as _;
use std::path::Path;

use connecto_store::Tool;

/// The recap, as the built-in package's Claude rule has it (compiled into
/// the hook helper, which hands it to Codex).
pub const STATUS_RECAP: &str = connecto_hook::codex::STATUS_RECAP;

/// The built-in package's id in the lockfile.
const BUILTIN_ID: &str = "connecto";
const LOCK_DIR: &str = ".connecto";
const LOCK_FILE: &str = "packages.lock";
/// A lockfile is a list of paths and hashes: past this it is not read.
const MAX_LOCK_BYTES: u64 = 4 * 1024 * 1024;

/// The text for a launch of `tool`: `None` for Claude, for a utility session
/// and for a project without the built-in package.
#[must_use]
pub fn for_launch(tool: Tool, utility: bool, project: &Path) -> Option<String> {
    if tool == Tool::Claude || utility || !has_builtin(project) {
        return None;
    }
    Some(STATUS_RECAP.trim().to_owned())
}

/// Whether the project's lockfile lists the built-in package. A lockfile or
/// folder that is a link, or a lockfile that is not a plain file, counts as
/// none, as the installer treats it.
#[must_use]
pub fn has_builtin(project: &Path) -> bool {
    let dir = project.join(LOCK_DIR);
    let file = dir.join(LOCK_FILE);
    let plain = |path: &Path, folder: bool| {
        fs::symlink_metadata(path).is_ok_and(|m| {
            !m.file_type().is_symlink() && if folder { m.is_dir() } else { m.is_file() }
        })
    };
    if !plain(&dir, true) || !plain(&file, false) {
        return false;
    }
    let mut text = String::new();
    let read =
        fs::File::open(&file).and_then(|f| f.take(MAX_LOCK_BYTES + 1).read_to_string(&mut text));
    if read.is_err() || text.len() as u64 > MAX_LOCK_BYTES {
        return false;
    }
    let Ok(lock) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    lock.get("packages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|packages| {
            packages
                .iter()
                .any(|p| p.get("id").and_then(serde_json::Value::as_str) == Some(BUILTIN_ID))
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn project(packages: &[(&str, u32)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let packages: Vec<serde_json::Value> = packages
            .iter()
            .map(|(id, v)| serde_json::json!({"id": id, "version": v, "hash": "h"}))
            .collect();
        let lock = serde_json::json!({"version": 1, "packages": packages, "files": {}});
        fs::create_dir_all(dir.path().join(LOCK_DIR)).unwrap();
        fs::write(dir.path().join(LOCK_DIR).join(LOCK_FILE), lock.to_string()).unwrap();
        dir
    }

    #[test]
    fn the_recap_is_the_built_in_text_and_names_the_four_points() {
        for label in [
            "**Done**",
            "**Committed/pushed**",
            "**Not done / blocked**",
            "**Waiting on user**",
        ] {
            assert!(STATUS_RECAP.contains(label), "{label}");
        }
    }

    /// Every agent but Claude gets the recap, in a project with the built-in
    /// package, whatever version the lockfile records (one from before the
    /// recap included: the text is Connecto's, not the project's).
    #[test]
    fn the_recap_goes_to_every_agent_but_claude_when_the_builtin_is_installed() {
        for version in [8, 9] {
            let dir = project(&[("connecto", version), ("team", 1)]);
            for tool in [Tool::Codex, Tool::Pi, Tool::Antigravity, Tool::OpenCode] {
                assert_eq!(
                    for_launch(tool, false, dir.path()).as_deref(),
                    Some(STATUS_RECAP.trim()),
                    "{tool:?}"
                );
                assert_eq!(for_launch(tool, true, dir.path()), None, "utility");
            }
            assert_eq!(for_launch(Tool::Claude, false, dir.path()), None);
        }
    }

    /// Nothing without the built-in package, and nothing the project wrote is
    /// ever passed: an instructions file of its own is not read.
    #[test]
    fn nothing_without_the_builtin_package_and_never_project_text() {
        let none = tempfile::tempdir().unwrap();
        assert_eq!(
            for_launch(Tool::Codex, false, none.path()),
            None,
            "no lockfile"
        );
        let other = project(&[("team", 1)]);
        assert_eq!(for_launch(Tool::Codex, false, other.path()), None);
        let edited = project(&[("connecto", 9)]);
        let rule = edited
            .path()
            .join(".agents/instructions/connecto-status.md");
        fs::create_dir_all(rule.parent().unwrap()).unwrap();
        fs::write(&rule, "PROJECT TEXT").unwrap();
        let text = for_launch(Tool::Codex, false, edited.path()).unwrap();
        assert!(!text.contains("PROJECT TEXT"));
        let broken = project(&[]);
        fs::write(broken.path().join(LOCK_DIR).join(LOCK_FILE), "not json").unwrap();
        assert_eq!(for_launch(Tool::Codex, false, broken.path()), None);
    }

    /// Codex gets the recap and the bridge text through Connecto's
    /// `SessionStart` hook: the launch and the helper must name them by the
    /// same variables.
    #[test]
    fn the_codex_launch_and_hook_agree_on_the_recap_flag() {
        assert_eq!(
            connecto_adapter::CODEX_RECAP_ENV,
            connecto_hook::codex::RECAP_ENV
        );
        assert_eq!(
            connecto_adapter::CODEX_BRIDGE_TEXT_ENV,
            connecto_hook::codex::BRIDGE_TEXT_ENV
        );
        // The hook knows Connecto's messages in a thread by how every bridge
        // text starts.
        assert!(
            crate::bridge::render::launch_text_with("codex-1", true, true)
                .starts_with(connecto_hook::codex::BRIDGE_TEXT_START)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_lockfile_counts_as_none() {
        let real = project(&[("connecto", 9)]);
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(real.path().join(LOCK_DIR), dir.path().join(LOCK_DIR)).unwrap();
        assert_eq!(for_launch(Tool::Codex, false, dir.path()), None);
    }
}
