//! Git for project memory, confined (spec §5.9, D7): before any git command
//! runs for a project, its repository is found and checked by reading the
//! files ourselves, and git is refused for it when anything could make git
//! run a program or read outside the repository.
//!
//! # Posture
//!
//! Isolation, not overriding. The repository's own configuration is not
//! trusted: it is read here (a plain file read, no link followed) and every
//! key must be in [`ALLOWED`], a small list of keys git uses only for local
//! bookkeeping or for network commands this host never runs. Any other key
//! (filters, textconv, external diff, fsmonitor, hooks, pagers, editors,
//! gpg and signature display, credential, ssh and proxy commands, aliases,
//! includes, `core.worktree`, excludes or attributes files, extensions such
//! as partial clone, …), any `include`/`includeIf`, or a file that cannot
//! be read or parsed: git is not used for that project, and the build
//! declares why. System configuration is not read; of the user's global
//! configuration only the settings that change how files are compared
//! ([`SEMANTIC`]) are read, and passed explicitly (see [`global_semantics`]).
//!
//! The layout is checked the same way: `.git` is a real folder with `HEAD`
//! (a regular file), `objects/` and `refs/`, no `commondir`, no
//! `objects/info/alternates`, no link among the files git would follow
//! there; or a linked worktree's `.git` file pointing into the same
//! repository's `.git/worktrees/<name>`, whose main repository passes the
//! same checks.
//!
//! Commands then run with a cleared environment, the repository and work
//! tree bound explicitly, the global safe options (`--no-pager
//! --no-replace-objects --no-lazy-fetch --no-optional-locks
//! --literal-pathspecs`) and, as a second line, configuration overrides of
//! the dangerous keys.
//!
//! # Subfolder projects
//!
//! A project folder inside a repository (a monorepo) uses the first
//! enclosing repository found walking up its real path (never past the
//! user's home folder, which is not itself taken: a dotfiles repository
//! there would capture every project). Every command is restricted to the
//! project's subfolder with a literal pathspec; paths are reported relative
//! to the project, and anything outside it is dropped.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long one git command may take.
const GIT_WAIT: Duration = Duration::from_secs(10);
/// Largest configuration file read, in bytes.
const MAX_CONFIG: u64 = 256 * 1024;

/// Configuration keys a repository may have for git to be used: section,
/// and key (`*`: any key of the section). Section names are compared
/// without case; `remote.<name>` and `branch.<name>` keys under any name.
const ALLOWED: &[(&str, &str)] = &[
    ("core", "repositoryformatversion"),
    ("core", "filemode"),
    ("core", "bare"),
    ("core", "logallrefupdates"),
    ("core", "ignorecase"),
    ("core", "precomposeunicode"),
    ("core", "symlinks"),
    ("core", "autocrlf"),
    ("core", "eol"),
    ("core", "safecrlf"),
    ("core", "checkstat"),
    ("core", "trustctime"),
    ("core", "quotepath"),
    ("core", "longpaths"),
    ("core", "protectntfs"),
    ("core", "protecthfs"),
    ("core", "abbrev"),
    ("extensions", "objectformat"),
    ("extensions", "worktreeconfig"),
    ("remote", "url"),
    ("remote", "pushurl"),
    ("remote", "fetch"),
    ("remote", "prune"),
    ("remote", "tagopt"),
    ("branch", "remote"),
    ("branch", "merge"),
    ("branch", "rebase"),
    ("branch", "pushremote"),
    ("branch", "description"),
    ("user", "name"),
    ("user", "email"),
    ("pull", "rebase"),
    ("pull", "ff"),
    ("push", "default"),
    ("push", "autosetupremote"),
    ("init", "defaultbranch"),
    ("color", "*"),
    ("advice", "*"),
    ("column", "ui"),
];

/// Settings given on every command as a second line: none of them can be
/// set by the repository (its configuration is checked first). System
/// configuration is not read; the global comparison settings are passed
/// explicitly ([`global_semantics`]), never read by git itself.
const OVERRIDES: &[(&str, &str)] = &[
    ("core.fsmonitor", "false"),
    ("core.useBuiltinFSMonitor", "false"),
    ("core.untrackedCache", "false"),
    ("core.hooksPath", NULL_DEVICE),
    ("core.pager", "cat"),
    ("core.askPass", ""),
    ("core.sshCommand", ""),
    ("core.excludesFile", ""),
    ("core.attributesFile", ""),
    ("core.bare", "false"),
    ("credential.helper", ""),
    ("diff.external", ""),
    ("log.showSignature", "false"),
    ("gpg.program", ""),
    ("protocol.allow", "never"),
    ("submodule.recurse", "false"),
];

#[cfg(unix)]
const NULL_DEVICE: &str = "/dev/null";
#[cfg(not(unix))]
const NULL_DEVICE: &str = "NUL";

/// The only variables a git call inherits: what finding the program and a
/// temporary folder need. Nothing named `GIT_*`, `SSH_*`, `GCM_*`, no pager
/// or editor.
const INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SystemRoot",
    "SYSTEMROOT",
    "windir",
    "USERPROFILE",
    "PATHEXT",
    "ComSpec",
    "COMSPEC",
];

/// The environment the app was started with.
pub(crate) fn inherited() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

/// A repository git may be used for, and where the project sits in it.
#[derive(Debug, Clone)]
pub(crate) struct Repo {
    /// The repository's work tree root (real path).
    pub root: PathBuf,
    /// Its git dir (real path): for a linked worktree, the worktree's own.
    pub git_dir: PathBuf,
    /// The project's folder relative to `root`, `/`-separated; empty when
    /// the project is the root.
    pub prefix: String,
    /// What was read and checked when the repository was located: git is
    /// never asked again (a file changed since is not reread).
    pub facts: Facts,
    /// The private git folder commands run against ([`shadow`]); `None`
    /// until [`bind`] made it.
    pub shadow: Option<Arc<crate::memory_private::PrivateDir>>,
}

/// What the checks read from the repository's own files, once.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Facts {
    /// Where objects, refs and `packed-refs` are (the main git dir).
    pub common_dir: PathBuf,
    /// The checked-out branch, when HEAD names one.
    pub branch: Option<String>,
    /// The commit HEAD resolves to; `None` before the first commit.
    pub head: Option<String>,
    /// `core.repositoryformatversion`.
    pub format_version: u32,
    /// `extensions.objectformat` when not SHA-1.
    pub object_format: Option<String>,
    /// The semantic settings ([`SEMANTIC`]) private git is given.
    pub semantics: Vec<(String, String)>,
    /// What of the user's global configuration was not taken, and why.
    pub limits: Vec<String>,
}

impl Repo {
    /// A project path as the repository names it.
    pub fn to_repo(&self, path: &str) -> String {
        if self.prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{}/{path}", self.prefix)
        }
    }

    /// A repository path as the project names it; `None` outside the
    /// project.
    pub fn to_project(&self, path: &str) -> Option<String> {
        if self.prefix.is_empty() {
            return Some(path.to_owned());
        }
        path.strip_prefix(&self.prefix)?
            .strip_prefix('/')
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
    }

    /// The literal pathspec that restricts a command to the project.
    pub fn scope(&self) -> &str {
        if self.prefix.is_empty() {
            "."
        } else {
            &self.prefix
        }
    }
}

/// Why git is not used for a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unusable {
    /// No repository encloses the project (below the home folder).
    NotARepository,
    /// A repository was found but cannot be used safely; the reason names
    /// what was found (never a value from the file).
    Refused(String),
}

fn refused(why: impl Into<String>) -> Unusable {
    Unusable::Refused(why.into())
}

/// A small regular file, read without following a link at its last
/// component (its folders were checked by the caller).
fn small_file(path: &Path, max: u64) -> Result<String, Unusable> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|_| refused(format!("{} cannot be read", name(path))))?;
    if !meta.is_file() || meta.len() > max {
        return Err(refused(format!(
            "{} is not a small regular file",
            name(path)
        )));
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(max + 1).read_to_string(&mut text))
        .map_err(|_| refused(format!("{} cannot be read", name(path))))?;
    Ok(text)
}

fn name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Lines of a configuration file, at most.
const MAX_CONFIG_LINES: usize = 2_000;

