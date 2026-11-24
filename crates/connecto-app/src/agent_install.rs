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
//! - Pi: `https://pi.dev/install.sh`, run with `sh`. Pi is a Node.js program:
//!   the installer needs `node` (22.19 or newer) and `npm` and is given the
//!   user's own search path to find them. It puts Pi under `~/.pi/agent/install`
//!   and a `pi` command in a folder of the search path (`~/.local/bin`), or in
//!   `~/.pi/agent/bin`. On Windows, `https://pi.dev/install.ps1` (below).
//! - Antigravity CLI: `https://antigravity.google/cli/install.sh`, run with
//!   `bash`. It checks the download's SHA-512 against Google's manifest, puts
//!   the program at `~/.local/bin/agy`, and then runs `agy install` to set up
//!   the shell's search path. It needs no Node, no Homebrew and no password.
//!   On Windows, `https://antigravity.google/cli/install.ps1`, which puts
//!   `agy.exe` in `%LOCALAPPDATA%\agy\bin`.
//! - OpenCode: `https://opencode.ai/install`, run with `bash` and
//!   `--no-modify-path`. It downloads the newest 1.x release from OpenCode's
//!   GitHub releases and puts the program at `~/.opencode/bin/opencode`,
//!   where Connecto looks for it; told so, it leaves the shell's startup file
//!   alone. It needs no Node, no Homebrew and no password. It has no
//!   PowerShell installer, so on Windows it is installed by hand.
//!
//! The script is downloaded first (`curl`, HTTPS only, TLS 1.2 or newer, with a
//! time limit), written to a private folder under the data folder, and run from
//! there with a minimal environment, no terminal and no input, under a time
//! limit. Afterwards the program must exist where Connecto looks for it and
//! must answer `--version`; otherwise the install counts as failed.
//!
//! Windows (decision D-2 in `docs/windows-port.md`): the vendors' PowerShell
//! installers from the same hosts, `https://claude.ai/install.ps1` and
//! `https://chatgpt.com/codex/install.ps1`, downloaded the same way with the
//! `curl.exe` Windows ships and run with Windows PowerShell by absolute path
//! (`-NoProfile -NonInteractive -ExecutionPolicy Bypass -File`, the policy for
//! that one process only), with no console window. Claude Code's puts
//! `claude.exe` in `~\.local\bin`; Codex's puts its release under
//! `~\.codex\packages\standalone`, behind
//! `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin`, and adds that folder to the
//! user's `PATH`. Pi's, `https://pi.dev/install.ps1`, puts Pi under
//! `~\.pi\agent\install` and its launcher `pi.cmd` in `~\.pi\agent\bin`; as
//! on macOS it needs the user's own Node.js. Details in
//! `crate::platform::installer_command`.
//!
//! On every platform the program is trusted only after this: the setup and
//! Settings > Agents then copy it into Connecto's store, which checks its
//! signature (macOS: Apple Team ID; Windows: Authenticode and publisher), as
//! for a program that was already installed. Pi is the exception: it is
//! published without a code signature, so it is run where it is installed and
//! the store only records where that is (`connecto_store::Tool::Pi`). So is
//! Antigravity CLI: it is signed (Google LLC), but it updates itself in place,
//! so it too runs where it is installed. So does OpenCode, which upgrades
//! itself in `~/.opencode/bin`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use connecto_store::{Store, Tool};

