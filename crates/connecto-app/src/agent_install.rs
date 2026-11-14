//! Installing an agent that is not on this computer, the way its vendor says to.
//!
//! Only the vendor's own installer, from the vendor's own address, over HTTPS:
//!
//! - Claude Code: `https://claude.ai/install.sh`, run with `bash`. It puts the
//!   program in `~/.local/share/claude/versions/` and a link at
//!   `~/.local/bin/claude`. It needs no Node, no Homebrew and no password.
//! - Codex: `https://chatgpt.com/codex/install.sh`, run with `sh`. It puts the
//!   program in `~/.local/bin/codex` (its files under `~/.codex/packages/`),
//!   and, when that folder is not on the shell's search path, adds it in the
//!   user's shell startup file. It is run with that folder already on the
//!   path (Connecto looks for the program there itself), so it leaves the
//!   startup file alone. It needs no Node, no Homebrew and no password.
//!
//! The script is downloaded first (`curl`, HTTPS only, TLS 1.2 or newer, with a
//! time limit), written to a private folder under the data folder, and run from
//! there with a minimal environment, no terminal and no input, under a time
//! limit. Afterwards the program must exist where Connecto looks for it and
//! must answer `--version`; otherwise the install counts as failed.
//!
//! Windows: the vendors' PowerShell
//! installers from the same hosts, `https://claude.ai/install.ps1` and
//! `https://chatgpt.com/codex/install.ps1`, downloaded the same way with the
//! `curl.exe` Windows ships and run with Windows PowerShell by absolute path
//! (`-NoProfile -NonInteractive -ExecutionPolicy Bypass -File`, the policy for
//! that one process only), with no console window. Claude Code's puts
//! `claude.exe` in `~\.local\bin`; Codex's puts its release under
//! `~\.codex\packages\standalone`, behind
//! `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin`, and adds that folder to the
//! user's `PATH`. Details in `crate::platform::installer_command`.
//!
//! On every platform the program is trusted only after this: the setup and
//! Settings > Agents then copy it into Connecto's store, which checks its
//! signature (macOS: Apple Team ID; Windows: Authenticode and publisher), as
//! for a program that was already installed.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use connecto_store::{Store, Tool};

use crate::platform::{INSTALLER_EXTENSION, THIS_COMPUTER};

/// How long the installer script may take.
const INSTALL_LIMIT: Duration = Duration::from_secs(15 * 60);
/// How long the download of the script may take.
const DOWNLOAD_LIMIT_SECS: &str = "120";
/// How long `--version` may take.
const VERSION_LIMIT: Duration = Duration::from_secs(20);

/// What the installer says about itself, for the screen.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    /// "Claude Code".
    pub name: &'static str,
    /// The vendor, for "from Anthropic".
    pub vendor: &'static str,
    /// The address the script comes from. Always HTTPS.
    pub url: &'static str,
    /// The host in plain words, for "from claude.ai".
    pub host: &'static str,
}

/// The official source of an agent, if Connecto can install it.
#[must_use]
pub fn source_of(tool: Tool) -> Option<Source> {
    let url = crate::platform::installer_url(tool)?;
    match tool {
        Tool::Claude => Some(Source {
            name: "Claude Code",
            vendor: "Anthropic",
            url,
            host: "claude.ai",
        }),
        Tool::Codex => Some(Source {
            name: "Codex",
            vendor: "OpenAI",
            url,
            host: "chatgpt.com",
        }),
        Tool::Shell => None,
    }
}

/// Said before anything is installed: what, from where, what it needs.
#[must_use]
pub fn announcement(tool: Tool) -> Option<String> {
    let source = source_of(tool)?;
    Some(format!(
        "Not on {THIS_COMPUTER} yet. Connecto will install {} for you from {}'s official installer at {}. It goes in your own folder and needs no password.{}",
        source.name,
        source.vendor,
        source.host,
        crate::platform::installer_note(tool)
    ))
}