/// The keys of a git configuration file (`section`, `subsection`, `key`),
/// lowercased where git ignores case, read with a deliberately tiny grammar
/// (what git itself writes), not as a second git parser. Every line is one
/// of:
///
/// - empty;
/// - a comment: `#` or `;` first, then printable ASCII;
/// - `[name]` or `[name "sub"]`, at the start of the line, `name` of ASCII
///   letters, digits and `-`, `sub` of printable ASCII other than `"` and
///   `\`;
/// - `<tab>key = value` (or the same with no indent, or spaces instead of
///   the tab), `key` of ASCII letters, digits and `-` starting with a
///   letter, `value` of printable ASCII other than `"`, `\`, `#` and `;`.
///
/// Anything else is refused: a backslash anywhere (so no continuation, no
/// escape), quotes in a value, a key with no value, non-ASCII, a byte order
/// mark, a CR, a NUL, a tab other than the indent, an old `[a.b]` header,
/// more than [`MAX_CONFIG_LINES`] lines.
#[cfg(test)]
pub(crate) fn config_keys(text: &str) -> Result<Vec<(String, Option<String>, String)>, String> {
    Ok(config_entries(text)?
        .into_iter()
        .map(|(s, sub, k, _)| (s, sub, k))
        .collect())
}

/// One configuration entry: section, subsection, key, value.
type Entry = (String, Option<String>, String, String);

/// [`config_keys`] with each value.
fn config_entries(text: &str) -> Result<Vec<Entry>, String> {
    if !text.is_ascii() {
        return Err("non-ASCII text".to_owned());
    }
    if text.contains(['\\', '\r', '\0']) {
        return Err("a backslash, a CR or a NUL".to_owned());
    }
    let printable = |s: &str| s.bytes().all(|b| (0x20..0x7f).contains(&b));
    let plain_name =
        |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let mut out = Vec::new();
    let mut section: Option<(String, Option<String>)> = None;
    for (n, line) in text.split('\n').enumerate() {
        if n >= MAX_CONFIG_LINES {
            return Err("too many lines".to_owned());
        }
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') || line.starts_with(';') {
            if !printable(line) {
                return Err("a comment with control characters".to_owned());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let header = rest
                .strip_suffix(']')
                .ok_or("a section header is not plain")?;
            let (name, sub) = match header.split_once(' ') {
                Some((name, quoted)) => {
                    let sub = quoted
                        .strip_prefix('"')
                        .and_then(|q| q.strip_suffix('"'))
                        .ok_or("a subsection is not quoted")?;
                    if sub.contains('"') || !printable(sub) {
                        return Err("a subsection is not plain".to_owned());
                    }
                    (name, Some(sub.to_owned()))
                }
                None => (header, None),
            };
            if !plain_name(name) {
                return Err("a section name is not plain".to_owned());
            }
            section = Some((name.to_ascii_lowercase(), sub));
            continue;
        }
        let body = line
            .strip_prefix('\t')
            .unwrap_or_else(|| line.trim_start_matches(' '));
        if !printable(body) {
            return Err("a tab or a control character inside a line".to_owned());
        }
        let (key, value) = body.split_once(" = ").ok_or("a key without ` = value`")?;
        if !plain_name(key) || !key.starts_with(|c: char| c.is_ascii_alphabetic()) {
            return Err("a key is not plain".to_owned());
        }
        if value.contains(['"', '#', ';']) || value.trim() != value {
            return Err("a value is not plain".to_owned());
        }
        let Some((sec, sub)) = &section else {
            return Err("a key outside any section".to_owned());
        };
        out.push((
            sec.clone(),
            sub.clone(),
            key.to_ascii_lowercase(),
            value.to_owned(),
        ));
    }
    Ok(out)
}

/// A key as git lists it: `section.key` or `section.sub.key`, section and
/// key lowercased.
fn listed(section: &str, sub: Option<&str>, key: &str) -> String {
    match sub {
        Some(sub) => format!("{section}.{sub}.{key}"),
        None => format!("{section}.{key}"),
    }
}

