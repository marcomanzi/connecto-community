//! Windows side of [`crate::platform`].
//!
//! The default shell is PowerShell: `pwsh.exe` when it is on `PATH`, else
//! Windows PowerShell (`powershell.exe`). Folders get no Unix modes: the data
//! folder is under `%APPDATA%`, which only the user (and administrators) can
//! read. Agent terminals run Claude Code and Codex natively. The hook and
//! bridge endpoints are named pipes.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Parent variables a tool may inherit: the ones Windows programs, PowerShell
/// included, expect, as in `connecto_pty::ENV_ALLOWLIST`, without `HOME`
/// (the launch sets it). None of these carries a credential.
pub(crate) const INHERITED: &[&str] = &[
    "USERPROFILE",
    "USERNAME",
    "USERDOMAIN",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ALLUSERSPROFILE",
    "PUBLIC",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATH",
    "PATHEXT",
    "PSModulePath",
    "TEMP",
    "TMP",
    "COMPUTERNAME",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "LANG",
];

/// What joins the folders of `PATH`.
pub(crate) const PATH_SEPARATOR: &str = ";";

/// Folders in the home folder that hold the CLIs' or the system's state: a
/// project may not be one of them or sit inside one. `AppData` is what
/// `Library` is on macOS.
pub(crate) const HOME_STATE: [&str; 4] = [".claude", ".codex", ".local", "AppData"];

/// The shell used by the panes: Windows PowerShell, where Windows installs it.
/// `-NoProfile` skips the user's profile scripts. Where Windows is not on
/// `C:`, [`shell_spec_program`] finds it under `%SystemRoot%`.
pub const SHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";

/// Windows PowerShell under `%SystemRoot%`.
fn system_powershell() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")?;
    Some(
        PathBuf::from(root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"),
    )
}

/// The program [`crate::shell_spec`] runs: Windows PowerShell under
/// `%SystemRoot%`, else [`SHELL`]. The PTY takes absolute programs only.
pub(crate) fn shell_spec_program() -> PathBuf {
    system_powershell()
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(SHELL))
}

/// The arguments for [`SHELL`]: no logo, no profile scripts, and `command` if
/// given.
pub(crate) fn shell_spec_args(command: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-NoLogo".into(), "-NoProfile".into()];
    if let Some(command) = command {
        args.push("-Command".into());
        args.push(command.into());
    }
    args
}

/// A folder for the user's own state, with its parents. Windows has no mode
/// bits; the folders are under the user's profile, whose access list already
/// keeps other users out.
pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}

/// As [`private_dir`], but this must be the call that creates it.
pub(crate) fn new_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

pub(crate) fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|err| io::Error::other(err.to_string()))?;
    Ok(bytes)
}

/// The folder the hook and bridge endpoints are named in: the named-pipe
/// namespace (served by [`crate::local_socket`]).
pub(crate) fn socket_dir() -> io::Result<PathBuf> {
    Ok(PathBuf::from(
        connecto_hook::transport::PIPE_PREFIX.trim_end_matches('\\'),
    ))
}

/// A hook or bridge endpoint: `\\.\pipe\connecto-<kind>-<32 random hex
/// digits>`. The pipe namespace is shared by every user and process on the
/// machine, so the name is not one another process can guess and take first
/// (the endpoint reaches the children by environment variable). The Unix
/// socket `name` is not used.
pub(crate) fn endpoint(kind: &str, _name: String) -> io::Result<PathBuf> {
    let random: String = random_bytes::<16>()?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(socket_dir()?.join(format!("connecto-{kind}-{random}")))
}

/// Whether no process has this pid. When the process list cannot be read, the
/// process is taken to exist (the safe side: its leftovers are kept).
pub(crate) fn no_such_process(pid: i32) -> bool {
    crate::process_tree::exists(pid) == Some(false)
}

/// Named pipes vanish with their server: there is no file to sweep.
pub(crate) const fn is_socket(_kind: &fs::FileType) -> bool {
    false
}

/// Whether `path` is a program Windows runs directly (`.exe`, `.com`).
pub(crate) fn is_executable(path: &Path, meta: &fs::Metadata) -> bool {
    meta.is_file()
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe") || ext.eq_ignore_ascii_case("com"))
}

/// `SHELL` is not a Windows variable (Git Bash sets it to a Unix path): it is
/// not consulted.
pub(crate) const fn shell_from_env() -> Option<OsString> {
    None
}

/// Shells to try, in order, when none was chosen: PowerShell 7 (`pwsh.exe`)
/// on `PATH`, then where its installers put it (see [`powershell_7_installs`]),
/// Windows PowerShell on `PATH`, then Windows PowerShell where Windows
/// installs it.
pub(crate) fn shell_candidates() -> Vec<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .collect();
    let mut found: Vec<PathBuf> = dirs.iter().map(|dir| dir.join("pwsh.exe")).collect();
    found.extend(powershell_7_installs());
    found.extend(dirs.iter().map(|dir| dir.join("powershell.exe")));
    found.extend(system_powershell());
    found
}

