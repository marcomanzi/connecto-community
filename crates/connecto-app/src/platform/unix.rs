//! Unix (macOS) side of [`crate::platform`]: what the app did before the
//! Windows port, moved here unchanged.

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use connecto_store::Tool;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

/// How the user's computer is named in the app's words.
pub const THIS_COMPUTER: &str = "this Mac";

/// The user's computer, as "the …", in the app's words.
pub const THE_COMPUTER: &str = "the Mac";

/// What the computer is called to a paired machine when it has no name.
pub const UNNAMED_COMPUTER: &str = "a Mac";

/// Where the setup says it looked for the agents.
pub const AGENT_PLACES: &str = "~/.local/bin, /opt/homebrew/bin and /usr/local/bin";

/// The setup offers PowerShell 7 on Windows only.
pub const OFFERS_POWERSHELL_7: bool = false;

/// Whether PowerShell 7 is what a terminal runs: not asked off Windows.
pub const fn has_powershell_7() -> bool {
    false
}

/// PowerShell 7 is installed by the setup on Windows only.
pub fn install_powershell_7() -> Result<(), String> {
    Err("PowerShell 7 is installed by the setup on Windows only".to_owned())
}

/// The computer's own name (`scutil --get ComputerName`), else `HOSTNAME`.
pub fn computer_name() -> Option<String> {
    Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
}

/// Run `then` on a thread of its own when SIGTERM, SIGINT or SIGHUP arrives.
pub fn on_stop_signal(then: impl FnOnce() + Send + 'static) -> io::Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                then();
            }
        })?;
    Ok(())
}

/// Replace this process with `command`. Returns only when that failed.
pub fn start_again(command: &mut Command) -> io::Result<()> {
    Err(command.exec())
}

/// A failure that ends the app, told to the user: on stderr.
pub fn fatal(message: &str) {
    eprintln!("{message}");
}

/// Free space under `path`, in bytes, from `df`; `None` when it cannot be told.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let probe = if path.exists() { path } else { path.parent()? };
    let out = Command::new("df").arg("-k").arg(probe).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let kib: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kib * 1024)
}

/// The vendors' installer scripts: Claude Code's at `claude.ai`, Codex's at
/// `chatgpt.com`, Pi's at `pi.dev`, Antigravity CLI's at `antigravity.google`.
pub const fn installer_url(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Claude => Some("https://claude.ai/install.sh"),
        Tool::Codex => Some("https://chatgpt.com/codex/install.sh"),
        Tool::Pi => Some("https://pi.dev/install.sh"),
        Tool::Antigravity => Some("https://antigravity.google/cli/install.sh"),
        Tool::OpenCode => Some("https://opencode.ai/install"),
        Tool::Shell => None,
    }
}

/// What else is said about an installer: Pi is a Node.js program, and its
/// installer does not bring Node.js when nobody is there to say yes.
pub const fn installer_note(tool: Tool) -> &'static str {
    match tool {
        Tool::Pi => " It needs Node.js 22.19 or newer, which must be installed already.",
        Tool::Claude | Tool::Codex | Tool::Antigravity | Tool::OpenCode | Tool::Shell => "",
    }
}

/// Node.js's program.
pub const NODE_PROGRAM: &str = "node";

/// Where Connecto installs Node.js for Pi: the folder Pi's own installer
/// uses for it (`~/.local/share/pi-node/current/bin`), which Pi's launcher
/// prefers when the install was made with it.
pub fn standalone_node_dir(home: &Path) -> PathBuf {
    home.join(".local/share/pi-node/current/bin")
}

/// The end of the name of the official Node.js build for this computer.
pub const fn node_archive_suffix() -> Option<&'static str> {
    match (cfg!(target_os = "macos"), cfg!(target_arch = "aarch64")) {
        (true, true) => Some("-darwin-arm64.tar.gz"),
        (true, false) => Some("-darwin-x64.tar.gz"),
        (false, true) => Some("-linux-arm64.tar.gz"),
        (false, false) => Some("-linux-x64.tar.gz"),
    }
}