/// The keys git itself reads from one configuration file, with nothing run:
/// `git config --file <path> --no-includes --list -z` reads only that file
/// (`git help config`), in a cleared environment. `None` when git cannot
/// tell.
fn keys_by_git(path: &Path) -> Option<Vec<String>> {
    let mut cmd = Command::new("git");
    cmd.env_clear();
    for (k, v) in inherited() {
        if k.to_str().is_some_and(|k| INHERITED.contains(&k)) {
            cmd.env(k, v);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .current_dir(path.parent()?)
        .args([
            "--no-pager",
            "config",
            "--no-includes",
            "--list",
            "-z",
            "--file",
        ])
        .arg(path);
    let (out, ok) = run(&mut cmd, MAX_CONFIG as usize * 2)?;
    if !ok {
        return None;
    }
    let mut keys: Vec<String> = out
        .split('\0')
        .filter(|e| !e.is_empty())
        .map(|e| e.split_once('\n').map_or(e, |(k, _)| k).to_owned())
        .collect();
    keys.sort();
    Some(keys)
}

/// The exact values accepted for an allowed key: the spellings git itself
/// writes. Anything else (`yes`/`on`/`1`/`2`, other cases, `k`/`m`/`g`
/// suffixes, leading zeros) refuses git: a value is never understood
/// differently from git, nor silently defaulted.
///
/// | key | accepted |
/// | - | - |
/// | `core.repositoryformatversion` | `0`, `1` |
/// | `core.bare` | `false` |
/// | `core.logallrefupdates` | `true`, `false`, `always` |
/// | `core.filemode`, `core.ignorecase`, `core.precomposeunicode`, `core.symlinks`, `core.trustctime`, `core.quotepath`, `core.longpaths`, `core.protectntfs`, `core.protecthfs` | `true`, `false` |
/// | `core.autocrlf` | `true`, `false`, `input` |
/// | `core.eol` | `lf`, `crlf`, `native` |
/// | `core.safecrlf` | `true`, `false`, `warn` |
/// | `core.checkstat` | `default`, `minimal` |
/// | `core.abbrev` | `auto`, or a decimal 4 to 64 without leading zero |
/// | `extensions.objectformat` | `sha1`, `sha256` |
/// | `extensions.worktreeconfig` | `true`, `false` |
/// | `remote.<n>.prune`, `push.autosetupremote`, `advice.*` | `true`, `false` |
/// | `remote.<n>.tagopt` | `--no-tags`, `--tags` |
/// | `branch.<n>.rebase`, `pull.rebase` | `true`, `false`, `merges` |
/// | `pull.ff` | `true`, `false`, `only` |
/// | `push.default` | `nothing`, `current`, `upstream`, `simple`, `matching` |
/// | `remote.<n>.url/pushurl/fetch`, `branch.<n>.remote/merge/pushremote/description`, `user.*`, `init.defaultbranch`, `color.*`, `column.ui` | any plain value (none of these changes what the read-only commands see) |
pub(crate) fn value_accepted(section: &str, key: &str, value: &str) -> bool {
    let one_of = |set: &[&str]| set.contains(&value);
    let boolean = one_of(&["true", "false"]);
    match (section, key) {
        ("core", "repositoryformatversion") => one_of(&["0", "1"]),
        ("core", "bare") => value == "false",
        ("core", "logallrefupdates") => boolean || value == "always",
        ("core", "autocrlf") => boolean || value == "input",
        ("core", "eol") => one_of(&["lf", "crlf", "native"]),
        ("core", "safecrlf") => boolean || value == "warn",
        ("core", "checkstat") => one_of(&["default", "minimal"]),
        ("core", "abbrev") => {
            value == "auto"
                || (!value.starts_with('0')
                    && value.bytes().all(|b| b.is_ascii_digit())
                    && value.parse::<u8>().is_ok_and(|n| (4..=64).contains(&n)))
        }
        ("core", _) => boolean,
        ("extensions", "objectformat") => one_of(&["sha1", "sha256"]),
        ("extensions", "worktreeconfig") => boolean,
        ("extensions", _) => false,
        ("remote", "prune") | ("push", "autosetupremote") | ("advice", _) => boolean,
        ("remote", "tagopt") => one_of(&["--no-tags", "--tags"]),
        ("branch", "rebase") | ("pull", "rebase") => boolean || value == "merges",
        ("pull", "ff") => boolean || value == "only",
        ("push", "default") => one_of(&["nothing", "current", "upstream", "simple", "matching"]),
        _ => true,
    }
}

/// Whether the entries are all allowed, with accepted values, and no key is
/// given twice with different values; also whether
/// `extensions.worktreeConfig` is `true`. Section and key names are compared
/// without case (as git does: [`config_entries`] lowercases them),
/// subsection names exactly.
fn allowed_keys(entries: &[Entry]) -> Result<bool, Unusable> {
    let mut worktree_config = false;
    let mut seen: BTreeMap<(String, Option<String>, String), &str> = BTreeMap::new();
    for (section, sub, key, value) in entries {
        if section == "include" || section == "includeif" {
            return Err(refused("its configuration includes another file"));
        }
        let ok = ALLOWED.iter().any(|(s, k)| {
            *s == section
                && (*k == "*" || *k == key)
                && (sub.is_none() || matches!(section.as_str(), "remote" | "branch"))
        });
        let name = sub.as_ref().map_or_else(
            || format!("{section}.{key}"),
            |_| format!("{section}.<name>.{key}"),
        );
        if !ok {
            return Err(refused(format!(
                "its configuration sets {name}, which git could use to run a program or read elsewhere"
            )));
        }
        if !value_accepted(section, key, value) {
            return Err(refused(format!("configuration not understood: {name}")));
        }
        let id = (section.clone(), sub.clone(), key.clone());
        if let Some(before) = seen.insert(id, value)
            && before != value
        {
            return Err(refused(format!(
                "configuration not understood: {name} is given twice"
            )));
        }
        worktree_config |= section == "extensions" && key == "worktreeconfig" && value == "true";
    }
    Ok(worktree_config)
}

/// Whether a configuration text is in the grammar and has only allowed keys
/// with accepted values (no git run). Returns its keys.
pub(crate) fn check_config(text: &str) -> Result<Vec<(String, Option<String>, String)>, Unusable> {
    let entries = config_entries(text)
        .map_err(|e| refused(format!("its configuration is not understood: {e}")))?;
    allowed_keys(&entries)?;
    Ok(entries
        .into_iter()
        .map(|(s, sub, k, _)| (s, sub, k))
        .collect())
}

/// Check one configuration file: read without following a link, in the
/// grammar, only allowed keys with accepted values, and exactly the keys git
/// reads from it (git as the oracle, run so that it executes nothing).
/// Returns whether it turns on `extensions.worktreeConfig`.
pub(crate) fn check_config_file(path: &Path) -> Result<bool, Unusable> {
    let text = small_file(path, MAX_CONFIG)?;
    let keys = check_config(&text)?;
    let mut ours: Vec<String> = keys
        .iter()
        .map(|(s, sub, k)| listed(s, sub.as_deref(), k))
        .collect();
    ours.sort();
    let theirs =
        keys_by_git(path).ok_or_else(|| refused("git could not read its configuration"))?;
    if ours != theirs {
        return Err(refused(
            "its configuration is not understood: git reads other keys than this check",
        ));
    }
    let entries = config_entries(&text)
        .map_err(|e| refused(format!("its configuration is not understood: {e}")))?;
    allowed_keys(&entries)
}

/// Check a git dir's own files (no git run): layout, links, configuration.
/// `common`: for a linked worktree, the main repository's git dir, checked
/// as a whole by the caller.
fn check_git_dir(dir: &Path, linked: bool) -> Result<(), Unusable> {
    if !is_real_dir(dir) {
        return Err(refused("its git folder is not a real folder"));
    }
    let head = dir.join("HEAD");
    let head_text = small_file(&head, 4096)?;
    let head_ok = head_text
        .trim()
        .strip_prefix("ref: refs/")
        .is_some_and(|r| !r.is_empty() && !r.contains(".."))
        || head_text.trim().bytes().all(|b| b.is_ascii_hexdigit());
    if !head_ok {
        return Err(refused("its HEAD is not a branch or a commit"));
    }
    for redirect in ["gitdir", "gitfile"] {
        if !linked && std::fs::symlink_metadata(dir.join(redirect)).is_ok() {
            return Err(refused(format!(
                "its git folder holds a {redirect} redirection"
            )));
        }
    }
    if !linked {
        if std::fs::symlink_metadata(dir.join("commondir")).is_ok() {
            return Err(refused(
                "its git folder names another repository's (commondir)",
            ));
        }
        for sub in ["objects", "refs"] {
            if !is_real_dir(&dir.join(sub)) {
                return Err(refused(format!("its {sub} folder is missing or a link")));
            }
        }
        if std::fs::symlink_metadata(dir.join("objects/info/alternates")).is_ok() {
            return Err(refused("it borrows objects from elsewhere (alternates)"));
        }
        if std::fs::symlink_metadata(dir.join("reftable")).is_ok() {
            return Err(refused("its refs are kept in a format not checked here"));
        }
    }
    // Every file system object git may open there: a regular file or a
    // real folder, physically inside this git folder.
    let started = Instant::now();
    let mut clock = || started.elapsed() > MAX_GIT_WALK;
    walk_git_dir(
        dir,
        &mut crate::memory_host::Budget::new(2 * MAX_GIT_ENTRIES, &mut clock),
    )
}

/// Entries of a git folder looked at, at most: past it git is refused (a
/// repository with more loose objects than this is not checked whole).
const MAX_GIT_ENTRIES: usize = 300_000;
/// Time the walk may take, at most: past it git is refused.
const MAX_GIT_WALK: Duration = Duration::from_secs(5);

/// Walk a whole git folder (objects with their fan-out folders, packs and
/// info, refs at every level, logs, info, worktrees, everything else), no
/// link followed: every entry must be a regular file or a real folder. A
/// link, a socket, a FIFO or a device anywhere refuses git, and so does a
/// walk that cannot finish within its budget (about [`MAX_GIT_ENTRIES`]
/// entries: every folder opened and every entry read spends one unit, at
/// most twice that, and [`MAX_GIT_WALK`]) — it fails closed: incomplete is
/// refused. Opening a folder spends too, so empty folders and deep chains
/// are bounded like full ones.
///
/// `modules/` (the git folders of submodules) is not walked: the commands
/// run here never enter a submodule (`--ignore-submodules=all`,
/// `submodule.recurse=false`), so its content is not read.
fn walk_git_dir(dir: &Path, budget: &mut crate::memory_host::Budget<'_>) -> Result<(), Unusable> {
    let too_large = |budget: &crate::memory_host::Budget<'_>| {
        if budget.timed_out() {
            refused(format!(
                "its git folder could not be checked whole within {} seconds",
                MAX_GIT_WALK.as_secs()
            ))
        } else {
            refused("its git folder is too large to check whole (too many loose objects or refs)")
        }
    };
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        if !budget.spend() {
            return Err(too_large(budget));
        }
        let entries =
            std::fs::read_dir(&d).map_err(|_| refused(format!("{} cannot be read", name(&d))))?;
        for e in entries {
            if !budget.spend() {
                return Err(too_large(budget));
            }
            let e = e.map_err(|_| refused("its git folder cannot be read whole"))?;
            // `DirEntry::file_type` does not follow a link.
            let kind = e
                .file_type()
                .map_err(|_| refused("an entry of its git folder cannot be read"))?;
            if kind.is_dir() {
                if d == dir && e.file_name() == "modules" {
                    continue;
                }
                stack.push(e.path());
            } else if !kind.is_file() {
                return Err(refused(format!(
                    "its git folder holds a link or a special file ({})",
                    e.path()
                        .strip_prefix(dir)
                        .map_or_else(|_| String::new(), |p| p.display().to_string())
                )));
            }
        }
    }
    Ok(())
}

/// The configuration files git reads for a repository (system and global
/// are switched off), all checked.
fn check_configs(main: &Path, worktree: Option<&Path>) -> Result<(), Unusable> {
    let main_config = main.join("config");
    let worktree_config = match std::fs::symlink_metadata(&main_config) {
        Ok(_) => check_config_file(&main_config)?,
        Err(_) => false,
    };
    // `config.worktree` of the main work tree and of a linked one.
    for dir in std::iter::once(main).chain(worktree) {
        let file = dir.join("config.worktree");
        if std::fs::symlink_metadata(&file).is_ok() {
            if !worktree_config {
                // Not read by git without the extension; refused anyway:
                // it is unexpected.
                return Err(refused("an unexpected config.worktree"));
            }
            check_config_file(&file)?;
        }
    }
    Ok(())
}

/// The path of `path` with every link resolved, in a form git can open.
///
/// On Windows `canonicalize` gives a `\\?\C:\...` path, which Git for
/// Windows cannot open (it reports "No such file or directory"), so every
/// repository would be refused. The prefix is dropped there: `C:\...` for a
/// drive, `\\server\share\...` for a network share.
fn real_path(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let real = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        Ok(without_verbatim_prefix(real))
    }
    #[cfg(not(windows))]
    {
        Ok(real)
    }
}

/// `\\?\C:\a` as `C:\a` and `\\?\UNC\server\share\a` as
/// `\\server\share\a`. Any other path is returned as it is.
#[cfg(windows)]
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return path;
    };
    let Some(text) = path.to_str() else {
        return path;
    };
    let plain = match prefix.kind() {
        Prefix::VerbatimDisk(_) => text.strip_prefix(r"\\?\").map(PathBuf::from),
        Prefix::VerbatimUNC(..) => text
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| PathBuf::from(format!(r"\\{rest}"))),
        _ => None,
    };
    plain.unwrap_or(path)
}