use crate::node_setup::NodeCheck;
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
        Tool::Pi => Some(Source {
            name: "Pi",
            vendor: "Earendil Works",
            url,
            host: "pi.dev",
        }),
        Tool::Antigravity => Some(Source {
            name: "Antigravity CLI",
            vendor: "Google",
            url,
            host: "antigravity.google",
        }),
        Tool::OpenCode => Some(Source {
            name: "OpenCode",
            vendor: "OpenCode",
            url,
            // Its script on macOS and Linux; its GitHub releases on Windows.
            host: if url.starts_with("https://github.com/") {
                "github.com"
            } else {
                "opencode.ai"
            },
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

    /// Whether the Node.js Pi's installer would use will do. Test fakes say
    /// it does unless told otherwise, and never run `node`.
    fn node_for_pi(&self, home: &Path) -> NodeCheck {
        let _ = home;
        NodeCheck::Ready
    }

    /// Install Node.js for Pi ([`crate::node_setup`]), telling `say` which step
    /// it is on. Installs nothing unless an installer says otherwise.
    ///
    /// # Errors
    /// A [`Failure`], worded for a person.
    fn install_node(&self, home: &Path, say: &dyn Fn(&str)) -> Result<(), Failure> {
        let _ = (home, say);
        Err(Failure {
            what: "Node.js was not installed.".to_owned(),
            next: String::new(),
            details: "this installer does not install Node.js".to_owned(),
        })
    }
}

/// OpenCode's releases, newest first, as GitHub lists them; and where each
/// one's files are. The 1.x line is the one Connecto's plugin is for.
const OPENCODE_RELEASES: &str =
    "https://api.github.com/repos/anomalyco/opencode/releases?per_page=100";
const OPENCODE_DOWNLOADS: &str = "https://github.com/anomalyco/opencode/releases/download";
/// How many pages of 100 releases are looked through for a 1.x one.
const RELEASE_PAGES: u32 = 10;

/// Page `page` (from 1) of OpenCode's release list.
pub(crate) fn opencode_releases_page(page: u32) -> String {
    format!("{OPENCODE_RELEASES}&page={page}")
}

/// Whether a page of GitHub's release list holds any release: an empty one
/// is past the end.
fn lists_releases(list: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(list)
        .ok()
        .and_then(|v| v.as_array().map(|releases| !releases.is_empty()))
        .unwrap_or(false)
}
/// How long the download of a release (a program of some 200 MB) may take.
const RELEASE_LIMIT_SECS: &str = "900";

/// The newest OpenCode 1.x release in GitHub's list (`tag_name` `v1.N.M`),
/// leaving out drafts and pre-releases, as OpenCode's own installer picks
/// it. `None` when the list names none.
pub(crate) fn newest_opencode_1(list: &str) -> Option<String> {
    let releases: serde_json::Value = serde_json::from_str(list).ok()?;
    releases.as_array()?.iter().find_map(|release| {
        let flag = |name: &str| release.get(name).and_then(serde_json::Value::as_bool);
        if flag("draft") == Some(true) || flag("prerelease") == Some(true) {
            return None;
        }
        let version = release.get("tag_name")?.as_str()?.strip_prefix("v1.")?;
        let parts: Vec<&str> = version.split('.').collect();
        let numeric = parts.len() == 2
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
        numeric.then(|| format!("1.{version}"))
    })
}

/// OpenCode's Windows archive for a processor: ARM's own, else x64, and the
/// x64 build for processors without AVX2 (`-baseline`) where AVX2 is
/// missing, as OpenCode's installer script picks it.
#[must_use]
pub fn opencode_windows_asset(arm64: bool, avx2: bool) -> &'static str {
    match (arm64, avx2) {
        (true, _) => "opencode-windows-arm64.zip",
        (false, true) => "opencode-windows-x64.zip",
        (false, false) => "opencode-windows-x64-baseline.zip",
    }
}

/// Where release `version`'s file `asset` is downloaded from.
pub(crate) fn opencode_release_url(version: &str, asset: &str) -> String {
    format!("{OPENCODE_DOWNLOADS}/v{version}/{asset}")
}

/// The vendors' own installers.
pub struct Official {
    /// Where the script is kept while it runs (a private folder).
    work: PathBuf,
    /// Tests give a script file instead of an address.
    #[cfg(test)]
    local: Option<PathBuf>,
    /// Tests give local files for addresses, and see which were asked for.
    #[cfg(test)]
    served: std::sync::Mutex<Vec<(String, PathBuf)>>,
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
            #[cfg(test)]
            served: std::sync::Mutex::default(),
        }
    }

    #[cfg(test)]
    fn with_script(work: PathBuf, script: PathBuf) -> Self {
        Self {
            work,
            local: Some(script),
            served: std::sync::Mutex::default(),
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
        self.download(source, source.url, to, DOWNLOAD_LIMIT_SECS)
    }

    /// `url` into `to`: HTTPS only, TLS 1.2 or newer, within `limit` seconds.
    fn download(&self, source: Source, url: &str, to: &Path, limit: &str) -> Result<(), Failure> {
        #[cfg(test)]
        {
            let served = self
                .served
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !served.is_empty() {
                let file = served
                    .iter()
                    .find(|(at, _)| at == url)
                    .map(|(_, file)| file);
                return match file {
                    Some(file) => fs::copy(file, to).map(|_| ()).map_err(|err| Failure {
                        what: "The download failed.".to_owned(),
                        next: "Try again.".to_owned(),
                        details: err.to_string(),
                    }),
                    None => Err(download_failure(
                        source,
                        Some(22),
                        &format!("not served: {url}"),
                    )),
                };
            }
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
                limit,
                "--output",
            ])
            .arg(to)
            .arg(url)
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

    fn node_for_pi(&self, home: &Path) -> NodeCheck {
        crate::node_setup::check(home)
    }

    fn install_node(&self, home: &Path, say: &dyn Fn(&str)) -> Result<(), Failure> {
        crate::node_setup::install(home, &self.work.with_extension("node"), say)
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
        fs::create_dir_all(&self.work)
            .map_err(|e| io("Connecto could not prepare a folder.", e))?;
        crate::platform::make_private(&self.work);
        // Where the vendor has no installer script for this system, its own
        // release, as its script would fetch it (OpenCode on Windows).
        if let Some(asset) = crate::platform::release_asset(tool) {
            let outcome = self.install_release(tool, source, asset, home, say);
            let _ = fs::remove_dir_all(&self.work);
            return outcome;
        }
        say(&format!("Downloading the installer from {}…", source.host));
        let script = self
            .work
            .join(format!("install-{}.{INSTALLER_EXTENSION}", tool.name()));
        let outcome = self.run(tool, source, home, &script, say);
        let _ = fs::remove_dir_all(&self.work);
        outcome
    }
}