/// An agent that is now on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub path: PathBuf,
    /// The first line `--version` printed.
    pub version: String,
}

/// Why an install did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// In plain words.
    pub what: String,
    /// What to do about it.
    pub next: String,
    /// The raw reason, behind "Show details".
    pub details: String,
}

/// Something that can install an agent. A fake stands in for tests.
pub trait AgentInstaller: Send + Sync {
    /// Install `tool` for the user whose home is `home`, telling `say` which step it is on.
    ///
    /// # Errors
    /// A [`Failure`], worded for a person.
    fn install(&self, tool: Tool, home: &Path, say: &dyn Fn(&str)) -> Result<Installed, Failure>;

    /// Install PowerShell 7 for the terminals (the setup offers it on Windows
    /// only), telling `say` which step it is on. Installs nothing unless an
    /// installer says otherwise: test fakes never run winget.
    ///
    /// # Errors
    /// A [`Failure`], worded for a person.
    fn install_powershell(&self, say: &dyn Fn(&str)) -> Result<(), Failure> {
        let _ = say;
        Err(Failure {
            what: "Not installed.".to_owned(),
            next: String::new(),
            details: "this installer does not install PowerShell 7".to_owned(),
        })
    }
}

/// The vendors' own installers.
pub struct Official {
    /// Where the script is kept while it runs (a private folder).
    work: PathBuf,
    /// Tests give a script file instead of an address.
    #[cfg(test)]
    local: Option<PathBuf>,
}

impl Official {
    #[must_use]
    pub fn new(data_root: &Path) -> Self {
        Self {
            work: data_root
                .join("run")
                .join(format!("install-{}", std::process::id())),
            #[cfg(test)]
            local: None,
        }
    }

    #[cfg(test)]
    fn with_script(work: PathBuf, script: PathBuf) -> Self {
        Self {
            work,
            local: Some(script),
        }
    }