/// The git dir of a work tree root `root` whose `.git` is `dot_git`, checked
/// (layout and configuration), or why not.
fn validate(root: &Path, dot_git: &Path) -> Result<PathBuf, Unusable> {
    let meta = std::fs::symlink_metadata(dot_git).map_err(|_| refused(".git cannot be read"))?;
    if meta.file_type().is_symlink() {
        return Err(refused(".git is a link"));
    }
    if meta.is_dir() {
        let dir = real_path(dot_git).map_err(|_| refused(".git cannot be read"))?;
        check_git_dir(&dir, false)?;
        check_configs(&dir, None)?;
        return Ok(dir);
    }
    // A linked worktree: `gitdir: <main>/.git/worktrees/<name>`, whose
    // `commondir` is that main git dir and whose `gitdir` names this file.
    let target = small_file(dot_git, 4096)?
        .trim()
        .strip_prefix("gitdir:")
        .map(|t| t.trim().to_owned())
        .ok_or_else(|| refused(".git is a file that is not a worktree pointer"))?;
    let dir =
        real_path(root.join(target)).map_err(|_| refused("the worktree pointer leads nowhere"))?;
    let worktrees = dir
        .parent()
        .ok_or_else(|| refused("bad worktree pointer"))?;
    if worktrees.file_name().is_none_or(|n| n != "worktrees") {
        return Err(refused(
            ".git points to a folder that is not a linked worktree's",
        ));
    }
    let main = worktrees
        .parent()
        .ok_or_else(|| refused("bad worktree pointer"))?
        .to_owned();
    let common_text = small_file(&dir.join("commondir"), 4096)?;
    let common = real_path(dir.join(common_text.trim()))
        .map_err(|_| refused("the worktree's repository cannot be found"))?;
    if common != main {
        return Err(refused("the worktree names another repository"));
    }
    let back_text = small_file(&dir.join("gitdir"), 4096)?;
    let back = real_path(dir.join(back_text.trim()))
        .map_err(|_| refused("the worktree does not name this folder back"))?;
    let own = real_path(dot_git).map_err(|_| refused(".git cannot be read"))?;
    if back != own {
        return Err(refused("the worktree does not name this folder back"));
    }
    check_git_dir(&main, false)?;
    check_git_dir(&dir, true)?;
    check_configs(&main, Some(&dir))?;
    Ok(dir)
}

/// The home folder, which the walk up never reaches or takes: the one git
/// uses ([`git_home`]), so the repository search and git agree on it.
fn home(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    home_here(env).and_then(|h| real_path(h).ok())
}

/// Find and check the repository that encloses `project` (by reading files;
/// no git is run).
pub(crate) fn locate(project: &Path, env: &[(OsString, OsString)]) -> Result<Repo, Unusable> {
    let real = real_path(project).map_err(|_| Unusable::NotARepository)?;
    let home = home(env);
    let mut at = real.clone();
    loop {
        if home.as_deref() == Some(at.as_path()) {
            return Err(Unusable::NotARepository);
        }
        let dot_git = at.join(".git");
        if std::fs::symlink_metadata(&dot_git).is_ok() {
            // The nearest repository decides: a refused one is not skipped
            // for an outer one.
            let git_dir = validate(&at, &dot_git)?;
            let rel = real
                .strip_prefix(&at)
                .map_err(|_| Unusable::NotARepository)?;
            let mut parts = Vec::new();
            for c in rel.components() {
                let s = c
                    .as_os_str()
                    .to_str()
                    .filter(|s| !s.contains(['\\', ':']) && !s.is_empty())
                    .ok_or_else(|| refused("the project's path is not plain"))?;
                parts.push(s.to_owned());
            }
            let facts = facts(&git_dir, env)?;
            return Ok(Repo {
                root: at,
                git_dir,
                prefix: parts.join("/"),
                facts,
                shadow: None,
            });
        }
        if !at.pop() {
            return Err(Unusable::NotARepository);
        }
    }
}

/// Largest index copied, in bytes: past it git is not used (declared).
const MAX_INDEX: u64 = 256 * 1024 * 1024;

/// A commit id: 40 or 64 hexadecimal characters.
fn hex_id(text: &str) -> Option<String> {
    let t = text.trim();
    (matches!(t.len(), 40 | 64) && t.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| t.to_ascii_lowercase())
}

/// A ref name as git allows it here: `refs/` then plain segments.
fn plain_ref(name: &str) -> bool {
    name.strip_prefix("refs/").is_some_and(|rest| {
        !rest.is_empty()
            && rest.split('/').all(|seg| {
                !seg.is_empty()
                    && seg != "."
                    && seg != ".."
                    && !seg.ends_with(".lock")
                    && seg
                        .bytes()
                        .all(|b| b.is_ascii_graphic() && !b"~^:?*[\\".contains(&b))
            })
    })
}

/// The commit a ref names, read from the (checked) common git dir: its
/// loose file, else `packed-refs`. `None`: no such ref (a branch with no
/// commit yet).
fn resolve_ref(common: &Path, name: &str) -> Result<Option<String>, Unusable> {
    if !plain_ref(name) {
        return Err(refused("HEAD names a ref that is not plain"));
    }
    let loose = common.join(name);
    if std::fs::symlink_metadata(&loose).is_ok() {
        let text = small_file(&loose, 4096)?;
        return hex_id(&text)
            .map(Some)
            .ok_or_else(|| refused("a ref does not hold a commit id"));
    }
    let packed = common.join("packed-refs");
    if std::fs::symlink_metadata(&packed).is_ok() {
        let text = small_file(&packed, 64 * 1024 * 1024)?;
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            if let Some((id, refname)) = line.split_once(' ')
                && refname == name
            {
                return hex_id(id)
                    .map(Some)
                    .ok_or_else(|| refused("packed-refs is not understood"));
            }
        }
    }
    Ok(None)
}

/// The facts git will be given instead of reading them itself: HEAD
/// resolved to a commit id, the branch, the object format. Read from the
/// files [`validate`] checked.
fn facts(git_dir: &Path, env: &[(OsString, OsString)]) -> Result<Facts, Unusable> {
    let common = match std::fs::symlink_metadata(git_dir.join("commondir")) {
        Ok(_) => {
            let text = small_file(&git_dir.join("commondir"), 4096)?;
            real_path(git_dir.join(text.trim()))
                .map_err(|_| refused("the worktree's repository cannot be found"))?
        }
        Err(_) => git_dir.to_owned(),
    };
    let head = small_file(&git_dir.join("HEAD"), 4096)?;
    let (branch, id) = match head.trim().strip_prefix("ref: ") {
        Some(name) => {
            let id = resolve_ref(&common, name)?;
            (name.strip_prefix("refs/heads/").map(str::to_owned), id)
        }
        None => (
            None,
            Some(hex_id(&head).ok_or_else(|| refused("HEAD is not understood"))?),
        ),
    };
    let config = common.join("config");
    let entries = if std::fs::symlink_metadata(&config).is_ok() {
        config_entries(&small_file(&config, MAX_CONFIG)?)
            .map_err(|e| refused(format!("its configuration is not understood: {e}")))?
    } else {
        Vec::new()
    };
    let value = |section: &str, key: &str| {
        entries
            .iter()
            .find(|(s, sub, k, _)| s == section && sub.is_none() && k == key)
            .map(|(_, _, _, v)| v.to_ascii_lowercase())
    };
    let format_version = match value("core", "repositoryformatversion").as_deref() {
        None | Some("0") => 0,
        Some("1") => 1,
        Some(_) => return Err(refused("an unknown repository format version")),
    };
    let object_format = match value("extensions", "objectformat").as_deref() {
        None | Some("sha1") => None,
        Some("sha256") => Some("sha256".to_owned()),
        Some(_) => return Err(refused("an unknown object format")),
    };
    let (semantics, limits) = semantics(&common, git_dir, env)?;
    Ok(Facts {
        common_dir: common,
        branch,
        head: id,
        format_version,
        object_format,
        semantics,
        limits,
    })
}

/// A file inside a checked git dir, read without following a link (on
/// Unix through handles; elsewhere after a link check), at most `max`.
fn read_private(dir: &Path, name: &str, max: u64) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        crate::memory_host::read_no_follow(dir, name, max)
    }
    #[cfg(not(unix))]
    {
        let path = dir.join(name);
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > max {
            return None;
        }
        std::fs::read(path).ok()
    }
}

