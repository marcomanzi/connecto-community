//! Windows side of discovery.
//!
//! There is no login shell to ask: a program started from Explorer gets the
//! user's full `PATH`, which [`super::ambient_path`] already adds. The known
//! folders are taken under `home` (the default places of `%APPDATA%` and
//! `%LOCALAPPDATA%`), so that a test's home sees nothing of the machine.

use std::path::{Path, PathBuf};

use super::newest_first;

/// No login shell is asked on Windows.
#[must_use]
pub const fn login_path() -> &'static [PathBuf] {
    &[]
}

/// Where the usual installers put global programs, under `home`.
pub fn known_dirs(home: &Path) -> Vec<PathBuf> {
    let roaming = home.join("AppData").join("Roaming");
    let local = home.join("AppData").join("Local");
    let mut dirs = vec![
        // `npm install -g` (its default prefix on Windows).
        roaming.join("npm"),
        home.join(".bun").join("bin"),
        home.join(".cargo").join("bin"),
        // pnpm's default `PNPM_HOME`.
        local.join("pnpm"),
    ];
    // Node version managers: one folder per node version, the newest first.
    // fnm keeps `node.exe` and the global programs in `installation` itself;
    // nvm-windows in the version folder.
    dirs.extend(
        newest_first(&roaming.join("fnm").join("node-versions"))
            .into_iter()
            .map(|v| v.join("installation")),
    );
    dirs.extend(newest_first(&roaming.join("nvm")));
    dirs
}

/// The fixed folders, searched last: where Claude Code's native installer
/// puts `claude.exe` (a real file, the same as
/// `~\.local\share\claude\versions\<version>`), and the folder Codex's
/// standalone installer (`install.ps1`) puts on the user's `PATH`
/// (`%LOCALAPPDATA%\Programs\OpenAI\Codex\bin`, a junction to
/// `~\.codex\packages\standalone\current\bin`), for when the `PATH` this
/// process got predates the install.
pub fn fixed_dirs(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".local").join("bin"),
        home.join("AppData")
            .join("Local")
            .join("Programs")
            .join("OpenAI")
            .join("Codex")
            .join("bin"),
    ]
}

/// The system's own folders, the end of every [`super::run_path`]: those the
/// default system `PATH` starts with, under `%SystemRoot%` (`C:\Windows`
/// when it is not set).
pub fn system_dirs() -> Vec<PathBuf> {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let system32 = root.join("System32");
    let wbem = system32.join("Wbem");
    let powershell = system32.join("WindowsPowerShell").join("v1.0");
    vec![system32, root, wbem, powershell]
}

/// [`system_dirs`] as one `PATH` value, for when they cannot be joined.
pub const SYSTEM_PATH: &str = r"C:\Windows\System32;C:\Windows";

/// The file names under which a program called `name` is looked for: the
/// program, then an npm-style `.cmd` shim. For Codex the store maps npm's
/// `codex.cmd` to the package beside it; any other shim is reported as
/// unusable. An extensionless file (a script for Git Bash) is not a Windows
/// program and is not looked for.
pub fn executable_names(name: &str) -> Vec<String> {
    vec![format!("{name}.exe"), format!("{name}.cmd")]
}