impl Official {
    /// OpenCode from its own release, the way its installer script does it
    /// on a system with a shell: the newest 1.x release from GitHub's list,
    /// its archive `asset` for this system, and the program from it in
    /// `~/.opencode/bin`, where Connecto looks for it. Only OpenCode has a
    /// release install.
    fn install_release(
        &self,
        tool: Tool,
        source: Source,
        asset: &str,
        home: &Path,
        say: &dyn Fn(&str),
    ) -> Result<Installed, Failure> {
        let failed = |what: String, details: String| Failure {
            what,
            next: "Try again, or skip this for now.".to_owned(),
            details,
        };
        say(&format!("Finding the newest {} 1.x release…", source.name));
        // Page after page, newest first, until a 1.x release or the end of
        // the list: OpenCode 2's releases come first, and in time fill pages.
        let list = self.work.join("releases.json");
        let mut version = None;
        for page in 1..=RELEASE_PAGES {
            self.download(
                source,
                &opencode_releases_page(page),
                &list,
                DOWNLOAD_LIMIT_SECS,
            )?;
            let text = fs::read_to_string(&list).unwrap_or_default();
            version = newest_opencode_1(&text);
            if version.is_some() || !lists_releases(&text) {
                break;
            }
        }
        let version = version.ok_or_else(|| {
            failed(
                format!("No {} 1.x release was found.", source.name),
                "GitHub's release list names no v1 release".to_owned(),
            )
        })?;
        say(&format!(
            "Downloading {} {version} (this can take a minute)…",
            source.name
        ));
        let archive = self.work.join(asset);
        self.download(
            source,
            &opencode_release_url(&version, asset),
            &archive,
            RELEASE_LIMIT_SECS,
        )?;
        let unpacked = self.work.join("unpacked");
        fs::create_dir_all(&unpacked).map_err(|e| {
            failed(
                "Connecto could not prepare a folder.".to_owned(),
                e.to_string(),
            )
        })?;
        crate::platform::unpack_zip(&archive, &unpacked).map_err(|details| {
            failed(
                format!("The {} download could not be unpacked.", source.name),
                details,
            )
        })?;
        let program = format!("{}{}", tool.program(), std::env::consts::EXE_SUFFIX);
        let found = unpacked.join(&program);
        if !found.is_file() {
            return Err(failed(
                format!("The {} download did not hold its program.", source.name),
                format!("no {program} in {asset}"),
            ));
        }
        let bin = home.join(".opencode").join("bin");
        fs::create_dir_all(&bin).map_err(|e| {
            failed(
                "Connecto could not prepare a folder.".to_owned(),
                e.to_string(),
            )
        })?;
        // Whole or not at all: an OpenCode that runs from there stays as it
        // was until the new one is in place.
        let staged = bin.join(format!(".{program}.connecto"));
        fs::copy(&found, &staged)
            .and_then(|_| fs::rename(&staged, bin.join(&program)))
            .map_err(|e| {
                let _ = fs::remove_file(&staged);
                failed(
                    format!("{} could not be put in its folder.", source.name),
                    e.to_string(),
                )
            })?;
        say("Checking that it runs…");
        verify(tool, source, home)
    }

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
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    // Read while it runs: a full pipe would hold it until the time limit.
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
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
    // Its stdout, or its stderr when only that names the version: the rule
    // the store and the check before a start read it by.
    let text = connecto_store::version_text(
        &stdout.join().unwrap_or_default(),
        &stderr.join().unwrap_or_default(),
    );
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
        for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
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
    // `--version` (the vendor's script is not run here: see the report in
    // docs/windows-port.md section 10).
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

