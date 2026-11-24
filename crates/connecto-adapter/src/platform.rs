//! Which operating system a launch is described for.
//!
//! The shapes that differ per system (hook commands, quoting, the environment,
//! Codex's hook keys) are written as functions of [`Flavor`], so both shapes
//! are unit-tested on every host. The public API always uses [`Flavor::HOST`].
//! Facts for the Windows shapes: `docs/windows-port.md` section 4.8.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    /// macOS (and Linux): hooks run through `sh -c`.
    Unix,
    /// Windows: Claude runs exec-form hooks directly, Codex runs hook commands
    /// through PowerShell.
    Windows,
}

impl Flavor {
    /// The system this build runs on.
    pub(crate) const HOST: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };

    /// What joins the folders of `PATH`.
    pub(crate) const fn path_separator(self) -> &'static str {
        match self {
            Self::Unix => ":",
            Self::Windows => ";",
        }
    }
}

/// `path` in the form a tool reports paths in, for comparing with them.
///
/// On Windows the session layer passes canonical paths, which carry the
/// verbatim prefix (`\\?\C:\…`), while the tools report plain ones (`C:\…`),
/// and `Path::starts_with` tells the two prefixes apart. Elsewhere the path is
/// used as it is.
#[cfg(windows)]
pub(crate) fn comparable(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_owned();
    };
    match text.strip_prefix(r"\\?\") {
        Some(rest)
            if rest.len() >= 3
                && rest.as_bytes()[0].is_ascii_alphabetic()
                && rest.as_bytes()[1] == b':'
                && rest.as_bytes()[2] == b'\\' =>
        {
            PathBuf::from(rest)
        }
        _ => path.to_owned(),
    }
}

/// See the Windows version: elsewhere the path is used as it is.
#[cfg(not(windows))]
pub(crate) fn comparable(path: &Path) -> PathBuf {
    path.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_separator_follows_the_flavor() {
        assert_eq!(Flavor::Unix.path_separator(), ":");
        assert_eq!(Flavor::Windows.path_separator(), ";");
    }

    #[cfg(windows)]
    #[test]
    fn a_verbatim_drive_path_compares_as_the_plain_one() {
        assert_eq!(
            comparable(Path::new(r"\\?\C:\Users\a b\p")),
            PathBuf::from(r"C:\Users\a b\p")
        );
        assert_eq!(
            comparable(Path::new(r"C:\Users\p")),
            PathBuf::from(r"C:\Users\p")
        );
        // A verbatim UNC path keeps its prefix: it has no plain drive form.
        assert_eq!(
            comparable(Path::new(r"\\?\UNC\server\share\p")),
            PathBuf::from(r"\\?\UNC\server\share\p")
        );
    }
}
