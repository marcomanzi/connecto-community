//! File writes that never follow a link and never reuse a predictable name.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

use connecto_hook::package::TOOL_DIRS;

use crate::platform;

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// A relative path of normal components only. On Windows every part must also
/// be a name Windows keeps as it is, so `..\x`, `C:\x`, `\\server\share`,
/// `a:stream`, `.. ` or `nul` never pass.
#[must_use]
pub fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && platform::portable_name(part)
        })
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// A project-relative path a package may install: inside `.claude/`, `.codex/`
/// or `.agents/`, with no `..`, no absolute part, no empty part.
#[must_use]
pub fn tool_path(path: &str) -> bool {
    safe_relative(path)
        && !path.contains("//")
        && path
            .split_once('/')
            .is_some_and(|(first, rest)| TOOL_DIRS.contains(&first) && !rest.is_empty())
}

/// True if some existing part of `base/rel` is a link.
#[must_use]
pub fn has_link(base: &Path, rel: &str) -> bool {
    let mut at = base.to_owned();
    for part in Path::new(rel).components() {
        at.push(part);
        if fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            return true;
        }
    }
    false
}

/// Make the folders above `base/rel`, refusing to pass through a link or a file.
pub fn ensure_parents(base: &Path, rel: &str) -> io::Result<()> {
    let Some(parent) = Path::new(rel).parent() else {
        return Ok(());
    };
    let mut at = base.to_owned();
    for part in parent.components() {
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::other(format!(
                    "{} is a link or not a folder",
                    at.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&at)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Write `base/rel` through a new file with an unpredictable name (created
/// exclusively, so a planted link is never followed), then rename it into
/// place. A link at `rel` itself is replaced, not written through.
pub fn write_atomic(base: &Path, rel: &str, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    if !safe_relative(rel) {
        return Err(io::Error::other(format!("unsafe path {rel}")));
    }
    ensure_parents(base, rel)?;
    let target = base.join(rel);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let staged = target.with_file_name(format!(
        ".{name}.{}-{}.connecto-tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        platform::create_mode(&mut options, mode.unwrap_or(0o644));
        let mut file = options.open(&staged)?;
        file.write_all(bytes)?;
        file.flush()?;
        fs::rename(&staged, &target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&staged);
    }
    written
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn only_tool_folder_paths_pass() {
        for ok in [".claude/a", ".codex/agents/x.toml", ".agents/s/SKILL.md"] {
            assert!(tool_path(ok), "{ok}");
        }
        for bad in [
            "",
            "../x",
            ".claude/../../x",
            "/etc/x",
            ".claude",
            ".claude/",
            "x/y",
            ".claude//a",
            "./.claude/a",
            ".claude/./a",
            ".connecto/packages.lock",
        ] {
            assert!(!tool_path(bad), "{bad}");
        }
    }

    /// What escapes the root on Windows: never a tool path anywhere (on Unix a
    /// backslash is part of a name, so these are odd names but not tool paths).
    #[test]
    fn windows_separators_drives_and_shares_are_not_tool_paths() {
        for bad in [
            r"..\x",
            r"C:\x",
            "C:x",
            "C:/x",
            r"\\server\share",
            r"\\server\share\.claude\x",
            r"\\?\C:\x",
            r"\\.\pipe\x",
            r"\x",
            r".claude\x",
            r".claude\..\..\x",
        ] {
            assert!(!tool_path(bad), "{bad}");
        }
    }

    /// On Windows a backslash, a colon, a trailing dot or space and a device
    /// name inside a tool folder are refused too: each would reach another
    /// file than the one named (a parent, a stream, a device).
    #[cfg(windows)]
    #[test]
    fn windows_only_escapes_inside_a_tool_folder_are_refused() {
        for bad in [
            r".claude/..\..\x",
            r".claude/a\..\..\..\x",
            r".claude/C:\x",
            r".claude/\\?\C:\x",
            ".claude/a:stream",
            ".claude/settings.json:x",
            ".claude/.. /x",
            ".claude/a.",
            ".claude/a ",
            ".claude/nul",
            ".claude/CON.md",
            ".claude/skills/com1/x",
            ".claude/a*",
        ] {
            assert!(!tool_path(bad), "{bad}");
            assert!(!safe_relative(bad), "{bad}");
        }
        assert!(tool_path(".claude/skills/console/SKILL.md"));
    }

    #[cfg(windows)]
    #[test]
    fn writes_do_not_pass_a_junction() {
        let base = std::env::temp_dir().join(format!("mg-fsx-junction-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let outside = base.join("outside");
        let project = base.join("project");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(project.join(".claude")).unwrap();
        platform::link_dir(&outside, &project.join(".codex")).unwrap();
        assert!(has_link(&project, ".codex/x"));
        assert!(write_atomic(&project, ".codex/x", b"x", None).is_err());
        platform::link_dir(&outside, &project.join(".claude/inner")).unwrap();
        assert!(write_atomic(&project, ".claude/inner/x", b"x", None).is_err());
        // A junction at the file itself is replaced, as a link is on Unix.
        write_atomic(&project, ".claude/inner", b"x", None).unwrap();
        assert_eq!(fs::read(project.join(".claude/inner")).unwrap(), b"x");
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn writes_do_not_pass_links_and_leave_no_temp_files() {
        let base = std::env::temp_dir().join(format!("mg-fsx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let outside = base.join("outside");
        let project = base.join("project");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&project).unwrap();
        write_atomic(&project, ".claude/a.md", b"a", None).unwrap();
        assert_eq!(fs::read(project.join(".claude/a.md")).unwrap(), b"a");
        std::os::unix::fs::symlink(&outside, project.join(".codex")).unwrap();
        assert!(write_atomic(&project, ".codex/x", b"x", None).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        // A link at the file itself is replaced, the target left alone.
        fs::write(outside.join("victim"), "v").unwrap();
        std::os::unix::fs::symlink(outside.join("victim"), project.join(".claude/l")).unwrap();
        write_atomic(&project, ".claude/l", b"new", None).unwrap();
        assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "v");
        assert_eq!(fs::read(project.join(".claude/l")).unwrap(), b"new");
        let names: Vec<_> = fs::read_dir(project.join(".claude"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| !n.ends_with(".connecto-tmp")),
            "{names:?}"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