/// The allowlisted keys, each decided: copied into the private
/// configuration because it changes how git compares, converts or names
/// content (so private git sees what the user's git sees), or not copied
/// because it does not matter to the read-only commands run here.
///
/// | key | copied |
/// | - | - |
/// | `core.autocrlf`, `core.eol`, `core.safecrlf` | yes: line endings decide whether a file differs |
/// | `core.filemode`, `core.symlinks`, `core.trustctime`, `core.checkstat` | yes: which stat changes count |
/// | `core.ignorecase`, `core.precomposeunicode`, `core.protectntfs`, `core.protecthfs`, `core.longpaths` | yes: how paths are compared and named |
/// | `core.quotepath`, `core.abbrev` | yes: how names and ids are printed (`-z` and full ids are used anyway) |
/// | `core.repositoryformatversion`, `extensions.objectformat` | yes, as [`Facts`] fields |
/// | `core.bare`, `core.logallrefupdates`, `extensions.worktreeconfig` | no: fixed or irrelevant to reads |
/// | `remote.*`, `branch.*`, `user.*`, `pull.*`, `push.*`, `init.*`, `color.*`, `advice.*`, `column.ui` | no: network, writing or display only |
const SEMANTIC: &[&str] = &[
    "core.autocrlf",
    "core.eol",
    "core.safecrlf",
    "core.filemode",
    "core.symlinks",
    "core.trustctime",
    "core.checkstat",
    "core.ignorecase",
    "core.precomposeunicode",
    "core.protectntfs",
    "core.protecthfs",
    "core.longpaths",
    "core.quotepath",
    "core.abbrev",
];

/// A semantic key's value when it is exactly one of the accepted spellings
/// ([`value_accepted`]); `None` otherwise.
fn semantic_value(key: &str, value: &str) -> Option<String> {
    let (section, name) = key.split_once('.')?;
    value_accepted(section, name, value).then(|| value.to_owned())
}

/// A non-empty variable of `env` (the last one given wins).
fn env_var(env: &[(OsString, OsString)], name: &str) -> Option<OsString> {
    env.iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
}

/// The home folder git uses, from `env` (never the process directly),
/// computed as git does on the platform `windows` names, so both platforms
/// are tested here. `is_dir` checks a folder exists.
///
/// - Unix: `HOME`.
/// - Windows, as Git for Windows' wrapper sets `HOME` when it is not set
///   (`git-wrapper.c`, `setup_environment()`): `HOME` when non-empty; else
///   `HOMEDRIVE` + `HOMEPATH` when `HOMEPATH` is set and that is an existing
///   folder other than the system folder (`…\Windows\System32`); else
///   `USERPROFILE` when non-empty.
///
/// `None`: git's home cannot be determined (declared by the callers).
pub(crate) fn git_home(
    env: &[(OsString, OsString)],
    windows: bool,
    is_dir: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(home) = env_var(env, "HOME") {
        return Some(PathBuf::from(home));
    }
    if !windows {
        return None;
    }
    if let Some(path) = env_var(env, "HOMEPATH") {
        let mut joined = env_var(env, "HOMEDRIVE").unwrap_or_default();
        joined.push(&path);
        let candidate = PathBuf::from(joined);
        let system = candidate
            .to_string_lossy()
            .to_ascii_lowercase()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .ends_with("\\windows\\system32");
        if !system && is_dir(&candidate) {
            return Some(candidate);
        }
    }
    env_var(env, "USERPROFILE").map(PathBuf::from)
}

/// [`git_home`] for this platform.
fn home_here(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    git_home(env, cfg!(windows), &|p: &Path| p.is_dir())
}

/// The user's global configuration files, in git's order (`git help
/// config`, FILES): `$XDG_CONFIG_HOME/git/config`, with `<home>/.config`
/// used when `XDG_CONFIG_HOME` is not set or empty, then `<home>/.gitconfig`,
/// `<home>` being [`git_home`]. Both are read when both exist; the later one
/// wins. `None` when they cannot be determined (no home): never "no files".
pub(crate) fn global_files_for(
    env: &[(OsString, OsString)],
    home: Option<&Path>,
) -> Option<Vec<PathBuf>> {
    let home = home?;
    let xdg = env_var(env, "XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    Some(vec![
        xdg.join("git").join("config"),
        home.join(".gitconfig"),
    ])
}

/// [`global_files_for`] with this platform's [`git_home`].
pub(crate) fn global_files(env: &[(OsString, OsString)]) -> Option<Vec<PathBuf>> {
    global_files_for(env, home_here(env).as_deref())
}

/// The entries git reads from one file, in order, includes not followed:
/// `git config --file <path> --no-includes --list -z`, in a cleared
/// environment (reads that file only, runs nothing). Keys lowercased as git
/// prints them; a key with no value is `None`.
fn entries_by_git(path: &Path) -> Option<Vec<(String, Option<String>)>> {
    let mut cmd = Command::new("git");
    cmd.env_clear();
    for (k, v) in inherited() {
        if k.to_str().is_some_and(|k| INHERITED.contains(&k)) {
            cmd.env(k, v);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .current_dir(path.parent()?)
        .args([
            "--no-pager",
            "config",
            "--no-includes",
            "--list",
            "-z",
            "--file",
        ])
        .arg(path);
    let (out, ok) = run(&mut cmd, MAX_CONFIG as usize * 2)?;
    if !ok {
        return None;
    }
    Some(
        out.split('\0')
            .filter(|e| !e.is_empty())
            .map(|e| match e.split_once('\n') {
                Some((k, v)) => (k.to_owned(), Some(v.to_owned())),
                None => (e.to_owned(), None),
            })
            .collect(),
    )
}

/// The global values of the copied keys, and the limits of what was taken.
///
/// Both global files are read as layers, in git's order ([`global_files`]);
/// a missing file is simply absent; a file that is a link, not a regular
/// file, too large, or that git cannot read is not read (a limit). For each
/// copied key the LAST value across the layers is taken first, then checked:
/// a value not spelled as accepted here ([`value_accepted`]) is not taken,
/// so the key keeps git's default in private git (a limit, unless the
/// repository sets it). Global includes are not followed (a limit, said
/// plainly whenever a global file has one). System configuration is not
/// read at all.
pub(crate) fn global_semantics(
    env: &[(OsString, OsString)],
) -> (BTreeMap<String, Option<String>>, Vec<String>) {
    let mut last: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut limits = Vec::new();
    let Some(files) = global_files(env) else {
        // "Could not determine" is a declared limit, never "no files".
        limits.push(
            "global settings could not be discovered (git's home folder is not known)".to_owned(),
        );
        return (last, limits);
    };
    for file in files {
        let meta = match std::fs::symlink_metadata(&file) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                limits.push(format!("{} could not be read", file.display()));
                continue;
            }
        };
        if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_CONFIG {
            limits.push(format!(
                "{} is not read (a link, not a regular file, or too large)",
                file.display()
            ));
            continue;
        }
        let Some(entries) = entries_by_git(&file) else {
            limits.push(format!("git could not read {}", file.display()));
            continue;
        };
        for (key, value) in entries {
            let key = key.to_ascii_lowercase();
            if key.starts_with("include.") || key.starts_with("includeif.") {
                limits.push("global includes are not followed".to_owned());
                continue;
            }
            if SEMANTIC.contains(&key.as_str()) {
                last.insert(key, value);
            }
        }
    }
    // The effective value of each key, checked only now.
    let effective = last
        .into_iter()
        .map(|(key, value)| {
            let ok = value.as_deref().and_then(|v| semantic_value(&key, v));
            (key, ok)
        })
        .collect();
    limits.sort();
    limits.dedup();
    (effective, limits)
}

/// Settings as (key, value) pairs.
type Settings = Vec<(String, String)>;