    /// OpenCode's real installer, into a throwaway home: the program lands in
    /// `~/.opencode/bin`, where the store finds it and runs it in place, and
    /// no shell startup file is written. Run by hand, as above:
    /// `cargo test -p connecto-app --bin connecto-app real_opencode -- --ignored --nocapture`.
    #[cfg(unix)]
    #[test]
    #[ignore = "downloads from opencode.ai and GitHub; run by hand"]
    fn the_real_opencode_installer_works_into_an_isolated_home() {
        let root = temp("real-opencode");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let installer = Official::new(&root.join("data"));
        let done = installer
            .install(Tool::OpenCode, &home, &|s| println!("step: {s}"))
            .unwrap_or_else(|f| panic!("{f:?}"));
        println!("{} -> {}", done.path.display(), done.version);
        assert_eq!(done.path, home.join(".opencode/bin/opencode"));
        assert!(done.version.starts_with("1."), "{}", done.version);
        assert_eq!(
            Store::find_installed(Tool::OpenCode, &home),
            Some(done.path.clone())
        );
        let store = Store::new(&root.join("data"));
        let installed = store
            .provision(Tool::OpenCode, &done.path, &connecto_store::Codesign)
            .unwrap();
        assert_eq!(installed.entry, done.path, "run in place, not copied");
        for rc in [
            ".zshrc",
            ".bashrc",
            ".bash_profile",
            ".profile",
            ".config/fish",
        ] {
            assert!(!home.join(rc).exists(), "{rc} was written");
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// The release a release install takes: the newest 1.x one GitHub lists,
    /// never 2.x, a draft or a pre-release, as OpenCode's own script picks.
    #[test]
    fn the_newest_opencode_1_release_is_taken_and_never_2() {
        let list = r#"[
            {"tag_name":"v2.4.0","draft":false,"prerelease":false},
            {"tag_name":"v1.20.0","draft":true,"prerelease":false},
            {"tag_name":"v1.19.9","draft":false,"prerelease":true},
            {"tag_name":"v1.19.7-beta","draft":false,"prerelease":false},
            {"tag_name":"v1.19.6","draft":false,"prerelease":false},
            {"tag_name":"v1.19.5","draft":false,"prerelease":false}
        ]"#;
        assert_eq!(newest_opencode_1(list).as_deref(), Some("1.19.6"));
        assert_eq!(newest_opencode_1(r#"[{"tag_name":"v2.0.0"}]"#), None);
        assert_eq!(newest_opencode_1("not json"), None);
        assert_eq!(
            opencode_release_url("1.19.6", "opencode-windows-x64.zip"),
            "https://github.com/anomalyco/opencode/releases/download/v1.19.6/opencode-windows-x64.zip"
        );
    }

    /// Windows: OpenCode comes from its GitHub release (it has no PowerShell
    /// installer), the archive for this machine's processor.
    #[cfg(windows)]
    #[test]
    fn on_windows_opencode_is_installed_from_its_release() {
        let source = source_of(Tool::OpenCode).unwrap();
        assert_eq!(source.host, "github.com");
        assert_eq!(source.url, "https://github.com/anomalyco/opencode/releases");
        let asset = crate::platform::release_asset(Tool::OpenCode).unwrap();
        assert!(asset.starts_with("opencode-windows-"), "{asset}");
        assert_eq!(crate::platform::release_asset(Tool::Claude), None);
    }

    /// The Windows archive for each processor: ARM's own, x64 with AVX2, and
    /// the baseline x64 build without it, as OpenCode's installer chooses.
    #[test]
    fn the_windows_archive_follows_the_processor() {
        assert_eq!(
            opencode_windows_asset(false, true),
            "opencode-windows-x64.zip"
        );
        assert_eq!(
            opencode_windows_asset(false, false),
            "opencode-windows-x64-baseline.zip"
        );
        assert_eq!(
            opencode_windows_asset(true, false),
            "opencode-windows-arm64.zip"
        );
    }

    /// A zip holding one program, `name`, that prints `version`.
    #[cfg(unix)]
    fn release_zip(dir: &Path, name: &str, version: &str) -> PathBuf {
        let folder = dir.join("content");
        fs::create_dir_all(&folder).unwrap();
        crate::test_support::link_fake(
            &format!("#!/bin/sh\necho '{version}'\n"),
            &folder.join(name),
        );
        let zip = dir.join("release.zip");
        let made = std::process::Command::new("/usr/bin/tar")
            .args(["--format", "zip", "-cf"])
            .arg(&zip)
            .arg("-C")
            .arg(&folder)
            .arg(name)
            .status()
            .unwrap();
        assert!(made.success());
        zip
    }

    /// The release install, with the files served locally: the newest 1.x
    /// release's archive for this system is fetched (not 2.x's), its program
    /// lands in `~/.opencode/bin` whole, and it is found there and runs.
    #[cfg(unix)]
    #[test]
    fn a_release_install_puts_the_newest_1x_program_in_its_folder() {
        let root = temp("release");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        // The first page holds OpenCode 2's releases only; 1.x is on the next.
        let first = root.join("page1.json");
        fs::write(&first, r#"[{"tag_name":"v2.1.0"},{"tag_name":"v2.0.0"}]"#).unwrap();
        let second = root.join("page2.json");
        fs::write(
            &second,
            r#"[{"tag_name":"v1.19.6"},{"tag_name":"v1.19.5"}]"#,
        )
        .unwrap();
        let zip = release_zip(&root, "opencode", "1.19.6");
        let installer = Official::new(&root.join("data"));
        let asset = "opencode-test-x64.zip";
        *installer.served.lock().unwrap() = vec![
            (opencode_releases_page(1), first.clone()),
            (opencode_releases_page(2), second),
            (opencode_release_url("1.19.6", asset), zip),
        ];
        fs::create_dir_all(&installer.work).unwrap();
        let source = Source {
            name: "OpenCode",
            vendor: "OpenCode",
            url: "https://github.com/anomalyco/opencode/releases",
            host: "github.com",
        };
        let steps = Mutex::new(Vec::new());
        let done = installer
            .install_release(Tool::OpenCode, source, asset, &home, &|s| {
                steps.lock().unwrap().push(s.to_owned());
            })
            .unwrap_or_else(|f| panic!("{f:?}"));
        assert_eq!(done.path, home.join(".opencode/bin/opencode"));
        assert_eq!(done.version, "1.19.6");
        let steps = steps.lock().unwrap();
        assert!(
            steps.iter().any(|s| s.contains("OpenCode 1.19.6")),
            "{steps:?}"
        );
        // Nothing half-written is left beside it.
        let left: Vec<_> = fs::read_dir(home.join(".opencode/bin"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, ["opencode"]);
        // A release with no 1.x line installs nothing.
        // A list with no 1.x line at all ends at an empty page.
        let end = root.join("end.json");
        fs::write(&end, "[]").unwrap();
        *installer.served.lock().unwrap() = vec![
            (opencode_releases_page(1), first),
            (opencode_releases_page(2), end),
        ];
        let failure = installer
            .install_release(Tool::OpenCode, source, asset, &root.join("other"), &|_| {})
            .unwrap_err();
        assert!(
            failure.what.contains("No OpenCode 1.x release"),
            "{failure:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The release install against GitHub itself, with this Mac's own archive
    /// (`opencode-darwin-*.zip`): the same steps a Windows install takes,
    /// with the Windows archive. Run by hand, as above.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "downloads OpenCode's release from GitHub; run by hand"]
    fn the_real_opencode_release_installs_into_an_isolated_home() {
        let root = temp("real-release");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let installer = Official::new(&root.join("data"));
        fs::create_dir_all(&installer.work).unwrap();
        let asset = if cfg!(target_arch = "aarch64") {
            "opencode-darwin-arm64.zip"
        } else {
            "opencode-darwin-x64.zip"
        };
        let source = Source {
            name: "OpenCode",
            vendor: "OpenCode",
            url: "https://github.com/anomalyco/opencode/releases",
            host: "github.com",
        };
        let done = installer
            .install_release(Tool::OpenCode, source, asset, &home, &|s| {
                println!("step: {s}")
            })
            .unwrap_or_else(|f| panic!("{f:?}"));
        println!("{} -> {}", done.path.display(), done.version);
        assert_eq!(done.path, home.join(".opencode/bin/opencode"));
        assert!(done.version.starts_with("1."), "{}", done.version);
        let _ = fs::remove_dir_all(&root);
    }
}
