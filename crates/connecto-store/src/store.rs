//! The store layout: `<DATA>/cli/<tool>/<version>/`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};

use crate::discover;
use crate::hash::sha256_file;
use crate::platform::{self, CONNECTO_LINK, HOOK_NAME, codex_platform};
use crate::signature::{CHECK_ATTEMPTS, Failure, RETRY_PAUSE, Signature, check_all_with_retries};

/// One provisioning at a time in this process. Every provision of a tool in
/// one process uses the same work folder, so a second one would remove the
/// first one's copy in the middle of its check.
static PROVISIONING: Mutex<()> = Mutex::new(());

const SUMS_FILE: &str = "SHA256SUMS";
/// Written when a stored copy failed verification: the next start sets up again.
const REPAIR_MARK: &str = ".repair";
const HOOK_SUM_NAME: &str = "connecto-hook.sha256";
/// How a provision's work folder is named: this, the tool, `-`, the pid.
const STAGING_PREFIX: &str = ".staging-";

/// A kind of terminal. Claude and Codex are CLIs the store copies and verifies;
/// `Shell` is the user's own shell, which is not stored, provisioned or
/// signature-checked (it is not one of the CLIs `provision` copies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Codex,
    Shell,
}

impl Tool {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Shell => "shell",
        }
    }

    /// Whether the store holds a copy of this kind.
    #[must_use]
    pub const fn is_stored(self) -> bool {
        !matches!(self, Self::Shell)
    }

    /// Who must have signed the binary: on macOS the Apple Team ID
    /// (`Q6L2SF6YDW`, `2DC432GLL2`), on Windows the publisher named in the
    /// signing certificate (`Anthropic, PBC`, `OpenAI OpCo, LLC`).
    #[must_use]
    pub const fn team_id(self) -> &'static str {
        match self {
            Self::Claude => platform::CLAUDE_SIGNER,
            Self::Codex => platform::CODEX_SIGNER,
            Self::Shell => "",
        }
    }

    /// The executable, relative to the version folder (`claude.exe` and
    /// `bin/codex.exe` on Windows).
    #[must_use]
    pub const fn entry(self) -> &'static str {
        match self {
            Self::Claude => platform::CLAUDE_ENTRY,
            Self::Codex => platform::CODEX_ENTRY,
            Self::Shell => "",
        }
    }

    /// What is copied from a release folder, by exact name. Nothing else is
    /// read, because the Codex release sits inside the folder holding its auth.
    #[must_use]
    pub const fn release_entries(self) -> &'static [&'static str] {
        match self {
            Self::Claude | Self::Shell => &[],
            Self::Codex => &["bin", "codex-path", "codex-resources", "codex-package.json"],
        }
    }
}

/// Failures while provisioning or verifying.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("file error at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("unexpected source layout: {0}")]
    BadSource(String),
    /// The copy was checked and refused: unsigned, modified, or another team's.
    #[error("signature check failed: {0}")]
    Signature(String),
    /// The check could not be completed (timed out, could not run, stopped).
    /// Nothing was judged about the copy.
    #[error("signature check could not be completed: {0}")]
    Unchecked(String),
    #[error("copy does not match its source: {0}")]
    CopyMismatch(PathBuf),
    #[error("the source changed during provisioning: {0}")]
    SourceChanged(PathBuf),
    #[error("could not read the version of {0}")]
    UnknownVersion(&'static str),
    #[error("store copy failed verification: {0}")]
    Tampered(String),
    #[error("not provisioned: {tool} {version}")]
    NotProvisioned { tool: &'static str, version: String },
    #[error("no verified copy of {0} is set up in Connecto yet")]
    NoVersion(&'static str),
    /// Every install found was tried, or ruled out, and none can be used.
    /// `tried` names each one, in order, with why. `refused` says that at least
    /// one of them was a program whose signature was definitely refused.
    #[error("no copy of {tool} could be used: {}", describe_tried(.tried))]
    NoUsableCopy {
        tool: &'static str,
        tried: Vec<(PathBuf, String)>,
        refused: bool,
    },
    #[error("Connecto's helper is missing; it is set up again at the next start")]
    HookMissing,
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_owned(),
        source,
    }
}

/// A verified copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    /// Absolute path of the executable.
    pub entry: PathBuf,
}

/// The install that was copied, from the search path, with the ones before it
/// that were not used (each with why).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provisioned {
    pub installed: Installed,
    /// The copy on the search path that was provisioned, as found there.
    pub found: PathBuf,
    /// Installs found before `found` that were ruled out or refused.
    pub skipped: Vec<(PathBuf, String)>,
}

/// The most candidates named in one message; the rest are counted.
const MAX_LISTED: usize = 5;

/// A path as a person is shown it: control characters are left out.
#[must_use]
pub fn shown_path(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// Each install tried, with why, joined by `separator`. At most [`MAX_LISTED`]
/// are named, then how many more there were.
#[must_use]
pub fn describe_list(tried: &[(PathBuf, String)], separator: &str) -> String {
    let mut parts: Vec<String> = tried
        .iter()
        .take(MAX_LISTED)
        .map(|(path, why)| format!("{}: {why}", shown_path(path)))
        .collect();
    if tried.len() > MAX_LISTED {
        parts.push(format!("and {} more", tried.len() - MAX_LISTED));
    }
    parts.join(separator)
}

/// Each install tried, with why, as one line for a person.
fn describe_tried(tried: &[(PathBuf, String)]) -> String {
    if tried.is_empty() {
        return "none was found".to_owned();
    }
    describe_list(tried, "; ")
}

/// The data folder override: `CONNECTO_DATA_DIR`.
#[must_use]
pub fn data_dir_override() -> Option<OsString> {
    std::env::var_os("CONNECTO_DATA_DIR")
}

/// The data root: `CONNECTO_DATA_DIR`, or the app's Application Support folder
/// (`%APPDATA%\connecto` on Windows).
#[must_use]
pub fn data_root(override_dir: Option<OsString>, home: &Path) -> PathBuf {
    override_dir.map_or_else(|| platform::default_data_root(home), PathBuf::from)
}

/// The user's home folder: `HOME` (`USERPROFILE` on Windows). `None` when it
/// is not set.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    platform::home_dir()
}

/// The CLI store under a data root.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// Every regular file under `root`, relative to it. Anything else, including a
/// symlink, is an error.
fn walk(root: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> Result<(), StoreError> {
    let path = root.join(relative);
    let meta = fs::symlink_metadata(&path).map_err(io_error(&path))?;
    if meta.is_dir() {
        let mut names: Vec<_> = fs::read_dir(&path)
            .map_err(io_error(&path))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<_, _>>()
            .map_err(io_error(&path))?;
        names.sort();
        for name in names {
            walk(root, &relative.join(name), out)?;
        }
        Ok(())
    } else if meta.is_file() {
        out.push(relative.to_owned());
        Ok(())
    } else {
        Err(StoreError::BadSource(format!(
            "not a regular file: {}",
            path.display()
        )))
    }
}

/// The files of a Codex release folder: exactly what provisioning copies
/// ([`Tool::release_entries`]), each a regular file or a folder of them, with no
/// links. One definition, for finding an install and for copying it.
fn codex_release_files(root: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut files = Vec::new();
    for name in Tool::Codex.release_entries() {
        walk(root, Path::new(name), &mut files)?;
    }
    let entry = Tool::Codex.entry();
    if !files.iter().any(|file| file == Path::new(entry)) {
        return Err(StoreError::BadSource(format!(
            "{} has no {entry}",
            root.display()
        )));
    }
    Ok(files)
}

/// Whether `dir` is a complete Codex release folder.
fn is_codex_release(dir: &Path) -> bool {
    codex_release_files(dir).is_ok()
}

/// Where the files of a Codex install are, given the real path of its entry.
/// Either the release folder itself (`<release>/bin/codex`), or the npm
/// package (`<pkg>/bin/codex.js`, from `npm i -g @openai/codex`), whose native
/// files live in the platform package npm installs beside it as an optional
/// dependency: `<pkg>/node_modules/@openai/codex-darwin-arm64/vendor/<triple>/`
/// (or, with a hoisting installer such as pnpm, a sibling `@openai/` folder).
/// That folder has the same layout as a release folder.
fn codex_release_root(real: &Path) -> Option<PathBuf> {
    let package = real.parent().and_then(Path::parent)?;
    if is_codex_release(package) {
        return Some(package.to_owned());
    }
    // Only the npm package of Codex, named by its own package.json.
    let manifest = fs::read_to_string(package.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    if manifest["name"].as_str() != Some("@openai/codex") {
        return None;
    }
    let (triple, platform) = codex_platform();
    let beside = package.parent().map(|scope| scope.join(platform));
    [
        Some(package.join("node_modules/@openai").join(platform)),
        beside,
        Some(package.to_owned()),
    ]
    .into_iter()
    .flatten()
    .find_map(|dir| contained_release(&dir, triple))
}

/// `<dir>/vendor/<triple>` as a real path, when it is a release folder that
/// stays inside `dir`. `dir` itself may be a link (pnpm links packages in);
/// what is under it may not lead out of it.
fn contained_release(dir: &Path, triple: &str) -> Option<PathBuf> {
    let boundary = fs::canonicalize(dir).ok()?;
    let release = fs::canonicalize(boundary.join("vendor").join(triple)).ok()?;
    (release.starts_with(&boundary) && is_codex_release(&release)).then_some(release)
}

/// The real path of a candidate for `tool`. For Codex on Windows, npm's
/// launcher script `codex.cmd` stands for the npm package beside it
/// ([`platform::codex_npm_shim_target`]); on Unix that is always the link's
/// own target.
fn real_entry(tool: Tool, entry: &Path) -> io::Result<PathBuf> {
    let real = fs::canonicalize(entry)?;
    if tool == Tool::Codex
        && let Some(package) = platform::codex_npm_shim_target(&real)
        && let Ok(package) = fs::canonicalize(package)
    {
        return Ok(package);
    }
    Ok(real)
}

/// Why `entry` (as found on the search path) cannot be copied, or `None` when it can.
fn layout_problem(tool: Tool, entry: &Path) -> Option<String> {
    let Ok(real) = real_entry(tool, entry) else {
        return Some("it cannot be read".to_owned());
    };
    match tool {
        // Only on Windows, where npm's shims are `.cmd` and `.ps1` files.
        Tool::Claude if platform::is_launcher_by_name(&real) => {
            Some("it is a launcher script (an npm shim), not Claude Code's own program".to_owned())
        }
        Tool::Claude => claude_layout_reason(&real),
        Tool::Codex => codex_release_root(&real)
            .is_none()
            .then(|| codex_layout_reason(&real)),
        Tool::Shell => Some("a shell is not a CLI the store copies".to_owned()),
    }
}

/// Why the real path of a Claude candidate is not Claude Code's own program, or
/// `None` when it is. Only a native (Mach-O; on Windows PE) program is Claude
/// Code: a script named `claude` (a JavaScript launcher, or a shell wrapper
/// such as a terminal's per-session shim) is never copied, whatever its name.
fn claude_layout_reason(real: &Path) -> Option<String> {
    let javascript = real
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext, "js" | "mjs" | "cjs"));
    if javascript {
        return Some(
            "it is a JavaScript launcher (an npm install), not Claude Code's own program"
                .to_owned(),
        );
    }
    match platform::is_native_binary(real) {
        Ok(true) => None,
        Ok(false) => Some("it is a script or wrapper, not the Claude Code program".to_owned()),
        Err(_) => Some("it cannot be read".to_owned()),
    }
}

/// Said for a person: what a Codex entry that is not a release folder is.
fn codex_layout_reason(real: &Path) -> String {
    let (_, platform) = codex_platform();
    let Some(package) = real.parent().and_then(Path::parent) else {
        return "it is not inside a Codex release".to_owned();
    };
    if package.join("codex-package.json").is_file()
        && let Err(err) = codex_release_files(package)
    {
        return format!("its release folder is incomplete or holds links ({err})");
    }
    let named_codex = fs::read_to_string(package.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|manifest| manifest["name"].as_str() == Some("@openai/codex"));
    if named_codex {
        return format!(
            "it is the npm package of Codex, but its platform package @openai/{platform} is missing or does not hold a Codex release (reinstall with `npm install -g @openai/codex`, without omitting optional dependencies)"
        );
    }
    let mut head = [0_u8; 2];
    let script = platform::is_launcher_by_name(real)
        || (fs::File::open(real)
            .and_then(|mut file| io::Read::read_exact(&mut file, &mut head))
            .is_ok()
            && &head == b"#!");
    if script {
        "it is a launcher script (a shim of a package or version manager), not Codex itself"
            .to_owned()
    } else {
        "it is a bare program without Codex's support files (no codex-package.json beside it)"
            .to_owned()
    }
}

fn hash_all(root: &Path, files: &[PathBuf]) -> Result<BTreeMap<PathBuf, String>, StoreError> {
    files
        .iter()
        .map(|file| {
            let path = root.join(file);
            let digest = sha256_file(&path).map_err(io_error(&path))?;
            Ok((file.clone(), digest))
        })
        .collect()
}

/// Set a file's Unix mode; nothing on Windows (see [`platform::set_mode`]).
fn set_mode(path: &Path, mode: u32) -> Result<(), StoreError> {
    platform::set_mode(path, mode).map_err(io_error(path))
}

/// The first word that is a version (see [`version_key`]).
fn parse_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|word| version_key(word).is_some())
        .map(str::to_owned)
}

/// Ask a staged binary its version, with an empty environment and a scratch
/// home, so that it cannot see or touch the user's real CLI state.
fn read_version(tool: Tool, entry: &Path, scratch: &Path) -> Result<String, StoreError> {
    fs::create_dir_all(scratch).map_err(io_error(scratch))?;
    let output = platform::no_console(&mut Command::new(entry))
        .arg("--version")
        .env_clear()
        .env("HOME", scratch)
        .env("CLAUDE_CONFIG_DIR", scratch)
        .env("CODEX_HOME", scratch)
        .current_dir(scratch)
        .stdin(Stdio::null())
        .output()
        .map_err(io_error(entry))?;
    parse_version(&String::from_utf8_lossy(&output.stdout))
        .ok_or(StoreError::UnknownVersion(tool.name()))
}

/// One dot-separated part of a prerelease. A number sorts before a word.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PrePart {
    Number(u64),
    Word(String),
}

/// How versions sort: the three numbers, then a prerelease before its release
/// (`false` before `true`), then the prerelease's parts.
type VersionKey = ([u64; 3], bool, Vec<PrePart>);

