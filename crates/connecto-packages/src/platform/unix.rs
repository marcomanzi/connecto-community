//! macOS (and other Unix) side of [`crate::platform`].

use std::fs::{self, FileType, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Folders in the home folder that hold the CLIs' or the system's state.
pub(crate) const HOME_STATE: [&str; 6] =
    [".claude", ".codex", ".local", "Library", ".pi", ".gemini"];

/// The variable that names the user's home folder.
pub(crate) const HOME_VAR: &str = "HOME";

/// The error a rename across volumes gives (`EXDEV`).
pub(crate) const CROSS_DEVICE: i32 = 18;

/// The file's permission bits, as a restore puts them back.
pub(crate) fn mode(meta: &Metadata) -> u32 {
    meta.permissions().mode() & 0o777
}

/// Whether any executable bit is set.
pub(crate) fn any_exec(meta: &Metadata) -> bool {
    meta.permissions().mode() & 0o111 != 0
}

/// Whether a file read from a package folder is executable: its mode says.
pub(crate) fn marked_exec(meta: &Metadata, _bytes: &[u8]) -> bool {
    any_exec(meta)
}

/// Whether the owner may run the file.
pub(crate) fn owner_exec(meta: &Metadata) -> Option<bool> {
    Some(meta.permissions().mode() & 0o100 != 0)
}

/// Create the file with this mode.
pub(crate) fn create_mode(options: &mut OpenOptions, mode: u32) {
    options.mode(mode);
}

/// Make a file anyone may read and run.
pub(crate) fn make_executable(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

/// Remove the link itself, never what it points to.
pub(crate) fn remove_link(path: &Path, _kind: &FileType) -> io::Result<()> {
    fs::remove_file(path)
}

/// Every name Unix can hold is fine here (`/` never reaches this).
pub(crate) const fn portable_name(_name: &str) -> bool {
    true
}

/// The real path of a folder, or the path as given when it does not exist.
pub(crate) fn resolve(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// Make `link` a link to the folder `target` (tests).
#[cfg(test)]
pub(crate) fn link_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}