    fn fetch(&self, source: Source, to: &Path) -> Result<(), Failure> {
        #[cfg(test)]
        if let Some(local) = &self.local {
            return fs::copy(local, to).map(|_| ()).map_err(|err| Failure {
                what: "The installer could not be fetched.".to_owned(),
                next: "Try again.".to_owned(),
                details: err.to_string(),
            });
        }
        let output = crate::platform::curl()
            .args([
                "--proto",
                "=https",
                "--tlsv1.2",
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--connect-timeout",
                "20",
                "--max-time",
                DOWNLOAD_LIMIT_SECS,
                "--output",
            ])
            .arg(to)
            .arg(source.url)
            .stdin(Stdio::null())
            .output()
            .map_err(|err| Failure {
                what: "Connecto could not start the download.".to_owned(),
                next:
                    "Try again. If it keeps happening, install it yourself from its official page."
                        .to_owned(),
                details: err.to_string(),
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(download_failure(
            source,
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        ))
    }
}

/// Said for a person, from curl's exit code.
fn download_failure(source: Source, code: Option<i32>, stderr: &str) -> Failure {
    let (what, next) = match code {
        // Could not resolve, could not connect, timed out.
        Some(6 | 7 | 28) => (
            format!(
                "Connecto could not reach {}. Is {THIS_COMPUTER} online?",
                source.host
            ),
            "Check your internet connection, then try again, or skip this for now.".to_owned(),
        ),
        Some(22) => (
            format!("{} did not give the installer just now.", source.host),
            "Try again in a moment, or skip this for now.".to_owned(),
        ),
        _ => (
            format!("The {} installer could not be downloaded.", source.name),
            "Try again, or skip this for now.".to_owned(),
        ),
    };
    Failure {
        what,
        next,
        details: format!("curl exit {code:?}: {}", stderr.trim()),
    }
}

impl AgentInstaller for Official {
    fn install_powershell(&self, say: &dyn Fn(&str)) -> Result<(), Failure> {
        say("Installing with winget (this can take a few minutes)…");
        crate::platform::install_powershell_7().map_err(|details| Failure {
            what: "Not installed; the terminals use Windows PowerShell".to_owned(),
            next: "You can install it later with: winget install --id Microsoft.PowerShell -e"
                .to_owned(),
            details,
        })
    }

    fn install(&self, tool: Tool, home: &Path, say: &dyn Fn(&str)) -> Result<Installed, Failure> {
        let Some(source) = source_of(tool) else {
            return Err(Failure {
                what: "There is nothing to install for this.".to_owned(),
                next: String::new(),
                details: String::new(),
            });
        };
        let io = |what: &str, err: std::io::Error| Failure {
            what: what.to_owned(),
            next: "Try again, or skip this for now.".to_owned(),
            details: err.to_string(),
        };
        say(&format!("Downloading the installer from {}…", source.host));
        fs::create_dir_all(&self.work)
            .map_err(|e| io("Connecto could not prepare a folder.", e))?;
        crate::platform::make_private(&self.work);
        let script = self
            .work
            .join(format!("install-{}.{INSTALLER_EXTENSION}", tool.name()));
        let outcome = self.run(tool, source, home, &script, say);
        let _ = fs::remove_dir_all(&self.work);
        outcome
    }
}

impl Official {
    fn run(
        &self,
        tool: Tool,
        source: Source,
        home: &Path,
        script: &Path,
        say: &dyn Fn(&str),
    ) -> Result<Installed, Failure> {
        self.fetch(source, script)?;
        say(&format!(
            "Installing {} (this can take a minute)…",
            source.name
        ));
        let log = self.work.join("log.txt");
        let log_file = fs::File::create(&log).map_err(|err| Failure {
            what: "Connecto could not prepare a folder.".to_owned(),
            next: "Try again, or skip this for now.".to_owned(),
            details: err.to_string(),
        })?;
        let mut command = crate::platform::installer_command(tool, script, home);
        command
            .stdin(Stdio::null())
            .stdout(log_file.try_clone().map_err(|e| Failure {
                what: "Connecto could not prepare a folder.".to_owned(),
                next: "Try again, or skip this for now.".to_owned(),
                details: e.to_string(),
            })?)
            .stderr(log_file);
        let mut child = command.spawn().map_err(|err| Failure {
            what: format!("Connecto could not start the {} installer.", source.name),
            next: "Try again, or install it yourself from its official page.".to_owned(),
            details: err.to_string(),
        })?;
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() > INSTALL_LIMIT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Failure {
                        what: format!("The {} installer took too long.", source.name),
                        next: "Try again, or skip this for now.".to_owned(),
                        details: tail(&log),
                    });
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(err) => {
                    return Err(Failure {
                        what: format!("Connecto lost track of the {} installer.", source.name),
                        next: "Try again.".to_owned(),
                        details: err.to_string(),
                    });
                }
            }
        };
        if !status.success() {
            return Err(Failure {
                what: format!("The {} installer did not finish.", source.name),
                next: "Try again, or skip this for now. If it keeps happening, install it from its official page."
                    .to_owned(),
                details: format!("exit {:?}: {}", status.code(), tail(&log)),
            });
        }
        say("Checking that it runs…");
        verify(tool, source, home)
    }
}

/// The last lines of the installer's output, for the details.
fn tail(log: &Path) -> String {
    let mut text = String::new();
    if let Ok(mut file) = fs::File::open(log) {
        let _ = file.read_to_string(&mut text);
    }
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(12)..].join("\n")
}

