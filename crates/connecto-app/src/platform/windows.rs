//! Windows side of [`crate::platform`].
//!
//! Windows has no `exec`, no SIGHUP and no `df`; a release build has no
//! console (see `windows_subsystem` in `main.rs`), so a fatal error is also
//! shown in a message box. The agents (Claude Code, Codex) are installed with
//! their vendors' PowerShell installers (`install.ps1`).

use std::io;
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use connecto_store::Tool;
use signal_hook::consts::{SIGINT, SIGTERM};

/// How the user's computer is named in the app's words.
pub const THIS_COMPUTER: &str = "this PC";

/// The user's computer, as "the …", in the app's words.
pub const THE_COMPUTER: &str = "the PC";

/// What the computer is called to a paired machine when it has no name.
pub const UNNAMED_COMPUTER: &str = "a Windows PC";

/// Where the setup says it looked for the agents: Claude Code's native
/// install, npm's global folder, Codex's standalone install, and `PATH`.
pub const AGENT_PLACES: &str = r"%USERPROFILE%\.local\bin, %APPDATA%\npm, %LOCALAPPDATA%\Programs\OpenAI\Codex\bin and the folders on PATH";

/// The setup offers PowerShell 7 (`pwsh`) when it is missing: Windows ships
/// only Windows PowerShell 5.1, which stays the fallback.
pub const OFFERS_POWERSHELL_7: bool = true;

/// The winget package of PowerShell 7 (checked with
/// `winget show --id Microsoft.PowerShell -e` on 2026-11-12: 7.7.0 is an
/// MSIX package, which installs for the user; not yet installed by this code
/// on a real machine).
const POWERSHELL_PACKAGE: &str = "Microsoft.PowerShell";

/// How long the PowerShell 7 install may take.
const POWERSHELL_LIMIT: Duration = Duration::from_secs(15 * 60);

/// `CREATE_NO_WINDOW`: winget is a console program, and the app has no
/// console to lend it.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How often the stop flag is looked at.
const SIGNAL_POLL: Duration = Duration::from_millis(100);

/// The computer's name: `COMPUTERNAME`.
pub fn computer_name() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// Run `then` on a thread of its own when Ctrl+C (SIGINT) or SIGTERM arrives.
/// Only a build started from a console gets either; closing the window is
/// the usual way out.
pub fn on_stop_signal(then: impl FnOnce() + Send + 'static) -> io::Result<()> {
    let asked = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&asked))?;
    }
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            while !asked.load(Ordering::SeqCst) {
                std::thread::sleep(SIGNAL_POLL);
            }
            then();
        })?;
    Ok(())
}

/// Start `command` as a new process; the caller then ends this one. Windows
/// has no `exec`. The app holds no single-instance lock, so the new process
/// does not wait for this one; it inherits this one's standard handles.
pub fn start_again(command: &mut Command) -> io::Result<()> {
    command.spawn().map(drop)
}

/// A failure that ends the app, told to the user: on stderr (seen only when
/// it was redirected or a console is attached) and in a message box.
pub fn fatal(message: &str) {
    eprintln!("{message}");
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Connecto")
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// Whether a terminal runs PowerShell 7: connecto-session's own choice of
/// shell (`pwsh.exe` on `PATH` or where its installers put it).
pub fn has_powershell_7() -> bool {
    connecto_session::launch::shell_program()
        .file_stem()
        .is_some_and(|stem| stem.eq_ignore_ascii_case("pwsh"))
}

/// Install PowerShell 7 with winget, from the winget community source, with
/// no window and no questions, under a time limit. Afterwards a terminal
/// must find it; otherwise the install counts as failed.
pub fn install_powershell_7() -> Result<(), String> {
    use std::process::Stdio;
    let mut child = Command::new("winget")
        .args([
            "install",
            "--id",
            POWERSHELL_PACKAGE,
            "--exact",
            "--source",
            "winget",
            "--silent",
            "--disable-interactivity",
            "--accept-package-agreements",
            "--accept-source-agreements",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|err| format!("winget could not be started ({err}); is App Installer missing?"))?;
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > POWERSHELL_LIMIT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("winget took too long".to_owned());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(err) => return Err(format!("winget could not be watched: {err}")),
        }
    };
    if !status.success() {
        return Err(format!("winget ended with exit code {:?}", status.code()));
    }
    if has_powershell_7() {
        Ok(())
    } else {
        Err("winget finished, but pwsh.exe was not found".to_owned())
    }
}

/// Free space is not measured on Windows yet: the setup's space check is
/// skipped, as it is on macOS when `df` cannot tell.
pub const fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Windows has no mode bits: a folder under the user's profile is already
/// closed to other users by its access list.
pub const fn make_private(_path: &Path) {}

