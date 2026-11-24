//! macOS (and other Unix) side of [`crate::platform`].

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::store::Tool;

/// The variables a launcher script is run with besides `PATH`, when the rest
/// of the environment is cleared: none (`HOME` is set by each caller).
#[must_use]
pub const fn launcher_env(_home: &Path) -> Vec<(OsString, OsString)> {
    Vec::new()
}

/// The helper's file name in the store.
pub const HOOK_NAME: &str = "connecto-hook";
/// The `connecto` command beside it: a link to the helper.
pub const CONNECTO_LINK: &str = "connecto";
/// Claude's executable, relative to its version folder.
pub const CLAUDE_ENTRY: &str = "claude";
/// Codex's executable, relative to its version folder.
pub const CODEX_ENTRY: &str = "bin/codex";
/// The Apple Team ID that must have signed Claude Code.
pub const CLAUDE_SIGNER: &str = "Q6L2SF6YDW";
/// The Apple Team ID that must have signed Codex.
pub const CODEX_SIGNER: &str = "2DC432GLL2";

/// Whether the file is a program or library macOS would load (Mach-O),
/// judged by its content.
pub fn is_native_binary(file: &Path) -> io::Result<bool> {
    crate::signature::is_macho(file)
}

/// The signer every native binary of `tool`'s copy must have: the tool's
/// team, for every file. There is no per-file exemption on macOS.
#[must_use]
pub const fn bundled_signer(tool: Tool, _file: &Path) -> Option<&'static str> {
    Some(tool.team_id())
}

/// Kept while a staged copy is checked. Nothing is held on Unix: the copy's
/// folders are made read-only and its digests are checked before every use.
#[derive(Debug)]
pub struct Held;

/// See [`Held`].
pub const fn hold(_file: &Path) -> io::Result<Held> {
    Ok(Held)
}

/// The npm package a launcher script stands for: none on Unix, where npm's
/// command is a link that is followed.
#[must_use]
pub const fn codex_npm_shim_target(_real: &Path) -> Option<PathBuf> {
    None
}

/// The user's home folder: `HOME`.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The data root when it is not overridden: the app's Application Support folder.
#[must_use]
pub fn default_data_root(home: &Path) -> PathBuf {
    home.join("Library/Application Support/connecto")
}

/// Run `command` as it is: Unix programs get no window of their own.
pub fn no_console(command: &mut std::process::Command) -> &mut std::process::Command {
    command
}

/// Set the Unix mode bits of `path`.
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Whether the file is executable: its owner execute bit.
#[must_use]
pub fn is_executable(_path: &Path, meta: &fs::Metadata) -> bool {
    meta.permissions().mode() & 0o100 != 0
}

/// Whether a process with this id exists. One that may not be signalled
/// exists too.
#[must_use]
pub fn pid_alive(pid: i32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    pid > 0 && !matches!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH))
}

/// Whether `link` already is the `connecto` command for the helper `hook`
/// (a file name in the same folder): a relative link to it.
#[must_use]
pub fn connecto_link_is_current(_dir: &Path, link: &Path, hook: &str) -> bool {
    fs::read_link(link).is_ok_and(|to| to == Path::new(hook))
}

/// Make `staged` the `connecto` command for the helper `hook`: a relative link.
pub fn make_connecto_link(_dir: &Path, hook: &str, staged: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(hook, staged)
}

/// Put `staged` in place of `target`, atomically.
pub fn replace(staged: &Path, target: &Path) -> io::Result<()> {
    fs::rename(staged, target)
}

/// Nothing to do: [`replace`] leaves no old files behind on Unix.
pub const fn sweep_replaced(_dir: &Path) {}

/// Whether the file's name alone says it is a launcher script. On Unix only its
/// content (`#!`) says so.
#[must_use]
pub const fn is_launcher_by_name(_path: &Path) -> bool {
    false
}

/// The platform folder name npm's Codex package uses for this Mac, and the
/// name of the optional package that carries its binaries.
#[must_use]
pub const fn codex_platform() -> (&'static str, &'static str) {
    if cfg!(target_arch = "x86_64") {
        ("x86_64-apple-darwin", "codex-darwin-x64")
    } else {
        ("aarch64-apple-darwin", "codex-darwin-arm64")
    }
}