/// The program must be where Connecto looks, and must run. This runs the user's
/// own install that was just installed (there is no stored copy yet, so
/// `runnable_entry` does not apply here). A launcher script gets its `PATH` from
/// `run_path`, so `version_of` can find its `node`.
fn verify(tool: Tool, source: Source, home: &Path) -> Result<Installed, Failure> {
    let missing = || Failure {
        what: format!(
            "{} was installed, but Connecto cannot find it.",
            source.name
        ),
        next: "Try again, or skip this for now.".to_owned(),
        details: format!("looked for it in {}", crate::platform::install_places(home)),
    };
    let path = Store::find_installed(tool, home).ok_or_else(missing)?;
    let version = version_of_install(tool, &path, home).ok_or_else(|| Failure {
        what: format!("{} is installed but did not start.", source.name),
        next: "Try again, or skip this for now.".to_owned(),
        details: format!("{} --version gave nothing", path.display()),
    })?;
    Ok(Installed { path, version })
}

/// What the program of an install the store found (see
/// [`Store::find_installed`]) prints for `--version`. On Windows an npm
/// Codex is found as `codex.cmd`, which is never run: its release's
/// `codex.exe` is asked instead.
#[must_use]
pub fn version_of_install(tool: Tool, found: &Path, home: &Path) -> Option<String> {
    version_of(&crate::platform::runnable(tool, found)?, home)
}