/// Where PowerShell 7 is when it was installed after this process started
/// (the app's own setup installs it), so this process's `PATH` does not name
/// its folder yet: the MSI install folder (`%ProgramFiles%\PowerShell\7`) and
/// the MSIX package's alias (`%LOCALAPPDATA%\Microsoft\WindowsApps`).
fn powershell_7_installs() -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(dir) = std::env::var_os("ProgramFiles") {
        found.push(
            PathBuf::from(dir)
                .join("PowerShell")
                .join("7")
                .join("pwsh.exe"),
        );
    }
    if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
        found.push(
            PathBuf::from(dir)
                .join("Microsoft")
                .join("WindowsApps")
                .join("pwsh.exe"),
        );
    }
    found.retain(|path| path.is_absolute());
    found
}

/// The shell when no candidate exists: Windows PowerShell where Windows
/// installs it.
pub(crate) fn last_resort_shell() -> PathBuf {
    PathBuf::from(SHELL)
}

/// The working folder as a child is given it. A canonical Windows path is a
/// verbatim one (`\\?\C:\...`), which PowerShell shows as
/// `Microsoft.PowerShell.Core\FileSystem::\\?\C:\...` and `cmd.exe` refuses,
/// so a drive path short enough not to need the prefix loses it. Anything
/// else (a share, a long path) is kept as it is.
pub(crate) fn child_cwd(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_owned();
    };
    let Some(rest) = text.strip_prefix(r"\\?\") else {
        return path.to_owned();
    };
    let bytes = rest.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    // MAX_PATH less the terminating NUL; a reserved name such as `CON` or a
    // trailing dot or space only means something without the prefix.
    let plain = drive
        && rest.len() < 260
        && !rest.contains('/')
        && rest
            .trim_end_matches('\\')
            .split('\\')
            .skip(1)
            .all(plain_component);
    if plain {
        PathBuf::from(rest)
    } else {
        path.to_owned()
    }
}

/// A path component that means the same with and without `\\?\`.
fn plain_component(name: &str) -> bool {
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let numbered = |prefix: &str| {
        stem.len() == 4
            && stem
                .get(..3)
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            && stem.as_bytes()[3].is_ascii_digit()
    };
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !RESERVED.iter().any(|word| stem.eq_ignore_ascii_case(word))
        && !numbered("COM")
        && !numbered("LPT")
}

fn is_powershell(program: &Path) -> bool {
    program
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            stem.eq_ignore_ascii_case("pwsh") || stem.eq_ignore_ascii_case("powershell")
        })
}

/// A plain terminal's shell runs interactively with the user's profile;
/// PowerShell only skips its banner. Another chosen shell gets no flags.
pub(crate) fn login_shell_args(program: &Path) -> Vec<OsString> {
    if is_powershell(program) {
        vec!["-NoLogo".into()]
    } else {
        Vec::new()
    }
}

/// Nothing: Windows shells do not read `SHELL`.
pub(crate) const fn shell_env(_program: &Path) -> Vec<(OsString, OsString)> {
    Vec::new()
}

/// `CREATE_NO_WINDOW`: a console program started by the app (which has no
/// console) would otherwise open a console window of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Run `command` without a console window: the agents' status commands and
/// `codex app-server` are console programs, the app is a windowed one. (A
/// PTY child gets the pseudo console instead.)
pub(crate) fn no_console(command: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NO_WINDOW)
}

/// The whole environment of the `codex app-server` that reads hook trust
/// ([`crate::codex_trust`]): a scratch home, and `SystemRoot`, without which
/// Windows programs fail to load parts of the system. No `PATH` is needed
/// (checked on Codex 0.167.2).
pub(crate) fn codex_trust_env(scratch: &Path) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = vec![
        ("CODEX_HOME".into(), scratch.into()),
        ("HOME".into(), scratch.into()),
    ];
    if let Some(root) = std::env::var_os("SystemRoot") {
        env.push(("SystemRoot".into(), root));
    }
    env
}

