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
/// `chatgpt.com`.
pub const fn installer_url(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Claude => Some("https://claude.ai/install.sh"),
        Tool::Codex => Some("https://chatgpt.com/codex/install.sh"),
        Tool::Shell => None,
    }
}

/// What else the setup says about an installer: nothing here.
pub const fn installer_note(_tool: Tool) -> &'static str {
    ""
}

/// The extension a downloaded installer script is saved with.
pub const INSTALLER_EXTENSION: &str = "sh";

/// What downloads the installer script.
pub fn curl() -> Command {
    Command::new("/usr/bin/curl")
}

/// The installer script run the way its vendor says (Claude Code's with
/// `bash`, Codex's with `sh`), with a minimal environment: `home`, a fixed
/// `PATH` that already holds `~/.local/bin` (so Codex's installer leaves the
/// shell startup file alone), and no questions.
pub fn installer_command(tool: Tool, script: &Path, home: &Path) -> Command {
    let shell = if tool == Tool::Claude {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let mut command = Command::new(shell);
    command
        .arg(script)
        .env_clear()
        .env("HOME", home)
        .env(
            "PATH",
            format!(
                "/usr/bin:/bin:/usr/sbin:/sbin:{}",
                home.join(".local/bin").display()
            ),
        )
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
    home.join(".local/bin").display().to_string()
}

/// Make a folder that only the user can open (best effort).
pub fn make_private(path: &Path) {
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}
