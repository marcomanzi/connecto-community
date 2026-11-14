//! Unix side of [`crate::platform`].

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Parent variables a tool may inherit. The adapters add the rest.
pub(crate) const INHERITED: &[&str] = &["USER", "LOGNAME", "PATH", "LANG"];

/// What joins the folders of `PATH`.
pub(crate) const PATH_SEPARATOR: &str = ":";

/// Folders in the home folder that hold the CLIs' or the system's state: a
/// project may not be one of them or sit inside one.
pub(crate) const HOME_STATE: [&str; 4] = [".claude", ".codex", ".local", "Library"];

/// The shell used by the panes. `-f` skips user startup files.
pub const SHELL: &str = "/bin/zsh";

/// The program [`crate::shell_spec`] runs.
pub(crate) fn shell_spec_program() -> PathBuf {
    PathBuf::from(SHELL)
}

/// The working folder as a child is given it: as it is.
pub(crate) fn child_cwd(path: &Path) -> PathBuf {
    path.to_owned()
}

/// The arguments for [`SHELL`]: no startup files, and `command` if given.
pub(crate) fn shell_spec_args(command: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-f".into()];
    if let Some(command) = command {
        args.push("-c".into());
        args.push(command.into());
    }
    args
}

/// A folder only the user can enter, with its parents.
pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// As [`private_dir`], but this must be the call that creates it.
pub(crate) fn new_private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

pub(crate) fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// The owner-only socket folder, checked before use.
pub(crate) fn socket_dir() -> io::Result<PathBuf> {
    let uid = nix::unistd::getuid();
    let dir = PathBuf::from(format!("/private/tmp/mg-{uid}"));
    private_dir(&dir)?;
    let meta = fs::symlink_metadata(&dir)?;
    let private = meta.is_dir()
        && !meta.file_type().is_symlink()
        && std::os::unix::fs::MetadataExt::uid(&meta) == uid.as_raw()
        && meta.permissions().mode() & 0o077 == 0;
    if private {
        Ok(dir)
    } else {
        Err(io::Error::other("the socket folder is not private"))
    }
}

/// A hook or bridge endpoint: the socket `name` in [`socket_dir`] (`kind` is
/// for Windows, whose pipe names are random).
pub(crate) fn endpoint(_kind: &str, name: String) -> io::Result<PathBuf> {
    Ok(socket_dir()?.join(name))
}

/// Whether no process has this pid. One that only cannot be signalled exists.
pub(crate) fn no_such_process(pid: i32) -> bool {
    matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

/// Whether a directory entry is a socket a run left behind.
pub(crate) fn is_socket(kind: &fs::FileType) -> bool {
    kind.is_socket()
}

/// Whether `path` is a file the user may run.
pub(crate) fn is_executable(_path: &Path, meta: &fs::Metadata) -> bool {
    meta.is_file() && meta.permissions().mode() & 0o111 != 0
}

/// The shell named by the environment: `$SHELL`.
pub(crate) fn shell_from_env() -> Option<OsString> {
    std::env::var_os("SHELL")
}

/// Shells to try, in order, when none was chosen.
pub(crate) fn shell_candidates() -> Vec<PathBuf> {
    ["/bin/zsh", "/bin/bash", "/bin/sh"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// The shell when no candidate exists.
pub(crate) fn last_resort_shell() -> PathBuf {
    PathBuf::from("/bin/sh")
}

/// A plain terminal's shell runs as an interactive login shell.
pub(crate) fn login_shell_args(_program: &Path) -> Vec<OsString> {
    vec!["-l".into(), "-i".into()]
}

/// What a plain terminal's shell is told about itself: `SHELL`.
pub(crate) fn shell_env(program: &Path) -> Vec<(OsString, OsString)> {
    vec![("SHELL".into(), program.as_os_str().to_owned())]
}

/// Run `command` as it is: Unix programs get no window of their own.
pub(crate) fn no_console(command: &mut std::process::Command) -> &mut std::process::Command {
    command
}

/// The whole environment of the `codex app-server` that reads hook trust
/// ([`crate::codex_trust`]): a scratch home and a fixed `PATH`.
pub(crate) fn codex_trust_env(scratch: &Path) -> Vec<(OsString, OsString)> {
    vec![
        ("CODEX_HOME".into(), scratch.into()),
        ("HOME".into(), scratch.into()),
        ("PATH".into(), "/usr/bin:/bin".into()),
    ]
}

/// Agent terminals (Claude Code, Codex) can run here.
pub(crate) const fn agents_supported() -> io::Result<()> {
    Ok(())
}
