//! Unix side of [`crate::platform`]: everything as it always was.

use std::fs;
use std::io;
use std::path::Path;

/// A project's path as the sidebar, the recents and the other parts of the
/// app are given it: the real path, as it is.
pub(crate) fn shown(path: &Path) -> String {
    path.display().to_string()
}

/// Whether a folder saved in the settings file is an absolute path.
pub(crate) fn absolute(text: &str) -> bool {
    text.starts_with('/')
}

/// Put `staged` in place of `target` in one step.
pub(crate) fn replace(staged: &Path, target: &Path) -> io::Result<()> {
    fs::rename(staged, target)
}