/// The dot-separated parts of a prerelease or build suffix: letters, digits
/// and `-` only, none empty. So a version is always a safe folder name: no
/// path separator, no `..`.
fn suffix_parts(text: &str) -> Option<Vec<&str>> {
    text.split('.')
        .map(|part| {
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
                .then_some(part)
        })
        .collect()
}

/// A version's sort key: `N.N.N`, with an optional `-prerelease` and
/// `+build` as in semver (`0.50.0-alpha.3`). The build part does not order.
/// Anything else has none, so it is neither a version nor a version folder.
fn version_key(name: &str) -> Option<VersionKey> {
    let (rest, build) = match name.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (name, None),
    };
    if let Some(build) = build {
        suffix_parts(build)?;
    }
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let mut numbers = core.split('.').map(|part| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse::<u64>().ok())
            .flatten()
    });
    let core = [numbers.next()??, numbers.next()??, numbers.next()??];
    if numbers.next().is_some() {
        return None;
    }
    let pre = match pre {
        Some(pre) => suffix_parts(pre)?
            .into_iter()
            .map(|part| {
                part.parse()
                    .map_or_else(|_| PrePart::Word(part.to_owned()), PrePart::Number)
            })
            .collect(),
        None => Vec::new(),
    };
    Some((core, pre.is_empty(), pre))
}

/// The pid in a staging folder's name, if it is one: `.staging-<tool>-<pid>`.
fn staging_pid(name: &str) -> Option<i32> {
    let rest = name.strip_prefix(STAGING_PREFIX)?;
    let (tool, pid) = rest.split_once('-')?;
    ([Tool::Claude, Tool::Codex].iter().any(|t| t.name() == tool)
        && !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit()))
    .then(|| pid.parse().ok())
    .flatten()
}

/// Make every folder under `path` writable again, so the tree can be removed.
/// Links are not followed.
fn unlock_dirs(path: &Path) {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
        let _ = platform::set_mode(path, 0o755);
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                unlock_dirs(&entry.path());
            }
        }
    }
}

/// Remove a folder that may hold read-only folders (a store copy does).
pub(crate) fn remove_tree(path: &Path) {
    unlock_dirs(path);
    let _ = fs::remove_dir_all(path);
}

fn parse_sums(text: &str) -> Option<BTreeMap<PathBuf, String>> {
    text.lines()
        .map(|line| {
            let (digest, path) = line.split_once("  ")?;
            Some((PathBuf::from(path), digest.to_owned()))
        })
        .collect()
}

fn format_sums(sums: &BTreeMap<PathBuf, String>) -> String {
    sums.iter()
        .map(|(path, digest)| format!("{digest}  {}\n", path.display()))
        .collect()
}

impl Store {
    /// The store inside `data_root`.
    #[must_use]
    pub fn new(data_root: &Path) -> Self {
        Self {
            root: data_root.join("cli"),
        }
    }

    fn version_dir(&self, tool: Tool, version: &str) -> PathBuf {
        self.root.join(tool.name()).join(version)
    }

    /// Check a copy against its recorded digests. Slow for large binaries, so
    /// call it off the UI thread. A changed, missing or extra file fails.
    pub fn verify(&self, tool: Tool, version: &str) -> Result<Installed, StoreError> {
        let dir = self.version_dir(tool, version);
        let sums_path = dir.join(SUMS_FILE);
        if !sums_path.is_file() {
            return Err(StoreError::NotProvisioned {
                tool: tool.name(),
                version: version.to_owned(),
            });
        }
        let text = fs::read_to_string(&sums_path).map_err(io_error(&sums_path))?;
        let expected = parse_sums(&text)
            .ok_or_else(|| StoreError::Tampered("unreadable checksum file".to_owned()))?;

        let mut files = Vec::new();
        walk(&dir, Path::new(""), &mut files)?;
        files.retain(|file| file != Path::new(SUMS_FILE));
        let actual = hash_all(&dir, &files)?;
        if actual != expected {
            let changed = expected
                .keys()
                .chain(actual.keys())
                .find(|file| expected.get(*file) != actual.get(*file))
                .map_or_else(String::new, |file| file.display().to_string());
            return Err(StoreError::Tampered(changed));
        }
        Ok(Installed {
            version: version.to_owned(),
            entry: dir.join(tool.entry()),
        })
    }

    /// The version an installed `entry` reports, or `None` if it will not say.
    /// Runs the tool's `--version` in an empty home, as provisioning does.
    #[must_use]
    pub fn probe_version(&self, tool: Tool, entry: &Path) -> Option<String> {
        let scratch = self
            .root
            .join(format!(".probe-{}-{}", tool.name(), std::process::id()));
        let version = read_version(tool, entry, &scratch).ok();
        remove_tree(&scratch);
        version
    }

    /// Where the user's own install of `tool` is that Connecto can copy: the
    /// folders the user's shell searches (for the real home folder), those the
    /// usual installers use, then `~/.local/bin` and Homebrew's two. The first
    /// whose layout can be copied soundly; `None` if there is none (see
    /// [`Store::unusable_installs`] for what was found and could not be used).
    #[must_use]
    pub fn find_installed(tool: Tool, home: &Path) -> Option<PathBuf> {
        Self::find_installed_in(tool, home, &discover::ambient_path(home))
    }

    /// [`Store::find_installed`] with the shell's `PATH` given.
    #[must_use]
    pub fn find_installed_in(tool: Tool, home: &Path, path: &[PathBuf]) -> Option<PathBuf> {
        discover::candidates(tool.name(), home, path)
            .into_iter()
            .find(|file| layout_problem(tool, file).is_none())
    }

    /// Copy `tool` into the store from the first install that can be used, in
    /// the order of [`Store::find_installed`]. An install whose signature is
    /// definitely refused is not used, and the next is tried. A check that could
    /// not finish stops the search at once: it says nothing about that install,
    /// and another program must not be used in its place.
    pub fn provision_found(
        &self,
        tool: Tool,
        home: &Path,
        signature: &impl Signature,
    ) -> Result<Provisioned, StoreError> {
        self.provision_found_in(tool, home, &discover::ambient_path(home), signature)
    }

    /// [`Store::provision_found`] with the shell's `PATH` given.
    pub fn provision_found_in(
        &self,
        tool: Tool,
        home: &Path,
        path: &[PathBuf],
        signature: &impl Signature,
    ) -> Result<Provisioned, StoreError> {
        let mut tried = Vec::new();
        let mut refused = false;
        for file in discover::candidates(tool.name(), home, path) {
            if let Some(why) = layout_problem(tool, &file) {
                tried.push((file, why));
                continue;
            }
            match self.provision(tool, &file, signature) {
                Ok(installed) => {
                    return Ok(Provisioned {
                        installed,
                        found: file,
                        skipped: tried,
                    });
                }
                Err(StoreError::Signature(why)) => {
                    refused = true;
                    tried.push((file, why));
                }
                Err(other) => return Err(other),
            }
        }
        Err(StoreError::NoUsableCopy {
            tool: tool.name(),
            tried,
            refused,
        })
    }

    /// The copies of `tool` that were found and cannot be used, each with why.
    #[must_use]
    pub fn unusable_installs(tool: Tool, home: &Path) -> Vec<(PathBuf, String)> {
        Self::unusable_installs_in(tool, home, &discover::ambient_path(home))
    }

    /// [`Store::unusable_installs`] with the shell's `PATH` given.
    #[must_use]
    pub fn unusable_installs_in(
        tool: Tool,
        home: &Path,
        path: &[PathBuf],
    ) -> Vec<(PathBuf, String)> {
        discover::candidates(tool.name(), home, path)
            .into_iter()
            .filter_map(|file| layout_problem(tool, &file).map(|why| (file, why)))
            .collect()
    }

    /// The program a found install (from [`Store::find_installed`]) really
    /// runs: Claude Code's own file, or `bin/codex` (`bin\codex.exe`) in
    /// Codex's release folder. On Windows the install found for an npm Codex
    /// is the launcher `codex.cmd`, which needs Node.js and a `PATH`; ask
    /// this program its version instead. `None` when the layout cannot be
    /// copied.
    #[must_use]
    pub fn program_of(tool: Tool, entry: &Path) -> Option<PathBuf> {
        let (root, files) = Self::source_files(tool, entry).ok()?;
        match tool {
            Tool::Claude => files.first().map(|file| root.join(file)),
            Tool::Codex => Some(root.join(tool.entry())),
            Tool::Shell => None,
        }
    }

    /// Ask the next start to set the stored tools up again: something in a
    /// stored copy did not verify. Cleared by [`Store::take_repair`].
    pub fn mark_repair(&self) {
        let _ = fs::create_dir_all(&self.root);
        let _ = fs::write(
            self.root.join(REPAIR_MARK),
            "a stored copy did not verify\n",
        );
    }

    /// Whether a repair was asked for, and forget it.
    #[must_use]
    pub fn take_repair(&self) -> bool {
        fs::remove_file(self.root.join(REPAIR_MARK)).is_ok()
    }

    /// Whether a repair is waiting, without clearing it.
    #[must_use]
    pub fn repair_wanted(&self) -> bool {
        self.root.join(REPAIR_MARK).exists()
    }

    /// The newest version of `tool` in the store, comparing numerically; a
    /// prerelease comes before its release.
    pub fn newest(&self, tool: Tool) -> Result<String, StoreError> {
        let dir = self.root.join(tool.name());
        let Ok(entries) = fs::read_dir(&dir) else {
            return Err(StoreError::NoVersion(tool.name()));
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter_map(|name| Some((version_key(&name)?, name)))
            .max()
            .map(|(_, name)| name)
            .ok_or(StoreError::NoVersion(tool.name()))
    }

    /// Install the hook helper at `<store>/bin`. A new build replaces the old
    /// one atomically, so a hook that is running keeps its file. (Windows
    /// cannot replace a running program: there the old file is renamed aside
    /// first and removed once nothing runs it; see [`platform::replace`].)
    pub fn install_hook(&self, source: &Path) -> Result<PathBuf, StoreError> {
        let dir = self.root.join("bin");
        fs::create_dir_all(&dir).map_err(io_error(&dir))?;
        platform::sweep_replaced(&dir);
        let target = dir.join(HOOK_NAME);
        let staged = dir.join(format!(".{HOOK_NAME}-{}", std::process::id()));
        let digest = sha256_file(source).map_err(io_error(source))?;
        fs::copy(source, &staged).map_err(io_error(source))?;
        if sha256_file(&staged).map_err(io_error(&staged))? != digest {
            let _ = fs::remove_file(&staged);
            return Err(StoreError::CopyMismatch(staged));
        }
        set_mode(&staged, 0o555)?;
        let sum = dir.join(HOOK_SUM_NAME);
        fs::write(&sum, &digest).map_err(io_error(&sum))?;
        platform::replace(&staged, &target).map_err(io_error(&target))?;
        self.ensure_connecto_link()?;
        Ok(target)
    }

    /// `cli/bin/connecto`, a relative link to the helper, which runs as the
    /// `connecto` command when called by that name. Replaced atomically. On
    /// Windows it is `connecto.exe`, a copy of the helper, replaced when its
    /// content differs.
    fn ensure_connecto_link(&self) -> Result<(), StoreError> {
        let dir = self.root.join("bin");
        let link = dir.join(CONNECTO_LINK);
        if platform::connecto_link_is_current(&dir, &link, HOOK_NAME) {
            return Ok(());
        }
        let staged = dir.join(format!(".{CONNECTO_LINK}-{}", std::process::id()));
        let _ = fs::remove_file(&staged);
        platform::make_connecto_link(&dir, HOOK_NAME, &staged).map_err(io_error(&staged))?;
        platform::replace(&staged, &link).map_err(io_error(&link))
    }

    /// Put the helper beside the running app into the store when it differs
    /// from the stored one, and make sure `cli/bin/connecto` links to it. It
    /// never reads or changes the stored tools, so a newer build brings its
    /// helper without moving the bridge off the verified versions (R20). True
    /// when the helper was installed. `source` missing leaves the stored one.
    pub fn refresh_hook(&self, source: &Path) -> Result<bool, StoreError> {
        if !source.is_file() {
            return Ok(false);
        }
        let dir = self.root.join("bin");
        platform::sweep_replaced(&dir);
        let stored = fs::read_to_string(dir.join(HOOK_SUM_NAME)).ok();
        let current = sha256_file(source).map_err(io_error(source))?;
        let same = stored.as_deref() == Some(current.as_str());
        if same && self.verify_hook().is_ok() {
            self.ensure_connecto_link()?;
            return Ok(false);
        }
        self.install_hook(source)?;
        Ok(true)
    }

    /// The folder that holds the helper and the `connecto` link: first on a
    /// terminal's `PATH`. It holds no model CLI.
    #[must_use]
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// The hook helper, after checking it against its recorded digest.
    pub fn verify_hook(&self) -> Result<PathBuf, StoreError> {
        let dir = self.root.join("bin");
        let target = dir.join(HOOK_NAME);
        let Ok(expected) = fs::read_to_string(dir.join(HOOK_SUM_NAME)) else {
            return Err(StoreError::HookMissing);
        };
        let meta = fs::symlink_metadata(&target).map_err(|_| StoreError::HookMissing)?;
        if !meta.is_file() {
            return Err(StoreError::Tampered(HOOK_NAME.to_owned()));
        }
        if sha256_file(&target).map_err(io_error(&target))? != expected {
            return Err(StoreError::Tampered(HOOK_NAME.to_owned()));
        }
        Ok(target)
    }

    /// The source files for a tool, as (root, relative files). Follows the
    /// entry's symlink chain to the real install.
    fn source_files(tool: Tool, entry: &Path) -> Result<(PathBuf, Vec<PathBuf>), StoreError> {
        let real = real_entry(tool, entry).map_err(io_error(entry))?;
        match tool {
            Tool::Claude => {
                let parent = real
                    .parent()
                    .ok_or_else(|| StoreError::BadSource(real.display().to_string()))?;
                let name = real
                    .file_name()
                    .ok_or_else(|| StoreError::BadSource(real.display().to_string()))?;
                Ok((parent.to_owned(), vec![PathBuf::from(name)]))
            }
            Tool::Codex => {
                let root = codex_release_root(&real).ok_or_else(|| {
                    StoreError::BadSource(format!(
                        "{}: {}",
                        real.display(),
                        codex_layout_reason(&real)
                    ))
                })?;
                let files = codex_release_files(&root)?;
                Ok((root, files))
            }
            Tool::Shell => Err(StoreError::BadSource(
                "a shell is not a CLI the store copies".to_owned(),
            )),
        }
    }

    /// Remove the work folders left by a provision that was killed: only
    /// names of the exact form, and only when their process is gone.
    fn sweep_stale_staging(&self) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let stale = entry
                .file_name()
                .to_str()
                .and_then(staging_pid)
                .is_some_and(|pid| !platform::pid_alive(pid));
            if stale && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                remove_tree(&entry.path());
            }
        }
    }

    /// Copy a tool into the store from its installed `entry`. The source is
    /// only ever read. An already provisioned version is verified and reused;
    /// one that fails verification is replaced by the new, verified copy.
    pub fn provision(
        &self,
        tool: Tool,
        entry: &Path,
        signature: &impl Signature,
    ) -> Result<Installed, StoreError> {
        let _one_at_a_time = PROVISIONING.lock().unwrap_or_else(PoisonError::into_inner);
        self.sweep_stale_staging();
        let (source_root, files) = Self::source_files(tool, entry)?;
        let source_hashes = hash_all(&source_root, &files)?;

        let staging = self.root.join(format!(
            "{STAGING_PREFIX}{}-{}",
            tool.name(),
            std::process::id()
        ));
        remove_tree(&staging);
        let result = self.stage_and_install(
            tool,
            &source_root,
            &files,
            &source_hashes,
            &staging,
            signature,
        );
        // It may hold a damaged copy that was moved aside, read-only folders
        // and all.
        remove_tree(&staging);
        result
    }

    fn stage_and_install(
        &self,
        tool: Tool,
        source_root: &Path,
        files: &[PathBuf],
        source_hashes: &BTreeMap<PathBuf, String>,
        staging: &Path,
        signature: &impl Signature,
    ) -> Result<Installed, StoreError> {
        let copy_root = staging.join("copy");
        let staged_entry = copy_root.join(tool.entry());
        // Each copy is held unchanged (on Windows) from its digest through
        // its signature check and trial run, so what is checked is exactly
        // what the digests then protect.
        let (mut sums, mut held) = copy_files(tool, source_root, files, source_hashes, &copy_root)?;
        let checked = verify_signatures(tool, &copy_root, &staged_entry, sums.keys(), signature);
        if let Err(StoreError::Signature(_)) = checked {
            // A definite refusal gets one fresh copy from the source, in case
            // this copy was damaged. A second refusal is final.
            held.clear();
            remove_tree(&copy_root);
            (sums, held) = copy_files(tool, source_root, files, source_hashes, &copy_root)?;
            let rechecked =
                verify_signatures(tool, &copy_root, &staged_entry, sums.keys(), signature);
            rechecked?;
        } else {
            checked?;
        }
        let version = read_version(tool, &staged_entry, &staging.join("home"))?;

        // Hash again, to prove the shared install did not change meanwhile.
        if hash_all(source_root, files)? != *source_hashes {
            return Err(StoreError::SourceChanged(source_root.to_owned()));
        }
        // Let go before the copy's folder is moved (Windows does not move a
        // folder with files open in it). From here on the digests protect it.
        drop(held);

        let final_dir = self.version_dir(tool, &version);
        if let Ok(existing) = fs::symlink_metadata(&final_dir) {
            if let Ok(installed) = self.verify(tool, &version) {
                return Ok(installed);
            }
            // The stored copy is damaged, and nothing would ever repair it:
            // every launch of the tool would be refused. The staged copy has
            // passed every check above, so it takes the damaged one's place.
            // Moved, not deleted in place, so the folder is never half there;
            // it goes with the staging folder. A folder must be writable to
            // be moved; a link in its place is moved as it is, never followed.
            if existing.is_dir() {
                let _ = platform::set_mode(&final_dir, 0o755);
            }
            let aside = staging.join("damaged");
            fs::rename(&final_dir, &aside).map_err(io_error(&final_dir))?;
        }

        let sums_path = copy_root.join(SUMS_FILE);
        fs::write(&sums_path, format_sums(&sums)).map_err(io_error(&sums_path))?;
        let mut all = Vec::new();
        walk(&copy_root, Path::new(""), &mut all)?;
        for file in &all {
            let path = copy_root.join(file);
            let meta = fs::metadata(&path).map_err(io_error(&path))?;
            let executable = platform::is_executable(&path, &meta);
            set_mode(&path, if executable { 0o555 } else { 0o444 })?;
        }

        if let Some(parent) = final_dir.parent() {
            fs::create_dir_all(parent).map_err(io_error(parent))?;
        }
        fs::rename(&copy_root, &final_dir).map_err(io_error(&final_dir))?;
        // Folders go read-only only now: renaming one needs it to be writable.
        lock_dirs(&final_dir)?;
        self.verify(tool, &version)
    }
}

