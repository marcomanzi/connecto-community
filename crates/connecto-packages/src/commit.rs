//! A small git commit of the package files an install changed.
//!
//! Only for a project that already keeps its package files in git: nothing is
//! committed unless at least one of the changed paths is tracked, so a first
//! install, and a project that ignores these folders, are left alone. Only the
//! given paths go into the commit. Whatever else is modified or staged in the
//! project stays as it was.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The message of every such commit.
pub const MESSAGE: &str = "Connecto Packages update";

/// How long one git command may take. A hook or a passphrase prompt that
/// never returns must not hold the packages thread.
const WAIT: Duration = Duration::from_secs(20);

/// Work in progress that a commit would land in the middle of.
const IN_PROGRESS: [&str; 6] = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "rebase-merge",
    "rebase-apply",
];

/// What one git command printed, when it ended well.
fn git(project: &Path, args: &[&str], paths: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(project)
        .args(args)
        .args(paths)
        // The paths are names, never patterns, and git never asks anything.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("git: {e}"))?;
    let deadline = Instant::now() + WAIT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("git {} took too long", args.join(" ")));
            }
            Err(e) => return Err(format!("git: {e}")),
        }
    };
    let read = |pipe: Option<&mut dyn Read>| {
        let mut text = String::new();
        if let Some(pipe) = pipe {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    };
    let out = read(child.stdout.as_mut().map(|p| p as &mut dyn Read));
    if status.success() {
        return Ok(out);
    }
    let err = read(child.stderr.as_mut().map(|p| p as &mut dyn Read));
    Err(err
        .lines()
        .next()
        .map_or_else(|| format!("git {} failed", args.join(" ")), str::to_owned))
}

/// Whether `project` is inside a git work tree, found without running git (on
/// a Mac without the developer tools, running it opens an install prompt).
fn in_repo(project: &Path) -> bool {
    project.ancestors().any(|dir| dir.join(".git").exists())
}

