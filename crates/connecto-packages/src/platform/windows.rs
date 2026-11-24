//! Windows side of [`crate::platform`].
//!
//! Windows files have no mode bits: a package's executable flag still counts
//! for its hash, but installing sets nothing. A file read from a package
//! folder is executable when it starts with `#!`, as the build script decides
//! for the built-in package. Links are file symlinks, folder symlinks and
//! junctions; `std` reports all three as links, so every link check of the
//! crate also stops at a junction.

use std::fs::{self, FileType, Metadata, OpenOptions};
use std::io;
use std::os::windows::fs::FileTypeExt;
use std::path::{Path, PathBuf};

/// Folders in the home folder that hold the CLIs' or the system's state.
/// `AppData` is what `Library` is on macOS.
pub(crate) const HOME_STATE: [&str; 6] =
    [".claude", ".codex", ".local", "AppData", ".pi", ".gemini"];

/// The variable that names the user's home folder.
pub(crate) const HOME_VAR: &str = "USERPROFILE";

/// The error a rename across volumes gives (`ERROR_NOT_SAME_DEVICE`).
pub(crate) const CROSS_DEVICE: i32 = 17;

/// Windows keeps no permission bits: the usual mode, which nothing applies.
pub(crate) const fn mode(_meta: &Metadata) -> u32 {
    0o644
}

/// There is no executable bit to read.
pub(crate) const fn any_exec(_meta: &Metadata) -> bool {
    false
}

/// Whether a file read from a package folder is a script: it starts with
/// `#!` (a checkout on Windows cannot carry git's executable mode).
pub(crate) fn marked_exec(_meta: &Metadata, bytes: &[u8]) -> bool {
    bytes.starts_with(b"#!")
}

/// No bit to compare with, so a file's mode never calls for a rewrite.
pub(crate) const fn owner_exec(_meta: &Metadata) -> Option<bool> {
    None
}

/// Nothing: Windows files have no mode.
pub(crate) const fn create_mode(_options: &mut OpenOptions, _mode: u32) {}

/// Nothing: Windows runs a script by its extension or its interpreter.
pub(crate) const fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Remove the link itself, never what it points to. A folder symlink or a
/// junction is removed as a folder (`remove_file` is refused for those).
pub(crate) fn remove_link(path: &Path, kind: &FileType) -> io::Result<()> {
    if kind.is_symlink_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// Whether Windows stores a file or folder of this name as it is: no
/// separator, drive or stream colon, wildcard or control character, no
/// trailing dot or space (Windows drops them, so `a.` would be `a` and `.. `
/// would be `..`), and no device name (`NUL`, `con.txt`, `COM1`, ...).
pub(crate) fn portable_name(name: &str) -> bool {
    const DEVICES: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let numbered = {
        let mut chars = stem.chars();
        let head: String = chars.by_ref().take(3).collect();
        let tail: Vec<char> = chars.collect();
        (head.eq_ignore_ascii_case("COM") || head.eq_ignore_ascii_case("LPT"))
            && matches!(tail.as_slice(), [c] if c.is_ascii_digit() || matches!(c, '¹' | '²' | '³'))
    };
    !name.is_empty()
        && !name.ends_with(['.', ' '])
        && !name.chars().any(|c| {
            c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
        && !DEVICES
            .iter()
            .any(|device| stem.eq_ignore_ascii_case(device))
        && !numbered
}

/// The real path of a folder. One that does not exist yet is its nearest
/// existing parent's real path with the rest added, so that it compares
/// with other real paths: those carry the `\\?\` prefix, which a path as
/// given does not, and two paths that differ there never match.
pub(crate) fn resolve(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = fs::canonicalize(at) {
            let mut out = real;
            for name in rest.iter().rev() {
                out.push(name);
            }
            return out;
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                at = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

/// Make `link` a junction to the folder `target` (tests). A junction needs
/// no privilege, unlike a symlink.
#[cfg(test)]
pub(crate) fn link_dir(target: &Path, link: &Path) -> io::Result<()> {
    // `cmd` reads a `/` as the start of a switch: backslashes only.
    let plain = |path: &Path| path.components().collect::<PathBuf>();
    let out = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(plain(link))
        .arg(plain(target))
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "mklink /J failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn names_windows_cannot_hold_as_they_are_are_refused() {
        for ok in [
            "a.md", "SKILL.md", "console", "nul-2", "com10", "lpt", ".claude", "a b",
        ] {
            assert!(portable_name(ok), "{ok}");
        }
        for bad in [
            "",
            "NUL",
            "nul",
            "nul.txt",
            "Con",
            "aux.tar.gz",
            "COM1",
            "lpt9",
            "com²",
            "CONIN$",
            "nul .txt",
            "a.",
            "a ",
            ".. ",
            "..",
            ".",
            "a:b",
            "C:",
            r"a\b",
            "a/b",
            "a*",
            "a?",
            "a|b",
            "a<b",
            "a\"b",
            "a\u{1}",
        ] {
            assert!(!portable_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_missing_folder_resolves_under_its_real_parent() {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let missing = std::env::temp_dir()
            .join("mg-resolve-missing")
            .join("deeper");
        assert_eq!(
            resolve(&missing),
            base.join("mg-resolve-missing").join("deeper")
        );
        assert!(resolve(&missing).starts_with(&base));
    }

    #[test]
    fn a_junction_is_a_link_and_is_removed_as_one() {
        let base = std::env::temp_dir().join(format!("mg-junction-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (target, link) = (base.join("target"), base.join("link"));
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep"), "k").unwrap();
        link_dir(&target, &link).unwrap();
        let kind = fs::symlink_metadata(&link).unwrap().file_type();
        assert!(kind.is_symlink() && !kind.is_dir());
        remove_link(&link, &kind).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(target.join("keep").is_file(), "the target stays");
        let _ = fs::remove_dir_all(&base);
    }
}