/// Agent terminals (Claude Code, Codex) run natively on Windows.
pub(crate) const fn agents_supported() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn powershell_gets_no_logo_and_other_shells_no_flags() {
        let ps = Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
        assert_eq!(login_shell_args(ps), vec![OsString::from("-NoLogo")]);
        assert_eq!(
            login_shell_args(Path::new(r"C:\Program Files\PowerShell\7\PWSH.EXE")),
            vec![OsString::from("-NoLogo")]
        );
        assert!(login_shell_args(Path::new(r"C:\Windows\System32\cmd.exe")).is_empty());
        assert!(shell_env(ps).is_empty());
    }

    #[test]
    fn a_powershell_7_installed_after_start_comes_before_windows_powershell() {
        let candidates = shell_candidates();
        let first_windows_powershell = candidates
            .iter()
            .position(|path| is_named(path, "powershell.exe"))
            .unwrap();
        for install in powershell_7_installs() {
            assert!(is_named(&install, "pwsh.exe"), "{install:?}");
            let at = candidates.iter().position(|path| *path == install).unwrap();
            assert!(at < first_windows_powershell, "{candidates:?}");
        }
    }

    fn is_named(path: &Path, name: &str) -> bool {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    }

    #[test]
    fn programs_are_told_by_extension() {
        let exe = std::env::current_exe().unwrap();
        let meta = fs::metadata(&exe).unwrap();
        assert!(is_executable(&exe, &meta));
        assert!(!is_executable(Path::new(r"C:\x\script.ps1"), &meta));
        let dir = fs::metadata(std::env::temp_dir()).unwrap();
        assert!(!is_executable(Path::new(r"C:\x\folder.exe"), &dir));
    }

    #[test]
    fn random_bytes_differ() {
        let a = random_bytes::<32>().unwrap();
        let b = random_bytes::<32>().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_socket_folder_is_the_pipe_namespace() {
        let name = socket_dir().unwrap().join("shell-1-2.sock");
        assert_eq!(name, PathBuf::from(r"\\.\pipe\shell-1-2.sock"));
    }

    #[test]
    fn endpoints_are_random_one_component_pipe_names() {
        let hook = endpoint("hook", "shell-1-2.sock".into()).unwrap();
        let again = endpoint("hook", "shell-1-2.sock".into()).unwrap();
        assert_ne!(hook, again);
        let text = hook.to_str().unwrap();
        let random = text.strip_prefix(r"\\.\pipe\connecto-hook-").unwrap();
        assert_eq!(random.len(), 32);
        assert!(random.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let bridge = endpoint("bridge", "bridge-1.sock".into()).unwrap();
        assert!(
            bridge
                .to_str()
                .unwrap()
                .starts_with(r"\\.\pipe\connecto-bridge-")
        );
        // The `connecto` command and the hook accept it as an endpoint: it is
        // only not served (a bad name fails with `InvalidInput`).
        let err = connecto_hook::transport::connect(&hook, std::time::Duration::ZERO).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }

    #[test]
    fn a_plain_drive_path_loses_the_verbatim_prefix_and_nothing_else_does() {
        let cases = [
            (r"\\?\C:\Users\u\work\p", r"C:\Users\u\work\p"),
            (r"\\?\d:\", r"d:\"),
            (r"\\?\C:\Users\u\my.project", r"C:\Users\u\my.project"),
            (r"C:\already\plain", r"C:\already\plain"),
            (r"\\?\UNC\server\share\p", r"\\?\UNC\server\share\p"),
            (r"\\?\C:\work\CON", r"\\?\C:\work\CON"),
            (r"\\?\C:\work\nul.txt", r"\\?\C:\work\nul.txt"),
            (r"\\?\C:\work\com1", r"\\?\C:\work\com1"),
            (r"\\?\C:\work\dot.", r"\\?\C:\work\dot."),
            (r"\\?\C:\work\space ", r"\\?\C:\work\space "),
            (r"\\?\C:\work\..\x", r"\\?\C:\work\..\x"),
            (r"\\?\C:\work\common", r"C:\work\common"),
        ];
        for (from, to) in cases {
            assert_eq!(child_cwd(Path::new(from)), PathBuf::from(to), "{from}");
        }
        let long = format!(r"\\?\C:\{}", "x".repeat(300));
        assert_eq!(child_cwd(Path::new(&long)), PathBuf::from(&long));
    }

    #[test]
    fn the_scripted_shell_is_an_absolute_windows_powershell() {
        let program = shell_spec_program();
        assert!(program.is_absolute() && program.is_file(), "{program:?}");
        assert!(is_powershell(&program));
    }

    #[test]
    fn agents_are_supported() {
        assert!(agents_supported().is_ok());
    }

    #[test]
    fn the_trust_read_gets_a_scratch_home_and_system_root_only() {
        let scratch = Path::new(r"C:\data\run\trust-1");
        let env = codex_trust_env(scratch);
        let names: Vec<&str> = env.iter().filter_map(|(name, _)| name.to_str()).collect();
        assert_eq!(&names[..2], ["CODEX_HOME", "HOME"]);
        assert!(names.iter().all(|name| *name != "PATH"), "{names:?}");
        assert_eq!(env[0].1, scratch.as_os_str());
        if std::env::var_os("SystemRoot").is_some() {
            assert_eq!(names.get(2), Some(&"SystemRoot"));
        }
    }
}