fn listed(out: &str) -> BTreeSet<String> {
    out.split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Commit `paths` (relative to `project`: files written, changed or removed)
/// and nothing else. `Ok(true)` when a commit was made, `Ok(false)` when there
/// was nothing to do here, `Err` when git refused.
pub fn commit(project: &Path, paths: &[String]) -> Result<bool, String> {
    if paths.is_empty() || !in_repo(project) {
        return Ok(false);
    }
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    // No repository after all, no git, or no commit yet: not ours to start.
    let Ok(git_dir) = git(project, &["rev-parse", "--absolute-git-dir"], &[]) else {
        return Ok(false);
    };
    if git(project, &["rev-parse", "--verify", "-q", "HEAD"], &[]).is_err() {
        return Ok(false);
    }
    let git_dir = Path::new(git_dir.trim());
    if IN_PROGRESS.iter().any(|name| git_dir.join(name).exists()) {
        return Ok(false);
    }
    let tracked = listed(&git(project, &["ls-files", "-z", "--"], &paths)?);
    if tracked.is_empty() {
        return Ok(false);
    }
    // New files join the commit unless the project ignores them.
    let new = listed(&git(
        project,
        &["ls-files", "-z", "--others", "--exclude-standard", "--"],
        &paths,
    )?);
    // Staged in two steps: a tracked file under a folder that is ignored now
    // is refused by a plain `git add`, but not by `-u`.
    let known: Vec<&str> = tracked.iter().map(String::as_str).collect();
    git(project, &["add", "-u", "--"], &known)?;
    let added: Vec<&str> = new.iter().map(String::as_str).collect();
    if !added.is_empty() {
        git(project, &["add", "--"], &added)?;
    }
    let ours: Vec<&str> = known.iter().chain(&added).copied().collect();
    // Exit 1 is "there is a difference"; anything else here means none.
    if git(project, &["diff", "--cached", "--quiet", "--"], &ours).is_ok() {
        return Ok(false);
    }
    // No hooks: this is Connecto's own bookkeeping, not the user's work.
    git(
        project,
        &["commit", "-q", "--no-verify", "-m", MESSAGE, "--only", "--"],
        &ours,
    )?;
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-commit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "test@example.invalid"],
            &["config", "user.name", "Test"],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(&dir, args, &[]).unwrap();
        }
        dir
    }

    fn put(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn commit_all(dir: &Path) {
        git(dir, &["add", "-A"], &[]).unwrap();
        git(dir, &["commit", "-q", "-m", "base"], &[]).unwrap();
    }

    fn owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn only_the_package_files_are_committed_and_the_rest_stays_as_it_was() {
        let dir = repo("only");
        put(&dir, ".claude/skills/a/SKILL.md", "a1");
        put(&dir, ".claude/skills/old/SKILL.md", "old");
        put(&dir, "src/main.rs", "fn main() {}");
        put(&dir, "notes.md", "one");
        commit_all(&dir);
        // The user's own work: one file staged, one only modified.
        put(&dir, "src/main.rs", "fn main() { todo!() }");
        git(&dir, &["add", "src/main.rs"], &[]).unwrap();
        put(&dir, "notes.md", "two");
        // The update: one file changed, one dropped, one new.
        put(&dir, ".claude/skills/a/SKILL.md", "a2");
        fs::remove_file(dir.join(".claude/skills/old/SKILL.md")).unwrap();
        put(&dir, ".claude/skills/new/SKILL.md", "new");
        let paths = owned(&[
            ".claude/skills/a/SKILL.md",
            ".claude/skills/old/SKILL.md",
            ".claude/skills/new/SKILL.md",
        ]);
        assert_eq!(commit(&dir, &paths), Ok(true));
        assert_eq!(
            git(&dir, &["log", "-1", "--format=%s"], &[])
                .unwrap()
                .trim(),
            MESSAGE
        );
        let in_commit = git(&dir, &["show", "--name-status", "--format="], &[]).unwrap();
        let mut lines: Vec<&str> = in_commit.lines().filter(|l| !l.is_empty()).collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            [
                "A\t.claude/skills/new/SKILL.md",
                "D\t.claude/skills/old/SKILL.md",
                "M\t.claude/skills/a/SKILL.md",
            ]
        );
        let status = git(&dir, &["status", "--porcelain"], &[]).unwrap();
        assert_eq!(status, " M notes.md\nM  src/main.rs\n", "{status}");
        // Nothing changed since: no second commit.
        assert_eq!(commit(&dir, &paths), Ok(false));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_committed_where_the_package_files_are_not_in_git() {
        // A first install, or a project that never committed them.
        let dir = repo("untracked");
        put(&dir, "README.md", "hi");
        commit_all(&dir);
        put(&dir, ".claude/skills/a/SKILL.md", "a1");
        assert_eq!(
            commit(&dir, &owned(&[".claude/skills/a/SKILL.md"])),
            Ok(false)
        );
        // Ignored files stay out even when others are tracked.
        put(&dir, ".codex/agents/x.toml", "x1");
        commit_all(&dir);
        put(&dir, ".gitignore", ".claude/\n");
        put(&dir, ".codex/agents/x.toml", "x2");
        let paths = owned(&[".codex/agents/x.toml", ".claude/skills/a/SKILL.md"]);
        // `.claude/skills/a` was committed above, before it was ignored.
        put(&dir, ".claude/skills/b/SKILL.md", "b");
        let mut with_new = paths.clone();
        with_new.push(".claude/skills/b/SKILL.md".to_owned());
        assert_eq!(commit(&dir, &with_new), Ok(true));
        let in_commit = git(&dir, &["show", "--name-only", "--format="], &[]).unwrap();
        assert_eq!(in_commit.trim(), ".codex/agents/x.toml");
        // Not a repository at all.
        let plain = std::env::temp_dir().join(format!("mg-commit-plain-{}", std::process::id()));
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(commit(&plain, &paths), Ok(false));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&plain);
    }
}