/// The semantic settings private git is given, with git's precedence:
/// system (not read, declared) < the user's global files ([`global_semantics`])
/// < the repository's `config` < the active worktree's `config.worktree` (the
/// main worktree's `.git/config.worktree`, a linked one's
/// `.git/worktrees/<name>/config.worktree`), the last only when
/// `extensions.worktreeConfig` is `true` in the repository's `config`. The
/// last value per key across the layers decides; repository layers were
/// checked whole before ([`check_configs`]: every value accepted, no key
/// given twice with different values). Returns the settings and the limits
/// (global values not taken, global files not read, global includes).
fn semantics(
    common: &Path,
    git_dir: &Path,
    env: &[(OsString, OsString)],
) -> Result<(Settings, Vec<String>), Unusable> {
    let private = crate::memory_private::active()
        .ok_or_else(|| refused("no private git folder is available"))?;
    // Cached by which files and what they hold (small: read here only to
    // tell a change; git reads them for the values).
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for file in global_files(env).unwrap_or_default() {
            file.hash(&mut h);
            std::fs::symlink_metadata(&file)
                .ok()
                .filter(|m| m.is_file() && m.len() <= MAX_CONFIG)
                .and_then(|_| std::fs::read(&file).ok())
                .hash(&mut h);
        }
        format!("{:x}", h.finish())
    };
    let (global, mut limits) = private.global(&key, || global_semantics(env));
    let mut out: BTreeMap<String, Option<String>> = global;
    let layer =
        |file: &Path, out: &mut BTreeMap<String, Option<String>>| -> Result<bool, Unusable> {
            let mut worktree_config = false;
            if std::fs::symlink_metadata(file).is_err() {
                return Ok(false);
            }
            let entries = config_entries(&small_file(file, MAX_CONFIG)?)
                .map_err(|e| refused(format!("its configuration is not understood: {e}")))?;
            for (section, sub, key, value) in entries {
                let name = format!("{section}.{key}");
                if sub.is_none() && name == "extensions.worktreeconfig" {
                    // Checked before: exactly `true` or `false`.
                    worktree_config = value == "true";
                }
                if sub.is_none() && SEMANTIC.contains(&name.as_str()) {
                    let v = semantic_value(&name, &value)
                        .ok_or_else(|| refused(format!("configuration not understood: {name}")))?;
                    out.insert(name, Some(v));
                }
            }
            Ok(worktree_config)
        };
    if layer(&common.join("config"), &mut out)? {
        layer(&git_dir.join("config.worktree"), &mut out)?;
    }
    let mut settings = Vec::new();
    for (key, value) in out {
        match value {
            Some(v) => settings.push((key, v)),
            None => limits.push(format!(
                "the global {key} is not spelled as git writes it: git's default is used"
            )),
        }
    }
    Ok((settings, limits))
}

/// The private configuration: the checked format and the semantic
/// settings, nothing else.
fn private_config(facts: &Facts) -> String {
    let mut core = format!(
        "[core]\n\trepositoryformatversion = {}\n\tbare = false\n",
        facts.format_version
    );
    for (key, value) in &facts.semantics {
        if let Some(k) = key.strip_prefix("core.") {
            core.push_str(&format!("\t{k} = {value}\n"));
        }
    }
    if let Some(format) = &facts.object_format {
        core.push_str(&format!("[extensions]\n\tobjectformat = {format}\n"));
    }
    core
}

/// Check an index copy: `DIRC`, version 2, 3 or 4. Its extensions (split
/// index, untracked cache, fsmonitor) are data for git; with the private
/// configuration none of them can run anything.
fn check_index(bytes: &[u8]) -> Result<(), String> {
    let version = bytes
        .get(4..8)
        .map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]));
    if bytes.get(..4) != Some(b"DIRC") || !matches!(version, Some(2..=4)) {
        return Err("its index is in a format not understood here".to_owned());
    }
    Ok(())
}

/// The `sharedindex.<hex>` files of a split index, in the git dir.
fn shared_indexes(git_dir: &Path) -> Vec<String> {
    std::fs::read_dir(git_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|k| k.is_file()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| {
            n.strip_prefix("sharedindex.").is_some_and(|h| {
                matches!(h.len(), 40 | 64) && h.bytes().all(|b| b.is_ascii_hexdigit())
            })
        })
        .collect()
}

/// What the private folder is made of, as a cache key: the checked facts
/// and the size and time of every file copied.
fn cache_key(repo: &Repo, copies: &[(PathBuf, String, String)]) -> String {
    let stamp = |path: &Path| {
        std::fs::symlink_metadata(path)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .and_then(|m| Some((m.len(), m.modified().ok()?)))
    };
    let stamps: Vec<_> = copies
        .iter()
        .map(|(dir, name, _)| stamp(&dir.join(name)))
        .collect();
    format!("{:?}|{:?}|{:?}", repo.git_dir, repo.facts, stamps)
}

/// The files copied into the private folder, data only: the index (and a
/// split index's shared parts), `info/exclude`, `shallow`; from where, and
/// as what.
fn copies(repo: &Repo) -> Vec<(PathBuf, String, String)> {
    let mut out = vec![
        (repo.git_dir.clone(), "index".to_owned(), "index".to_owned()),
        (
            repo.facts.common_dir.join("info"),
            "exclude".to_owned(),
            "info/exclude".to_owned(),
        ),
        (
            repo.facts.common_dir.clone(),
            "shallow".to_owned(),
            "shallow".to_owned(),
        ),
    ];
    for name in shared_indexes(&repo.git_dir) {
        out.push((repo.git_dir.clone(), name.clone(), name));
    }
    out
}

/// The private git folder for `repo` ([`crate::memory_private`]): a
/// `config` with the checked format and semantic settings only, `HEAD` as
/// the commit id resolved from the checked refs, empty `refs/` and
/// `objects/`, and copies (read without following links, written `0600`) of
/// the index and its shared parts, `info/exclude` and `shallow`. Git reads
/// its configuration, HEAD and index from here only; objects come from the
/// real, checked object store (`GIT_OBJECT_DIRECTORY`), which holds data.
///
/// Index variants: absent (a fresh repository) works (empty index); a split
/// index works (its `sharedindex.*` are copied); versions 2 to 4 work; an
/// `index.lock` being written is not copied (the last complete index is
/// used); anything else is refused. A sparse index needs settings that are
/// not allowed, so its repository is refused at the configuration check.
fn shadow(repo: &Repo) -> Result<Arc<crate::memory_private::PrivateDir>, Unusable> {
    let private = crate::memory_private::active()
        .ok_or_else(|| refused("no private git folder is available"))?;
    let copies = copies(repo);
    let key = cache_key(repo, &copies);
    private
        .folder(&repo.root, &repo.root.join(&repo.prefix), &key, |dir| {
            let write = crate::memory_private::write_new;
            write(dir, "config", private_config(&repo.facts).as_bytes())?;
            // Before the first commit: an unborn branch, read by nobody.
            let head = repo
                .facts
                .head
                .clone()
                .unwrap_or_else(|| "ref: refs/heads/unborn".to_owned());
            write(dir, "HEAD", format!("{head}\n").as_bytes())?;
            for (from, name, to) in &copies {
                if std::fs::symlink_metadata(from.join(name)).is_err() {
                    continue;
                }
                let bytes = read_private(from, name, MAX_INDEX)
                    .ok_or_else(|| format!("its {name} file cannot be copied safely"))?;
                if name == "index" {
                    check_index(&bytes)?;
                }
                let modified = std::fs::symlink_metadata(from.join(name))
                    .and_then(|m| m.modified())
                    .ok();
                crate::memory_private::write_new_at(dir, to, &bytes, modified)?;
            }
            Ok(())
        })
        .map_err(refused)
}

/// A git command failed for `repo` after it was bound: remembered, so the
/// build falls back and declares it.
pub(crate) fn record_failure(repo: &Repo, what: &str) {
    if let Some(p) = crate::memory_private::active() {
        p.record_failure(&repo.root, what);
    }
}

/// The latest recorded failure for `repo`, if recent.
pub(crate) fn recent_failure(repo: &Repo) -> Option<String> {
    crate::memory_private::active()?.failure(&repo.root)
}

/// The local branches, from the checked refs (loose and packed); `None`
/// when they cannot be read.
pub(crate) fn local_branches(repo: &Repo) -> Option<Vec<String>> {
    let common = &repo.facts.common_dir;
    let mut out = std::collections::BTreeSet::new();
    let heads = common.join("refs/heads");
    let mut stack = vec![heads.clone()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let kind = e.file_type().ok()?;
            if kind.is_dir() {
                stack.push(e.path());
            } else if kind.is_file() {
                let name = e
                    .path()
                    .strip_prefix(&heads)
                    .ok()?
                    .to_str()?
                    .replace('\\', "/");
                out.insert(name);
            }
        }
    }
    if let Ok(text) = small_file(&common.join("packed-refs"), 64 * 1024 * 1024) {
        for line in text.lines() {
            if let Some((_, name)) = line.split_once(' ')
                && let Some(branch) = name.strip_prefix("refs/heads/")
            {
                out.insert(branch.to_owned());
            }
        }
    }
    Some(out.into_iter().collect())
}