/// The vendors' PowerShell installers, from the same hosts as on macOS
/// as on macOS: Claude Code's puts `claude.exe`
/// in `~\.local\bin` (a real file; versions in `~\.local\share\claude`);
/// Codex's puts its release under `~\.codex\packages\standalone`, makes
/// `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin` a junction to it and adds that
/// folder to the user's `PATH`.
pub const fn installer_url(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Claude => Some("https://claude.ai/install.ps1"),
        Tool::Codex => Some("https://chatgpt.com/codex/install.ps1"),
        Tool::Shell => None,
    }
}

/// What else the setup says about an installer: Codex's adds its folder to
/// the user's `PATH` (Claude Code's install is found where it is).
pub const fn installer_note(tool: Tool) -> &'static str {
    match tool {
        Tool::Codex => " Its installer also adds its folder to your PATH.",
        Tool::Claude | Tool::Shell => "",
    }
}

/// The extension a downloaded installer script is saved with: `-File` runs
/// only `.ps1` files.
pub const INSTALLER_EXTENSION: &str = "ps1";

/// `%SystemRoot%`, where Windows is (`C:\Windows` when the variable is gone).
fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

/// What downloads the installer script: the `curl.exe` Windows ships, by
/// absolute path, with no console window.
pub fn curl() -> Command {
    let mut command = Command::new(system_root().join("System32").join("curl.exe"));
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// Windows PowerShell, by absolute path.
fn windows_powershell() -> PathBuf {
    system_root()
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

/// The installer script run with Windows PowerShell, by absolute path:
/// `-NoProfile -NonInteractive -ExecutionPolicy Bypass -File <script>`. The
/// execution policy is for this one process (no setting changes; a Group
/// Policy can still forbid it, and the script then fails and says so). No
/// console window and no input.
///
/// The environment is built, not inherited: the Windows variables the
/// scripts and PowerShell need (`SystemRoot`, `OS` — Codex's installer
/// stops without `Windows_NT` —, `PROCESSOR_ARCHITECTURE` — Claude's picks
/// the ARM64 build by it —), the user's folders under `home` as discovery
/// looks for them (`USERPROFILE`, `APPDATA`, `LOCALAPPDATA`), `TEMP` and
/// `TMP` in the install's own work folder (Codex's installer compiles C#
/// there), Windows PowerShell's own modules, a `PATH` of the system folders
/// plus where the two installers put the programs, and no questions
/// (`CODEX_NON_INTERACTIVE`).
pub fn installer_command(_tool: Tool, script: &Path, home: &Path) -> Command {
    // PowerShell's `Join-Path` refuses a verbatim (`\\?\C:\…`) path, as a
    // canonical one is: the scripts get the plain drive form.
    let script = &plain(script);
    let home = &plain(home);
    let root = system_root();
    let system32 = root.join("System32");
    let powershell_dir = system32.join("WindowsPowerShell").join("v1.0");
    let local = home.join("AppData").join("Local");
    let tmp = script
        .parent()
        .map_or_else(|| home.to_owned(), |work| work.join("tmp"));
    let _ = std::fs::create_dir_all(&tmp);
    let path = std::env::join_paths([
        system32.clone(),
        root.clone(),
        system32.join("Wbem"),
        powershell_dir.clone(),
        home.join(".local").join("bin"),
        local
            .join("Programs")
            .join("OpenAI")
            .join("Codex")
            .join("bin"),
    ])
    .unwrap_or_default();
    let mut command = Command::new(windows_powershell());
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .env_clear()
        .env("SystemRoot", &root)
        .env("windir", &root)
        .env("ComSpec", system32.join("cmd.exe"))
        .env("OS", "Windows_NT")
        .env(
            "PATHEXT",
            ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC",
        )
        .env("PSModulePath", powershell_dir.join("Modules"))
        .env("USERPROFILE", home)
        .env("HOME", home)
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", &local)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .env("PATH", path)
        // Codex's installer asks nothing when told so.
        .env("CODEX_NON_INTERACTIVE", "1")
        .current_dir(home)
        .creation_flags(CREATE_NO_WINDOW);
    for name in [
        "SystemDrive",
        "PROCESSOR_ARCHITECTURE",
        "NUMBER_OF_PROCESSORS",
        "USERNAME",
        "USERDOMAIN",
        "COMPUTERNAME",
        "ProgramData",
        "ALLUSERSPROFILE",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "CommonProgramFiles",
        "CommonProgramFiles(x86)",
        "CommonProgramW6432",
        "PUBLIC",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

/// `path` without the verbatim prefix when it is a drive path short enough
/// not to need it (`\\?\C:\x` → `C:\x`); anything else as it is.
fn plain(path: &Path) -> PathBuf {
    let Some(rest) = path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) else {
        return path.to_owned();
    };
    let bytes = rest.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    if drive && rest.len() < 260 && !rest.contains('/') {
        PathBuf::from(rest)
    } else {
        path.to_owned()
    }
}

/// `<program> --version`, with no console window and an environment of
/// `SystemRoot` and the home folder only (checked on Claude Code 2.1.322
/// and Codex 0.167.2: both answer with it). No `PATH`, unlike macOS's
/// `run_path`: the program asked is never a launcher script here (an npm
/// Codex's `codex.cmd` is replaced by its `codex.exe`, see [`runnable`]).
pub fn version_command(program: &Path, home: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .arg("--version")
        .env_clear()
        .env("SystemRoot", system_root())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .creation_flags(CREATE_NO_WINDOW);
    command
}

/// The program to ask `--version` of, for an install the store found. For
/// an npm Codex that is `codex.cmd`, a batch file that needs Node.js: it is
/// never run; the release's own `codex.exe` is ([`Store::program_of`]).
pub fn runnable(tool: Tool, found: &Path) -> Option<PathBuf> {
    connecto_store::Store::program_of(tool, found)
}

/// Where the installers put the programs, for "looked for it in".
pub fn install_places(home: &Path) -> String {
    format!(
        "{} and {}",
        home.join(".local").join("bin").display(),
        home.join("AppData")
            .join("Local")
            .join("Programs")
            .join("OpenAI")
            .join("Codex")
            .join("bin")
            .display()
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn env_of(command: &Command) -> Vec<(String, String)> {
        command
            .get_envs()
            .filter_map(|(name, value)| {
                Some((
                    name.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect()
    }

    fn value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn the_installers_are_the_vendors_powershell_scripts_over_https() {
        assert_eq!(
            installer_url(Tool::Claude),
            Some("https://claude.ai/install.ps1")
        );
        assert_eq!(
            installer_url(Tool::Codex),
            Some("https://chatgpt.com/codex/install.ps1")
        );
        assert_eq!(installer_url(Tool::Shell), None);
        assert_eq!(INSTALLER_EXTENSION, "ps1");
    }

    #[test]
    fn curl_is_the_one_windows_ships_by_absolute_path() {
        let curl = curl();
        let program = PathBuf::from(curl.get_program());
        assert!(program.is_absolute(), "{program:?}");
        assert!(program.ends_with(r"System32\curl.exe"), "{program:?}");
        assert!(program.is_file(), "{program:?}");
    }

    #[test]
    fn the_script_runs_in_windows_powershell_by_path_with_a_built_environment() {
        let home = Path::new(r"C:\Users\someone");
        let script = Path::new(r"C:\data\run\install-1\install-codex.ps1");
        let command = installer_command(Tool::Codex, script, home);
        let program = PathBuf::from(command.get_program());
        assert!(program.is_absolute() && program.is_file(), "{program:?}");
        assert!(
            program.ends_with(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
            "{program:?}"
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                r"C:\data\run\install-1\install-codex.ps1"
            ]
        );
        assert_eq!(
            command.get_current_dir(),
            Some(home),
            "run from the home folder"
        );
        let env = env_of(&command);
        // Built, not inherited: cargo's variables do not get in.
        assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
        assert!(value(&env, "CARGO_PKG_NAME").is_none(), "{env:?}");
        assert_eq!(value(&env, "OS"), Some("Windows_NT"));
        assert_eq!(value(&env, "USERPROFILE"), Some(r"C:\Users\someone"));
        assert_eq!(
            value(&env, "LOCALAPPDATA"),
            Some(r"C:\Users\someone\AppData\Local")
        );
        assert_eq!(
            value(&env, "APPDATA"),
            Some(r"C:\Users\someone\AppData\Roaming")
        );
        assert_eq!(value(&env, "TEMP"), Some(r"C:\data\run\install-1\tmp"));
        assert_eq!(value(&env, "TMP"), Some(r"C:\data\run\install-1\tmp"));
        assert_eq!(value(&env, "CODEX_NON_INTERACTIVE"), Some("1"));
        assert!(value(&env, "SystemRoot").is_some_and(|root| Path::new(root).is_dir()));
        assert!(
            value(&env, "PSModulePath").is_some_and(|dir| Path::new(dir).is_dir()),
            "{env:?}"
        );
        let path = value(&env, "PATH").unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(path).collect();
        assert!(dirs.iter().all(|dir| dir.is_absolute()), "{path}");
        assert!(
            dirs.iter().any(|dir| dir.join("tar.exe").is_file()),
            "{path}"
        );
        assert!(dirs.contains(&home.join(".local").join("bin")), "{path}");
        let _ = std::fs::remove_dir_all(r"C:\data\run\install-1\tmp");
    }

    #[test]
    fn version_is_asked_with_system_root_and_the_home_only() {
        let home = Path::new(r"C:\Users\someone");
        let command = version_command(Path::new(r"C:\x\claude.exe"), home);
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--version"]);
        let env = env_of(&command);
        let mut names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["HOME", "SystemRoot", "USERPROFILE"]);
    }

    #[test]
    fn an_npm_codex_launcher_is_never_what_is_run() {
        let dir = std::env::temp_dir().join(format!("connecto-runnable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("codex.cmd");
        std::fs::write(&shim, "@node codex.js %*\r\n").unwrap();
        // No package beside it: nothing runnable, and certainly not the .cmd.
        assert_ne!(runnable(Tool::Codex, &shim), Some(shim.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