/// What `<program> --version` prints on its first line.
#[must_use]
pub fn version_of(program: &Path, home: &Path) -> Option<String> {
    let mut child = crate::platform::version_command(program, home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => return None,
            None if started.elapsed() > VERSION_LIMIT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::sync::Mutex;

    fn temp(name: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "connecto-install-{name}-{}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(dir).unwrap()
    }

    /// A stand-in for a vendor's installer: puts a program where the real one does.
    #[cfg(unix)]
    fn script(dir: &Path, body: &str) -> PathBuf {
        let file = dir.join("fake-install.sh");
        fs::write(&file, body).unwrap();
        file
    }

    /// An installer script that puts a working Claude in `$HOME`. The program is a
    /// hard link to the shared fake (see `test_support`), not a new file.
    #[cfg(unix)]
    fn good() -> String {
        let fake = crate::test_support::native_program("9.9.9 (Fake Agent)");
        format!(
            "mkdir -p \"$HOME/.local/bin\"\nln -f '{}' \"$HOME/.local/bin/claude\"\n",
            fake.display()
        )
    }

    #[test]
    fn every_source_is_https_from_the_vendors_own_host() {
        for tool in [Tool::Claude, Tool::Codex] {
            let source = source_of(tool).unwrap();
            assert!(source.url.starts_with("https://"), "{}", source.url);
            assert!(
                source.url.starts_with(&format!("https://{}/", source.host)),
                "{}",
                source.url
            );
        }
        #[cfg(unix)]
        assert_eq!(
            source_of(Tool::Claude).unwrap().url,
            "https://claude.ai/install.sh"
        );
        #[cfg(unix)]
        assert_eq!(
            source_of(Tool::Codex).unwrap().url,
            "https://chatgpt.com/codex/install.sh"
        );
        #[cfg(windows)]
        assert_eq!(
            source_of(Tool::Claude).unwrap().url,
            "https://claude.ai/install.ps1"
        );
        #[cfg(windows)]
        assert_eq!(
            source_of(Tool::Codex).unwrap().url,
            "https://chatgpt.com/codex/install.ps1"
        );
        assert!(source_of(Tool::Shell).is_none());
    }

    #[test]
    fn the_announcement_says_what_from_where_and_that_no_password_is_needed() {
        let text = announcement(Tool::Claude).unwrap();
        assert!(
            text.contains("Claude Code")
                && text.contains("claude.ai")
                && text.contains("Anthropic")
        );
        assert!(text.contains("no password"));
        assert!(announcement(Tool::Codex).unwrap().contains("OpenAI"));
        assert!(announcement(Tool::Shell).is_none());
    }

    // Runs installer scripts with sh: Unix only.
    #[cfg(unix)]
    #[test]
    fn an_install_into_an_isolated_home_puts_the_program_there_and_proves_it_runs() {
        let root = temp("good");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let installer = Official::with_script(root.join("work"), script(&root, &good()));
        let steps = Mutex::new(Vec::new());
        let done = installer
            .install(Tool::Claude, &home, &|s| {
                steps.lock().unwrap().push(s.to_owned())
            })
            .unwrap();
        assert_eq!(done.path, home.join(".local/bin/claude"));
        assert_eq!(done.version, "9.9.9 (Fake Agent)");
        let steps = steps.lock().unwrap();
        assert!(steps[0].contains("claude.ai"), "{steps:?}");
        assert!(steps.iter().any(|s| s.contains("Checking")), "{steps:?}");
        assert!(
            !root.join("work").exists(),
            "the downloaded script is removed"
        );
        // Found the way Isolated and System modes find it.
        assert_eq!(Store::find_installed(Tool::Claude, &home), Some(done.path));
    }

    // Runs installer scripts with sh: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_script_that_fails_says_so_plainly_and_keeps_the_raw_reason_apart() {
        let root = temp("bad");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let installer = Official::with_script(
            root.join("work"),
            script(&root, "echo 'disk is on fire' >&2\nexit 3\n"),
        );
        let failure = installer.install(Tool::Claude, &home, &|_| {}).unwrap_err();
        assert!(failure.what.contains("did not finish"), "{failure:?}");
        assert!(failure.next.contains("skip"));
        assert!(failure.details.contains("disk is on fire"), "{failure:?}");
        assert!(!home.join(".local").exists());
    }

    // Runs installer scripts with sh: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_script_that_ends_well_but_leaves_nothing_runnable_counts_as_failed() {
        let root = temp("nothing");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let none = Official::with_script(root.join("w1"), script(&root, "exit 0\n"));
        let failure = none.install(Tool::Claude, &home, &|_| {}).unwrap_err();
        assert!(failure.what.contains("cannot find it"), "{failure:?}");
        let broken = Official::with_script(
            root.join("w2"),
            script(
                &root,
                &format!(
                    "mkdir -p \"$HOME/.local/bin\"\nln -f '{}' \"$HOME/.local/bin/claude\"\n",
                    crate::test_support::native_program("").display()
                ),
            ),
        );
        let failure = broken.install(Tool::Claude, &home, &|_| {}).unwrap_err();
        assert!(failure.what.contains("did not start"), "{failure:?}");
    }

    // Runs installer scripts with sh: Unix only.
    #[cfg(unix)]
    #[test]
    fn the_script_runs_with_a_minimal_environment_and_no_input() {
        let root = temp("env");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        // Writes what it saw.
        let body = format!(
            "env > \"{0}/seen.txt\"\nif [ -t 0 ]; then echo tty >> \"{0}/seen.txt\"; fi\n{1}",
            root.display(),
            good()
        );
        let installer = Official::with_script(root.join("work"), script(&root, &body));
        installer.install(Tool::Claude, &home, &|_| {}).unwrap();
        let seen = fs::read_to_string(root.join("seen.txt")).unwrap();
        // The test process itself has CARGO_* variables; the script must not.
        assert!(!seen.contains("CARGO"), "{seen}");
        assert!(seen.contains(&format!("HOME={}", home.display())));
        assert!(!seen.contains("tty"));
    }

    #[test]
    fn offline_and_refused_downloads_are_worded_for_a_person() {
        let source = source_of(Tool::Claude).unwrap();
        let offline = download_failure(source, Some(6), "Could not resolve host");
        assert!(
            offline
                .what
                .contains(&format!("Is {THIS_COMPUTER} online?")),
            "{offline:?}"
        );
        assert!(offline.details.contains("Could not resolve host"));
        assert!(
            download_failure(source, Some(28), "")
                .what
                .contains("reach claude.ai")
        );
        assert!(
            download_failure(source, Some(22), "")
                .what
                .contains("did not give")
        );
        assert!(
            download_failure(source, Some(1), "")
                .what
                .contains("could not be downloaded")
        );
    }

    /// A stand-in for a vendor's PowerShell installer.
    #[cfg(windows)]
    fn ps_script(dir: &Path, body: &str) -> PathBuf {
        let file = dir.join("fake-install.ps1");
        fs::write(&file, body).unwrap();
        file
    }

    /// Puts a program where Claude Code's installer does
    /// (`$env:USERPROFILE\.local\bin\claude.exe`): Windows' own `curl.exe`,
    /// which answers `--version` with a line of its own.
    #[cfg(windows)]
    const PS_GOOD: &str = r#"$ErrorActionPreference = 'Stop'