/// A git command for `repo`: a cleared environment with only [`INHERITED`]
/// variables and the controlled ones, the repository and work tree bound,
/// the global safe options, and [`OVERRIDES`].
pub(crate) fn command(repo: &Repo, env: &[(OsString, OsString)]) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear();
    for (k, v) in env {
        if k.to_str().is_some_and(|k| INHERITED.contains(&k)) {
            cmd.env(k, v);
        }
    }
    // The home git and Connecto agree on ([`git_home`]).
    if let Some(home) = home_here(env) {
        cmd.env("HOME", home);
    }
    cmd.args([
        "--no-pager",
        "--no-replace-objects",
        "--no-lazy-fetch",
        "--no-optional-locks",
        "--literal-pathspecs",
    ])
    .arg("-C")
    .arg(&repo.root)
    .env(
        "GIT_DIR",
        repo.shadow.as_ref().map_or_else(
            || repo.root.join(".connecto-no-git"),
            |d| d.path().to_owned(),
        ),
    )
    .env("GIT_WORK_TREE", &repo.root)
    .env(
        "GIT_OBJECT_DIRECTORY",
        repo.facts.common_dir.join("objects"),
    )
    .env(
        "GIT_INDEX_FILE",
        repo.shadow.as_ref().map_or_else(
            || repo.root.join(".connecto-no-index"),
            |d| d.path().join("index"),
        ),
    )
    .env("LANG", "C")
    .env("LC_ALL", "C")
    .env("GIT_OPTIONAL_LOCKS", "0")
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_LITERAL_PATHSPECS", "1")
    .env("GIT_NO_LAZY_FETCH", "1")
    .env("GIT_NO_REPLACE_OBJECTS", "1")
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
    .env("GIT_ATTR_NOSYSTEM", "1")
    .env("GIT_PAGER", "cat")
    .env("PAGER", "cat")
    .env("GIT_CONFIG_COUNT", OVERRIDES.len().to_string());
    for (i, (key, value)) in OVERRIDES.iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{i}"), key)
            .env(format!("GIT_CONFIG_VALUE_{i}"), value);
    }
    cmd
}

/// Run a prepared git command: its stdout (at most `max` bytes) and whether
/// it succeeded; `None` when it could not run, timed out, or wrote more
/// than `max`.
pub(crate) fn run(cmd: &mut Command, max: usize) -> Option<(String, bool)> {
    run_input(cmd, None, max)
}

/// [`run`], with `input` written to the command's standard input.
pub(crate) fn run_input(
    cmd: &mut Command,
    input: Option<Vec<u8>>,
    max: usize,
) -> Option<(String, bool)> {
    run_bytes(cmd, input, max).map(|(b, ok)| (String::from_utf8_lossy(&b).into_owned(), ok))
}