/// Copy every file of the tool from its source into `copy_root`, and prove
/// each copy matches its source. Returns the digests for the sums file, and
/// the copies held unchanged (on Windows; see [`platform::hold`]).
fn copy_files(
    tool: Tool,
    source_root: &Path,
    files: &[PathBuf],
    source_hashes: &BTreeMap<PathBuf, String>,
    copy_root: &Path,
) -> Result<(BTreeMap<PathBuf, String>, Vec<platform::Held>), StoreError> {
    let mut sums = BTreeMap::new();
    let mut held = Vec::with_capacity(files.len());
    for file in files {
        // A Claude install is one file named after its version.
        let target_name = match tool {
            Tool::Claude => PathBuf::from(tool.entry()),
            Tool::Codex | Tool::Shell => file.clone(),
        };
        let target = copy_root.join(&target_name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error(parent))?;
        }
        let source = source_root.join(file);
        fs::copy(&source, &target).map_err(io_error(&source))?;
        held.push(platform::hold(&target).map_err(io_error(&target))?);
        let digest = sha256_file(&target).map_err(io_error(&target))?;
        if source_hashes.get(file) != Some(&digest) {
            return Err(StoreError::CopyMismatch(target));
        }
        sums.insert(target_name, digest);
    }
    Ok((sums, held))
}

/// Check the entry, then every other native binary in the copy (Mach-O on
/// macOS, PE on Windows), against the tool's team. On macOS there is no
/// per-file exemption and each file is checked on its own; on Windows all of
/// them go to the checker in one call.
fn verify_signatures<'a>(
    tool: Tool,
    copy_root: &Path,
    entry: &Path,
    files: impl Iterator<Item = &'a PathBuf>,
    signature: &impl Signature,
) -> Result<(), StoreError> {
    let checks = signature_checks(tool, copy_root, entry, files)?;
    let checks: Vec<(&Path, Option<&str>)> = checks
        .iter()
        .map(|(path, team)| (path.as_path(), *team))
        .collect();
    check_files(signature, &checks)
}

/// What [`verify_signatures`] checks, in order: the entry against the tool's
/// team, then each other native binary against
/// [`platform::bundled_signer`]. On macOS that is the tool's team for every
/// file, with no exemption. On Windows, for Codex, the Visual C++ runtime
/// must be Microsoft's and ripgrep may be unsigned (D-3; see
/// `platform/windows.rs`); every other file must be the tool's publisher's.
fn signature_checks<'a>(
    tool: Tool,
    copy_root: &Path,
    entry: &Path,
    files: impl Iterator<Item = &'a PathBuf>,
) -> Result<Vec<(PathBuf, Option<&'static str>)>, StoreError> {
    let mut checks = vec![(entry.to_owned(), Some(tool.team_id()))];
    for file in files {
        let path = copy_root.join(file);
        if path != entry
            && platform::is_native_binary(&path).map_err(io_error(&path))?
            && let Some(team) = platform::bundled_signer(tool, file)
        {
            checks.push((path, Some(team)));
        }
    }
    Ok(checks)
}

/// The files' checks, in order. A check that could not finish is retried a
/// few times first (each file on its own, or all of them for a checker that
/// checks them together); only a definite answer is returned as a signature
/// failure.
fn check_files(
    signature: &impl Signature,
    files: &[(&Path, Option<&str>)],
) -> Result<(), StoreError> {
    check_all_with_retries(signature, files, CHECK_ATTEMPTS, RETRY_PAUSE).map_err(|failure| {
        match failure {
            Failure::Invalid(reason) => StoreError::Signature(reason),
            Failure::Incomplete(reason) => StoreError::Unchecked(reason),
        }
    })
}