$bin = Join-Path $env:USERPROFILE '.local\bin'
New-Item -ItemType Directory -Force -Path $bin | Out-Null
Copy-Item -LiteralPath (Join-Path $env:SystemRoot 'System32\curl.exe') -Destination (Join-Path $bin 'claude.exe')
Write-Output 'installed'
"#;

    // Windows: the downloaded script runs in Windows PowerShell with
    // `-File`, its program is found where discovery looks, and it must answer
    // `--version` (the vendor's script is not run here; it was tried by hand on
    // Windows).
    #[cfg(windows)]
    #[test]
    fn on_windows_a_powershell_install_puts_the_program_there_and_proves_it_runs() {
        let root = temp("ps-good");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        // Writes what it saw: its environment, its policy, and whether it
        // had input.
        let body = format!(
            "Get-ChildItem Env: | ForEach-Object {{ \"$($_.Name)=$($_.Value)\" }} | Set-Content -Encoding UTF8 -LiteralPath '{0}\\seen.txt'\n\
             Add-Content -LiteralPath '{0}\\seen.txt' -Value (\"policy=\" + (Get-ExecutionPolicy -Scope Process))\n\
             Add-Content -LiteralPath '{0}\\seen.txt' -Value (\"redirected=\" + [Console]::IsInputRedirected)\n\
             {PS_GOOD}",
            root.display()
        );
        let installer = Official::with_script(root.join("work"), ps_script(&root, &body));
        let steps = std::sync::Mutex::new(Vec::new());
        let done = installer
            .install(Tool::Claude, &home, &|s| {
                steps.lock().unwrap().push(s.to_owned());
            })
            .unwrap();
        assert_eq!(
            done.path,
            home.join(".local").join("bin").join("claude.exe")
        );
        assert!(done.version.starts_with("curl "), "{}", done.version);
        let steps = steps.lock().unwrap();
        assert!(steps[0].contains("claude.ai"), "{steps:?}");
        assert!(steps.iter().any(|s| s.contains("Checking")), "{steps:?}");
        assert!(
            !root.join("work").exists(),
            "the script and its log are removed"
        );
        assert_eq!(Store::find_installed(Tool::Claude, &home), Some(done.path));
        let seen = fs::read_to_string(root.join("seen.txt")).unwrap();
        assert!(!seen.contains("CARGO"), "{seen}");
        // The plain drive form of the (canonical, verbatim) test home.
        let plain_home = home.display().to_string();
        let plain_home = plain_home.trim_start_matches(r"\\?\");
        assert!(
            seen.contains(&format!("USERPROFILE={plain_home}\r\n")),
            "{seen}"
        );
        assert!(seen.contains("OS=Windows_NT"), "{seen}");
        assert!(seen.contains("CODEX_NON_INTERACTIVE=1"), "{seen}");
        assert!(seen.contains("policy=Bypass"), "{seen}");
        assert!(seen.contains("redirected=True"), "{seen}");
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_script_that_fails_says_so_plainly_and_keeps_the_raw_reason_apart() {
        let root = temp("ps-bad");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let installer = Official::with_script(
            root.join("work"),
            ps_script(&root, "Write-Error 'disk is on fire'\nexit 3\n"),
        );
        let failure = installer.install(Tool::Claude, &home, &|_| {}).unwrap_err();
        assert!(failure.what.contains("did not finish"), "{failure:?}");
        assert!(failure.details.contains("Some(3)"), "{failure:?}");
        assert!(failure.details.contains("disk is on fire"), "{failure:?}");
        assert!(!home.join(".local").exists());
        // Ends well but leaves nothing: failed too.
        let none = Official::with_script(root.join("w2"), ps_script(&root, "exit 0\n"));
        let failure = none.install(Tool::Codex, &home, &|_| {}).unwrap_err();
        assert!(failure.what.contains("cannot find it"), "{failure:?}");
        assert!(failure.details.contains("OpenAI"), "{failure:?}");
        let _ = fs::remove_dir_all(&root);
    }

    // Windows: the download runs the system's curl.exe with the macOS flags
    // (HTTPS only); an address nobody answers at is worded for a person.
    #[cfg(windows)]
    #[test]
    fn on_windows_the_download_uses_curl_and_an_unreachable_host_is_worded_for_a_person() {
        let root = temp("ps-fetch");
        let installer = Official::new(&root.join("data"));
        let source = Source {
            name: "Claude Code",
            vendor: "Anthropic",
            url: "https://127.0.0.1:9/install.ps1",
            host: "127.0.0.1",
        };
        fs::create_dir_all(&root).unwrap();
        let failure = installer
            .fetch(source, &root.join("install-claude.ps1"))
            .unwrap_err();
        assert!(
            failure.what.contains("could not reach 127.0.0.1"),
            "{failure:?}"
        );
        assert!(
            failure.details.starts_with("curl exit Some(7)"),
            "{failure:?}"
        );
        // Plain HTTP is refused by curl itself (`--proto =https`).
        let plain = Source {
            url: "http://127.0.0.1:9/install.ps1",
            ..source
        };
        let failure = installer
            .fetch(plain, &root.join("install-claude.ps1"))
            .unwrap_err();
        assert!(failure.details.contains("Some(1)"), "{failure:?}");
        assert!(!root.join("install-claude.ps1").exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// The vendors' real installers, into a throwaway home (never the user's).
    /// Needs the network and downloads the programs, so it is run by hand:
    /// `cargo test -p connecto-app --bin connecto-app real_vendor -- --ignored --nocapture`.
    #[cfg(unix)]
    #[test]
    #[ignore = "downloads from the vendors; run by hand"]
    fn real_vendor_installers_work_into_an_isolated_home() {
        for tool in [Tool::Claude, Tool::Codex] {
            let root = temp(&format!("real-{}", tool.name()));
            let home = root.join("home");
            fs::create_dir_all(&home).unwrap();
            let installer = Official::new(&root.join("data"));
            let steps = Mutex::new(Vec::new());
            let done = installer
                .install(tool, &home, &|s| steps.lock().unwrap().push(s.to_owned()))
                .unwrap_or_else(|f| panic!("{tool:?}: {f:?}"));
            println!(
                "{}: {} -> {}",
                tool.name(),
                done.path.display(),
                done.version
            );
            println!("steps: {:?}", steps.lock().unwrap());
            assert!(done.path.starts_with(&home), "{:?}", done.path);
            assert!(!done.version.is_empty());
            assert_eq!(Store::find_installed(tool, &home), Some(done.path.clone()));
            // The isolated sessions use a verified copy: the real program must pass that check.
            let store = Store::new(&root.join("data"));
            store
                .provision(tool, &done.path, &connecto_store::Codesign)
                .unwrap_or_else(|e| panic!("{tool:?} was not accepted: {e}"));
            assert!(store.newest(tool).is_ok());
            for entry in fs::read_dir(&home).unwrap().flatten() {
                println!(
                    "  in the isolated home: {}",
                    entry.file_name().to_string_lossy()
                );
            }
        }
    }
}