/// [`run_input`], with the output as the bytes git wrote (for size-framed
/// output such as `cat-file --batch`).
pub(crate) fn run_bytes(
    cmd: &mut Command,
    input: Option<Vec<u8>>,
    max: usize,
) -> Option<(Vec<u8>, bool)> {
    let mut child = cmd
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        // On a thread: a command may answer before it has read everything.
        std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input);
        });
    }
    // Read on a thread of its own: a large output must not fill the pipe
    // while we wait for the exit.
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.take(max as u64 + 1).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + GIT_WAIT;
    loop {
        if reader.is_finished() {
            let bytes = reader.join().ok()?;
            if bytes.len() > max {
                // Over the cap: the rest is never read, the command is stopped.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            // Output closed: the exit follows, within the same deadline.
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                }
            };
            return Some((bytes, status.success()));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Locate `project`'s repository, make its private git folder, and check,
/// with git itself, that it resolves the work tree and that folder.
pub(crate) fn bind(project: &Path, env: &[(OsString, OsString)]) -> Result<Repo, Unusable> {
    let mut repo = locate(project, env)?;
    let dir = shadow(&repo)?;
    repo.shadow = Some(dir);
    let (out, ok) = run(
        command(&repo, env).args(["rev-parse", "--show-toplevel", "--absolute-git-dir"]),
        64 * 1024,
    )
    .ok_or_else(|| refused("git did not answer"))?;
    let mut lines = out.lines();
    let resolved = |l: Option<&str>| l.and_then(|l| real_path(l).ok());
    let top = resolved(lines.next());
    let dir = resolved(lines.next());
    let private = repo.shadow.as_ref().and_then(|d| real_path(d.path()).ok());
    if ok && top.as_ref() == Some(&repo.root) && dir.is_some() && dir == private {
        Ok(repo)
    } else {
        Err(refused("git resolved another work tree or git folder"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    #[test]
    fn a_real_path_is_one_git_can_open() {
        let dir = tempfile::tempdir().unwrap();
        let real = real_path(dir.path()).unwrap();
        assert!(real.is_absolute() && real.is_dir(), "{real:?}");
        assert!(!real.to_string_lossy().starts_with(r"\\?\"), "{real:?}");
        std::fs::write(real.join("config"), "[core]\n\tbare = false\n").unwrap();
        // Git itself opens it, when git is installed.
        if let Some(keys) = keys_by_git(&real.join("config")) {
            assert_eq!(keys, ["core.bare"]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_verbatim_windows_path_loses_only_its_prefix() {
        let plain = |p: &str| without_verbatim_prefix(PathBuf::from(p));
        assert_eq!(plain(r"\\?\C:\Users\a"), PathBuf::from(r"C:\Users\a"));
        assert_eq!(
            plain(r"\\?\UNC\server\share\a"),
            PathBuf::from(r"\\server\share\a")
        );
        assert_eq!(plain(r"C:\Users\a"), PathBuf::from(r"C:\Users\a"));
        assert_eq!(
            plain(r"\\?\GLOBALROOT\Device\x"),
            PathBuf::from(r"\\?\GLOBALROOT\Device\x")
        );
    }

    #[test]
    fn configuration_keys_are_read_strictly() {
        let plain = "[core]\n\trepositoryformatversion = 0\n\tbare = false\n[remote \"origin\"]\n\turl = x\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[branch \"main\"]\n\tremote = origin\n";
        assert!(check_config(plain).is_ok());
        for bad in [
            "[core]\n\tfsmonitor = x\n",
            "[filter \"lfs\"]\n\tclean = x\n",
            "[diff \"x\"]\n\ttextconv = y\n",
            "[log]\n\tshowSignature = true\n",
            "[gpg]\n\tprogram = x\n",
            "[core]\n\tworktree = /elsewhere\n",
            "[include]\n\tpath = ../x\n",
            "[includeIf \"gitdir:/x\"]\n\tpath = y\n",
            "[alias]\n\tst = !evil\n",
            "[extensions]\n\tpartialclone = origin\n",
            "[core \"sub\"]\n\tbare = false\n",
            "[remote \"o\"]\n\tuploadpack = evil\n",
            "[Core]\n\tPager = evil\n",
            "key = outside\n",
            "[core\n",
            "[core]\n\tbad key\n",
            // The round-4 reproduction: comments ending in a backslash.
            "[core]\n\tbare = false\n# c \\\n[filter \"evil\"]\n# c \\\n\tclean = evil\n",
            "[core]\n\tbare = false \\\n",
            "[core]\n\tbare = \"false\"\n",
            "[core]\r\n\tbare = false\r\n",
            "\u{feff}[core]\n\tbare = false\n",
            "[core]\n\tbare = fals\u{e9}\n",
            "[core]\n\tbare\n",
            "[core]\n\tbare =\tfalse\n",
            "[core.sub]\n\tbare = false\n",
            " [core]\n\tbare = false\n",
            "[core] # x\n\tbare = false\n",
            "[core]\n\tbare = false ; x\n",
            "[core]\n\tbare = false\0\n",
        ] {
            assert!(check_config(bad).is_err(), "{bad:?}");
        }
        let keys = check_config("[core]\n\tbare = false\n[extensions]\n\tworktreeConfig = true\n")
            .unwrap();
        let _ = keys;
        let entries =
            config_entries("[core]\n\tbare = false\n[extensions]\n\tworktreeConfig = true\n")
                .unwrap();
        assert_eq!(allowed_keys(&entries), Ok(true));
        assert!(check_config(&"[core]\n".repeat(MAX_CONFIG_LINES + 1)).is_err());
    }

    #[test]
    fn project_paths_map_to_and_from_the_repository() {
        let repo = Repo {
            root: PathBuf::from("/r"),
            git_dir: PathBuf::from("/r/.git"),
            prefix: "apps/web".to_owned(),
            facts: Facts::default(),
            shadow: None,
        };
        assert_eq!(repo.to_repo("src/a.rs"), "apps/web/src/a.rs");
        assert_eq!(
            repo.to_project("apps/web/src/a.rs").as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(repo.to_project("apps/webby/x"), None);
        assert_eq!(repo.to_project("apps/api/x"), None);
        assert_eq!(repo.to_project("apps/web"), None);
        assert_eq!(repo.scope(), "apps/web");
    }

    /// What git itself writes passes, and git reads the same keys.
    #[test]
    fn a_configuration_git_wrote_is_understood_and_git_agrees() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !git(&["init", "-q", "-b", "main"]) {
            return;
        }
        assert!(git(&[
            "remote",
            "add",
            "origin",
            "https://example.invalid/r.git"
        ]));
        assert!(git(&["config", "branch.main.remote", "origin"]));
        assert!(git(&["config", "user.name", "T"]));
        assert_eq!(check_config_file(&dir.join(".git/config")), Ok(false));
    }

    /// A small differential fuzz (seeded, reproducible): random texts made
    /// of plain and tricky lines. Whenever the check accepts one, the keys
    /// git reads from that file are exactly the check's and all allowed.
    #[test]
    fn whenever_the_check_accepts_git_reads_the_same_allowed_keys() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("config");
        std::fs::write(&file, "[core]\n\tbare = false\n").unwrap();
        if keys_by_git(&file).is_none() {
            return;
        }
        const PIECES: &[&str] = &[
            "[core]",
            "[remote \"origin\"]",
            "[branch \"main\"]",
            "[filter \"evil\"]",
            "[diff \"x\"]",
            "[core \"x\"]",
            "[Core]",
            "[core.x]",
            "\tbare = false",
            "\tfilemode = true",
            "\turl = https://example.invalid/x",
            "\tremote = origin",
            "\tclean = evil",
            "\ttextconv = evil",
            "\tfsmonitor = evil",
            "bare = false",
            "  bare = false",
            "# comment",
            "# comment \\",
            "; comment",
            "\tbare = false \\",
            "\tbare = \"false\"",
            "\tbare",
            "\tbare=false",
            "\tBare = false",
            "[include]",
            "\tpath = /dev/null",
            "",
        ];
        let mut seed: u64 = 0x5eed_1234_abcd_0042;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut accepted = 0;
        for _ in 0..300 {
            let lines = 1 + (next() % 7) as usize;
            let text: String = (0..lines)
                .map(|_| format!("{}\n", PIECES[(next() % PIECES.len() as u64) as usize]))
                .collect();
            std::fs::write(&file, &text).unwrap();
            if check_config_file(&file).is_ok() {
                accepted += 1;
                let theirs = keys_by_git(&file).unwrap();
                let ours: Vec<String> = {
                    let mut k: Vec<String> = config_keys(&text)
                        .unwrap()
                        .iter()
                        .map(|(s, sub, k)| listed(s, sub.as_deref(), k))
                        .collect();
                    k.sort();
                    k
                };
                assert_eq!(ours, theirs, "{text:?}");
                for key in &theirs {
                    let mut parts = key.split('.');
                    let section = parts.next().unwrap();
                    let name = key.rsplit('.').next().unwrap();
                    assert!(
                        ALLOWED
                            .iter()
                            .any(|(s, k)| *s == section && (*k == "*" || *k == name)),
                        "{key} accepted in {text:?}"
                    );
                }
            }
        }
        assert!(accepted > 10, "the fuzz accepted too little to mean much");
    }

    /// Round 10: git's home on both platforms, from the given environment.
    #[test]
    fn the_git_folder_walk_spends_on_empty_folders_and_deep_chains() {
        use crate::memory_host::Budget;
        let walk = |dir: &Path, cap: usize, ticks: usize| {
            let mut asked = 0;
            let mut clock = || {
                asked += 1;
                asked > ticks
            };
            walk_git_dir(dir, &mut Budget::new(cap, &mut clock))
        };
        let empty = tempfile::tempdir().unwrap();
        for i in 0..200 {
            std::fs::create_dir(empty.path().join(format!("d{i:03}"))).unwrap();
        }
        let deep = tempfile::tempdir().unwrap();
        let mut at = deep.path().to_owned();
        for _ in 0..60 {
            at = at.join("d");
            std::fs::create_dir(&at).unwrap();
        }
        for dir in [empty.path(), deep.path()] {
            assert!(walk(dir, usize::MAX, usize::MAX).is_ok(), "{dir:?}");
            // 200 empty folders: 401 units; the chain: 121.
            let why = |r: Result<(), Unusable>| match r {
                Err(Unusable::Refused(why)) => why,
                other => format!("{other:?}"),
            };
            let clock = why(walk(dir, usize::MAX, 100));
            assert!(clock.contains("within 5 seconds"), "{dir:?}: {clock}");
            let units = why(walk(dir, 100, usize::MAX));
            assert!(units.contains("too large"), "{dir:?}: {units}");
        }
    }

    #[test]
    fn the_repository_search_stops_at_gits_own_home() {
        let dir = tempfile::tempdir().unwrap();
        let home_dir = real_path(dir.path()).unwrap();
        let os = |pairs: &[(&str, &Path)]| -> Vec<(OsString, OsString)> {
            pairs
                .iter()
                .map(|(k, v)| (OsString::from(k), v.as_os_str().to_owned()))
                .collect()
        };
        assert_eq!(home(&os(&[("HOME", dir.path())])), Some(home_dir.clone()));
        // `USERPROFILE` is git's home only on Windows, when `HOME` and
        // `HOMEDRIVE`/`HOMEPATH` give none: the search follows git there too.
        let profile_only = home(&os(&[("USERPROFILE", dir.path())]));
        if cfg!(windows) {
            assert_eq!(profile_only, Some(home_dir));
        } else {
            assert_eq!(profile_only, None);
        }
    }

    #[test]
    fn git_home_follows_git_on_each_platform() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(OsString, OsString)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        let exists = |p: &Path| p == Path::new("C:\\Users\\me");
        let none = |_: &Path| false;
        let home = |e: &[(&str, &str)], windows: bool, is_dir: &dyn Fn(&Path) -> bool| {
            git_home(&env(e), windows, is_dir)
        };
        // Windows: USERPROFILE only.
        assert_eq!(
            home(&[("USERPROFILE", "C:\\Users\\me")], true, &none),
            Some(PathBuf::from("C:\\Users\\me"))
        );
        // HOME and USERPROFILE: HOME.
        assert_eq!(
            home(
                &[("HOME", "D:\\h"), ("USERPROFILE", "C:\\Users\\me")],
                true,
                &none
            ),
            Some(PathBuf::from("D:\\h"))
        );
        // An empty HOME is not set.
        assert_eq!(
            home(
                &[("HOME", ""), ("USERPROFILE", "C:\\Users\\me")],
                true,
                &none
            ),
            Some(PathBuf::from("C:\\Users\\me"))
        );
        // HOMEDRIVE + HOMEPATH when that folder exists...
        assert_eq!(
            home(
                &[
                    ("HOMEDRIVE", "C:"),
                    ("HOMEPATH", "\\Users\\me"),
                    ("USERPROFILE", "E:\\p")
                ],
                true,
                &exists
            ),
            Some(PathBuf::from("C:\\Users\\me"))
        );
        // ... else USERPROFILE; and never the system folder.
        assert_eq!(
            home(
                &[
                    ("HOMEDRIVE", "C:"),
                    ("HOMEPATH", "\\gone"),
                    ("USERPROFILE", "E:\\p")
                ],
                true,
                &exists
            ),
            Some(PathBuf::from("E:\\p"))
        );
        assert_eq!(
            home(
                &[
                    ("HOMEDRIVE", "C:"),
                    ("HOMEPATH", "\\Windows\\System32"),
                    ("USERPROFILE", "E:\\p")
                ],
                true,
                &|_| true
            ),
            Some(PathBuf::from("E:\\p"))
        );
        // None set: unknown.
        assert_eq!(home(&[], true, &|_| true), None);
        // Unix: HOME only; USERPROFILE does not count.
        assert_eq!(
            home(&[("HOME", "/h")], false, &none),
            Some(PathBuf::from("/h"))
        );
        assert_eq!(home(&[("USERPROFILE", "/p")], false, &|_| true), None);
        // The files follow the home, with the XDG fallback under it.
        let h = PathBuf::from("C:\\Users\\me");
        assert_eq!(
            global_files_for(&env(&[]), Some(&h)),
            Some(vec![
                h.join(".config").join("git").join("config"),
                h.join(".gitconfig")
            ])
        );
        assert_eq!(
            global_files_for(&env(&[("XDG_CONFIG_HOME", "")]), Some(&h)),
            Some(vec![
                h.join(".config").join("git").join("config"),
                h.join(".gitconfig")
            ])
        );
        assert_eq!(global_files_for(&env(&[]), None), None);
    }

    /// Round 10: when git's home cannot be determined, that is declared,
    /// never taken as "no global files".
    #[test]
    fn an_unknown_home_is_a_declared_limit() {
        let env: Vec<(OsString, OsString)> = inherited()
            .into_iter()
            .filter(|(k, _)| k != "HOME" && k != "XDG_CONFIG_HOME" && k != "USERPROFILE")
            .collect();
        if cfg!(windows) {
            return;
        }
        let (values, limits) = global_semantics(&env);
        assert!(values.is_empty());
        assert!(
            limits.iter().any(|l| l.contains("could not be discovered")),
            "{limits:?}"
        );
    }
}