fn lock_dirs(dir: &Path) -> Result<(), StoreError> {
    for entry in fs::read_dir(dir).map_err(io_error(dir))? {
        let path = entry.map_err(io_error(dir))?.path();
        if path.is_dir() {
            lock_dirs(&path)?;
        }
    }
    set_mode(dir, 0o555)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::cell::RefCell;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use super::*;

    /// Accepts or rejects without running codesign, recording what it saw.
    #[derive(Default)]
    struct FakeSignature {
        reject: bool,
        /// Reject only files with this name.
        reject_name: Option<&'static str>,
        seen: RefCell<Vec<(PathBuf, Option<String>)>>,
    }

    impl Signature for FakeSignature {
        fn verify(&self, file: &Path, team: Option<&str>) -> Result<(), String> {
            self.seen
                .borrow_mut()
                .push((file.to_owned(), team.map(str::to_owned)));
            let named = self
                .reject_name
                .is_some_and(|name| file.file_name().is_some_and(|actual| actual == name));
            if self.reject || named {
                Err("rejected".to_owned())
            } else {
                Ok(())
            }
        }
    }

    /// Judges a copy by what is in its bytes: `REFUSE!` is a definite refusal,
    /// `UNFINISHED` a check that could not finish. Anything else passes.
    struct Verdicts;

    impl Signature for Verdicts {
        fn verify(&self, file: &Path, team: Option<&str>) -> Result<(), String> {
            self.check(file, team)
                .map_err(|failure| failure.to_string())
        }

        fn check(&self, file: &Path, _team: Option<&str>) -> Result<(), Failure> {
            let bytes = fs::read(file).unwrap_or_default();
            let has = |marker: &[u8]| bytes.windows(marker.len()).any(|window| window == marker);
            if has(b"REFUSE!") {
                Err(Failure::Invalid("invalid signature: marked".to_owned()))
            } else if has(b"UNFINISHED") {
                Err(Failure::Incomplete("the check timed out".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    /// A real, runnable native program that reports `2.1.0`: a compiled Rust
    /// program, built once per test run with the toolchain that runs the tests
    /// (found beside `CARGO`, as cargo sets it, else on the `PATH`).
    #[cfg(unix)]
    fn built_claude() -> &'static Path {
        static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        BUILT.get_or_init(|| {
            let rustc = std::env::var_os("CARGO")
                .map(PathBuf::from)
                .and_then(|cargo| cargo.parent().map(|dir| dir.join("rustc")))
                .filter(|path| path.is_file())
                .unwrap_or_else(|| PathBuf::from("rustc"));
            let dir = std::env::temp_dir().join(format!("mg-store-native-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let source = dir.join("claude.rs");
            fs::write(&source, "fn main() { println!(\"2.1.0 (Claude Code)\"); }").unwrap();
            let program = dir.join("claude-native");
            let status = Command::new(rustc)
                .arg("-o")
                .arg(&program)
                .arg(&source)
                .status()
                .unwrap();
            assert!(status.success());
            program
        })
    }

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("mg-store-{name}-{}", std::process::id()));
            unlock(&root);
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("data")).unwrap();
            Self {
                root: fs::canonicalize(root).unwrap(),
            }
        }

        fn store(&self) -> Store {
            Store::new(&self.root.join("data"))
        }

        fn script(&self, relative: &str, version: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, format!("#!/bin/sh\necho '{version} (tool)'\n")).unwrap();
            #[cfg(unix)]
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        #[cfg(unix)]
        fn claude(&self) -> PathBuf {
            let real = self.script("share/versions/9.9.9", "9.9.9");
            let link = self.root.join("bin-claude");
            symlink(real, &link).unwrap();
            link
        }

        /// Claude Code as the native installer leaves it on Windows, with a
        /// runnable stand-in (see [`Fixture::runnable_claude`]).
        #[cfg(windows)]
        fn claude(&self) -> PathBuf {
            self.runnable_claude(r"bin\claude.exe")
        }

        /// A file that starts with the Mach-O magic, followed by `rest`. Never run:
        /// it is for discovery, which only reads its first bytes.
        #[cfg(unix)]
        fn native(&self, relative: &str, rest: &[u8]) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut bytes = b"\xcf\xfa\xed\xfe".to_vec();
            bytes.extend_from_slice(rest);
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        /// A copy of the real native program built by [`built_claude`], as the
        /// Claude install of this machine would be.
        #[cfg(unix)]
        fn runnable_claude(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::copy(built_claude(), &path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        /// A minimal PE file (see [`fake_pe`]) followed by `rest`. Never run:
        /// it is for discovery and the signature check, which only read it.
        #[cfg(windows)]
        fn native(&self, relative: &str, rest: &[u8]) -> PathBuf {
            let path = self.root.join(relative);
            fake_pe(&path);
            let mut bytes = fs::read(&path).unwrap();
            bytes.extend_from_slice(rest);
            fs::write(&path, bytes).unwrap();
            path
        }

        /// A real, runnable Windows program as Claude Code: a copy of
        /// Windows' `tar.exe`, which says its version (`3.8.9`) with an
        /// empty environment.
        #[cfg(windows)]
        fn runnable_claude(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::copy(system32().join("tar.exe"), &path).unwrap();
            path
        }

        #[cfg(unix)]
        fn codex(&self) -> PathBuf {
            let release = "home/.codex/packages/release";
            let entry = self.script(&format!("{release}/bin/codex"), "codex-cli 1.2.3");
            self.script(&format!("{release}/codex-path/rg"), "x");
            self.script(&format!("{release}/codex-resources/voice/data"), "x");
            let package = self.root.join(release).join("codex-package.json");
            fs::write(package, "{}").unwrap();
            // Must never be copied, wherever it sits.
            fs::write(self.root.join(release).join("auth.json"), "secret").unwrap();
            fs::write(self.root.join("home/.codex/auth.json"), "secret").unwrap();
            entry
        }
    }

    /// Every file under `dir` with its digest, to see that nothing changed.
    #[cfg(unix)]
    fn tree_digest(dir: &Path) -> Vec<(PathBuf, String)> {
        let mut found = Vec::new();
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(tree_digest(&path));
            } else {
                found.push((path.clone(), sha256_file(&path).unwrap_or_default()));
            }
        }
        found.sort();
        found
    }

    fn unlock(path: &Path) {
        unlock_dirs(path);
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            unlock(&self.root);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn claude_is_copied_under_its_own_name_and_version() {
        let fixture = Fixture::new("claude");
        let installed = fixture
            .store()
            .provision(Tool::Claude, &fixture.claude(), &FakeSignature::default())
            .unwrap();
        assert_eq!(installed.version, "9.9.9");
        assert!(installed.entry.ends_with("cli/claude/9.9.9/claude"));
        assert!(installed.entry.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn the_signature_is_checked_against_the_tools_team() {
        let fixture = Fixture::new("team");
        let signature = FakeSignature::default();
        fixture
            .store()
            .provision(Tool::Claude, &fixture.claude(), &signature)
            .unwrap();
        let seen = signature.seen.borrow();
        assert_eq!(seen[0].1.as_deref(), Some("Q6L2SF6YDW"));
    }

    #[cfg(unix)]
    #[test]
    fn a_rejected_signature_installs_nothing() {
        let fixture = Fixture::new("reject");
        let signature = FakeSignature {
            reject: true,
            ..FakeSignature::default()
        };
        let result = fixture
            .store()
            .provision(Tool::Claude, &fixture.claude(), &signature);
        assert!(matches!(result, Err(StoreError::Signature(_))));
        assert!(!fixture.root.join("data/cli/claude").exists());
        let leftovers: Vec<_> = fs::read_dir(fixture.root.join("data/cli"))
            .unwrap()
            .collect();
        assert!(leftovers.is_empty(), "staging must be removed");
    }

    /// Answers `check` from a script; once the script is spent every check passes.
    struct Scripted {
        answers: Vec<Failure>,
        calls: std::cell::Cell<u32>,
    }

    impl Signature for Scripted {
        fn verify(&self, file: &Path, team: Option<&str>) -> Result<(), String> {
            self.check(file, team)
                .map_err(|failure| failure.to_string())
        }

        fn check(&self, _file: &Path, _team: Option<&str>) -> Result<(), Failure> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            match self.answers.get(call as usize) {
                Some(failure) => Err(failure.clone()),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn a_refused_copy_is_copied_once_more_and_a_second_refusal_is_final() {
        let fixture = Fixture::new("recopy");
        let entry = fixture.claude();
        let refused = Failure::Invalid("invalid signature: x".to_owned());
        let once = Scripted {
            answers: vec![refused.clone()],
            calls: std::cell::Cell::new(0),
        };
        let installed = fixture.store().provision(Tool::Claude, &entry, &once);
        assert!(installed.is_ok(), "a fresh copy that passes is installed");
        assert_eq!(once.calls.get(), 2);

        // The fresh copy is refused as well: final, after the one re-copy.
        let always = Scripted {
            answers: vec![refused; 10],
            calls: std::cell::Cell::new(0),
        };
        let result = fixture.store().provision(Tool::Claude, &entry, &always);
        assert!(
            matches!(result, Err(StoreError::Signature(_))),
            "{result:?}"
        );
        assert_eq!(always.calls.get(), 2, "one re-copy, then final");
    }

    #[test]
    fn a_check_that_never_finishes_is_not_called_a_bad_signature() {
        let fixture = Fixture::new("unchecked");
        let signature = Scripted {
            answers: vec![Failure::Incomplete("codesign did not finish".to_owned()); 10],
            calls: std::cell::Cell::new(0),
        };
        let result = fixture
            .store()
            .provision(Tool::Claude, &fixture.claude(), &signature);
        match result {
            Err(StoreError::Unchecked(reason)) => {
                assert!(reason.contains("did not finish"), "{reason}");
            }
            other => panic!("expected an unchecked copy, got {other:?}"),
        }
        assert_eq!(signature.calls.get(), CHECK_ATTEMPTS);
        assert!(!fixture.root.join("data/cli/claude").exists());
    }

    #[cfg(unix)]
    const THIN_MACHO: &[u8] = &[0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0];

    /// Mach-O files with no execute bit and misleading names, plus a text file.
    #[cfg(unix)]
    fn add_bundled_files(fixture: &Fixture) {
        let release = fixture.root.join("home/.codex/packages/release");
        let voice = release.join("codex-resources/voice");
        fs::write(voice.join("notes.txt"), THIN_MACHO).unwrap();
        fs::write(voice.join("libhost.dylib"), THIN_MACHO).unwrap();
        fs::write(voice.join("readme.md"), "plain text").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn every_mach_o_in_the_copy_is_checked_against_the_team() {
        let fixture = Fixture::new("macho");
        let entry = fixture.codex();
        add_bundled_files(&fixture);
        let signature = FakeSignature::default();
        fixture
            .store()
            .provision(Tool::Codex, &entry, &signature)
            .unwrap();
        let seen = signature.seen.borrow();
        let names: Vec<_> = seen
            .iter()
            .map(|(path, _)| path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["codex", "libhost.dylib", "notes.txt"]);
        assert!(
            seen.iter()
                .all(|(_, team)| team.as_deref() == Some("2DC432GLL2"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn one_bad_bundled_library_refuses_the_whole_copy() {
        let fixture = Fixture::new("badlib");
        let entry = fixture.codex();
        add_bundled_files(&fixture);
        let signature = FakeSignature {
            reject_name: Some("libhost.dylib"),
            ..FakeSignature::default()
        };
        let result = fixture.store().provision(Tool::Codex, &entry, &signature);
        assert!(matches!(result, Err(StoreError::Signature(_))));
        assert!(!fixture.root.join("data/cli/codex").exists());
    }

    /// An npm global install: the package, and the platform package inside it.
    #[cfg(unix)]
    fn npm_codex(fx: &Fixture, nested: bool) -> PathBuf {
        let (triple, platform) = codex_platform();
        let pkg = "lib/node_modules/@openai/codex";
        let entry = fx.root.join(pkg).join("bin/codex.js");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, "#!/usr/bin/env node\n").unwrap();
        fs::write(
            fx.root.join(pkg).join("package.json"),
            r#"{"name": "@openai/codex", "bin": {"codex": "bin/codex.js"}}"#,
        )
        .unwrap();
        let platform_dir = if nested {
            format!("{pkg}/node_modules/@openai/{platform}")
        } else {
            format!("lib/node_modules/@openai/{platform}")
        };
        let release = format!("{platform_dir}/vendor/{triple}");
        fx.script(&format!("{release}/bin/codex"), "codex-cli 1.2.3");
        fx.script(&format!("{release}/codex-path/rg"), "x");
        fx.script(&format!("{release}/codex-resources/voice/data"), "x");
        fs::write(fx.root.join(&release).join("codex-package.json"), "{}").unwrap();
        fs::write(fx.root.join(&platform_dir).join("package.json"), "{}").unwrap();
        // The bin on PATH is a symlink to the .js, as npm and Homebrew make it.
        let link = fx.root.join("bin-codex");
        symlink(&entry, &link).unwrap();
        link
    }

    #[cfg(unix)]
    #[test]
    fn codex_from_an_npm_global_install_is_provisioned_from_its_platform_package() {
        for nested in [true, false] {
            let fx = Fixture::new(if nested { "npm-nested" } else { "npm-hoisted" });
            let entry = npm_codex(&fx, nested);
            let store = fx.store();
            let installed = store
                .provision(Tool::Codex, &entry, &FakeSignature::default())
                .unwrap();
            assert_eq!(installed.version, "1.2.3");
            assert!(installed.entry.ends_with("bin/codex"));
            let copy = installed
                .entry
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned();
            assert!(copy.join("codex-package.json").is_file());
            assert!(copy.join("codex-path/rg").is_file());
            // The JavaScript launcher is not part of the copy.
            assert!(!copy.join("bin/codex.js").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_npm_codex_without_its_platform_package_is_refused_with_a_clear_layout_error() {
        let fx = Fixture::new("npm-missing");
        let entry = npm_codex(&fx, true);
        let (_, platform) = codex_platform();
        fs::remove_dir_all(
            fx.root
                .join("lib/node_modules/@openai/codex/node_modules/@openai")
                .join(platform),
        )
        .unwrap();
        let err = fx
            .store()
            .provision(Tool::Codex, &entry, &FakeSignature::default())
            .unwrap_err();
        assert!(matches!(err, StoreError::BadSource(_)), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_package_json_of_some_other_package_is_not_taken_for_codex() {
        let fx = Fixture::new("npm-other");
        let entry = npm_codex(&fx, true);
        fs::write(
            fx.root.join("lib/node_modules/@openai/codex/package.json"),
            r#"{"name": "something-else"}"#,
        )
        .unwrap();
        assert!(matches!(
            fx.store()
                .provision(Tool::Codex, &entry, &FakeSignature::default()),
            Err(StoreError::BadSource(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_vendor_link_that_leaves_its_package_is_not_followed() {
        let fx = Fixture::new("npm-escape");
        let entry = npm_codex(&fx, true);
        let (triple, platform) = codex_platform();
        let vendor = fx
            .root
            .join("lib/node_modules/@openai/codex/node_modules/@openai")
            .join(platform)
            .join("vendor");
        // A real release folder elsewhere, linked in as vendor/<triple>.
        let outside = fx.root.join("elsewhere");
        fs::create_dir_all(outside.join("bin")).unwrap();
        fx.script("elsewhere/bin/codex", "codex-cli 6.6.6");
        fs::write(outside.join("codex-package.json"), "{}").unwrap();
        fs::remove_dir_all(vendor.join(triple)).unwrap();
        symlink(&outside, vendor.join(triple)).unwrap();
        assert!(matches!(
            fx.store()
                .provision(Tool::Codex, &entry, &FakeSignature::default()),
            Err(StoreError::BadSource(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_platform_package_linked_in_as_a_whole_is_accepted() {
        // pnpm: the platform package is a link to its own folder elsewhere.
        let fx = Fixture::new("npm-pnpm");
        let entry = npm_codex(&fx, true);
        let (_, platform) = codex_platform();
        let nested = fx
            .root
            .join("lib/node_modules/@openai/codex/node_modules/@openai")
            .join(platform);
        let store_dir = fx.root.join("pnpm-store").join(platform);
        fs::create_dir_all(store_dir.parent().unwrap()).unwrap();
        fs::rename(&nested, &store_dir).unwrap();
        symlink(&store_dir, &nested).unwrap();
        let installed = fx
            .store()
            .provision(Tool::Codex, &entry, &FakeSignature::default())
            .unwrap();
        assert_eq!(installed.version, "1.2.3");
    }

    #[cfg(unix)]
    #[test]
    fn a_package_that_only_depends_on_codex_or_has_a_broken_manifest_is_refused() {
        for (name, manifest) in [
            (
                "npm-dep",
                r#"{"name": "wrapper", "dependencies": {"@openai/codex": "1"}}"#,
            ),
            ("npm-broken", r#"{"name": "@openai/codex""#),
        ] {
            let fx = Fixture::new(name);
            let entry = npm_codex(&fx, true);
            fs::write(
                fx.root.join("lib/node_modules/@openai/codex/package.json"),
                manifest,
            )
            .unwrap();
            assert!(matches!(
                fx.store()
                    .provision(Tool::Codex, &entry, &FakeSignature::default()),
                Err(StoreError::BadSource(_))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn discovery_skips_what_cannot_be_copied_and_takes_the_next_that_can() {
        let fx = Fixture::new("discover");
        let home = fx.root.join("home");
        // First on the PATH: a pnpm-style shim script. Second: a bare binary.
        // Third: the npm global install, which works.
        let shims = fx.root.join("shims");
        fx.script("shims/codex", "shim");
        let bare = fx.root.join("bare");
        fs::create_dir_all(&bare).unwrap();
        fs::write(bare.join("codex"), b"\xcf\xfa\xed\xfe native").unwrap();
        let npm = npm_codex(&fx, true);
        let npm_bin = fx.root.join("npm-bin");
        fs::create_dir_all(&npm_bin).unwrap();
        symlink(fs::canonicalize(&npm).unwrap(), npm_bin.join("codex")).unwrap();
        let path = [shims.clone(), bare.clone(), npm_bin.clone()];
        assert_eq!(
            Store::find_installed_in(Tool::Codex, &home, &path),
            Some(npm_bin.join("codex"))
        );
        let bad = Store::unusable_installs_in(Tool::Codex, &home, &path);
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert_eq!(bad[0].0, shims.join("codex"));
        assert!(bad[0].1.contains("launcher script"), "{}", bad[0].1);
        assert_eq!(bad[1].0, bare.join("codex"));
        assert!(bad[1].1.contains("bare program"), "{}", bad[1].1);
    }

    #[cfg(unix)]
    #[test]
    fn nothing_usable_is_none_and_says_what_was_found_and_why() {
        let fx = Fixture::new("discover-none");
        let home = fx.root.join("home");
        let entry = npm_codex(&fx, true);
        let (_, platform) = codex_platform();
        fs::remove_dir_all(
            fx.root
                .join("lib/node_modules/@openai/codex/node_modules/@openai")
                .join(platform),
        )
        .unwrap();
        let bin = fx.root.join("npm-bin");
        fs::create_dir_all(&bin).unwrap();
        symlink(fs::canonicalize(&entry).unwrap(), bin.join("codex")).unwrap();
        assert_eq!(
            Store::find_installed_in(Tool::Codex, &home, std::slice::from_ref(&bin)),
            None
        );
        let bad = Store::unusable_installs_in(Tool::Codex, &home, std::slice::from_ref(&bin));
        assert_eq!(bad.len(), 1);
        assert!(bad[0].1.contains("platform package"), "{}", bad[0].1);
        // Provisioning it says the same.
        let err = fx
            .store()
            .provision(Tool::Codex, &bin.join("codex"), &FakeSignature::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("platform package"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_standalone_release_and_a_cask_style_link_are_found_on_the_path() {
        let fx = Fixture::new("discover-release");
        let home = fx.root.join("home");
        let entry = fx.codex();
        let bin = fx.root.join("brew-bin");
        fs::create_dir_all(&bin).unwrap();
        symlink(&entry, bin.join("codex")).unwrap();
        assert_eq!(
            Store::find_installed_in(Tool::Codex, &home, std::slice::from_ref(&bin)),
            Some(bin.join("codex"))
        );
        assert!(Store::unusable_installs_in(Tool::Codex, &home, &[bin]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn an_npm_claude_is_not_taken_but_a_native_one_further_on_is() {
        let fx = Fixture::new("discover-claude");
        let home = fx.root.join("home");
        fx.script("lib/claude-code/cli.js", "js");
        fs::create_dir_all(fx.root.join("npm-bin")).unwrap();
        symlink(
            fx.root.join("lib/claude-code/cli.js"),
            fx.root.join("npm-bin/claude"),
        )
        .unwrap();
        fx.native("native/claude", b"");
        let path = [fx.root.join("npm-bin"), fx.root.join("native")];
        assert_eq!(
            Store::find_installed_in(Tool::Claude, &home, &path),
            Some(fx.root.join("native/claude"))
        );
        let bad = Store::unusable_installs_in(Tool::Claude, &home, &path);
        assert_eq!(bad.len(), 1);
        assert!(bad[0].1.contains("JavaScript"));
    }

    #[cfg(unix)]
    #[test]
    fn an_incomplete_or_linked_release_is_rejected_and_the_next_candidate_is_used() {
        let fx = Fixture::new("discover-incomplete");
        let home = fx.root.join("home");
        // First: only the package file and the program.
        let first = fx.root.join("first-release");
        fx.script("first-release/bin/codex", "codex-cli 1.0.0");
        fs::write(first.join("codex-package.json"), "{}").unwrap();
        let first_bin = fx.root.join("first-bin");
        fs::create_dir_all(&first_bin).unwrap();
        symlink(first.join("bin/codex"), first_bin.join("codex")).unwrap();
        // Second: complete.
        let entry = fx.codex();
        let second_bin = fx.root.join("second-bin");
        fs::create_dir_all(&second_bin).unwrap();
        symlink(&entry, second_bin.join("codex")).unwrap();
        let path = [first_bin.clone(), second_bin.clone()];
        assert_eq!(
            Store::find_installed_in(Tool::Codex, &home, &path),
            Some(second_bin.join("codex"))
        );
        let bad = Store::unusable_installs_in(Tool::Codex, &home, &path);
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert!(bad[0].1.contains("incomplete"), "{}", bad[0].1);
        // What discovery accepts, provisioning copies.
        fx.store()
            .provision(
                Tool::Codex,
                &Store::find_installed_in(Tool::Codex, &home, &path).unwrap(),
                &FakeSignature::default(),
            )
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_release_with_a_missing_entry_or_a_linked_support_tree_is_not_usable() {
        for (name, damage) in [
            ("codex-path", "remove"),
            ("codex-resources", "remove"),
            ("codex-path", "link"),
            ("codex-resources", "link"),
        ] {
            let fx = Fixture::new(&format!("discover-{name}-{damage}"));
            let home = fx.root.join("home");
            let entry = fx.codex();
            let release = fx.root.join("home/.codex/packages/release");
            let tree = release.join(name);
            fs::remove_dir_all(&tree).unwrap();
            if damage == "link" {
                let elsewhere = fx.root.join("elsewhere");
                fs::create_dir_all(&elsewhere).unwrap();
                fs::write(elsewhere.join("data"), "x").unwrap();
                symlink(&elsewhere, &tree).unwrap();
            }
            let bin = fx.root.join("bin");
            fs::create_dir_all(&bin).unwrap();
            symlink(&entry, bin.join("codex")).unwrap();
            let path = [bin.clone()];
            assert_eq!(
                Store::find_installed_in(Tool::Codex, &home, &path),
                None,
                "{name} {damage}"
            );
            assert_eq!(
                Store::unusable_installs_in(Tool::Codex, &home, &path).len(),
                1
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_fixed_folders_still_work_without_any_path() {
        let fx = Fixture::new("discover-fixed");
        let home = fx.root.join("home");
        fx.native("home/.local/bin/claude", b"");
        assert_eq!(
            Store::find_installed_in(Tool::Claude, &home, &[]),
            Some(home.join(".local/bin/claude"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_wrapper_named_claude_first_on_the_path_is_skipped_for_the_native_one() {
        let fx = Fixture::new("claude-shim");
        let home = fx.root.join("home");
        // A terminal's per-session shim, first on the PATH.
        fx.script("shim/claude", "wrapper");
        fx.runnable_claude("native/claude");
        let path = [fx.root.join("shim"), fx.root.join("native")];
        assert_eq!(
            Store::find_installed_in(Tool::Claude, &home, &path),
            Some(fx.root.join("native/claude"))
        );
        let bad = Store::unusable_installs_in(Tool::Claude, &home, &path);
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert_eq!(bad[0].0, fx.root.join("shim/claude"));
        assert!(bad[0].1.contains("script or wrapper"), "{}", bad[0].1);
        let provisioned = fx
            .store()
            .provision_found_in(Tool::Claude, &home, &path, &Verdicts)
            .unwrap();
        assert_eq!(provisioned.found, fx.root.join("native/claude"));
        assert_eq!(provisioned.skipped.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn only_a_wrapper_is_not_found_and_says_what_it_is() {
        let fx = Fixture::new("claude-only-shim");
        let home = fx.root.join("home");
        fx.script("shim/claude", "wrapper");
        let path = [fx.root.join("shim")];
        assert_eq!(Store::find_installed_in(Tool::Claude, &home, &path), None);
        let bad = Store::unusable_installs_in(Tool::Claude, &home, &path);
        assert_eq!(bad.len(), 1);
        assert!(bad[0].1.contains("script or wrapper"), "{}", bad[0].1);
        let err = fx
            .store()
            .provision_found_in(Tool::Claude, &home, &path, &Verdicts)
            .unwrap_err();
        let StoreError::NoUsableCopy { refused, tried, .. } = &err else {
            panic!("{err:?}");
        };
        assert!(!refused, "a wrapper is not a refused signature");
        assert_eq!(tried.len(), 1);
        assert!(err.to_string().contains("script or wrapper"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_chain_to_a_native_claude_is_found_and_a_link_to_a_script_is_not() {
        let fx = Fixture::new("claude-links");
        let home = fx.root.join("home");
        let native = fx.native("native/claude-2.1.0", b"");
        fs::create_dir_all(fx.root.join("chain")).unwrap();
        symlink(fx.root.join("chain/second"), fx.root.join("chain/claude")).unwrap();
        symlink(&native, fx.root.join("chain/second")).unwrap();
        let script = fx.script("scripts/wrapper.sh", "wrapper");
        fs::create_dir_all(fx.root.join("linked")).unwrap();
        symlink(&script, fx.root.join("linked/claude")).unwrap();
        let chain = [fx.root.join("chain")];
        assert_eq!(
            Store::find_installed_in(Tool::Claude, &home, &chain),
            Some(fx.root.join("chain/claude"))
        );
        let linked = [fx.root.join("linked")];
        assert_eq!(Store::find_installed_in(Tool::Claude, &home, &linked), None);
        let bad = Store::unusable_installs_in(Tool::Claude, &home, &linked);
        assert_eq!(bad.len(), 1);
        assert!(bad[0].1.contains("script or wrapper"), "{}", bad[0].1);
    }

    #[cfg(unix)]
    #[test]
    fn a_definitely_refused_native_claude_is_skipped_for_the_next_one() {
        let fx = Fixture::new("claude-refused-first");
        let home = fx.root.join("home");
        fx.native("first/claude", b" REFUSE!");
        fx.runnable_claude("second/claude");
        let path = [fx.root.join("first"), fx.root.join("second")];
        let provisioned = fx
            .store()
            .provision_found_in(Tool::Claude, &home, &path, &Verdicts)
            .unwrap();
        assert_eq!(provisioned.found, fx.root.join("second/claude"));
        assert_eq!(provisioned.installed.version, "2.1.0");
        assert_eq!(provisioned.skipped.len(), 1, "{:?}", provisioned.skipped);
        assert_eq!(provisioned.skipped[0].0, fx.root.join("first/claude"));
        assert!(
            provisioned.skipped[0].1.contains("invalid signature"),
            "{:?}",
            provisioned.skipped
        );
    }

    #[cfg(unix)]
    #[test]
    fn when_every_native_claude_is_refused_the_error_names_each_one() {
        let fx = Fixture::new("claude-all-refused");
        let home = fx.root.join("home");
        fx.native("first/claude", b" REFUSE!");
        fx.native("second/claude", b" REFUSE!");
        let path = [fx.root.join("first"), fx.root.join("second")];
        let err = fx
            .store()
            .provision_found_in(Tool::Claude, &home, &path, &Verdicts)
            .unwrap_err();
        let StoreError::NoUsableCopy { tried, refused, .. } = &err else {
            panic!("{err:?}");
        };
        assert!(refused);
        assert_eq!(tried.len(), 2);
        let text = err.to_string();
        assert!(
            text.contains("first/claude") && text.contains("second/claude"),
            "{text}"
        );
        assert!(matches!(
            fx.store().newest(Tool::Claude),
            Err(StoreError::NoVersion(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_unfinished_check_stops_the_search_and_no_other_program_is_used() {
        let fx = Fixture::new("claude-unfinished");
        let home = fx.root.join("home");
        fx.native("first/claude", b" UNFINISHED");
        fx.runnable_claude("second/claude");
        let path = [fx.root.join("first"), fx.root.join("second")];
        let err = fx
            .store()
            .provision_found_in(Tool::Claude, &home, &path, &Verdicts)
            .unwrap_err();
        assert!(matches!(err, StoreError::Unchecked(_)), "{err:?}");
        assert!(matches!(
            fx.store().newest(Tool::Claude),
            Err(StoreError::NoVersion(_))
        ));
    }

    /// Windows: a `claude.exe` first on the `PATH` that is not a program (a
    /// wrapper given the program's name) is skipped for the real one, and
    /// npm's `claude.cmd` is a launcher, never copied.
    #[cfg(windows)]
    #[test]
    fn a_wrapper_or_shim_named_claude_first_on_the_path_is_skipped_on_windows() {
        let fx = Fixture::new("claude-shim-windows");
        let home = fx.root.join("home");
        let shim = fx.root.join("shim");
        fs::create_dir_all(&shim).unwrap();
        fs::write(shim.join("claude.exe"), "@echo off\r\nclaude %*\r\n").unwrap();
        fs::write(shim.join("claude.cmd"), "@ECHO off\r\nnode cli.js %*\r\n").unwrap();
        fx.runnable_claude(r"native\claude.exe");
        let path = [shim.clone(), fx.root.join("native")];
        assert_eq!(
            Store::find_installed_in(Tool::Claude, &home, &path),
            Some(fx.root.join(r"native\claude.exe"))
        );
        let bad = Store::unusable_installs_in(Tool::Claude, &home, &path);
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert_eq!(bad[0].0, shim.join("claude.exe"));
        assert!(bad[0].1.contains("script or wrapper"), "{}", bad[0].1);
        assert!(bad[1].1.contains("launcher script"), "{}", bad[1].1);
        let provisioned = fx
            .store()
            .provision_found_in(Tool::Claude, &home, &path, &Verdicts)
            .unwrap();
        assert_eq!(provisioned.found, fx.root.join(r"native\claude.exe"));
        assert_eq!(provisioned.skipped.len(), 2);
    }

    /// Windows: a refused program falls through to the next one; when every
    /// one is refused the error names each; an unfinished check stops the
    /// search.
    #[cfg(windows)]
    #[test]
    fn refused_and_unfinished_checks_of_native_claudes_on_windows() {
        let fx = Fixture::new("claude-refused-windows");
        let home = fx.root.join("home");
        fx.native(r"first\claude.exe", b" REFUSE!");
        fx.runnable_claude(r"second\claude.exe");
        let path = [fx.root.join("first"), fx.root.join("second")];
        let provisioned = fx
            .store()
            .provision_found_in(Tool::Claude, &home, &path, &Verdicts)
            .unwrap();
        assert_eq!(provisioned.found, fx.root.join(r"second\claude.exe"));
        assert_eq!(provisioned.skipped.len(), 1, "{:?}", provisioned.skipped);
        assert!(provisioned.skipped[0].1.contains("invalid signature"));

        let all = Fixture::new("claude-all-refused-windows");
        all.native(r"first\claude.exe", b" REFUSE!");
        all.native(r"second\claude.exe", b" REFUSE!");
        let path = [all.root.join("first"), all.root.join("second")];
        let err = all
            .store()
            .provision_found_in(Tool::Claude, &home, &path, &Verdicts)
            .unwrap_err();
        let StoreError::NoUsableCopy { tried, refused, .. } = &err else {
            panic!("{err:?}");
        };
        assert!(refused);
        assert_eq!(tried.len(), 2);
        let text = err.to_string();
        assert!(
            text.contains(r"first\claude.exe") && text.contains(r"second\claude.exe"),
            "{text}"
        );

        let unfinished = Fixture::new("claude-unfinished-windows");
        unfinished.native(r"first\claude.exe", b" UNFINISHED");
        unfinished.runnable_claude(r"second\claude.exe");
        let path = [
            unfinished.root.join("first"),
            unfinished.root.join("second"),
        ];
        let err = unfinished
            .store()
            .provision_found_in(Tool::Claude, &home, &path, &Verdicts)
            .unwrap_err();
        assert!(matches!(err, StoreError::Unchecked(_)), "{err:?}");
        assert!(matches!(
            unfinished.store().newest(Tool::Claude),
            Err(StoreError::NoVersion(_))
        ));
    }

    #[test]
    fn with_no_claude_on_the_path_the_error_says_none_was_found() {
        let fx = Fixture::new("claude-none");
        let home = fx.root.join("home");
        let err = fx
            .store()
            .provision_found_in(Tool::Claude, &home, &[], &Verdicts)
            .unwrap_err();
        assert!(err.to_string().contains("none was found"), "{err}");
    }

    #[test]
    fn the_candidates_named_are_capped_and_control_characters_are_left_out() {
        let tried: Vec<(PathBuf, String)> = (0..7)
            .map(|i| (PathBuf::from(format!("/bin/c{i}\u{7}")), "why".to_owned()))
            .collect();
        let text = describe_list(&tried, "; ");
        assert_eq!(text.matches(": why").count(), MAX_LISTED, "{text}");
        assert!(text.ends_with("; and 2 more"), "{text}");
        assert!(!text.contains('\u{7}'), "{text:?}");
    }

    #[cfg(unix)]
    #[test]
    fn codex_copies_the_listed_entries_and_never_credentials() {
        let fixture = Fixture::new("codex");
        let installed = fixture
            .store()
            .provision(Tool::Codex, &fixture.codex(), &FakeSignature::default())
            .unwrap();
        assert_eq!(installed.version, "1.2.3");
        let dir = fixture.root.join("data/cli/codex/1.2.3");
        assert!(dir.join("bin/codex").is_file());
        assert!(dir.join("codex-path/rg").is_file());
        assert!(!dir.join("auth.json").exists());
        let mut files = Vec::new();
        walk(&dir, Path::new(""), &mut files).unwrap();
        assert!(
            files.iter().all(|file| !file.ends_with("auth.json")),
            "{files:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_copy_is_read_only() {
        let fixture = Fixture::new("readonly");
        let installed = fixture
            .store()
            .provision(Tool::Claude, &fixture.claude(), &FakeSignature::default())
            .unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&installed.entry), 0o555);
        assert_eq!(mode(installed.entry.parent().unwrap()), 0o555);
    }

    #[cfg(unix)]
    #[test]
    fn the_source_is_left_unchanged() {
        let fixture = Fixture::new("source");
        let link = fixture.claude();
        let real = fs::canonicalize(&link).unwrap();
        let before = sha256_file(&real).unwrap();
        let mode = fs::metadata(&real).unwrap().permissions().mode();
        fixture
            .store()
            .provision(Tool::Claude, &link, &FakeSignature::default())
            .unwrap();
        assert_eq!(sha256_file(&real).unwrap(), before);
        assert_eq!(fs::metadata(&real).unwrap().permissions().mode(), mode);
    }

    #[cfg(unix)]
    #[test]
    fn provisioning_twice_reuses_the_verified_copy() {
        let fixture = Fixture::new("twice");
        let store = fixture.store();
        let link = fixture.claude();
        let first = store
            .provision(Tool::Claude, &link, &FakeSignature::default())
            .unwrap();
        let second = store
            .provision(Tool::Claude, &link, &FakeSignature::default())
            .unwrap();
        assert_eq!(first, second);
    }

    #[cfg(unix)]
    fn tamper(path: &Path, change: impl FnOnce(&Path)) {
        let parent = path.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
        change(path);
    }

    #[cfg(unix)]
    #[test]
    fn a_modified_binary_fails_verification() {
        let fixture = Fixture::new("modified");
        let store = fixture.store();
        let installed = store
            .provision(Tool::Claude, &fixture.claude(), &FakeSignature::default())
            .unwrap();
        tamper(&installed.entry, |path| {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(path, "#!/bin/sh\necho evil\n").unwrap();
        });
        assert!(matches!(
            store.verify(Tool::Claude, "9.9.9"),
            Err(StoreError::Tampered(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_added_file_fails_verification() {
        let fixture = Fixture::new("added");
        let store = fixture.store();
        let installed = store
            .provision(Tool::Claude, &fixture.claude(), &FakeSignature::default())
            .unwrap();
        tamper(&installed.entry, |path| {
            fs::write(path.with_file_name("extra"), "x").unwrap();
        });
        assert!(matches!(
            store.verify(Tool::Claude, "9.9.9"),
            Err(StoreError::Tampered(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_removed_file_fails_verification() {
        let fixture = Fixture::new("removed");
        let store = fixture.store();
        let installed = store
            .provision(Tool::Claude, &fixture.claude(), &FakeSignature::default())
            .unwrap();
        tamper(&installed.entry, |path| fs::remove_file(path).unwrap());
        assert!(matches!(
            store.verify(Tool::Claude, "9.9.9"),
            Err(StoreError::Tampered(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_replaced_by_a_symlink_fails_verification() {
        let fixture = Fixture::new("symlinked");
        let store = fixture.store();
        let installed = store
            .provision(Tool::Claude, &fixture.claude(), &FakeSignature::default())
            .unwrap();
        tamper(&installed.entry, |path| {
            fs::remove_file(path).unwrap();
            symlink("/bin/sh", path).unwrap();
        });
        assert!(store.verify(Tool::Claude, "9.9.9").is_err());
    }

    /// What is left in `data/cli` beside the tools' own folders.
    #[cfg(unix)]
    fn leftovers(fixture: &Fixture) -> Vec<String> {
        fs::read_dir(fixture.root.join("data/cli"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_damaged_copy_is_replaced_by_the_next_provision() {
        let fixture = Fixture::new("repair");
        let store = fixture.store();
        let link = fixture.claude();
        let good = store
            .provision(Tool::Claude, &link, &FakeSignature::default())
            .unwrap();
        let digest = sha256_file(&good.entry).unwrap();
        type Damage = (&'static str, fn(&Path));
        let damages: [Damage; 4] = [
            ("changed", |path| {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
                fs::write(path, "#!/bin/sh\necho evil\n").unwrap();
            }),
            ("added", |path| {
                fs::write(path.with_file_name("extra"), "x").unwrap();
            }),
            ("removed", |path| fs::remove_file(path).unwrap()),
            ("no checksums", |path| {
                fs::remove_file(path.with_file_name(SUMS_FILE)).unwrap();
            }),
        ];
        for (what, damage) in damages {
            tamper(&good.entry, damage);
            assert!(store.verify(Tool::Claude, "9.9.9").is_err(), "{what}");
            let signature = FakeSignature::default();
            let again = store.provision(Tool::Claude, &link, &signature).unwrap();
            assert_eq!(again, good, "{what}");
            assert!(
                !signature.seen.borrow().is_empty(),
                "{what}: the new copy went through the signature check"
            );
            assert_eq!(sha256_file(&again.entry).unwrap(), digest, "{what}");
            assert!(store.verify(Tool::Claude, "9.9.9").is_ok(), "{what}");
            assert!(!again.entry.with_file_name("extra").exists(), "{what}");
            let mode = fs::metadata(again.entry.parent().unwrap()).unwrap();
            assert_eq!(mode.permissions().mode() & 0o777, 0o555, "{what}");
            assert_eq!(leftovers(&fixture), Vec::<String>::new(), "{what}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_damaged_copy_is_not_replaced_by_one_that_fails_its_checks() {
        let fixture = Fixture::new("repair-refused");
        let store = fixture.store();
        let link = fixture.claude();
        let good = store
            .provision(Tool::Claude, &link, &FakeSignature::default())
            .unwrap();
        tamper(&good.entry, |path| {
            fs::write(path.with_file_name("extra"), "x").unwrap();
        });
        let rejecting = FakeSignature {
            reject: true,
            ..FakeSignature::default()
        };
        let result = store.provision(Tool::Claude, &link, &rejecting);
        assert!(matches!(result, Err(StoreError::Signature(_))));
        assert!(
            good.entry.with_file_name("extra").exists(),
            "the damaged copy was not touched"
        );
        assert!(store.verify(Tool::Claude, "9.9.9").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn staging_folders_of_dead_processes_are_swept_and_nothing_else_is() {
        let fixture = Fixture::new("sweep");
        let store = fixture.store();
        let cli = fixture.root.join("data/cli");
        // A process that has ended: its pid is free (a reuse only keeps the
        // folder, which is the safe side).
        let mut child = Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let stale = cli.join(format!(".staging-codex-{dead}"));
        // As a killed provision leaves it: with a read-only folder inside.
        fs::create_dir_all(stale.join("damaged/bin")).unwrap();
        fs::write(stale.join("damaged/bin/codex"), "x").unwrap();
        fs::set_permissions(stale.join("damaged/bin"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(stale.join("damaged"), fs::Permissions::from_mode(0o555)).unwrap();
        let keep = [
            // Init is always alive.
            ".staging-claude-1".to_owned(),
            format!(".staging-other-{dead}"),
            format!(".staging-claude-{dead}x"),
            format!("staging-claude-{dead}"),
            ".staging-claude-".to_owned(),
        ];
        for name in &keep {
            fs::create_dir_all(cli.join(name)).unwrap();
        }
        // A file of that name is not a staging folder.
        let file = cli.join(format!(".staging-claude-{dead}"));
        fs::write(&file, "x").unwrap();
        store
            .provision(Tool::Claude, &fixture.claude(), &FakeSignature::default())
            .unwrap();
        assert!(!stale.exists(), "the dead provision's folder is gone");
        for name in &keep {
            assert!(cli.join(name).is_dir(), "{name}");
        }
        assert!(file.is_file());
    }

    #[test]
    fn an_unprovisioned_version_is_reported_as_such() {
        let fixture = Fixture::new("missing");
        assert!(matches!(
            fixture.store().verify(Tool::Claude, "1.0.0"),
            Err(StoreError::NotProvisioned { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_binary_that_reports_no_version_is_refused() {
        let fixture = Fixture::new("noversion");
        let entry = fixture.script("share/versions/x", "not a version");
        let result = fixture
            .store()
            .provision(Tool::Claude, &entry, &FakeSignature::default());
        assert!(matches!(result, Err(StoreError::UnknownVersion("claude"))));
    }

    #[test]
    fn the_newest_version_is_chosen_numerically() {
        let fixture = Fixture::new("newest");
        let store = fixture.store();
        assert!(matches!(
            store.newest(Tool::Claude),
            Err(StoreError::NoVersion("claude"))
        ));
        let add = |names: &[&str]| {
            for name in names {
                fs::create_dir_all(fixture.root.join("data/cli/claude").join(name)).unwrap();
            }
        };
        add(&["2.1.9", "2.1.318", ".staging", "notes", "9", "9.9"]);
        assert_eq!(store.newest(Tool::Claude).unwrap(), "2.1.318");
        // A prerelease is older than its release and newer than the one before.
        add(&["2.2.0-alpha.3", "2.2.0-alpha.10", "2.2.0-alpha"]);
        assert_eq!(store.newest(Tool::Claude).unwrap(), "2.2.0-alpha.10");
        add(&["2.2.0-beta.1"]);
        assert_eq!(store.newest(Tool::Claude).unwrap(), "2.2.0-beta.1");
        add(&["2.2.0"]);
        assert_eq!(store.newest(Tool::Claude).unwrap(), "2.2.0");
        add(&["2.2.1-rc.1"]);
        assert_eq!(store.newest(Tool::Claude).unwrap(), "2.2.1-rc.1");
    }

    #[cfg(unix)]
    #[test]
    fn a_prerelease_is_provisioned_under_its_full_version() {
        let fixture = Fixture::new("prerelease");
        let store = fixture.store();
        let entry = fixture.script("share/versions/pre", "codex-cli 0.50.0-alpha.3");
        let installed = store
            .provision(Tool::Claude, &entry, &FakeSignature::default())
            .unwrap();
        assert_eq!(installed.version, "0.50.0-alpha.3");
        assert!(
            installed
                .entry
                .ends_with("cli/claude/0.50.0-alpha.3/claude")
        );
        assert_eq!(store.newest(Tool::Claude).unwrap(), "0.50.0-alpha.3");
        assert!(store.verify(Tool::Claude, "0.50.0-alpha.3").is_ok());
    }

    #[test]
    fn the_hook_helper_is_installed_verified_and_replaceable() {
        let fixture = Fixture::new("hook");
        let store = fixture.store();
        assert!(matches!(store.verify_hook(), Err(StoreError::HookMissing)));
        let first = fixture.script("build/one", "1.0.0");
        let installed = store.install_hook(&first).unwrap();
        assert_eq!(store.verify_hook().unwrap(), installed);
        assert!(installed.ends_with(Path::new("cli/bin").join(HOOK_NAME)));
        let second = fixture.script("build/two", "2.0.0");
        store.install_hook(&second).unwrap();
        assert_eq!(
            sha256_file(&installed).unwrap(),
            sha256_file(&second).unwrap()
        );
        assert_eq!(store.verify_hook().unwrap(), installed);
    }

    /// Windows: a new helper replaces `connecto-hook.exe` and `connecto.exe`
    /// while both are running (a terminal's `connecto`, a hook), which a plain
    /// rename cannot do there; the old files go once they stop running.
    #[cfg(windows)]
    #[test]
    fn a_running_helper_and_connecto_command_are_replaced() {
        let fixture = Fixture::new("hook-running");
        let store = fixture.store();
        // A real program as the "helper", so its copies can run.
        let system = std::env::var_os("SystemRoot").unwrap();
        let cmd = PathBuf::from(system).join("System32").join("cmd.exe");
        let hook = store.install_hook(&cmd).unwrap();
        let connecto = store.bin_dir().join(CONNECTO_LINK);
        let run = |program: &Path| {
            Command::new(program)
                .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut running = [run(&hook), run(&connecto)];
        std::thread::sleep(std::time::Duration::from_millis(300));

        let newer = fixture.script("build/two", "2.0.0");
        assert!(store.refresh_hook(&newer).unwrap());
        let want = sha256_file(&newer).unwrap();
        assert_eq!(sha256_file(&hook).unwrap(), want);
        assert_eq!(sha256_file(&connecto).unwrap(), want);
        assert_eq!(store.verify_hook().unwrap(), hook);
        // The same helper again changes nothing.
        assert!(!store.refresh_hook(&newer).unwrap());

        for child in &mut running {
            let _ = child.kill();
            child.wait().unwrap();
        }
        drop(running);
        let aside = || {
            fs::read_dir(store.bin_dir())
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".old-"))
                .count()
        };
        assert_eq!(aside(), 2);
        // The next refresh clears them, even when the helper is unchanged.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while aside() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!store.refresh_hook(&newer).unwrap());
        }
        assert_eq!(aside(), 0);
    }

    /// AC8: a changed helper reaches the store, the stored tools stay as they
    /// are, and `newest` does not move.
    #[cfg(unix)]
    #[test]
    fn refreshing_the_helper_leaves_the_tools_alone_and_links_connecto() {
        let fixture = Fixture::new("refresh");
        let store = fixture.store();
        let signature = FakeSignature::default();
        store
            .provision(
                Tool::Claude,
                &fixture.script("source/claude", "2.1.318"),
                &signature,
            )
            .unwrap();
        let tools_before = tree_digest(&fixture.root.join("data/cli/claude"));
        let newest = store.newest(Tool::Claude).unwrap();

        // No stored helper yet: the refresh installs it.
        let one = fixture.script("build/one", "1.0.0");
        assert!(store.refresh_hook(&one).unwrap());
        let link = fixture.root.join("data/cli/bin/connecto");
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("connecto-hook"));
        // The same helper again does nothing.
        assert!(!store.refresh_hook(&one).unwrap());
        // A changed helper replaces it, and the link still points at it.
        let two = fixture.script("build/two", "2.0.0");
        assert!(store.refresh_hook(&two).unwrap());
        assert_eq!(
            sha256_file(&store.verify_hook().unwrap()).unwrap(),
            sha256_file(&two).unwrap()
        );
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("connecto-hook"));
        // No helper beside the app: the stored one stays.
        assert!(
            !store
                .refresh_hook(Path::new("/nonexistent/connecto-hook"))
                .unwrap()
        );
        assert!(store.verify_hook().is_ok());

        assert_eq!(
            tree_digest(&fixture.root.join("data/cli/claude")),
            tools_before
        );
        assert_eq!(store.newest(Tool::Claude).unwrap(), newest);
    }

    #[test]
    fn a_changed_hook_helper_fails_verification() {
        let fixture = Fixture::new("hookbad");
        let store = fixture.store();
        let installed = store
            .install_hook(&fixture.script("build/one", "1.0.0"))
            .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&installed, "#!/bin/sh\necho evil\n").unwrap();
        assert!(matches!(store.verify_hook(), Err(StoreError::Tampered(_))));
    }

    #[test]
    fn versions_are_found_in_real_cli_output() {
        assert_eq!(
            parse_version("2.1.318 (Claude Code)\n"),
            Some("2.1.318".to_owned())
        );
        assert_eq!(
            parse_version("codex-cli 0.166.0\n"),
            Some("0.166.0".to_owned())
        );
        assert_eq!(parse_version("1.2 or 1.2.x"), None);
        assert_eq!(
            parse_version("codex-cli 0.50.0-alpha.3\n"),
            Some("0.50.0-alpha.3".to_owned())
        );
        assert_eq!(
            parse_version("1.2.3-rc.1+build.5 (Tool)"),
            Some("1.2.3-rc.1+build.5".to_owned())
        );
    }

    #[test]
    fn a_version_is_always_a_safe_folder_name() {
        for bad in [
            "1.2.3/../../x",
            "1.2.3-../x",
            "1.2.3-a..b",
            "1.2.3-",
            "1.2.3+",
            "1.2.3-a/b",
            "1.2.3.4",
            "1.2",
            "..",
            "1.2.3-\u{e9}",
            "+1.2.3",
            "-1.2.3",
            "1.-2.3",
            "99999999999999999999999.1.1",
        ] {
            assert_eq!(version_key(bad), None, "{bad}");
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        // Semver order: numbers before words, fewer parts first, build ignored.
        let ordered = [
            "1.0.0-1",
            "1.0.0-2",
            "1.0.0-10",
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1-a",
            "1.0.1",
            "1.1.0",
            "10.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(
                version_key(pair[0]).unwrap() < version_key(pair[1]).unwrap(),
                "{pair:?}"
            );
        }
        assert_eq!(version_key("1.0.0+a"), version_key("1.0.0+b"));
    }

    #[cfg(unix)]
    #[test]
    fn the_data_root_can_be_overridden() {
        let home = Path::new("/Users/u");
        assert_eq!(
            data_root(None, home),
            PathBuf::from("/Users/u/Library/Application Support/connecto")
        );
        assert_eq!(
            data_root(Some("/tmp/x".into()), home),
            PathBuf::from("/tmp/x")
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_data_root_is_under_appdata_and_can_be_overridden() {
        let home = Path::new(r"C:\Users\u");
        let root = data_root(None, home);
        assert!(root.ends_with("connecto"), "{}", root.display());
        assert!(
            !root.components().any(|part| part.as_os_str() == "Library"),
            "{}",
            root.display()
        );
        assert_eq!(
            data_root(Some(r"D:\x".into()), home),
            PathBuf::from(r"D:\x")
        );
    }

    /// A file that stands in for a Windows program: discovery and the copy
    /// only read it; nothing runs it.
    #[cfg(windows)]
    fn program(path: &Path) {
        fake_pe(path);
    }

    #[cfg(windows)]
    #[test]
    fn the_native_claude_is_found_in_the_fixed_folder_and_an_npm_shim_is_not_taken() {
        let fx = Fixture::new("discover-windows");
        let home = fx.root.join("home");
        program(&home.join(r".local\bin\claude.exe"));
        assert_eq!(
            Store::find_installed_in(Tool::Claude, &home, &[]),
            Some(home.join(r".local\bin\claude.exe"))
        );
        // First on the PATH: npm's `.cmd` shim, which cannot be copied.
        let npm = fx.root.join("npm");
        fs::create_dir_all(&npm).unwrap();
        fs::write(npm.join("claude.cmd"), "@ECHO off\r\nnode cli.js %*\r\n").unwrap();
        // Git Bash's script of the same name is not a Windows program.
        fs::write(npm.join("claude"), "#!/bin/sh\n").unwrap();
        let path = [npm.clone()];
        assert_eq!(
            Store::find_installed_in(Tool::Claude, &home, &path),
            Some(home.join(r".local\bin\claude.exe"))
        );
        let bad = Store::unusable_installs_in(Tool::Claude, &home, &path);
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert_eq!(bad[0].0, npm.join("claude.cmd"));
        assert!(bad[0].1.contains("launcher script"), "{}", bad[0].1);
    }

    #[cfg(windows)]
    #[test]
    fn a_codex_shim_is_reported_as_a_launcher() {
        let fx = Fixture::new("discover-windows-codex");
        let home = fx.root.join("home");
        let npm = home.join(r"AppData\Roaming\npm");
        fs::create_dir_all(&npm).unwrap();
        fs::write(npm.join("codex.cmd"), "@ECHO off\r\n").unwrap();
        assert_eq!(Store::find_installed_in(Tool::Codex, &home, &[]), None);
        let bad = Store::unusable_installs_in(Tool::Codex, &home, &[]);
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert!(bad[0].1.contains("launcher script"), "{}", bad[0].1);
    }

    /// The real checker (Authenticode) refuses a program that is not a
    /// signed program, and a validly signed one from another publisher (a
    /// copy of a Windows program, signed by Microsoft): nothing is stored.
    /// One PowerShell run for both.
    #[cfg(windows)]
    #[test]
    fn provisioning_with_the_real_check_refuses_what_anthropic_did_not_sign() {
        let fx = Fixture::new("windows-unsigned");
        program(&fx.root.join(r"fake\claude.exe"));
        let foreign = fx.root.join(r"foreign\claude.exe");
        fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        fs::copy(system32().join("where.exe"), &foreign).unwrap();
        // Both through the store's own list, checked in one run.
        let checks = [
            (
                fx.root.join(r"fake\claude.exe"),
                Some(Tool::Claude.team_id()),
            ),
            (foreign.clone(), Some(Tool::Claude.team_id())),
        ];
        let checks: Vec<(&Path, Option<&str>)> =
            checks.iter().map(|(p, t)| (p.as_path(), *t)).collect();
        let err = crate::Codesign.check_all(&checks).unwrap_err();
        assert!(matches!(err, Failure::Invalid(_)), "{err:?}");
        assert!(err.to_string().contains("claude.exe"), "{err}");
        // And through provisioning: refused, nothing stored.
        let err = fx
            .store()
            .provision(Tool::Claude, &foreign, &crate::Codesign)
            .unwrap_err();
        assert!(matches!(err, StoreError::Signature(_)), "{err}");
        assert!(
            err.to_string().contains("Microsoft Windows")
                && err.to_string().contains("Anthropic, PBC"),
            "{err}"
        );
        assert!(!fx.root.join(r"data\cli\claude").exists());
    }

    #[cfg(windows)]
    fn system32() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32")
    }

    /// A minimal PE file: not runnable, but a PE image by its headers.
    #[cfg(windows)]
    fn fake_pe(path: &Path) {
        let mut bytes = vec![0_u8; 0x48];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c] = 0x40;
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// A Codex release folder as Codex 0.167.2 ships it on Windows, with a
    /// real program as `bin\codex.exe` (Windows' `tar.exe`, which says its
    /// version with an empty environment) and stand-ins for the rest.
    #[cfg(windows)]
    fn windows_release(release: &Path) {
        fs::create_dir_all(release.join("bin")).unwrap();
        fs::copy(system32().join("tar.exe"), release.join(r"bin\codex.exe")).unwrap();
        fake_pe(&release.join(r"bin\codex-code-mode-host.exe"));
        fake_pe(&release.join(r"codex-path\rg.exe"));
        fake_pe(&release.join(r"codex-resources\codex-command-runner.exe"));
        fake_pe(&release.join(r"codex-resources\voice\bin\vcruntime140.dll"));
        fake_pe(&release.join(r"codex-resources\voice\bin\z.dll"));
        // Not PE: data, and a file that only starts like one.
        fs::write(release.join(r"codex-resources\voice\manifest.json"), "{}").unwrap();
        fs::write(release.join(r"codex-resources\voice\notes.txt"), "MZ text").unwrap();
        fs::write(
            release.join("codex-package.json"),
            r#"{"layoutVersion":1,"entrypoint":"bin/codex.exe"}"#,
        )
        .unwrap();
    }

    /// `npm install -g @openai/codex` in `npm`: the `.cmd` launcher, the
    /// package and its nested platform package. Returns the release folder.
    #[cfg(windows)]
    fn windows_npm_codex(npm: &Path) -> PathBuf {
        let (triple, platform) = codex_platform();
        fs::create_dir_all(npm).unwrap();
        fs::write(
            npm.join("codex.cmd"),
            "@ECHO off\r\nnode \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n",
        )
        .unwrap();
        fs::write(npm.join("codex"), "#!/bin/sh\n").unwrap();
        let package = npm.join(r"node_modules\@openai\codex");
        fs::create_dir_all(package.join("bin")).unwrap();
        fs::write(package.join(r"bin\codex.js"), "#!/usr/bin/env node\n").unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name": "@openai/codex", "bin": {"codex": "bin/codex.js"}}"#,
        )
        .unwrap();
        let release = package
            .join(r"node_modules\@openai")
            .join(platform)
            .join("vendor")
            .join(triple);
        windows_release(&release);
        release
    }

    /// The file names and teams a fake checker saw.
    #[cfg(windows)]
    fn seen_names(signature: &FakeSignature) -> Vec<(String, String)> {
        signature
            .seen
            .borrow()
            .iter()
            .map(|(path, team)| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (name, team.clone().unwrap_or_default())
            })
            .collect()
    }

    #[cfg(windows)]
    #[test]
    fn codex_from_npm_on_windows_is_found_by_its_cmd_launcher_and_copied_from_the_platform_package()
    {
        let fx = Fixture::new("windows-npm");
        let home = fx.root.join("home");
        let npm = home.join(r"AppData\Roaming\npm");
        let release = windows_npm_codex(&npm);
        // Must never be copied.
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(home.join(r".codex\auth.json"), "secret").unwrap();

        let found = Store::find_installed_in(Tool::Codex, &home, &[]);
        assert_eq!(found, Some(npm.join("codex.cmd")));
        assert!(Store::unusable_installs_in(Tool::Codex, &home, &[]).is_empty());
        let program = Store::program_of(Tool::Codex, &npm.join("codex.cmd")).unwrap();
        assert_eq!(
            program,
            fs::canonicalize(release.join(r"bin\codex.exe")).unwrap()
        );

        let signature = FakeSignature::default();
        let installed = fx
            .store()
            .provision(Tool::Codex, &npm.join("codex.cmd"), &signature)
            .unwrap();
        assert!(
            installed.entry.ends_with(r"bin\codex.exe"),
            "{}",
            installed.entry.display()
        );
        let copy = installed
            .entry
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        assert!(copy.join(r"codex-path\rg.exe").is_file());
        assert!(copy.join("codex-package.json").is_file());
        assert!(!copy.join(r"bin\codex.js").exists());
        let mut files = Vec::new();
        walk(&copy, Path::new(""), &mut files).unwrap();
        assert!(files.iter().all(|f| !f.ends_with("auth.json")), "{files:?}");

        // Every PE file is checked: the entry first, against OpenAI; the
        // Visual C++ runtime against Microsoft; ripgrep is the one file
        // accepted unsigned. Files that are not PE are not checked.
        let seen = seen_names(&signature);
        let openai = CODEX_SIGNER_FOR_TEST.to_owned();
        assert_eq!(seen[0], ("codex.exe".to_owned(), openai.clone()));
        let mut rest = seen[1..].to_vec();
        rest.sort();
        assert_eq!(
            rest,
            [
                ("codex-code-mode-host.exe".to_owned(), openai.clone()),
                ("codex-command-runner.exe".to_owned(), openai.clone()),
                (
                    "vcruntime140.dll".to_owned(),
                    "Microsoft Corporation".to_owned()
                ),
                ("z.dll".to_owned(), openai),
            ]
        );
    }

    #[cfg(windows)]
    const CODEX_SIGNER_FOR_TEST: &str = "OpenAI OpCo, LLC";

    #[cfg(windows)]
    #[test]
    fn one_bad_library_refuses_the_whole_codex_copy_on_windows() {
        for name in ["z.dll", "vcruntime140.dll", "codex-command-runner.exe"] {
            let fx = Fixture::new(&format!("windows-badlib-{name}"));
            let npm = fx.root.join("npm");
            windows_npm_codex(&npm);
            let signature = FakeSignature {
                reject_name: Some(name),
                ..FakeSignature::default()
            };
            let result = fx
                .store()
                .provision(Tool::Codex, &npm.join("codex.cmd"), &signature);
            assert!(matches!(result, Err(StoreError::Signature(_))), "{name}");
            assert!(!fx.root.join(r"data\cli\codex").exists(), "{name}");
        }
    }

    /// The unsigned ripgrep exception is that one path and nothing else, and
    /// the switch turns it off.
    #[cfg(windows)]
    #[test]
    fn only_codex_path_rg_exe_may_be_unsigned_and_only_while_the_switch_allows_it() {
        use crate::platform::{CODEX_UNSIGNED_RG, bundled_signer, bundled_signer_with};
        let openai = Some(CODEX_SIGNER_FOR_TEST);
        assert_eq!(
            bundled_signer_with(Tool::Codex, Path::new(r"codex-path\rg.exe"), true),
            None
        );
        assert_eq!(
            bundled_signer_with(Tool::Codex, Path::new(CODEX_UNSIGNED_RG), true),
            None
        );
        assert_eq!(
            bundled_signer_with(Tool::Codex, Path::new(r"codex-path\rg.exe"), false),
            openai
        );
        for other in [
            r"bin\rg.exe",
            r"rg.exe",
            r"codex-path\sub\rg.exe",
            r"codex-path\RG.EXE",
            r"codex-path\rg.dll",
            r"codex-resources\codex-path\rg.exe",
            r"codex-path\rg2.exe",
        ] {
            assert_eq!(
                bundled_signer_with(Tool::Codex, Path::new(other), true),
                openai,
                "{other}"
            );
        }
        // Claude has no exception, not even for a file of that name.
        assert_eq!(
            bundled_signer(Tool::Claude, Path::new(r"codex-path\rg.exe")),
            Some("Anthropic, PBC")
        );
        assert_eq!(
            bundled_signer(Tool::Claude, Path::new("vcruntime140.dll")),
            Some("Anthropic, PBC")
        );
        // The Microsoft runtime, wherever it is and in any case; nothing else.
        for runtime in [
            r"codex-resources\voice\bin\vcruntime140.dll",
            r"bin\VCRUNTIME140_1.DLL",
            "msvcp140.dll",
        ] {
            assert_eq!(
                bundled_signer(Tool::Codex, Path::new(runtime)),
                Some("Microsoft Corporation"),
                "{runtime}"
            );
        }
        assert_eq!(
            bundled_signer(Tool::Codex, Path::new("vcruntime141.dll")),
            openai
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_npm_cmd_launcher_without_a_usable_package_says_why() {
        // No package beside the launcher: a launcher script.
        let fx = Fixture::new("windows-npm-bad");
        let npm = fx.root.join("npm-empty");
        fs::create_dir_all(&npm).unwrap();
        fs::write(npm.join("codex.cmd"), "@ECHO off\r\n").unwrap();
        let bad = Store::unusable_installs_in(Tool::Codex, &fx.root, std::slice::from_ref(&npm));
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert!(bad[0].1.contains("launcher script"), "{}", bad[0].1);
        assert_eq!(Store::program_of(Tool::Codex, &npm.join("codex.cmd")), None);

        // The package without its platform package.
        let npm = fx.root.join("npm-no-platform");
        windows_npm_codex(&npm);
        let (_, platform) = codex_platform();
        fs::remove_dir_all(
            npm.join(r"node_modules\@openai\codex\node_modules\@openai")
                .join(platform),
        )
        .unwrap();
        let bad = Store::unusable_installs_in(Tool::Codex, &fx.root, std::slice::from_ref(&npm));
        assert!(bad[0].1.contains("platform package"), "{}", bad[0].1);
        let err = fx
            .store()
            .provision(
                Tool::Codex,
                &npm.join("codex.cmd"),
                &FakeSignature::default(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("platform package"), "{err}");

        // A package of another name is not taken for Codex.
        let npm = fx.root.join("npm-other");
        windows_npm_codex(&npm);
        fs::write(
            npm.join(r"node_modules\@openai\codex\package.json"),
            r#"{"name": "something-else"}"#,
        )
        .unwrap();
        assert_eq!(
            Store::find_installed_in(Tool::Codex, &fx.root, std::slice::from_ref(&npm)),
            None
        );
        // A `.cmd` named otherwise is never mapped to Codex's package.
        let npm = fx.root.join("npm-renamed");
        windows_npm_codex(&npm);
        fs::rename(npm.join("codex.cmd"), npm.join("other.cmd")).unwrap();
        assert_eq!(
            crate::platform::codex_npm_shim_target(&npm.join("other.cmd")),
            None
        );
        // Claude's `.cmd` stays a launcher, even beside a Codex package.
        fs::write(npm.join("claude.cmd"), "@ECHO off\r\n").unwrap();
        assert_eq!(
            Store::find_installed_in(Tool::Claude, &fx.root, std::slice::from_ref(&npm)),
            None
        );
    }

    /// The standalone installer: `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin`
    /// is a junction to `…\standalone\current\bin`, and `current` one to the
    /// release folder. Junctions are followed to the release; nothing else
    /// of `.codex` is copied.
    #[cfg(windows)]
    #[test]
    fn codex_from_the_standalone_installer_is_found_through_its_junctions() {
        let fx = Fixture::new("windows-standalone");
        let home = fx.root.join("home");
        let (triple, _) = codex_platform();
        let standalone = home.join(r".codex\packages\standalone");
        let release = standalone
            .join("releases")
            .join(format!("0.167.2-{triple}"));
        windows_release(&release);
        fs::write(home.join(r".codex\auth.json"), "secret").unwrap();
        fs::write(release.join("auth.json"), "secret").unwrap();
        let junction = |link: &Path, target: &Path| {
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            let status = Command::new("cmd.exe")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        };
        junction(&standalone.join("current"), &release);
        let bin = home.join(r"AppData\Local\Programs\OpenAI\Codex\bin");
        junction(&bin, &standalone.join(r"current\bin"));

        let found = Store::find_installed_in(Tool::Codex, &home, &[]);
        assert_eq!(found, Some(bin.join("codex.exe")));
        assert_eq!(
            Store::program_of(Tool::Codex, &bin.join("codex.exe")),
            Some(fs::canonicalize(release.join(r"bin\codex.exe")).unwrap())
        );
        let installed = fx
            .store()
            .provision(
                Tool::Codex,
                &bin.join("codex.exe"),
                &FakeSignature::default(),
            )
            .unwrap();
        let copy = installed
            .entry
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        let mut files = Vec::new();
        walk(&copy, Path::new(""), &mut files).unwrap();
        assert!(files.iter().all(|f| !f.ends_with("auth.json")), "{files:?}");
        assert!(copy.join(r"codex-path\rg.exe").is_file());
    }

    #[cfg(windows)]
    #[test]
    fn claude_is_copied_as_claude_exe_and_checked_against_anthropic_on_windows() {
        let fx = Fixture::new("windows-claude");
        let home = fx.root.join("home");
        // As the native installer leaves it: a real file, and the same
        // program under its version's name.
        let local = home.join(r".local\bin\claude.exe");
        fs::create_dir_all(local.parent().unwrap()).unwrap();
        fs::copy(system32().join("tar.exe"), &local).unwrap();
        let versions = home.join(r".local\share\claude\versions");
        fs::create_dir_all(&versions).unwrap();
        fs::copy(&local, versions.join("2.1.322")).unwrap();
        assert_eq!(
            Store::find_installed_in(Tool::Claude, &home, &[]),
            Some(local.clone())
        );
        assert_eq!(
            Store::program_of(Tool::Claude, &local),
            Some(fs::canonicalize(&local).unwrap())
        );
        let signature = FakeSignature::default();
        let installed = fx
            .store()
            .provision(Tool::Claude, &local, &signature)
            .unwrap();
        // tar.exe's version, read by running the copy.
        assert_eq!(installed.version, "3.8.9");
        assert!(installed.entry.ends_with(r"cli\claude\3.8.9\claude.exe"));
        assert_eq!(
            seen_names(&signature),
            [("claude.exe".to_owned(), "Anthropic, PBC".to_owned())]
        );
        assert!(fx.store().verify(Tool::Claude, &installed.version).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn pe_files_are_recognised_by_their_headers() {
        let fx = Fixture::new("windows-pe");
        let real = system32().join("tar.exe");
        let fake = fx.root.join("fake.bin");
        fake_pe(&fake);
        let check = |bytes: &[u8]| {
            let path = fx.root.join("probe");
            fs::write(&path, bytes).unwrap();
            crate::platform::is_pe(&path).unwrap()
        };
        assert!(crate::platform::is_pe(&real).unwrap());
        assert!(crate::platform::is_pe(&fake).unwrap());
        assert!(crate::platform::is_native_binary(&fake).unwrap());
        assert!(!check(b""));
        assert!(!check(b"MZ"));
        assert!(!check(b"#!/bin/sh\n"));
        // `MZ`, but its PE offset points past the end, or at something else.
        let mut header = vec![0_u8; 0x40];
        header[..2].copy_from_slice(b"MZ");
        header[0x3c..0x40].copy_from_slice(&0xffff_fff0_u32.to_le_bytes());
        assert!(!check(&header));
        header[0x3c..0x40].copy_from_slice(&0x40_u32.to_le_bytes());
        let mut wrong = header.clone();
        wrong.extend_from_slice(b"NE\0\0");
        assert!(!check(&wrong));
        // A Mach-O file is not a Windows program.
        assert!(!check(&[0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0]));
        assert!(crate::platform::is_pe(&fx.root.join("missing")).is_err());
    }

    /// A held copy can be read, hashed, checked and run, but not changed,
    /// replaced or deleted; let go, it can.
    #[cfg(windows)]
    #[test]
    fn a_held_copy_cannot_be_changed_until_it_is_let_go() {
        let fx = Fixture::new("windows-hold");
        let file = fx.root.join("held.exe");
        fs::copy(system32().join("tar.exe"), &file).unwrap();
        let held = crate::platform::hold(&file).unwrap();
        assert!(sha256_file(&file).is_ok());
        assert!(fs::OpenOptions::new().write(true).open(&file).is_err());
        assert!(fs::write(&file, "evil").is_err());
        assert!(fs::remove_file(&file).is_err());
        assert!(fs::rename(&file, fx.root.join("moved.exe")).is_err());
        let other = fx.root.join("other.exe");
        fs::write(&other, "x").unwrap();
        assert!(fs::rename(&other, &file).is_err());
        let output = Command::new(&file)
            .arg("--version")
            .env_clear()
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("bsdtar"));
        drop(held);
        fs::write(&file, "changed").unwrap();
        fs::remove_file(&file).unwrap();
    }

    /// The real installs on this machine, when there are any: found, and
    /// every file the store would check passes the real Authenticode check
    /// with its rule (one PowerShell run for both tools). Each says its
    /// version with an empty environment.
    #[cfg(windows)]
    #[test]
    fn the_real_installs_are_found_and_pass_the_real_signature_check() {
        let Some(home) = home_dir() else {
            return;
        };
        let fx = Fixture::new("windows-real");
        let store = fx.store();
        let mut checks = Vec::new();
        for tool in [Tool::Claude, Tool::Codex] {
            let Some(entry) = Store::find_installed(tool, &home) else {
                eprintln!("{}: not installed here; skipped", tool.name());
                continue;
            };
            let program = Store::program_of(tool, &entry).unwrap();
            let version = store.probe_version(tool, &program);
            eprintln!(
                "{}: {} -> {} ({version:?})",
                tool.name(),
                entry.display(),
                program.display()
            );
            assert!(version.is_some(), "{}", tool.name());
            let (root, files) = Store::source_files(tool, &entry).unwrap();
            let found = signature_checks(tool, &root, &program, files.iter()).unwrap();
            eprintln!(
                "{}: {} of {} files to check",
                tool.name(),
                found.len(),
                files.len()
            );
            if tool == Tool::Codex {
                // ripgrep is the one PE file not checked.
                let pe = files
                    .iter()
                    .filter(|f| crate::platform::is_pe(&root.join(f)).unwrap())
                    .count();
                assert_eq!(found.len() + 1, pe);
            }
            checks.extend(found);
        }
        if checks.is_empty() {
            return;
        }
        let list: Vec<(&Path, Option<&str>)> =
            checks.iter().map(|(p, t)| (p.as_path(), *t)).collect();
        let started = std::time::Instant::now();
        let result = crate::Codesign.check_all(&list);
        eprintln!(
            "{} files verified in {:?}: {result:?}",
            list.len(),
            started.elapsed()
        );
        assert_eq!(result, Ok(()));
    }

    /// Provisioning the real installs end to end with the real check: copies
    /// about 800 MB, so it runs only when asked
    /// (`cargo test -p connecto-store -- --ignored`).
    #[cfg(windows)]
    #[test]
    #[ignore = "copies the real installs (about 800 MB)"]
    fn the_real_installs_are_provisioned_with_the_real_check() {
        let Some(home) = home_dir() else {
            return;
        };
        let fx = Fixture::new("windows-real-provision");
        let store = fx.store();
        for tool in [Tool::Claude, Tool::Codex] {
            let Some(entry) = Store::find_installed(tool, &home) else {
                continue;
            };
            let started = std::time::Instant::now();
            let installed = store.provision(tool, &entry, &crate::Codesign).unwrap();
            eprintln!(
                "{}: {} in {:?}",
                tool.name(),
                installed.entry.display(),
                started.elapsed()
            );
            assert!(store.verify(tool, &installed.version).is_ok());
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_connecto_command_is_a_copy_of_the_helper() {
        let fixture = Fixture::new("windows-connecto");
        let store = fixture.store();
        let one = fixture.script("build/one", "1.0.0");
        assert!(store.refresh_hook(&one).unwrap());
        let command = store.bin_dir().join(CONNECTO_LINK);
        assert!(command.ends_with(r"cli\bin\connecto.exe"));
        assert_eq!(sha256_file(&command).unwrap(), sha256_file(&one).unwrap());
        // The same helper again does nothing.
        assert!(!store.refresh_hook(&one).unwrap());
        // A changed helper replaces both.
        let two = fixture.script("build/two", "2.0.0");
        assert!(store.refresh_hook(&two).unwrap());
        assert_eq!(sha256_file(&command).unwrap(), sha256_file(&two).unwrap());
        // A changed command alone is put right at the next refresh.
        fs::write(&command, "other").unwrap();
        assert!(!store.refresh_hook(&two).unwrap());
        assert_eq!(sha256_file(&command).unwrap(), sha256_file(&two).unwrap());
        let leftovers: Vec<_> = fs::read_dir(store.bin_dir())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[cfg(windows)]
    #[test]
    fn staging_folders_of_ended_processes_are_swept_and_nothing_else_is() {
        let fixture = Fixture::new("windows-sweep");
        let store = fixture.store();
        let cli = fixture.root.join(r"data\cli");
        let mut child = Command::new("cmd.exe")
            .args(["/C", "exit"])
            .spawn()
            .unwrap();
        let dead = child.id();
        child.wait().unwrap();
        drop(child);
        let stale = cli.join(format!(".staging-codex-{dead}"));
        fs::create_dir_all(stale.join(r"damaged\bin")).unwrap();
        fs::write(stale.join(r"damaged\bin\codex.exe"), "x").unwrap();
        let keep = [
            format!(".staging-claude-{}", std::process::id()),
            format!(".staging-other-{dead}"),
            format!(".staging-claude-{dead}x"),
        ];
        for name in &keep {
            fs::create_dir_all(cli.join(name)).unwrap();
        }
        store.sweep_stale_staging();
        assert!(!stale.exists(), "the ended provision's folder is gone");
        for name in &keep {
            assert!(cli.join(name).is_dir(), "{name}");
        }
    }
}