/// Unpack the official Node.js archive `archive` (called `name`) with the
/// system's `tar` into `~/.local/share/pi-node/<version>`, and point
/// `current` at it, as Pi's installer does.
pub fn unpack_node(archive: &Path, name: &str, _work: &Path, home: &Path) -> Result<(), String> {
    let base = home.join(".local/share/pi-node");
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let version = base.join(name.trim_end_matches(".tar.gz"));
    let _ = std::fs::remove_dir_all(&version);
    let output = Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&base)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("tar could not be started: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tar exit {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if !version.join("bin").join(NODE_PROGRAM).is_file() {
        return Err(format!("{} has no bin/{NODE_PROGRAM}", version.display()));
    }
    let current = base.join("current");
    match std::fs::remove_file(&current) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            return Err(format!(
                "{} could not be replaced: {err}",
                current.display()
            ));
        }
        _ => {}
    }
    std::os::unix::fs::symlink(&version, &current).map_err(|e| e.to_string())
}

/// The extension a downloaded installer script is saved with.
pub const INSTALLER_EXTENSION: &str = "sh";

/// The release archive a tool is installed from when its vendor has no
/// installer script for this system: none here, every vendor has one.
pub const fn release_asset(_tool: Tool) -> Option<&'static str> {
    None
}

/// Unpack the zip `archive` into the folder `into`, with the system's `tar`
/// (bsdtar reads zip files).
pub fn unpack_zip(archive: &Path, into: &Path) -> Result<(), String> {
    let output = Command::new("/usr/bin/tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("tar could not be started: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "tar exit {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// What downloads the installer script.
pub fn curl() -> Command {
    Command::new("/usr/bin/curl")
}

/// The installer script run the way its vendor says (Claude Code's with
/// `bash`, Codex's with `sh`), with a minimal environment: `home`, a fixed
/// `PATH` that already holds `~/.local/bin` (so Codex's installer leaves the
/// shell startup file alone), and no questions.
pub fn installer_command(tool: Tool, script: &Path, home: &Path) -> Command {
    // Antigravity's and OpenCode's installers are bash scripts too.
    let shell = if matches!(tool, Tool::Claude | Tool::Antigravity | Tool::OpenCode) {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let mut path = format!(
        "/usr/bin:/bin:/usr/sbin:/sbin:{}",
        home.join(".local/bin").display()
    );
    if tool == Tool::Pi {
        // Pi's installer runs `node` and `npm`, which are wherever the user's
        // own shell finds them. It asks its questions on the terminal, and
        // goes on with its defaults when there is none (a Connecto started
        // from the Dock or Finder has none).
        // The Node.js Connecto installs for Pi comes first, before the
        // user's own (an older one there must not be the one used).
        path = format!(
            "{}:{}:{path}",
            standalone_node_dir(home).display(),
            connecto_store::run_path(script, home).to_string_lossy()
        );
    }
    let mut command = Command::new(shell);
    command.arg(script);
    if tool == Tool::OpenCode {
        // It would add `~/.opencode/bin` to the shell's startup file; Connecto
        // looks for the program there itself, and leaves the file alone.
        command.arg("--no-modify-path");
    }
    command
        .env_clear()
        .env("HOME", home)
        .env("PATH", path)
        .env("USER", std::env::var_os("USER").unwrap_or_default())
        .env("LOGNAME", std::env::var_os("LOGNAME").unwrap_or_default())
        .env("LANG", "en_US.UTF-8")
        .env("TERM", "dumb")
        // Codex's installer asks nothing when told so.
        .env("CODEX_NON_INTERACTIVE", "1")
        .current_dir(home);
    command
}

/// `<program> --version`, with a minimal environment. A launcher script gets
/// its `PATH` from `run_path`, so that its `node` is found.
pub fn version_command(program: &Path, home: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .arg("--version")
        .env_clear()
        .env("HOME", home)
        .env("PATH", connecto_store::run_path(program, home));
    command
}

/// The program to ask `--version` of, for an install the store found: the
/// found file itself.
pub fn runnable(_tool: Tool, found: &Path) -> Option<PathBuf> {
    Some(found.to_owned())
}

/// Where an installer puts the program, for "looked for it in".
pub fn install_places(home: &Path) -> String {
    format!(
        "{} and {}",
        home.join(".local/bin").display(),
        home.join(".opencode/bin").display()
    )
}

/// Make a folder that only the user can open (best effort).
pub fn make_private(path: &Path) {
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}
