//! The project-memory engine's view of the running app (spec §5.1, D1): the
//! utility runner for model calls, read-only git, Pause, the clock, the
//! projects and their settings. Also the glue that hands the engine to the
//! UI, the bridge, the terminals' capture and Stop Everything.
//!
//! Nothing here runs on the UI thread: the engine calls the host from its own
//! threads.
//!
//! Git: only commands that read (`rev-parse --show-toplevel
//! --absolute-git-dir` to confirm the checked repository, `config --file
//! <file> --no-includes --list` to read one configuration file, `rev-parse`,
//! `status`, `hash-object` without `-w`, and for the initial build
//! `ls-files`, `ls-tree`, `rev-list`, `cat-file --batch`, `diff-tree` and
//! `diff-index`), with
//! optional locks off so `status` does not refresh the index either, after
//! the precedent in `connecto-packages/src/commit.rs`. The branch, HEAD and
//! local branches are read from the checked files, not by a command.
//! Nothing writes the repository.

use std::io::Read as _;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, Offset, Timelike};
use connecto_core::memory::{
    Capability, LocalDate, LocalTime, MemorySettings, ModelChoice, Provider, Weekday,
};
use connecto_core::{ToolKind, WorkspaceControl};
use connecto_memory::capture::Author;
use connecto_memory::{
    CaptureHandle, Captured, CapturedReply, ChangedFile, Commit, Engine, EngineConfig, GitHistory,
    GitSnapshot, Happened, Host, LoggedReply, ModelJob, ModelReply, OpenProject, PastConversation,
    PastConversations, PastTurn, ProjectFile, ProjectFiles, ReplySource, RunError, TurnRole,
    WallClock,
};
use connecto_session::Wake;

use crate::memory_git::{self, Repo, Unusable};
use connecto_session::launch::Layout;
use connecto_session::memory_capture::{
    CaptureEvent, CaptureReply, CaptureSink, CaptureWhat, PromptSource,
};
use connecto_session::utility_run::{self, UtilityJob, UtilityPurpose, UtilityRunner};
use connecto_workspace::Workspace;

/// Changed files whose blob is hashed per snapshot, at most.
const MAX_HASHED: usize = 500;
/// Largest piece of a session log read to find a dropped reply, and the most
/// read in all.
const LOG_FIRST_PIECE: u64 = 1024 * 1024;
const LOG_MOST: u64 = 32 * 1024 * 1024;

pub struct MemoryHost {
    /// The private git folders ([`crate::memory_private`]), owned here.
    private: Option<Arc<crate::memory_private::PrivateGit>>,
    layout: Layout,
    /// Set once the workspace exists; until then there are no projects.
    workspace: OnceLock<Arc<Workspace>>,
    runner: UtilityRunner,
    wake: Wake,
}

impl MemoryHost {
    fn project_path(&self, project_id: &str) -> Option<PathBuf> {
        let view = self.workspace.get()?.view();
        view.projects
            .iter()
            .find(|p| p.id == project_id)
            .map(|p| PathBuf::from(&p.path))
    }
}

/// Most bytes of one git command's output kept. Past it the command counts
/// as failed: an output is never cut and used in part.
const GIT_MAX_OUTPUT: usize = 32 * 1024 * 1024;

/// The project's repository, located and checked by [`memory_git`] (its
/// layout and configuration read first, then git asked to confirm); `None`
/// when git is not used for this project.
fn repo_of(project: &Path) -> Option<Repo> {
    memory_git::bind(project, &memory_git::inherited()).ok()
}

/// Why git is not used for a project, when a repository was found but
/// refused; `None` when it is used, or when there is none.
pub fn git_refusal(project: &Path) -> Option<String> {
    match memory_git::bind(project, &memory_git::inherited()) {
        Err(Unusable::Refused(why)) => Some(why),
        Err(Unusable::NotARepository) => None,
        // Bound, but a command failed recently: declared too.
        Ok(repo) => memory_git::recent_failure(&repo)
            .map(|what| format!("{}{what}", connecto_memory::host::GIT_FAILED)),
    }
}

#[cfg(test)]
thread_local! {
    /// A command the tests make fail on their own thread, to check the
    /// fallbacks.
    static FAIL: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// Whether a test made `what` fail (always `false` outside tests).
fn injected(what: &str) -> bool {
    #[cfg(test)]
    {
        FAIL.with(|f| f.get() == Some(what))
    }
    #[cfg(not(test))]
    {
        let _ = what;
        false
    }
}

/// Run git with input for a bound repository, as bytes; a failure (or an
/// output git did not finish) is recorded like [`run_in`]'s.
fn run_bytes_in(repo: &Repo, args: &[&str], input: Vec<u8>, max: usize) -> Option<Vec<u8>> {
    let what = args.first().copied().unwrap_or("git");
    let out = if injected(what) {
        None
    } else {
        memory_git::run_bytes(
            memory_git::command(repo, &memory_git::inherited()).args(args),
            Some(input),
            max,
        )
        .filter(|(_, ok)| *ok)
        .map(|(out, _)| out)
    };
    if out.is_none() {
        memory_git::record_failure(repo, what);
    }
    out
}

/// What of the user's global git configuration was not taken for the
/// project's repository (see [`memory_git::global_semantics`]).
pub fn git_limits(project: &Path) -> Vec<String> {
    repo_of(project).map(|r| r.facts.limits).unwrap_or_default()
}

/// Run git for a checked repository; its stdout, or `None` when it failed.
/// A command that fails (or whose output is over its cap) is recorded for
/// the repository: the build falls back where it can and declares it.
fn run_in(repo: &Repo, args: &[&str], max: usize) -> Option<String> {
    let what = args.first().copied().unwrap_or("git");
    if injected(what) {
        memory_git::record_failure(repo, what);
        return None;
    }
    let out = memory_git::run(
        memory_git::command(repo, &memory_git::inherited()).args(args),
        max,
    )
    .filter(|(_, ok)| *ok)
    .map(|(out, _)| out);
    if out.is_none() {
        memory_git::record_failure(repo, what);
    }
    out
}

/// Blob hashes of `paths` (project-relative), in chunks; `None` for a
/// file whose hash is not known (its chunk failed, no repository). Never a
/// claim that a file is gone.
fn hash_files(project: &Path, paths: &[String]) -> Vec<Option<String>> {
    let Some(repo) = repo_of(project) else {
        return vec![None; paths.len()];
    };
    let mut out = Vec::with_capacity(paths.len());
    for chunk in paths.chunks(100) {
        let repo_paths: Vec<String> = chunk.iter().map(|p| repo.to_repo(p)).collect();
        let mut args = vec!["hash-object", "--no-filters", "--"];
        args.extend(repo_paths.iter().map(String::as_str));
        let lines: Vec<String> = run_in(&repo, &args, GIT_MAX_OUTPUT)
            .map(|o| o.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        if lines.len() == chunk.len() {
            out.extend(lines.into_iter().map(Some));
        } else {
            out.extend(std::iter::repeat_n(None, chunk.len()));
        }
    }
    out
}

/// What the file system says of a path: present as a file, confirmed gone,
/// or something else (a folder, a submodule, an error): unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnDisk {
    File,
    Gone,
    Unknown,
}

fn on_disk(project: &Path, path: &str) -> OnDisk {
    // A path through a folder link is not hashed (`hash-object` would
    // follow it): unknown.
    if !connecto_core::memory::is_project_relative(path) || has_link(project, path) {
        return OnDisk::Unknown;
    }
    match std::fs::symlink_metadata(project.join(path)) {
        Ok(m) if m.is_file() => OnDisk::File,
        Ok(_) => OnDisk::Unknown,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OnDisk::Gone,
        Err(_) => OnDisk::Unknown,
    }
}

/// Hash what can be hashed (at most `cap` files); a file over the cap or
/// whose hash failed is unknown (`blob: None, deleted: false`); only a file
/// confirmed gone is `deleted`.
fn observe_files(project: &Path, paths: Vec<String>, cap: usize) -> Vec<ChangedFile> {
    let state: Vec<OnDisk> = paths.iter().map(|p| on_disk(project, p)).collect();
    let to_hash: Vec<String> = paths
        .iter()
        .zip(&state)
        .filter(|(_, s)| **s == OnDisk::File)
        .map(|(p, _)| p.clone())
        .take(cap)
        .collect();
    let blobs = hash_files(project, &to_hash);
    paths
        .into_iter()
        .zip(state)
        .map(|(path, state)| {
            let blob = to_hash
                .iter()
                .position(|p| *p == path)
                .and_then(|i| blobs.get(i).cloned().flatten());
            ChangedFile {
                deleted: state == OnDisk::Gone,
                blob,
                path,
            }
        })
        .collect()
}

/// Parse `git status --porcelain=v1 -z --no-renames`.
fn parse_status(out: &str) -> Vec<(String, bool)> {
    out.split('\0')
        .filter(|e| e.len() > 3)
        .map(|e| {
            let (code, path) = e.split_at(3);
            let deleted = code.contains('D');
            (path.to_owned(), deleted)
        })
        .collect()
}

/// The git state of `project`, read-only: of its own folder only when it
/// is a subfolder of a repository.
pub fn git_snapshot(project: &Path) -> Option<GitSnapshot> {
    let repo = repo_of(project)?;
    // Branch and HEAD as the checks read them (git sees a detached HEAD).
    let branch = repo.facts.branch.clone();
    let head = repo.facts.head.clone();
    let status = run_in(
        &repo,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=all",
            "--ignore-submodules=all",
            "--",
            repo.scope(),
        ],
        GIT_MAX_OUTPUT,
    )?;
    // The status code is not trusted for deletion: the file system says
    // whether a path is gone. Paths outside the project are dropped.
    let paths: Vec<String> = parse_status(&status)
        .into_iter()
        .filter_map(|(p, _)| repo.to_project(&p))
        .collect();
    let mut changed = observe_files(project, paths, MAX_HASHED);
    changed.sort_by(|a, b| a.path.cmp(&b.path));
    Some(GitSnapshot {
        branch,
        head,
        changed,
    })
}

fn map_error(e: utility_run::RunError) -> RunError {
    use utility_run::RunError as U;
    match e {
        U::Timeout(_) => RunError::Timeout,
        U::Cancelled => RunError::Cancelled,
        U::Unavailable(why) => RunError::Unavailable(why),
        U::Untrusted(reason) => RunError::HeldUntrusted { reason },
        U::Provider(_) => RunError::ModelError,
        // Includes `SubmitUncertain` (the prompt may or may not have been
        // submitted) and `ReplyUnavailable` (cut or aborted): a failed
        // attempt. Whatever such a session did has no effect: only a reply
        // the host validates is ever stored.
        other => RunError::Failed(other.to_string()),
    }
}

/// Values in the app's environment that look like secrets (`*_API_KEY`,
/// `*_TOKEN`, `*_SECRET`, `*PASSWORD*`), removed first by redaction. The
/// vault's values are added by `known_secrets` from what the vault already
/// holds in memory; Connecto's own keys (device, relay, Jev) are not read.
fn env_secrets() -> Vec<String> {
    std::env::vars()
        .filter(|(k, v)| {
            let k = k.to_ascii_uppercase();
            v.len() >= 8
                && (k.ends_with("_API_KEY")
                    || k.ends_with("_TOKEN")
                    || k.ends_with("_SECRET")
                    || k.contains("PASSWORD"))
        })
        .map(|(_, v)| v)
        .collect()
}

/// Tries at reading a dropped reply, and the wait between them: the log is
/// written a little after the turn-end hook.
const LOG_TRIES: usize = 4;
const LOG_LAG: Duration = Duration::from_millis(150);

/// The reply of the turn that answers `sent`, read from the end of a session
/// log in growing pieces: only a turn whose prompt matches `sent` and whose
/// final response is marked complete, with no reported error. Retried a few
/// times for log-write lag; otherwise `None` (content not available).
/// Without `sent` nothing can be matched: `None`.
fn logged_reply(provider: Provider, log: &Path, sent: Option<&str>) -> Option<LoggedReply> {
    let sent = sent.filter(|s| !s.trim().is_empty())?;
    for attempt in 0..LOG_TRIES {
        if attempt > 0 {
            std::thread::sleep(LOG_LAG);
        }
        if let Some(found) = logged_reply_once(provider, log, sent) {
            return Some(found);
        }
    }
    None
}

fn logged_reply_once(provider: Provider, log: &Path, sent: &str) -> Option<LoggedReply> {
    use std::io::{Seek, SeekFrom};
    let tool = connecto_session::launch::adapter(utility_run::tool_of(provider)).utility()?;
    let mut file = std::fs::File::open(log).ok()?;
    let len = file.metadata().ok()?.len();
    let mut piece = LOG_FIRST_PIECE;
    loop {
        let start = len.saturating_sub(piece);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = Vec::new();
        (&mut file).take(piece).read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        // Whole records only: drop the first partial line.
        let text = if start > 0 {
            text.split_once('\n').map_or("", |(_, rest)| rest)
        } else {
            &text
        };
        let turn = tool.read_turn(text, sent);
        if turn.prompt_whole
            && turn.complete
            && turn.error.is_none()
            && let Some(reply) = turn.reply
        {
            return Some(LoggedReply {
                text: reply,
                offset: start,
            });
        }
        if start == 0 || piece >= LOG_MOST {
            return None;
        }
        piece = piece.saturating_mul(2).min(LOG_MOST);
    }
}

// ---- The initial build's reads (T9b). Read-only: listings, file reads,
// `ls-files`, `ls-tree`, `rev-list`, `cat-file`, `diff-tree`, `diff-index`;
// nothing is written anywhere.

/// Largest transcript read from the history, in bytes (what the history
/// keeps at most).
const MAX_TRANSCRIPT: u64 = 2 * 1024 * 1024;

/// A regular file (not a link) under `project`, and its size.
fn regular_size(project: &Path, path: &str) -> Option<u64> {
    let meta = std::fs::symlink_metadata(project.join(path)).ok()?;
    meta.is_file().then_some(meta.len())
}

/// The project's files: with git, tracked plus untracked not ignored;
/// without, a walk that follows no link and skips dependency and build
/// folders.
pub fn project_files(project: &Path, max: usize) -> Option<ProjectFiles> {
    let started = Instant::now();
    let mut clock = || started.elapsed() > MAX_WALK;
    project_files_within(project, max, &mut Budget::new(MAX_WALK_UNITS, &mut clock))
}

/// Units of work the walk without git may do (folders opened, entries read,
/// entries processed), at most.
const MAX_WALK_UNITS: usize = 1_000_000;

/// The one allowance of a walk: every unit of work (opening a folder,
/// reading an entry, processing an entry) spends from it first, so no path
/// can work without being counted and timed.
pub(crate) struct Budget<'a> {
    units: usize,
    cap: usize,
    out_of_time: &'a mut dyn FnMut() -> bool,
    timed_out: bool,
}

impl<'a> Budget<'a> {
    pub(crate) fn new(cap: usize, out_of_time: &'a mut dyn FnMut() -> bool) -> Self {
        Self {
            units: 0,
            cap,
            out_of_time,
            timed_out: false,
        }
    }

    /// Whether a refusal came from the clock (else from the unit cap).
    pub(crate) const fn timed_out(&self) -> bool {
        self.timed_out
    }

    /// One unit of work; `false` when the cap or the time is used up (the
    /// caller stops the whole walk and declares it incomplete).
    pub(crate) fn spend(&mut self) -> bool {
        if self.units >= self.cap {
            return false;
        }
        if (self.out_of_time)() {
            self.timed_out = true;
            return false;
        }
        self.units += 1;
        true
    }
}

/// Longest the walk without git may take: past it the listing stops and is
/// marked incomplete (`truncated`, declared by the build as the input-limit
/// gap), never silently short. Same shape as the `.git` walk's bound.
const MAX_WALK: Duration = Duration::from_secs(10);

/// [`project_files`], the walk without git spending from `budget` before
/// each folder it opens, each entry it reads and each entry it processes;
/// at most `max` entries are taken from one folder before they are sorted.
/// When the budget refuses or a cap is hit the whole walk stops and the
/// listing is `truncated`: what is returned is a part, never presented as
/// whole.
fn project_files_within(
    project: &Path,
    max: usize,
    budget: &mut Budget<'_>,
) -> Option<ProjectFiles> {
    let mut out = ProjectFiles::default();
    let repo = repo_of(project);
    let mut bounds = Boundaries::new(project, repo.as_ref());
    // A failed git listing is never "no files": the walker lists them, and
    // the failure is declared (it was recorded).
    let listed = repo.as_ref().and_then(|repo| {
        run_in(
            repo,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
                "--",
                repo.scope(),
            ],
            GIT_MAX_OUTPUT,
        )
    });
    if let (Some(repo), Some(list)) = (repo, listed) {
        let mut seen = std::collections::BTreeSet::new();
        for path in list
            .split('\0')
            .filter(|p| !p.is_empty())
            .filter_map(|p| repo.to_project(p))
        {
            if !seen.insert(path.clone()) || bounds.inside_nested(&path) {
                continue;
            }
            // A gitlink (a submodule's entry) is a folder: never entered.
            if std::fs::symlink_metadata(project.join(&path)).is_ok_and(|m| m.is_dir()) {
                bounds.nested.insert(path);
                continue;
            }
            if out.files.len() >= max {
                out.truncated = true;
                break;
            }
            if let Some(bytes) = regular_size(project, &path) {
                out.files.push(ProjectFile { path, bytes });
            }
        }
        out.nested = bounds.nested.into_iter().collect();
        return Some(out);
    }
    std::fs::read_dir(project).ok()?;
    let mut stack: Vec<String> = vec![String::new()];
    'walk: while let Some(dir) = stack.pop() {
        if !budget.spend() {
            out.truncated = true;
            break;
        }
        let Ok(entries) = std::fs::read_dir(project.join(&dir)) else {
            continue;
        };
        let mut names: Vec<(String, std::fs::FileType)> = Vec::new();
        for e in entries {
            if !budget.spend() || names.len() >= max {
                out.truncated = true;
                break 'walk;
            }
            let Ok(e) = e else { continue };
            if let (Some(name), Ok(kind)) = (e.file_name().to_str(), e.file_type()) {
                names.push((name.to_owned(), kind));
            }
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, kind) in names {
            if !budget.spend() {
                out.truncated = true;
                break 'walk;
            }
            let rel = if dir.is_empty() {
                name.clone()
            } else {
                format!("{dir}/{name}")
            };
            // `DirEntry::file_type` does not follow a link.
            if kind.is_dir() {
                if connecto_memory::bootstrap_inventory::IGNORED_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if bounds.is_boundary(&rel) {
                    bounds.nested.insert(rel);
                } else {
                    stack.push(rel);
                }
            } else if kind.is_file() {
                if out.files.len() >= max {
                    out.truncated = true;
                    break 'walk;
                }
                if let Some(bytes) = regular_size(project, &rel) {
                    out.files.push(ProjectFile { path: rel, bytes });
                }
            }
        }
        if out.truncated {
            break;
        }
    }
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    out.nested = bounds.nested.into_iter().collect();
    Some(out)
}

/// The folders of a project that are other repositories: a folder holding
/// its own `.git` (a file or a folder), or one `.gitmodules` names
/// (submodules). Nothing under them is listed or read.
struct Boundaries<'a> {
    project: &'a Path,
    /// Submodule paths from `.gitmodules`, relative to the project.
    submodules: std::collections::BTreeSet<String>,
    known: std::collections::BTreeMap<String, bool>,
    nested: std::collections::BTreeSet<String>,
}

impl<'a> Boundaries<'a> {
    fn new(project: &'a Path, repo: Option<&Repo>) -> Self {
        // `.gitmodules` at the repository's root (or the project's): its
        // `path = …` lines, read as data.
        let root = repo.map_or_else(|| project.to_owned(), |r| r.root.clone());
        let map = |p: &str| match repo {
            Some(r) => r.to_project(p),
            None => Some(p.to_owned()),
        };
        let mut submodules = std::collections::BTreeSet::new();
        if let Some(bytes) = read_no_follow(&root, ".gitmodules", 1024 * 1024) {
            for line in String::from_utf8_lossy(&bytes).lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("path")
                    && let Some(value) = rest.trim_start().strip_prefix('=')
                    && let Some(path) = map(value.trim().trim_matches('"').trim_end_matches('/'))
                {
                    submodules.insert(path);
                }
            }
        }
        Self {
            project,
            submodules,
            known: std::collections::BTreeMap::new(),
            nested: std::collections::BTreeSet::new(),
        }
    }

    /// Whether the folder `rel` (relative to the project, not its root) is
    /// another repository.
    fn is_boundary(&mut self, rel: &str) -> bool {
        if let Some(known) = self.known.get(rel) {
            return *known;
        }
        let found = self.submodules.contains(rel)
            || std::fs::symlink_metadata(self.project.join(rel).join(".git")).is_ok();
        self.known.insert(rel.to_owned(), found);
        found
    }

    /// Whether `path` lies under another repository; that folder is noted.
    fn inside_nested(&mut self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        for n in 1..parts.len() {
            let folder = parts[..n].join("/");
            if self.is_boundary(&folder) {
                self.nested.insert(folder);
                return true;
            }
        }
        false
    }
}

/// Whether any component of `path` under `project` is a link (or a Windows
/// reparse point): such a path is never read or hashed, so a folder link
/// cannot alias an excluded or outside file.
fn has_link(project: &Path, path: &str) -> bool {
    let mut at = project.to_owned();
    for part in path.split('/') {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

/// Whether project file contents are read for the initial build here: only
/// where every component is opened from its parent's handle without
/// following a link (Unix).
pub const FILE_READS: bool = cfg!(unix);

/// One project file's text: a regular file inside the project, at most
/// `max_bytes`, UTF-8 without NUL bytes. No link is followed anywhere in the
/// path: on Unix every component is opened relative to its parent's handle
/// with `O_NOFOLLOW` and the file is read through its own handle, so a swap
/// during the read cannot redirect it.
pub fn read_project_file(project: &Path, path: &str, max_bytes: u64) -> Option<String> {
    if !connecto_core::memory::is_project_relative(path) {
        return None;
    }
    let bytes = read_no_follow(project, path, max_bytes)?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(unix)]
pub(crate) fn read_no_follow(project: &Path, path: &str, max_bytes: u64) -> Option<Vec<u8>> {
    use nix::dir::Dir;
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::{Mode, SFlag, fstat};
    use std::os::fd::{AsRawFd, RawFd};

    /// Closes the file's descriptor when dropped.
    struct Fd(RawFd);
    impl Drop for Fd {
        fn drop(&mut self) {
            let _ = nix::unistd::close(self.0);
        }
    }

    let dir_flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    let mut dir = Dir::open(project, dir_flags, Mode::empty()).ok()?;
    let parts: Vec<&str> = path.split('/').collect();
    let (name, folders) = parts.split_last()?;
    for part in folders {
        dir = Dir::openat(
            Some(dir.as_raw_fd()),
            *part,
            dir_flags | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .ok()?;
    }
    let fd = Fd(openat(
        Some(dir.as_raw_fd()),
        *name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .ok()?);
    let stat = fstat(fd.0).ok()?;
    let regular = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFREG;
    if !regular || u64::try_from(stat.st_size).ok()? > max_bytes {
        return None;
    }
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = nix::unistd::read(fd.0, &mut buf).ok()?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(buf.get(..n)?);
        if out.len() as u64 > max_bytes {
            return None;
        }
    }
    Some(out)
}

/// Off Unix no handle-relative open without following links is available
/// with the safe APIs in the tree: the build reads no file content there
/// ([`FILE_READS`]) and declares it.
#[cfg(not(unix))]
pub(crate) fn read_no_follow(project: &Path, path: &str, max_bytes: u64) -> Option<Vec<u8>> {
    let _ = (project, path, max_bytes);
    None
}

/// Commits since `since_ms` that touch the project, newest first, at most
/// `max`, with the project's paths each changed (others dropped).
///
/// Authored text and observed facts never share a stream: the ids come
/// from `rev-list` (hexadecimal only, checked); each commit's committer time
/// and message from `cat-file --batch`, framed by the object's byte size
/// and split at its first blank line; the changed paths from `diff-tree
/// --stdin -z --name-status`, NUL-framed records where a path only ever
/// follows a one-letter status. No byte a commit's author wrote can make a
/// hash, a time or a changed path.
pub fn git_history(project: &Path, since_ms: u64, max: usize) -> Option<GitHistory> {
    let repo = repo_of(project)?;
    if run_in(&repo, &["rev-parse", "--verify", "--quiet", "HEAD"], 4096).is_none() {
        return Some(GitHistory::default());
    }
    // Git reads `@<seconds>` as a time only for large numbers (a small one
    // would be taken for something else): before 1973 means "no window".
    let secs = since_ms / 1000;
    let windowed = secs >= 100_000_000;
    let since = format!("--since=@{secs}");
    let before = format!("--before=@{secs}");
    let count = format!("-n{}", max.saturating_add(1));
    let mut args = vec!["rev-list", count.as_str()];
    if windowed {
        args.push(since.as_str());
    }
    args.extend(["HEAD", "--", repo.scope()]);
    let listed = run_in(&repo, &args, GIT_MAX_OUTPUT)?;
    let mut ids: Vec<String> = Vec::new();
    for line in listed.lines() {
        if !matches!(line.len(), 40 | 64) || !line.bytes().all(|b| b.is_ascii_hexdigit()) {
            memory_git::record_failure(&repo, "rev-list (output not understood)");
            return None;
        }
        ids.push(line.to_ascii_lowercase());
    }
    let mut history = GitHistory::default();
    if ids.len() > max {
        ids.truncate(max);
        history.older = true;
    }
    history.older |= windowed
        && run_in(
            &repo,
            &["rev-list", "-n1", &before, "HEAD", "--", repo.scope()],
            4096,
        )
        .is_some_and(|o| !o.trim().is_empty());
    if ids.is_empty() {
        return Some(history);
    }
    let input: Vec<u8> = ids
        .iter()
        .flat_map(|id| format!("{id}\n").into_bytes())
        .collect();
    let objects = run_bytes_in(
        &repo,
        &["cat-file", "--batch"],
        input.clone(),
        MAX_LOG_OUTPUT,
    )?;
    let Some(commits) = parse_batch(&objects, &ids) else {
        memory_git::record_failure(&repo, "cat-file (output not understood)");
        return None;
    };
    let changes = run_bytes_in(
        &repo,
        &[
            "diff-tree",
            "--stdin",
            "-z",
            "-r",
            "--root",
            "--no-renames",
            "--name-status",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "--",
            repo.scope(),
        ],
        input,
        MAX_LOG_OUTPUT,
    )?;
    let Some(mut files) = parse_diff_tree(&changes) else {
        memory_git::record_failure(&repo, "diff-tree (output not understood)");
        return None;
    };
    for (id, (time_ms, subject, body)) in ids.iter().zip(commits) {
        history.commits.push(Commit {
            hash: id.clone(),
            time_ms,
            subject,
            body,
            files: files
                .remove(id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| repo.to_project(&p))
                .filter(|p| connecto_core::memory::is_project_relative(p))
                .collect(),
        });
    }
    Some(history)
}

/// `cat-file --batch` output for `ids`, in order: each `<id> commit
/// <size>\n`, exactly `size` bytes, `\n`. Returns each commit's committer
/// time (ms), subject and body; `None` for anything else.
fn parse_batch(out: &[u8], ids: &[String]) -> Option<Vec<(u64, String, String)>> {
    let mut rest = out;
    let mut commits = Vec::with_capacity(ids.len());
    for id in ids {
        let nl = rest.iter().position(|b| *b == b'\n')?;
        let header = std::str::from_utf8(&rest[..nl]).ok()?;
        let mut fields = header.split(' ');
        let (Some(got), Some("commit"), Some(size), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
        if got != id {
            return None;
        }
        let size: usize = size.parse().ok()?;
        let body = rest.get(nl + 1..nl + 1 + size)?;
        if rest.get(nl + 1 + size) != Some(&b'\n') {
            return None;
        }
        rest = &rest[nl + 2 + size..];
        // Headers end at the first blank line; the message follows.
        let split = body.windows(2).position(|w| w == b"\n\n")?;
        let headers = std::str::from_utf8(&body[..split]).ok()?;
        let message = String::from_utf8_lossy(&body[split + 2..]);
        let committer = headers.lines().find_map(|l| l.strip_prefix("committer "))?;
        let mut tail = committer.rsplit(' ');
        let (_tz, secs) = (tail.next()?, tail.next()?);
        let time_ms = secs.parse::<u64>().ok()?.saturating_mul(1000);
        let (subject, rest_body) = message.split_once('\n').unwrap_or((&message, ""));
        commits.push((time_ms, subject.to_owned(), rest_body.trim().to_owned()));
    }
    rest.is_empty().then_some(commits)
}

/// `diff-tree --stdin -z --name-status` output: `<id>\0` then
/// `<status>\0<path>\0` records. A path is only ever read right after a
/// one-letter status, so no path can be taken for an id or a status.
fn parse_diff_tree(out: &[u8]) -> Option<std::collections::BTreeMap<String, Vec<String>>> {
    let mut by_commit: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut current: Option<String> = None;
    let mut tokens = out.split(|b| *b == 0).filter(|t| !t.is_empty());
    while let Some(token) = tokens.next() {
        let text = std::str::from_utf8(token).ok()?;
        let is_status = text.len() == 1 && text.bytes().all(|b| b.is_ascii_uppercase());
        if is_status {
            let path = std::str::from_utf8(tokens.next()?).ok()?;
            by_commit
                .entry(current.clone()?)
                .or_default()
                .push(path.to_owned());
        } else if matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            current = Some(text.to_ascii_lowercase());
        } else {
            return None;
        }
    }
    Some(by_commit)
}

/// A diff whose paths are the repository's, with the project's own as the
/// project names them (`a/<prefix>/x` to `a/x`); a header or file line of
/// a path outside the project is left for the caller's hunk filter to drop.
fn diff_to_project(repo: &Repo, diff: &str) -> String {
    if repo.prefix.is_empty() {
        return diff.to_owned();
    }
    let p = &repo.prefix;
    diff.split_inclusive('\n')
        .map(|line| {
            if let Some(rest) = line.strip_prefix("diff --git ") {
                format!(
                    "diff --git {}",
                    rest.replacen(&format!("a/{p}/"), "a/", 1).replacen(
                        &format!(" b/{p}/"),
                        " b/",
                        1
                    )
                )
            } else if let Some(rest) = line.strip_prefix(&format!("--- a/{p}/")) {
                format!("--- a/{rest}")
            } else if let Some(rest) = line.strip_prefix(&format!("+++ b/{p}/")) {
                format!("+++ b/{rest}")
            } else {
                line.to_owned()
            }
        })
        .collect()
}

/// The uncommitted diff of `paths` (project-relative) against HEAD; `None`
/// past `max_bytes` (never cut). Only the paths given reach git, and of
/// those only the ones whose committed version is a blob of at most
/// `max_file_bytes` (or that HEAD does not have): an excluded file's
/// content, old or new, is never read for it.
pub fn git_diff(
    project: &Path,
    paths: &[String],
    max_file_bytes: u64,
    max_bytes: u64,
) -> Option<String> {
    let repo = repo_of(project)?;
    run_in(&repo, &["rev-parse", "--verify", "--quiet", "HEAD"], 4096)?;
    if paths.is_empty() {
        return Some(String::new());
    }
    let repo_paths: Vec<String> = paths
        .iter()
        .filter(|p| connecto_core::memory::is_project_relative(p))
        .map(|p| repo.to_repo(p))
        .collect();
    // Committed sizes: `<mode> <type> <id> <size>\t<path>`.
    let mut args = vec!["ls-tree", "-l", "-z", "HEAD", "--"];
    args.extend(repo_paths.iter().map(String::as_str));
    let tree = run_in(&repo, &args, GIT_MAX_OUTPUT)?;
    let mut committed: std::collections::BTreeMap<&str, Option<u64>> =
        std::collections::BTreeMap::new();
    for entry in tree.split('\0').filter(|e| !e.is_empty()) {
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let size = (fields.get(1) == Some(&"blob"))
            .then(|| fields.get(3).and_then(|n| n.parse::<u64>().ok()))
            .flatten();
        committed.insert(path, size);
    }
    let allowed: Vec<&str> = repo_paths
        .iter()
        .map(String::as_str)
        .filter(|p| match committed.get(p) {
            None => true,
            Some(size) => size.is_some_and(|n| n <= max_file_bytes),
        })
        .collect();
    if allowed.is_empty() {
        return Some(String::new());
    }
    // The work tree against HEAD, plumbing with a fixed format.
    let mut args = vec![
        "diff-index",
        "-p",
        "--ignore-submodules=all",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "HEAD",
        "--",
    ];
    args.extend(allowed);
    let diff = run_in(
        &repo,
        &args,
        usize::try_from(max_bytes).unwrap_or(usize::MAX),
    )?;
    Some(diff_to_project(&repo, &diff))
}

/// Most bytes of `git log` output read for the history window.
const MAX_LOG_OUTPUT: usize = 16 * 1024 * 1024;
/// History entries looked at, at most (the history keeps 500).
const MAX_HISTORY_ENTRIES: usize = 500;

/// Most of one session log read, from its end, in bytes.
const MAX_LOG_TAIL: u64 = 8 * 1024 * 1024;
/// Most of all session logs read for one build, in bytes.
const MAX_LOGS_READ: u64 = 64 * 1024 * 1024;

/// The last `max` bytes of a regular file, whole lines only. Never the whole
/// of a large file.
fn read_tail(path: &Path, max: u64) -> Option<(String, bool)> {
    use std::io::{Seek, SeekFrom};
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let start = meta.len().saturating_sub(max);
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let text = if start > 0 {
        text.split_once('\n')
            .map_or_else(String::new, |(_, rest)| rest.to_owned())
    } else {
        text
    };
    Some((text, start == 0))
}

fn rfc3339_ms(text: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
}

/// A closed terminal's own session log: in any generation of the project's
/// profile for that tool (`<data>/projects/<id>/profiles/<tool>/g<n>/`),
/// newest generation first.
fn find_session_log(
    data_root: &Path,
    project_id: &str,
    tool: ToolKind,
    session: &str,
) -> Option<PathBuf> {
    let root = data_root
        .join("projects")
        .join(project_id)
        .join("profiles")
        .join(tool.name());
    let mut generations: Vec<(u64, PathBuf)> = std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|k| k.is_dir()))
        .filter_map(|e| {
            let n = e.file_name().to_str()?.strip_prefix('g')?.parse().ok()?;
            Some((n, e.path()))
        })
        .collect();
    generations.sort_by_key(|g| std::cmp::Reverse(g.0));
    generations
        .iter()
        .find_map(|(_, profile)| connecto_adapter::session_log(tool, profile, session))
}

/// The turns of a closed terminal from its tool's own log, with roles; `None`
/// when the log cannot be found, read or parsed (the caller falls back to the
/// screen transcript). `child`: another agent started the terminal, so its
/// opening message was that agent's (history records `parent`, nothing
/// else about how input arrived).
fn logged_turns(
    log: &Path,
    tool: ToolKind,
    child: bool,
    since_ms: u64,
    budget: &mut u64,
) -> Option<(Vec<PastTurn>, bool)> {
    if *budget == 0 {
        return None;
    }
    let want = MAX_LOG_TAIL.min(*budget);
    let (text, whole) = read_tail(log, want)?;
    // Charged for what was read, the dropped partial line included.
    *budget = budget.saturating_sub(if whole { text.len() as u64 } else { want });
    let exchanges = connecto_adapter::logged_exchanges(tool, &text);
    if exchanges.is_empty() {
        return None;
    }
    let mut turns = Vec::new();
    for (i, e) in exchanges.into_iter().enumerate() {
        let prompt_at = e.prompt_at.as_deref().and_then(rfc3339_ms);
        if prompt_at.is_some_and(|t| t < since_ms) {
            continue;
        }
        let opening = whole && i == 0;
        turns.push(PastTurn {
            // Not recognised as typed by a person, or the opening task
            // another agent gave: never the user's.
            role: if !e.typed || (child && opening) {
                TurnRole::Injected
            } else {
                TurnRole::User
            },
            text: e.prompt,
            at_ms: prompt_at,
        });
        turns.push(PastTurn {
            role: TurnRole::Agent,
            text: e.reply,
            at_ms: e.reply_at.as_deref().and_then(rfc3339_ms),
        });
    }
    Some((turns, whole))
}

/// The project's closed terminals from Connecto's history
/// (`<data>/projects/<id>/history/`). Each is read from its tool's own
/// session log, with roles, when the log is found; otherwise from the
/// redacted screen transcript, whose roles cannot be told apart.
pub fn past_conversations(
    data_root: &Path,
    project_id: &str,
    since_ms: u64,
) -> Option<PastConversations> {
    let safe = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && s != "."
            && s != ".."
    };
    if !safe(project_id) {
        return None;
    }
    let dir = data_root.join("projects").join(project_id).join("history");
    let text = match std::fs::read_to_string(dir.join("index.json")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(PastConversations::default());
        }
        Err(_) => return None,
    };
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut out = PastConversations::default();
    // One budget for session logs and screen transcripts together.
    let mut budget = MAX_LOGS_READ;
    let entries = value["entries"].as_array()?;
    if entries.len() > MAX_HISTORY_ENTRIES {
        out.truncated = true;
    }
    for item in entries.iter().take(MAX_HISTORY_ENTRIES) {
        let Some(id) = item["id"].as_str().filter(|id| {
            (1..=80).contains(&id.len())
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }) else {
            continue;
        };
        let ended = item["closed_at_ms"].as_u64().unwrap_or(0);
        if ended < since_ms {
            out.older = true;
            continue;
        }
        let tool = match item["tool"].as_str() {
            Some("claude") => Some(ToolKind::Claude),
            Some("codex") => Some(ToolKind::Codex),
            _ => None,
        };
        let session = item["session"].as_str();
        let child = item["parent"].as_str().is_some();
        let log = tool
            .zip(session)
            .and_then(|(t, s)| find_session_log(data_root, project_id, t, s).map(|l| (t, l)));
        if budget == 0 {
            out.truncated = true;
            continue;
        }
        let from_log = log
            .as_ref()
            .and_then(|(t, l)| logged_turns(l, *t, child, since_ms, &mut budget))
            .map(|(turns, whole)| {
                // Only the end of a large log was read.
                out.truncated |= !whole;
                turns
            });
        let turns = if let Some(turns) = from_log {
            turns
        } else {
            let file = dir.join(format!("{id}.txt"));
            let size = std::fs::symlink_metadata(&file)
                .ok()
                .filter(|m| m.is_file() && m.len() <= MAX_TRANSCRIPT)
                .map(|m| m.len());
            let transcript = match size {
                Some(n) if n <= budget => {
                    budget -= n;
                    read_tail(&file, n).map(|(t, _)| t).unwrap_or_default()
                }
                Some(_) => {
                    out.truncated = true;
                    String::new()
                }
                None => String::new(),
            };
            if transcript.trim().is_empty() {
                Vec::new()
            } else {
                vec![PastTurn {
                    role: TurnRole::Unknown,
                    text: transcript,
                    at_ms: Some(ended),
                }]
            }
        };
        let log = log
            .map(|(_, l)| l)
            .filter(|_| turns.iter().all(|t| t.role != TurnRole::Unknown));
        out.conversations.push(PastConversation {
            id: id.to_owned(),
            provider: item["tool"].as_str().and_then(Provider::from_word),
            terminal: item["terminal_id"].as_str().map(str::to_owned),
            tool_session: session.map(str::to_owned),
            ended_ms: ended,
            log,
            turns,
        });
    }
    out.conversations
        .sort_by_key(|c| std::cmp::Reverse(c.ended_ms));
    Some(out)
}

/// A memory model call as a utility session: every one, a cycle's run or
/// a user's question to the memory ("Ask the memory", no cycle), is an
/// ordinary memory-purpose session with the read-only launch. Utility
/// sessions get no capture tap, so none of them is ever captured as
/// project conversation.
fn utility_job(job: &ModelJob, project: PathBuf) -> UtilityJob {
    UtilityJob {
        project_id: job.project_id.clone(),
        project,
        choice: job.choice.clone(),
        prompt: job.prompt.clone(),
        purpose: UtilityPurpose::Memory,
        label: job
            .cycle
            .map_or("ask", connecto_core::memory::Cycle::word)
            .to_owned(),
    }
}

impl Host for MemoryHost {
    fn run_model(
        &self,
        job: &ModelJob,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<ModelReply, RunError> {
        let project = self
            .project_path(&job.project_id)
            .ok_or_else(|| RunError::Failed("the project is not open".to_owned()))?;
        let reply = self
            .runner
            .run(&utility_job(job, project), deadline, cancel)
            .map_err(map_error)?;
        Ok(ModelReply {
            text: reply.text,
            source: match reply.source {
                utility_run::ReplySource::Hook => ReplySource::Hook,
                utility_run::ReplySource::SessionLog => ReplySource::SessionLog,
            },
            chars_in: reply.chars_in as u64,
            chars_out: reply.chars_out as u64,
            duration: reply.duration,
        })
    }

    fn capability(&self, choice: &ModelChoice) -> Capability {
        let project = self
            .workspace
            .get()
            .and_then(|w| w.view().projects.first().map(|p| PathBuf::from(&p.path)))
            .unwrap_or_else(|| self.layout.home.clone());
        utility_run::capability(&self.layout, &project, choice.provider)
    }

    fn git_snapshot(&self, project: &Path) -> Option<GitSnapshot> {
        git_snapshot(project)
    }

    fn paused(&self) -> bool {
        self.workspace
            .get()
            .is_some_and(|w| w.pause_switch().is_paused())
    }

    fn pause_epoch(&self) -> u64 {
        self.workspace.get().map_or(0, |w| w.pause_switch().epoch())
    }

    fn known_secrets(&self) -> Vec<String> {
        let mut known = env_secrets();
        known.extend(connecto_redact::vault_known().iter().cloned());
        known
    }

    fn now(&self) -> WallClock {
        let now = Local::now();
        let weekday = match now.weekday() {
            chrono::Weekday::Mon => Weekday::Mon,
            chrono::Weekday::Tue => Weekday::Tue,
            chrono::Weekday::Wed => Weekday::Wed,
            chrono::Weekday::Thu => Weekday::Thu,
            chrono::Weekday::Fri => Weekday::Fri,
            chrono::Weekday::Sat => Weekday::Sat,
            chrono::Weekday::Sun => Weekday::Sun,
        };
        WallClock {
            utc_ms: u64::try_from(now.timestamp_millis()).unwrap_or(0),
            local_date: LocalDate {
                year: now.year(),
                month: u8::try_from(now.month()).unwrap_or(1),
                day: u8::try_from(now.day()).unwrap_or(1),
            },
            local_time: LocalTime::new(
                u8::try_from(now.hour()).unwrap_or(0),
                u8::try_from(now.minute()).unwrap_or(0),
            )
            .unwrap_or(connecto_core::memory::DreamSettings::default().time),
            weekday,
            utc_offset_secs: now.offset().fix().local_minus_utc(),
        }
    }

    fn wake(&self) {
        (self.wake)();
    }

    fn projects(&self) -> Vec<OpenProject> {
        let Some(workspace) = self.workspace.get() else {
            return Vec::new();
        };
        workspace
            .view()
            .projects
            .iter()
            .filter(|p| p.error.is_none())
            .map(|p| OpenProject {
                id: p.id.clone(),
                path: std::fs::canonicalize(&p.path).unwrap_or_else(|_| PathBuf::from(&p.path)),
            })
            .collect()
    }

    fn load_settings(&self, project_id: &str) -> MemorySettings {
        connecto_session::project_settings::load_memory(&self.layout.data_root, project_id)
    }

    fn save_settings(&self, project_id: &str, settings: &MemorySettings) -> Result<(), String> {
        connecto_session::project_settings::save_memory(
            &self.layout.data_root,
            project_id,
            settings,
        )
        .map_err(|e| e.to_string())
    }

    fn read_logged_reply(
        &self,
        provider: Provider,
        log: &Path,
        sent: Option<&str>,
    ) -> Option<LoggedReply> {
        logged_reply(provider, log, sent)
    }

    fn file_blobs(&self, project: &Path, paths: &[String]) -> Option<Vec<ChangedFile>> {
        Some(observe_files(project, paths.to_vec(), MAX_HASHED * 4))
    }

    fn branches(&self, project: &Path) -> Option<Vec<String>> {
        // From the checked refs: git, given the private folder, has none.
        memory_git::local_branches(&repo_of(project)?)
    }

    fn cancel_runs(&self) {
        self.runner.cancel_all();
    }

    fn project_files(&self, project: &Path, max: usize) -> Option<ProjectFiles> {
        project_files(project, max)
    }

    fn reads_files(&self) -> bool {
        FILE_READS
    }

    fn git_refusal(&self, project: &Path) -> Option<String> {
        git_refusal(project)
    }

    fn git_limits(&self, project: &Path) -> Vec<String> {
        git_limits(project)
    }

    fn project_started(&self, project: &Path) {
        if let Some(p) = &self.private {
            p.start(project);
        }
    }

    fn project_stopped(&self, project: &Path) {
        if let Some(p) = &self.private {
            p.release(project);
        }
    }

    fn shutdown(&self) {
        if let Some(p) = &self.private {
            p.clear();
        }
    }

    fn read_project_file(&self, project: &Path, path: &str, max_bytes: u64) -> Option<String> {
        read_project_file(project, path, max_bytes)
    }

    fn git_history(&self, project: &Path, since_ms: u64, max: usize) -> Option<GitHistory> {
        git_history(project, since_ms, max)
    }

    fn git_diff(
        &self,
        project: &Path,
        paths: &[String],
        max_file_bytes: u64,
        max_bytes: u64,
    ) -> Option<String> {
        git_diff(project, paths, max_file_bytes, max_bytes)
    }

    fn past_conversations(&self, project_id: &str, since_ms: u64) -> Option<PastConversations> {
        past_conversations(&self.layout.data_root, project_id, since_ms)
    }
}

/// The terminals' capture, into the engine. Never blocks.
struct Sink(CaptureHandle);

impl CaptureSink for Sink {
    fn offer(&self, event: CaptureEvent) {
        let what = match event.what {
            CaptureWhat::Prompt {
                text,
                source,
                truncated,
            } => Happened::Prompt {
                text,
                author: match source {
                    PromptSource::User => Author::User,
                    PromptSource::Agent => Author::Agent,
                    PromptSource::Connecto => Author::Connecto,
                    PromptSource::Unknown => Author::Unknown,
                },
                truncated,
            },
            CaptureWhat::TurnEnd { reply } => Happened::TurnEnd {
                reply: match reply {
                    CaptureReply::Text(t) => CapturedReply::Text(t),
                    CaptureReply::None => CapturedReply::None,
                    CaptureReply::Dropped => CapturedReply::Dropped,
                },
            },
            CaptureWhat::SessionEnd { how } => Happened::SessionEnd { how },
        };
        self.0.offer(Captured {
            project_id: event.project_id,
            terminal: event.terminal,
            provider: event.provider,
            tool_session: event.tool_session,
            session_start_ms: event.session_start_ms,
            turn: event.turn,
            log: event.log,
            what,
        });
    }
}

/// The engine and its host, before the workspace exists.
pub struct Memory {
    pub engine: Engine,
    host: Arc<MemoryHost>,
}

/// Start the engine and connect the terminals' capture and the bridge to
/// it. Call before the workspace starts its terminals; then [`attach`].
pub fn start(layout: &Layout, wake: &Wake) -> std::io::Result<Memory> {
    // The private git folders: owner-only, under Connecto's own data
    // folder. Without it git is simply not used (declared per project).
    let private = crate::memory_private::PrivateGit::open(layout.data_root.join("memory-git")).ok();
    if let Some(p) = &private {
        p.install();
    }
    let host = Arc::new(MemoryHost {
        private,
        layout: layout.clone(),
        workspace: OnceLock::new(),
        runner: UtilityRunner::new(layout.clone(), Arc::clone(wake)),
        wake: Arc::clone(wake),
    });
    let engine = Engine::start(EngineConfig {
        data_root: layout.data_root.clone(),
        host: Arc::clone(&host) as Arc<dyn Host>,
    })?;
    connecto_session::memory_capture::set_sink(Arc::new(Sink(engine.capture())));
    connecto_session::set_memory_bridge(Arc::new(engine.clone()));
    Ok(Memory { engine, host })
}

/// Give the engine the workspace, and have Stop Everything cancel memory
/// runs.
pub fn attach(memory: &Memory, workspace: &Arc<Workspace>) {
    let _ = memory.host.workspace.set(Arc::clone(workspace));
    let engine = memory.engine.clone();
    workspace.on_stop_everything(Arc::new(move || engine.cancel_runs()));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::type_complexity)]
mod tests {
    use super::*;

    /// "Ask the memory" runs as an ordinary memory utility session (the
    /// read-only launch, no capture tap: utility sessions never have one),
    /// labelled `ask`, with the model the user chose; a cycle keeps its own
    /// label.
    #[test]
    fn a_memory_ask_is_an_ordinary_memory_utility_session() {
        for (cycle, label) in [
            (None, "ask"),
            (Some(connecto_core::memory::Cycle::Dream), "dream"),
        ] {
            for choice in connecto_core::memory::ask_choices() {
                let job = ModelJob {
                    project_id: "p1".to_owned(),
                    cycle,
                    choice: choice.clone(),
                    prompt: "Q".to_owned(),
                };
                let u = utility_job(&job, PathBuf::from("/p"));
                assert_eq!(u.purpose, UtilityPurpose::Memory);
                assert_eq!(u.label, label);
                assert_eq!(u.choice, choice);
                assert_eq!(u.prompt, "Q");
            }
        }
    }

    /// Review finding 8: a dropped reply is taken from the log only when the
    /// turn answering the prompt is complete.
    #[test]
    fn a_dropped_reply_needs_a_finished_matching_turn() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("s.jsonl");
        let user = r#"{"type":"user","message":{"content":"explain the parser"}}"#;
        let partial = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Let me look at the files first."}],"stop_reason":"tool_use"}}"#;
        let done = r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"The parser has two stages."}],"stop_reason":"end_turn"}}"#;
        std::fs::write(&log, format!("{user}\n{partial}\n")).unwrap();
        assert_eq!(
            logged_reply(Provider::Claude, &log, Some("explain the parser")),
            None,
            "unfinished: commentary is not the reply"
        );
        std::fs::write(&log, format!("{user}\n{partial}\n{done}\n")).unwrap();
        assert_eq!(
            logged_reply(Provider::Claude, &log, Some("explain the parser")).map(|r| r.text),
            Some("The parser has two stages.".to_owned())
        );
        // Another prompt's turn, or no prompt to match: nothing.
        assert_eq!(
            logged_reply(Provider::Claude, &log, Some("something else")),
            None
        );
        assert_eq!(logged_reply(Provider::Claude, &log, None), None);
        assert_eq!(
            logged_reply(Provider::Claude, &temp.path().join("none"), Some("x")),
            None
        );
    }

    /// Review finding 9: over the cap, or when hashing fails, a file is
    /// unknown, never deleted; only a file that is gone is deleted.
    #[test]
    fn files_over_the_cap_are_unknown_not_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        let files = observe_files(
            dir,
            vec!["a.txt".into(), "b.txt".into(), "gone.txt".into()],
            0,
        );
        let state: Vec<(Option<String>, bool)> =
            files.iter().map(|f| (f.blob.clone(), f.deleted)).collect();
        assert_eq!(state, [(None, false), (None, false), (None, true)]);
        // Not a repository: hashing may still work (hash-object needs none),
        // but a failure would be unknown too, never deleted.
        let files = observe_files(dir, vec!["a.txt".into()], 10);
        assert!(!files[0].deleted);
    }

    #[test]
    fn status_lines_are_parsed_with_deletions() {
        let out = " M src/a.rs\0?? new file.txt\0 D gone.rs\0D  staged.rs\0";
        assert_eq!(
            parse_status(out),
            [
                ("src/a.rs".to_owned(), false),
                ("new file.txt".to_owned(), false),
                ("gone.rs".to_owned(), true),
                ("staged.rs".to_owned(), true),
            ]
        );
    }

    #[test]
    fn utility_errors_map_to_memory_errors() {
        use utility_run::RunError as U;
        assert_eq!(
            map_error(U::Untrusted("trust the folder".into())),
            RunError::HeldUntrusted {
                reason: "trust the folder".into()
            }
        );
        assert_eq!(map_error(U::Cancelled), RunError::Cancelled);
        assert_eq!(map_error(U::Timeout("x".into())), RunError::Timeout);
        assert_eq!(map_error(U::Provider("api".into())), RunError::ModelError);
        assert!(matches!(
            map_error(U::Session("ended".into())),
            RunError::Failed(_)
        ));
        // Never success, never held: a failed attempt, retried with delay.
        for e in [
            U::SubmitUncertain("unproven".into()),
            U::ReplyUnavailable("cut".into()),
        ] {
            assert!(matches!(map_error(e), RunError::Failed(_)));
        }
    }

    /// A real git repository in a temporary folder: the snapshot reads it and
    /// leaves it as it was (no index refresh, no lock files).
    #[test]
    fn a_snapshot_reads_git_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q", "-b", "main"]) {
            // No git here: nothing to check.
            return;
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        assert!(run(&["add", "a.txt"]));
        assert!(run(&["commit", "-q", "-m", "first"]));
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        let index = std::fs::read(dir.join(".git/index")).unwrap();
        let snap = git_snapshot(&dir).unwrap();
        assert_eq!(snap.branch.as_deref(), Some("main"));
        assert_eq!(snap.head.as_deref().map(str::len), Some(40));
        let paths: Vec<&str> = snap.changed.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.txt"]);
        assert!(
            snap.changed
                .iter()
                .all(|f| f.blob.as_deref().map(str::len) == Some(40))
        );
        assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index);
        assert!(!dir.join(".git/index.lock").exists());
        // `hash-object` without -w stored nothing.
        let blob = snap.changed[1].blob.clone().unwrap();
        assert!(!run(&["cat-file", "-e", &blob]));
        assert_eq!(git_snapshot(&dir.join("no-such-dir")), None);
    }

    /// The initial build's reads on a real repository: what they list and
    /// read, and that they leave the repository and the folder as they were.
    #[test]
    fn the_initial_build_reads_git_and_files_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q", "-b", "main"]) {
            return;
        }
        std::fs::write(dir.join("README.md"), "# Demo\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        assert!(run(&["add", "."]));
        assert!(run(&["commit", "-q", "-m", "first", "-m", "why: a body"]));
        std::fs::write(dir.join("src/lib.rs"), "pub fn b() {}\n").unwrap();
        std::fs::write(dir.join("notes.md"), "untracked\n").unwrap();
        std::fs::create_dir(dir.join("build")).unwrap();
        std::fs::write(dir.join("build/out.txt"), "ignored\n").unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2]).unwrap();
        let index = std::fs::read(dir.join(".git/index")).unwrap();
        let before = folder_bytes(&dir);

        let files = project_files(&dir, 100).unwrap();
        let paths: Vec<&str> = files.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"README.md") && paths.contains(&"src/lib.rs"));
        assert!(paths.contains(&"notes.md"), "untracked files are listed");
        assert!(!paths.iter().any(|p| p.starts_with("build/")), "{paths:?}");
        assert!(project_files(&dir, 1).unwrap().truncated);
        // Without git, a walk out of time stops and says it is incomplete.
        let plain = tempfile::tempdir().unwrap();
        std::fs::create_dir(plain.path().join("sub")).unwrap();
        std::fs::write(plain.path().join("sub/a.txt"), "a").unwrap();
        let walk = |dir: &Path, max: usize, cap: usize, ticks: usize| {
            let mut asked = 0;
            let mut clock = || {
                asked += 1;
                asked > ticks
            };
            project_files_within(dir, max, &mut Budget::new(cap, &mut clock)).unwrap()
        };
        let whole = walk(plain.path(), 100, usize::MAX, usize::MAX);
        assert!(!whole.truncated && whole.files.len() == 1);
        assert!(walk(plain.path(), 100, usize::MAX, 0).truncated);
        // Time runs out inside one flat folder (reading, then processing).
        let flat = tempfile::tempdir().unwrap();
        for i in 0..50 {
            std::fs::write(flat.path().join(format!("f{i:02}.txt")), "x").unwrap();
        }
        for ticks in [10, 70] {
            let part = walk(flat.path(), 1000, usize::MAX, ticks);
            assert!(part.truncated && part.files.len() < 50, "{ticks}");
        }
        // One folder larger than the per-folder cap.
        assert!(walk(flat.path(), 20, usize::MAX, usize::MAX).truncated);
        // Many empty folders, a deep chain of single folders, and a mix:
        // opening a folder is work too.
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
        let mix = tempfile::tempdir().unwrap();
        for i in 0..40 {
            std::fs::create_dir_all(mix.path().join(format!("e{i:02}/x/y"))).unwrap();
        }
        std::fs::write(mix.path().join("e00/x/y/f.txt"), "x").unwrap();
        for dir in [empty.path(), deep.path(), mix.path()] {
            assert!(
                !walk(dir, 1000, usize::MAX, usize::MAX).truncated,
                "{dir:?}"
            );
            // 200 empty folders need 401 units, the chain 121, the mix 241:
            // stopped well before by the clock, then by the unit cap.
            assert!(walk(dir, 1000, usize::MAX, 100).truncated, "{dir:?} clock");
            assert!(walk(dir, 1000, 100, usize::MAX).truncated, "{dir:?} units");
        }

        // Files are read only where no link can be followed (Unix).
        assert_eq!(
            read_project_file(&dir, "src/lib.rs", 1000).as_deref(),
            FILE_READS.then_some("pub fn b() {}\n")
        );
        assert_eq!(read_project_file(&dir, "blob.bin", 1000), None, "binary");
        assert_eq!(read_project_file(&dir, "README.md", 3), None, "too large");
        assert_eq!(read_project_file(&dir, "../x", 1000), None);
        assert_eq!(read_project_file(&dir, "/etc/hosts", 1000), None);
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.txt"), "s\n").unwrap();
            std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.join("link.txt"))
                .unwrap();
            assert_eq!(read_project_file(&dir, "link.txt", 1000), None, "a link");
        }

        let history = git_history(&dir, 0, 10).unwrap();
        assert_eq!(history.commits.len(), 1);
        let c = &history.commits[0];
        assert_eq!(c.subject, "first");
        assert_eq!(c.body, "why: a body");
        assert!(c.files.contains(&"src/lib.rs".to_owned()));
        assert!(!history.older);
        // 2096: after every commit.
        let later = git_history(&dir, 4_000_000_000_000, 10).unwrap();
        assert!(later.commits.is_empty() && later.older);
        let diff = git_diff(&dir, &["src/lib.rs".to_owned()], 100_000, 100_000).unwrap();
        assert!(diff.contains("+pub fn b() {}"));
        assert_eq!(
            git_diff(&dir, &["src/lib.rs".to_owned()], 100_000, 5),
            None,
            "never cut"
        );

        assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index);
        assert!(!dir.join(".git/index.lock").exists());
        let mut after = folder_bytes(&dir);
        after.retain(|(p, _)| p != "link.txt");
        assert_eq!(
            before, after,
            "the folder, .git included, is byte for byte the same"
        );
    }

    /// Every file under `dir` (links not followed) with its bytes.
    pub(crate) fn folder_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![String::new()];
        while let Some(rel) = stack.pop() {
            for entry in std::fs::read_dir(dir.join(&rel)).unwrap().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    stack.push(path);
                } else if kind.is_file() {
                    out.push((path.clone(), std::fs::read(dir.join(&path)).unwrap()));
                } else {
                    out.push((path, Vec::new()));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn without_git_the_walk_skips_dependencies_and_links() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        std::fs::write(dir.join("node_modules/x/index.js"), "x").unwrap();
        std::fs::create_dir(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs/a.md"), "a").unwrap();
        std::fs::write(dir.join("main.py"), "print(1)").unwrap();
        let files = project_files(&dir, 100).unwrap();
        let paths: Vec<&str> = files.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["docs/a.md", "main.py"]);
        assert_eq!(git_history(&dir, 0, 10), None);
        assert_eq!(git_diff(&dir, &[], 100, 100), None);
    }

    /// Closed terminals read from their tools' own logs, with roles: Claude
    /// and Codex, an injected opening, the window, a missing log.
    #[test]
    fn past_conversations_come_from_the_tools_own_logs_with_roles() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        let history = data.join("projects/p1/history");
        std::fs::create_dir_all(&history).unwrap();
        std::fs::write(
            history.join("index.json"),
            r#"{"version":1,"entries":[
                {"id":"h300-c1","terminal_id":"c1","tool":"claude","closed_at_ms":300,"session":"aaaa-1111"},
                {"id":"h290-c2","terminal_id":"c2","tool":"claude","closed_at_ms":290,"session":"bbbb-2222","parent":"c1"},
                {"id":"h280-x1","terminal_id":"x1","tool":"codex","closed_at_ms":280,"session":"cd34-ab12"},
                {"id":"h270-m1","terminal_id":"m1","tool":"claude","closed_at_ms":270,"session":"dead-beef"}
            ]}"#,
        )
        .unwrap();
        std::fs::write(history.join("h270-m1.txt"), "> screen only\nreply").unwrap();
        let claude = data.join("projects/p1/profiles/claude/g2/projects/-Users-x-repo");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(data.join("projects/p1/profiles/claude/g1")).unwrap();
        std::fs::write(
            claude.join("aaaa-1111.jsonl"),
            [
                r#"{"type":"user","timestamp":"1970-01-01T00:00:00.050Z","message":{"content":"Too old for the window."}}"#,
                r#"{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"old"}],"stop_reason":"end_turn"}}"#,
                r#"{"type":"user","timestamp":"1970-01-01T00:00:00.200Z","message":{"role":"user","content":"Requirement: paging must use cursors."}}"#,
                r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"COMMENTARY let me read"}],"stop_reason":"tool_use"}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
                r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"I propose cursor paging."}],"stop_reason":"end_turn"}}"#,
                r#"{"type":"user","timestamp":"1970-01-01T00:00:00.210Z","message":{"role":"user","content":"Yes, accepted."}}"#,
                r#"{"type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"Noted."}],"stop_reason":"end_turn"}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        std::fs::write(
            claude.join("bbbb-2222.jsonl"),
            [
                r#"{"type":"user","message":{"role":"user","content":"Opening task from the parent."}}"#,
                r#"{"type":"assistant","message":{"id":"a","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#,
                r#"{"type":"user","message":{"role":"user","content":"Typed later."}}"#,
                r#"{"type":"assistant","message":{"id":"b","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let codex = data.join("projects/p1/profiles/codex/g1/sessions/2026/11/01");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("rollout-2026-11-01T10-00-00-cd34-ab12.jsonl"),
            [
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Plan the export."}}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"COMMENTARY looking"}}"#,
                r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"I propose JSON."}}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let past = past_conversations(data, "p1", 100).unwrap();
        let by = |id: &str| past.conversations.iter().find(|c| c.id == id).unwrap();
        let roles = |id: &str| -> Vec<(TurnRole, String)> {
            by(id)
                .turns
                .iter()
                .map(|t| (t.role, t.text.clone()))
                .collect()
        };
        assert_eq!(
            roles("h300-c1"),
            [
                (
                    TurnRole::User,
                    "Requirement: paging must use cursors.".to_owned()
                ),
                (TurnRole::Agent, "I propose cursor paging.".to_owned()),
                (TurnRole::User, "Yes, accepted.".to_owned()),
                (TurnRole::Agent, "Noted.".to_owned()),
            ],
            "the window drops the old turn; commentary is not the reply"
        );
        assert!(by("h300-c1").log.is_some());
        assert_eq!(by("h290-c2").turns[0].role, TurnRole::Injected);
        assert_eq!(by("h290-c2").turns[2].role, TurnRole::User);
        assert_eq!(
            roles("h280-x1"),
            [
                (TurnRole::User, "Plan the export.".to_owned()),
                (TurnRole::Agent, "I propose JSON.".to_owned()),
            ]
        );
        assert_eq!(
            roles("h270-m1"),
            [(TurnRole::Unknown, "> screen only\nreply".to_owned())]
        );
        assert!(by("h270-m1").log.is_none());
    }

    #[test]
    fn a_large_log_is_read_from_its_end_only() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("big.jsonl");
        let line = r#"{"type":"assistant","message":{"id":"x","content":[{"type":"text","text":"padding"}],"stop_reason":"end_turn"}}"#;
        let mut text = format!("{line}\n").repeat(2000);
        text.push_str(r#"{"type":"user","message":{"role":"user","content":"Last prompt."}}"#);
        text.push('\n');
        text.push_str(line);
        std::fs::write(&log, &text).unwrap();
        let (tail, whole) = read_tail(&log, 4096).unwrap();
        assert!(!whole && tail.len() <= 4096);
        assert!(tail.lines().all(|l| l.starts_with('{')), "whole lines only");
        let mut budget = 4096;
        let (turns, whole) = logged_turns(&log, ToolKind::Claude, false, 0, &mut budget).unwrap();
        assert!(!whole);
        assert_eq!(turns[0].text, "Last prompt.");
        assert_eq!(budget, 0, "the budget is spent, never exceeded");
    }

    #[test]
    fn past_conversations_come_from_the_history_index() {
        let temp = tempfile::tempdir().unwrap();
        let history = temp.path().join("projects/p1/history");
        std::fs::create_dir_all(&history).unwrap();
        std::fs::write(
            history.join("index.json"),
            r#"{"version":1,"entries":[
                {"id":"h200-t2","terminal_id":"t2","tool":"codex","closed_at_ms":200,"session":"s2"},
                {"id":"h100-t1","terminal_id":"t1","tool":"claude","closed_at_ms":100},
                {"id":"../evil","terminal_id":"t3","tool":"claude","closed_at_ms":300}
            ]}"#,
        )
        .unwrap();
        std::fs::write(history.join("h200-t2.txt"), "> plan the cache\nproposal").unwrap();
        let past = past_conversations(temp.path(), "p1", 150).unwrap();
        assert!(past.older);
        assert_eq!(past.conversations.len(), 1);
        let c = &past.conversations[0];
        assert_eq!(c.id, "h200-t2");
        assert_eq!(c.provider, Some(Provider::Codex));
        assert_eq!(c.tool_session.as_deref(), Some("s2"));
        assert_eq!(c.turns.len(), 1);
        assert_eq!(c.turns[0].role, TurnRole::Unknown);
        assert_eq!(
            past_conversations(temp.path(), "none", 0),
            Some(PastConversations::default())
        );
        assert_eq!(past_conversations(temp.path(), "../p1", 0), None);
    }

    // ---- review round 1: regressions

    #[cfg(unix)]
    /// The memory host's reads, as a host the build can run against: the
    /// model and the clock fake, everything that touches the project real.
    struct RealReads {
        fake: connecto_memory::testing::FakeHost,
    }

    #[cfg(unix)]
    impl Host for RealReads {
        fn run_model(
            &self,
            job: &ModelJob,
            deadline: Instant,
            cancel: &AtomicBool,
        ) -> Result<ModelReply, RunError> {
            self.fake.run_model(job, deadline, cancel)
        }
        fn capability(&self, choice: &ModelChoice) -> Capability {
            self.fake.capability(choice)
        }
        fn git_snapshot(&self, project: &Path) -> Option<GitSnapshot> {
            git_snapshot(project)
        }
        fn paused(&self) -> bool {
            false
        }
        fn pause_epoch(&self) -> u64 {
            0
        }
        fn known_secrets(&self) -> Vec<String> {
            Vec::new()
        }
        fn now(&self) -> WallClock {
            self.fake.now()
        }
        fn wake(&self) {}
        fn file_blobs(&self, project: &Path, paths: &[String]) -> Option<Vec<ChangedFile>> {
            Some(observe_files(project, paths.to_vec(), MAX_HASHED * 4))
        }
        fn project_files(&self, project: &Path, max: usize) -> Option<ProjectFiles> {
            project_files(project, max)
        }
        fn read_project_file(&self, project: &Path, path: &str, max: u64) -> Option<String> {
            read_project_file(project, path, max)
        }
        fn git_history(&self, project: &Path, since: u64, max: usize) -> Option<GitHistory> {
            git_history(project, since, max)
        }
        fn git_diff(
            &self,
            project: &Path,
            paths: &[String],
            file: u64,
            max: u64,
        ) -> Option<String> {
            git_diff(project, paths, file, max)
        }
        fn git_refusal(&self, project: &Path) -> Option<String> {
            git_refusal(project)
        }
    }

    /// Finding 5: a folder link inside the project never aliases another
    /// file, and swapping a folder for a link after listing does not
    /// redirect the read.
    #[cfg(unix)]
    #[test]
    fn links_in_a_path_are_never_followed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        std::fs::create_dir_all(dir.join("private")).unwrap();
        std::fs::write(dir.join("private/data.rs"), "secret body").unwrap();
        std::os::unix::fs::symlink("private", dir.join("alias")).unwrap();
        assert_eq!(
            read_project_file(&dir, "private/data.rs", 100).as_deref(),
            Some("secret body")
        );
        assert_eq!(
            read_project_file(&dir, "alias/data.rs", 100),
            None,
            "ancestor link"
        );
        assert_eq!(
            observe_files(&dir, vec!["alias/data.rs".to_owned()], 10)[0].blob,
            None,
            "nothing hashed through a link"
        );
        // Listed as a real folder, then swapped for a link before the read.
        std::fs::create_dir(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/data.rs"), "public body").unwrap();
        assert!(
            project_files(&dir, 100)
                .unwrap()
                .files
                .iter()
                .any(|f| f.path == "src/data.rs")
        );
        std::fs::rename(dir.join("src"), dir.join("src-old")).unwrap();
        std::os::unix::fs::symlink("private", dir.join("src")).unwrap();
        assert_eq!(
            read_project_file(&dir, "src/data.rs", 100),
            None,
            "swapped folder"
        );
        // A file swapped for a link.
        std::fs::rename(dir.join("src-old/data.rs"), dir.join("src-old/kept.rs")).unwrap();
        std::os::unix::fs::symlink("../private/data.rs", dir.join("src-old/data.rs")).unwrap();
        assert_eq!(
            read_project_file(&dir, "src-old/data.rs", 100),
            None,
            "file link"
        );
    }

    /// Finding 8: logs and transcripts share one budget; transcripts of
    /// conversations whose log is gone stop at it, and that is declared.
    #[test]
    fn transcripts_share_the_history_budget() {
        let temp = tempfile::tempdir().unwrap();
        let history = temp.path().join("projects/p1/history");
        std::fs::create_dir_all(&history).unwrap();
        let n = 33;
        let entries: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    r#"{{"id":"h{}-t{i}","terminal_id":"t{i}","tool":"claude","closed_at_ms":{},"session":"gone-{i}"}}"#,
                    1000 + i,
                    1000 + i
                )
            })
            .collect();
        std::fs::write(
            history.join("index.json"),
            format!(r#"{{"version":1,"entries":[{}]}}"#, entries.join(",")),
        )
        .unwrap();
        let body = "x".repeat(usize::try_from(MAX_TRANSCRIPT).unwrap());
        for i in 0..n {
            std::fs::write(history.join(format!("h{}-t{i}.txt", 1000 + i)), &body).unwrap();
        }
        let past = past_conversations(temp.path(), "p1", 0).unwrap();
        let read: u64 = past
            .conversations
            .iter()
            .flat_map(|c| &c.turns)
            .map(|t| t.text.len() as u64)
            .sum();
        assert!(read <= MAX_LOGS_READ, "{read} > {MAX_LOGS_READ}");
        assert!(past.truncated, "what was left out is declared");
    }

    /// Finding 2 through the host: a compaction summary is not a turn, and
    /// a continuation summary written as a plain user record is not the
    /// user's.
    #[test]
    fn synthetic_records_never_become_the_users_turns() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        let history = data.join("projects/p1/history");
        std::fs::create_dir_all(&history).unwrap();
        std::fs::write(
            history.join("index.json"),
            r#"{"version":1,"entries":[{"id":"h1-c1","terminal_id":"c1","tool":"claude","closed_at_ms":300,"session":"cccc-1111"}]}"#,
        )
        .unwrap();
        let logs = data.join("projects/p1/profiles/claude/g1/projects/-x");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("cccc-1111.jsonl"),
            [
                r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"Summary: the user accepted SQLite."}}"#,
                r#"{"type":"assistant","message":{"id":"a","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#,
                r#"{"type":"user","message":{"role":"user","content":"This session is being continued from a previous conversation. The user accepted SQLite."}}"#,
                r#"{"type":"assistant","message":{"id":"b","content":[{"type":"text","text":"Continuing."}],"stop_reason":"end_turn"}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let past = past_conversations(data, "p1", 0).unwrap();
        let turns = &past.conversations[0].turns;
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[0].role, TurnRole::Injected);
        assert!(turns.iter().all(|t| !t.text.contains("Summary: the user")));
    }

    // ---- review round 2: regressions

    /// A repository for the regressions: one commit, then a local change.
    /// `None` when there is no git here.
    #[cfg(unix)]
    fn repo(dir: &Path, files: &[(&str, &[u8])]) -> Option<()> {
        let git_ok = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        std::fs::create_dir_all(dir).ok()?;
        if !git_ok(&["init", "-q", "-b", "main"]) {
            return None;
        }
        for (path, body) in files {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent()?).ok()?;
            std::fs::write(full, body).ok()?;
        }
        (git_ok(&["add", "."]) && git_ok(&["commit", "-q", "-m", "first"])).then_some(())
    }

    #[cfg(unix)]
    fn git_set(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Finding 4 (host): a path whose committed version is over the limit
    /// is left out of the diff even when the working file is small.
    #[cfg(unix)]
    #[test]
    fn a_large_committed_version_never_reaches_the_diff() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        let big = "old line\n".repeat(500);
        if repo(&dir, &[("big.txt", big.as_bytes()), ("small.txt", b"s\n")]).is_none() {
            return;
        }
        std::fs::write(dir.join("big.txt"), "now small\n").unwrap();
        std::fs::write(dir.join("small.txt"), "t\n").unwrap();
        let paths = ["big.txt".to_owned(), "small.txt".to_owned()];
        let diff = git_diff(&dir, &paths, 1000, 1_000_000).unwrap();
        assert!(diff.contains("+t"));
        assert!(!diff.contains("old line"), "{diff}");
        assert!(!diff.contains("big.txt"));
    }

    /// Finding 6: off Unix no file content is read; here it is, and the
    /// host says which.
    #[test]
    fn file_reads_are_declared_per_platform() {
        assert_eq!(FILE_READS, cfg!(unix));
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.md"), "hello").unwrap();
        assert_eq!(
            read_project_file(temp.path(), "a.md", 100).is_some(),
            FILE_READS
        );
    }

    // ---- review round 3: git confinement

    /// A script that records it ran, and passes its input through.
    #[cfg(unix)]
    fn sentinel(dir: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let marker = dir.join("ran");
        let evil = dir.join("evil.sh");
        std::fs::write(
            &evil,
            format!("#!/bin/sh\necho \"$0 $*\" >> '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
        (evil, marker)
    }

    /// Add a commit carrying a (fake) signature on top of HEAD: showing it
    /// would run the signature program.
    #[cfg(unix)]
    fn signed_commit(dir: &Path) -> bool {
        use std::io::Write as _;
        let out = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        };
        let (Some(tree), Some(parent)) = (
            out(&["rev-parse", "HEAD^{tree}"]),
            out(&["rev-parse", "HEAD"]),
        ) else {
            return false;
        };
        let body = format!(
            "tree {tree}\nparent {parent}\nauthor t <t@example.invalid> 1700000000 +0000\ncommitter t <t@example.invalid> 1700000000 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEE\n -----END PGP SIGNATURE-----\n\nsigned\n"
        );
        let Ok(mut child) = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["hash-object", "-t", "commit", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            return false;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body.as_bytes());
        }
        let Ok(o) = child.wait_with_output() else {
            return false;
        };
        let id = String::from_utf8_lossy(&o.stdout).trim().to_owned();
        git_set(dir, &["update-ref", "refs/heads/main", &id])
    }

    /// Round 3, the acceptance test: one repository with every hostile
    /// setting found so far (fsmonitor, filters, textconv, external diff,
    /// pager, hooks, `core.worktree` to a folder with private content,
    /// `log.showSignature` with `gpg.program` and a signed commit, includes)
    /// goes through a whole build. Git is refused for it and the coverage
    /// says why; nothing ran; no outside content reached a prompt; the
    /// folder, `.git` included, is byte for byte as it was.
    #[cfg(unix)]
    #[test]
    fn a_whole_build_against_a_hostile_repository_refuses_git_and_says_why() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        let outside = temp.path().join("outside");
        let (evil, marker) = sentinel(&outside);
        std::fs::create_dir_all(outside.join("src")).unwrap();
        std::fs::write(outside.join("src/lib.rs"), "OUTSIDE_PRIVATE\n").unwrap();
        std::fs::write(outside.join("inc.config"), "[core]\n\tpager = cat\n").unwrap();
        if repo(
            &dir,
            &[
                (".gitattributes", b"* filter=evil diff=evil\n"),
                ("README.md", b"# Demo\nBuild with make.\n"),
                ("src/lib.rs", b"pub fn a() {}\n"),
            ],
        )
        .is_none()
        {
            return;
        }
        assert!(signed_commit(&dir), "the signed commit was made");
        let quoted = format!("'{}'", evil.display());
        for (k, v) in [
            ("core.fsmonitor", quoted.clone()),
            ("filter.evil.clean", quoted.clone()),
            ("filter.evil.smudge", quoted.clone()),
            ("filter.evil.required", "true".to_owned()),
            ("diff.evil.textconv", quoted.clone()),
            ("diff.external", quoted.clone()),
            ("core.pager", quoted.clone()),
            ("core.worktree", outside.display().to_string()),
            ("log.showSignature", "true".to_owned()),
            ("gpg.program", quoted.clone()),
            (
                "include.path",
                outside.join("inc.config").display().to_string(),
            ),
            (
                "includeIf.gitdir:/.path",
                outside.join("inc.config").display().to_string(),
            ),
        ] {
            assert!(git_set(&dir, &["config", k, &v]), "{k}");
        }
        for hook in ["post-index-change", "reference-transaction", "pre-commit"] {
            let path = dir.join(".git/hooks").join(hook);
            std::fs::write(&path, format!("#!/bin/sh\n{quoted}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join("src/lib.rs"), "pub fn b() {}\n").unwrap();
        let _ = std::fs::remove_file(&marker);
        let before = folder_bytes(&dir);

        let host = Arc::new(RealReads {
            fake: connecto_memory::testing::FakeHost::new(),
        });
        for _ in 0..8 {
            host.fake.reply(r#"{"operations":[]}"#);
        }
        let data = temp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let writer = connecto_memory::Writer::spawn(
            &data.join("memory.db"),
            "p1",
            connecto_memory::WRITER_QUEUE,
        )
        .unwrap();
        let handle = writer.handle();
        let nothing = || {};
        let cx = connecto_memory::bootstrap::Context {
            host: Arc::clone(&host) as Arc<dyn Host>,
            writer: &handle,
            project_id: "p1",
            project: &dir,
            settings: MemorySettings::default(),
            stops: connecto_memory::bootstrap::Stops::default(),
            gate: Arc::default(),
            changed: &nothing,
        };
        let state = connecto_memory::bootstrap::run(
            &cx,
            connecto_memory::bootstrap::Start::New {
                id: connecto_memory::bootstrap::new_id(),
                options: connecto_core::memory::BootstrapOptions::default(),
            },
        )
        .unwrap();
        assert_eq!(state, connecto_core::memory::BootstrapState::Completed);
        let view = handle
            .call(|s| connecto_memory::bootstrap::view(s))
            .unwrap()
            .unwrap();
        assert!(
            view.gaps
                .contains(&connecto_core::memory::CoverageGap::GitNotUsable),
            "{:?}",
            view.gaps
        );
        assert!(git_refusal(&dir).is_some());
        let prompts: String = host.fake.jobs().iter().map(|j| j.prompt.clone()).collect();
        assert!(prompts.contains("Build with make."), "files are still read");
        assert!(!prompts.contains("OUTSIDE_PRIVATE"));
        assert!(
            !marker.exists(),
            "a program the repository names ran: {:?}",
            std::fs::read_to_string(&marker)
        );
        assert_eq!(
            before,
            folder_bytes(&dir),
            "the folder, .git included, is unchanged"
        );
    }

    /// Each layout or configuration bypass alone: git is refused, with a
    /// reason, and nothing runs. A clean repository is used.
    #[cfg(unix)]
    #[test]
    fn every_bypass_alone_makes_git_refused() {
        let temp = tempfile::tempdir().unwrap();
        let (evil, marker) = sentinel(&temp.path().join("outside"));
        let other = temp.path().join("other");
        if repo(&other, &[("secret.txt", b"OUTSIDE_PRIVATE_HISTORY\n")]).is_none() {
            return;
        }
        let quoted = format!("'{}'", evil.display());
        let mut n = 0;
        let mut fresh = || {
            n += 1;
            let dir = temp.path().join(format!("r{n}"));
            repo(&dir, &[("a.txt", b"a\n")]).map(|()| dir)
        };
        let clean = fresh().unwrap();
        assert!(git_snapshot(&clean).is_some());
        assert_eq!(git_refusal(&clean), None);
        type Setup = Box<dyn Fn(&Path)>;
        let other_git = other.join(".git");
        let cases: Vec<(&str, Setup)> = vec![
            (
                "commondir",
                Box::new(move |d: &Path| {
                    std::fs::write(
                        d.join(".git/commondir"),
                        format!("{}\n", other_git.display()),
                    )
                    .unwrap();
                }),
            ),
            (
                "alternates",
                Box::new({
                    let o = other.clone();
                    move |d: &Path| {
                        std::fs::write(
                            d.join(".git/objects/info/alternates"),
                            format!("{}\n", o.join(".git/objects").display()),
                        )
                        .unwrap();
                    }
                }),
            ),
            (
                "include",
                Box::new(|d: &Path| {
                    assert!(git_set(d, &["config", "include.path", "/dev/null"]));
                }),
            ),
            (
                "includeIf",
                Box::new(|d: &Path| {
                    assert!(git_set(
                        d,
                        &["config", "includeIf.onbranch:main.path", "/dev/null"]
                    ));
                }),
            ),
            (
                "core.worktree",
                Box::new(|d: &Path| {
                    assert!(git_set(d, &["config", "core.worktree", "/tmp"]));
                }),
            ),
            (
                "showSignature and gpg.program",
                Box::new({
                    let q = quoted.clone();
                    move |d: &Path| {
                        assert!(signed_commit(d));
                        assert!(git_set(d, &["config", "log.showSignature", "true"]));
                        assert!(git_set(d, &["config", "gpg.program", &q]));
                    }
                }),
            ),
            (
                "fsmonitor",
                Box::new({
                    let q = quoted.clone();
                    move |d: &Path| assert!(git_set(d, &["config", "core.fsmonitor", &q]))
                }),
            ),
            (
                "a ref link",
                Box::new(|d: &Path| {
                    std::os::unix::fs::symlink("/etc", d.join(".git/refs/heads/x")).unwrap();
                }),
            ),
            (
                "a foreign .git pointer",
                Box::new({
                    let o = other.clone();
                    move |d: &Path| {
                        std::fs::rename(d.join(".git"), d.join("old-git")).unwrap();
                        std::fs::write(
                            d.join(".git"),
                            format!("gitdir: {}\n", o.join(".git").display()),
                        )
                        .unwrap();
                    }
                }),
            ),
        ];
        for (what, setup) in cases {
            let dir = fresh().unwrap();
            setup(&dir);
            std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
            assert!(git_snapshot(&dir).is_none(), "{what}: git was used");
            assert!(git_history(&dir, 0, 10).is_none(), "{what}");
            assert!(git_refusal(&dir).is_some(), "{what}: no reason");
            assert!(!marker.exists(), "{what}: a program ran");
        }
    }

    /// Round 2 finding 1, kept with the new posture: an inherited
    /// environment cannot switch anything back on (fsmonitor, signature
    /// display with a signature program over a signed commit, external
    /// diff, a git dir elsewhere, hidden configuration).
    #[cfg(unix)]
    #[test]
    fn an_inherited_git_environment_changes_nothing() {
        use std::ffi::OsString;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        let (evil, marker) = sentinel(&temp.path().join("outside"));
        if repo(&dir, &[("a.txt", b"one\n")]).is_none() {
            return;
        }
        assert!(signed_commit(&dir), "the signed commit was made");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let e = evil.display().to_string();
        let mut env = memory_git::inherited();
        for (k, v) in [
            (
                "GIT_CONFIG_PARAMETERS",
                format!(
                    "'core.fsmonitor'='{e}' 'log.showsignature'='true' 'gpg.program'='{e}' 'diff.external'='{e}'"
                ),
            ),
            ("GIT_CONFIG", "/dev/null".to_owned()),
            ("GIT_DIR", temp.path().display().to_string()),
            ("GIT_EXTERNAL_DIFF", e.clone()),
            ("GIT_ASKPASS", e.clone()),
            ("GIT_PAGER", e.clone()),
        ] {
            env.push((OsString::from(k), OsString::from(v)));
        }
        let before = folder_bytes(&dir);
        let repo = memory_git::bind(&dir, &env).unwrap();
        for args in [
            &["status", "--porcelain=v1", "-z"][..],
            &["log", "-n2", "--format=%H"][..],
            &["diff-index", "-p", "HEAD", "--", "a.txt"][..],
            &["ls-files", "-z"][..],
        ] {
            let out = memory_git::run(memory_git::command(&repo, &env).args(args), 1 << 20);
            assert!(out.is_some_and(|(_, ok)| ok), "{args:?}");
        }
        assert!(!marker.exists(), "{:?}", std::fs::read_to_string(&marker));
        assert_eq!(before, folder_bytes(&dir));
    }

    /// A linked worktree of a valid repository is used.
    #[cfg(unix)]
    #[test]
    fn a_linked_worktree_of_the_same_repository_is_used() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        if repo(&main, &[("m.txt", b"m\n")]).is_none() {
            return;
        }
        let linked = temp.path().join("linked");
        assert!(git_set(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                &linked.display().to_string()
            ]
        ));
        std::fs::write(linked.join("m.txt"), "changed\n").unwrap();
        let diff = git_diff(&linked, &["m.txt".to_owned()], 1000, 100_000).unwrap();
        assert!(diff.contains("+changed"), "{diff}");
        assert_eq!(git_refusal(&linked), None);
    }

    /// Round 3 point 3: a project that is a subfolder of a repository sees
    /// only its own files, changes, history and diffs; a sibling folder's
    /// never appear.
    #[cfg(unix)]
    #[test]
    fn a_subfolder_project_sees_only_its_own_part_of_the_repository() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mono");
        if repo(
            &root,
            &[
                ("README.md", b"root\n"),
                ("apps/web/src/a.rs", b"pub fn web() {}\n"),
                ("apps/api/src/b.rs", b"pub fn api_secret() {}\n"),
            ],
        )
        .is_none()
        {
            return;
        }
        // A commit that touches only the sibling.
        std::fs::write(root.join("apps/api/src/b.rs"), "pub fn api_two() {}\n").unwrap();
        let commit = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(commit(&["commit", "-qam", "api only"]));
        std::fs::write(root.join("apps/web/src/a.rs"), "pub fn web_changed() {}\n").unwrap();
        std::fs::write(root.join("apps/api/src/b.rs"), "pub fn api_changed() {}\n").unwrap();
        std::fs::write(root.join("apps/web/new.md"), "untracked\n").unwrap();
        let web = root.join("apps/web");

        let files: Vec<String> = project_files(&web, 100)
            .unwrap()
            .files
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(files, ["new.md", "src/a.rs"]);
        let snap = git_snapshot(&web).unwrap();
        let changed: Vec<&str> = snap.changed.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(changed, ["new.md", "src/a.rs"]);
        assert!(
            snap.changed.iter().all(|f| f.blob.is_some()),
            "hashed in place"
        );
        let history = git_history(&web, 0, 10).unwrap();
        assert_eq!(
            history.commits.len(),
            1,
            "only commits touching the project"
        );
        assert_eq!(history.commits[0].files, ["src/a.rs"]);
        let diff = git_diff(&web, &["src/a.rs".to_owned()], 1000, 100_000).unwrap();
        assert!(diff.contains("diff --git a/src/a.rs b/src/a.rs"), "{diff}");
        assert!(diff.contains("+pub fn web_changed() {}"));
        assert!(!diff.contains("api"));
        let escape = git_diff(&web, &["../api/src/b.rs".to_owned()], 1000, 100_000).unwrap();
        assert!(escape.is_empty(), "{escape}");
        assert_eq!(git_refusal(&web), None);
    }

    // ---- review round 4

    /// Finding 1: a filter hidden from a naive parser behind comment lines
    /// that end in a backslash. Git would run it; the configuration is not
    /// understood, git is refused, and nothing runs.
    #[cfg(unix)]
    #[test]
    fn a_filter_hidden_behind_backslash_comments_is_refused_and_never_runs() {
        use std::io::Write as _;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        let (evil, marker) = sentinel(&temp.path().join("outside"));
        if repo(
            &dir,
            &[(".gitattributes", b"* filter=evil\n"), ("a.txt", b"one\n")],
        )
        .is_none()
        {
            return;
        }
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(".git/config"))
            .unwrap();
        write!(
            config,
            "# hidden \\\n[filter \"evil\"]\n# hidden \\\n\tclean = '{}'\n",
            evil.display()
        )
        .unwrap();
        drop(config);
        // Git does read the filter from it.
        let listed = Command::new("git")
            .args(["config", "--file"])
            .arg(dir.join(".git/config"))
            .args(["--list"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&listed.stdout).contains("filter.evil.clean"));
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let before = folder_bytes(&dir);
        assert!(git_snapshot(&dir).is_none());
        assert!(git_refusal(&dir).is_some_and(|r| r.contains("not understood")));
        assert!(project_files(&dir, 10).is_some(), "files are still listed");
        assert!(!marker.exists());
        assert_eq!(before, folder_bytes(&dir));
    }

    /// Finding 2: links inside a loose-object folder pointing at another
    /// repository's objects: git is refused, and the other repository's
    /// history never comes out.
    #[cfg(unix)]
    #[test]
    fn a_link_inside_a_loose_object_folder_refuses_git() {
        let temp = tempfile::tempdir().unwrap();
        let other = temp.path().join("other");
        let dir = temp.path().join("repo");
        if repo(&other, &[("secret.txt", b"OUTSIDE_PRIVATE_HISTORY\n")]).is_none()
            || repo(&dir, &[("a.txt", b"a\n")]).is_none()
        {
            return;
        }
        // Every loose object of the other repository, linked in.
        let objects = other.join(".git/objects");
        let mut linked = 0;
        for fan in std::fs::read_dir(&objects).unwrap().flatten() {
            let name = fan.file_name().to_string_lossy().into_owned();
            if name.len() != 2 {
                continue;
            }
            let target = dir.join(".git/objects").join(&name);
            std::fs::create_dir_all(&target).unwrap();
            for obj in std::fs::read_dir(fan.path()).unwrap().flatten() {
                let at = target.join(obj.file_name());
                if !at.exists() {
                    std::os::unix::fs::symlink(obj.path(), at).unwrap();
                    linked += 1;
                }
            }
        }
        assert!(linked > 0);
        assert!(git_snapshot(&dir).is_none());
        assert!(git_history(&dir, 0, 10).is_none());
        assert!(git_refusal(&dir).is_some_and(|r| r.contains("link")));
    }

    /// Finding 3: a commit message made of the old separators cannot forge
    /// a commit, a time or a changed path; in a subfolder project too.
    #[cfg(unix)]
    #[test]
    fn a_commit_message_cannot_forge_history_records() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mono");
        if repo(&root, &[("app/a.rs", b"a\n")]).is_none() {
            return;
        }
        std::fs::write(root.join("app/a.rs"), "b\n").unwrap();
        let forged = format!(
            "real subject\n\nbody\x1f\n\x1e{}\x1f1700000000\x1fforged\x1fx\x1f\napp/not-real.rs\napp/a.rs",
            "f".repeat(40)
        );
        let ok = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["commit", "-qam", &forged])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(ok);
        let history = git_history(&root.join("app"), 0, 10).unwrap();
        assert_eq!(history.commits.len(), 2, "{history:?}");
        let real: Vec<String> = String::from_utf8_lossy(
            &Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["rev-list", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .lines()
        .map(str::to_owned)
        .collect();
        for c in &history.commits {
            assert!(real.contains(&c.hash), "a forged hash: {}", c.hash);
            assert!(
                !c.files.iter().any(|f| f.contains("not-real")),
                "{:?}",
                c.files
            );
            assert_eq!(c.files, ["a.rs"]);
            assert_ne!(c.time_ms, 1_700_000_000_000);
        }
        assert_eq!(history.commits[0].subject, "real subject");
        assert!(
            history.commits[0].body.contains("not-real.rs"),
            "kept as text only"
        );
    }

    /// The cost of one turn-end snapshot on this very repository (ignored:
    /// a measurement, run by hand with `--ignored --nocapture`).
    #[test]
    #[ignore = "a measurement"]
    fn measure_the_turn_end_snapshot() {
        let here = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root = here.parent().and_then(Path::parent).unwrap();
        let _ = git_snapshot(root);
        let n = 10;
        let start = Instant::now();
        for _ in 0..n {
            assert!(git_snapshot(root).is_some());
        }
        eprintln!("SNAPSHOT_MS_PER_CALL {}", start.elapsed().as_millis() / n);
    }

    // ---- review round 5

    /// Finding 1: whatever changes in the real `.git` after the checks
    /// (filter, textconv, merge driver and fsmonitor configuration with
    /// attributes naming them, a hook, the index swapped for another
    /// repository's, HEAD and the branch moved) is never read by the
    /// commands: they run against the private git folder. Nothing runs, no
    /// other repository's content comes out, and nothing in the project or
    /// its `.git` is written.
    #[cfg(unix)]
    #[test]
    fn what_changes_in_dot_git_after_the_checks_is_never_read() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        let other = temp.path().join("other");
        let (evil, marker) = sentinel(&temp.path().join("outside"));
        if repo(&dir, &[("a.txt", b"one\n")]).is_none()
            || repo(&other, &[("a.txt", b"OUTSIDE_PRIVATE_HISTORY\n")]).is_none()
        {
            return;
        }
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let env = memory_git::inherited();
        let repo = memory_git::bind(&dir, &env).unwrap();
        let head_then = repo.facts.head.clone().unwrap();

        // After the checks: hostile configuration, attributes, a hook...
        let quoted = format!("'{}'", evil.display());
        std::fs::write(
            dir.join(".gitattributes"),
            "* filter=evil diff=evil merge=evil\n",
        )
        .unwrap();
        for (k, v) in [
            ("filter.evil.clean", quoted.as_str()),
            ("filter.evil.process", quoted.as_str()),
            ("diff.evil.textconv", quoted.as_str()),
            ("diff.evil.command", quoted.as_str()),
            ("merge.evil.driver", quoted.as_str()),
            ("core.fsmonitor", quoted.as_str()),
            ("core.worktree", other.to_str().unwrap()),
        ] {
            assert!(git_set(&dir, &["config", k, v]), "{k}");
        }
        let hook = dir.join(".git/hooks/post-index-change");
        std::fs::write(&hook, format!("#!/bin/sh\n{quoted}\n")).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        // ... the index and HEAD swapped for the other repository's.
        std::fs::copy(other.join(".git/index"), dir.join(".git/index")).unwrap();
        let other_head = std::fs::read_to_string(other.join(".git/refs/heads/main")).unwrap();
        std::fs::write(dir.join(".git/refs/heads/main"), &other_head).unwrap();
        let before = folder_bytes(&dir);

        let run = |args: &[&str]| {
            memory_git::run(memory_git::command(&repo, &env).args(args), 1 << 20)
                .map(|(o, _)| o)
                .unwrap_or_default()
        };
        let mut outputs = String::new();
        for args in [
            &["status", "--porcelain=v1", "-z", "--ignore-submodules=all"][..],
            &[
                "diff-index",
                "-p",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD",
                "--",
                "a.txt",
            ][..],
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ][..],
            &["rev-list", "HEAD"][..],
            &["hash-object", "--no-filters", "--", "a.txt"][..],
        ] {
            outputs.push_str(&run(args));
        }
        assert!(outputs.contains("+two"), "{outputs}");
        assert!(
            outputs.contains(&head_then),
            "the HEAD checked, not the one moved after"
        );
        assert!(!outputs.contains(other_head.trim()));
        assert!(!outputs.contains("OUTSIDE_PRIVATE_HISTORY"));
        assert!(!marker.exists(), "{:?}", std::fs::read_to_string(&marker));
        assert_eq!(
            before,
            folder_bytes(&dir),
            "nothing in the project or .git written"
        );
        // And a new check sees the change and refuses.
        assert!(git_refusal(&dir).is_some());
    }

    /// Finding 1, the engine's path: with the real configuration made
    /// hostile after a first snapshot, the next one is refused before git
    /// runs; nothing runs either time.
    #[cfg(unix)]
    #[test]
    fn the_turn_end_snapshot_never_reads_the_real_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        let (evil, marker) = sentinel(&temp.path().join("outside"));
        if repo(
            &dir,
            &[("a.txt", b"one\n"), (".gitattributes", b"* filter=evil\n")],
        )
        .is_none()
        {
            return;
        }
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        assert!(git_snapshot(&dir).is_some());
        assert!(git_set(
            &dir,
            &[
                "config",
                "filter.evil.clean",
                &format!("'{}'", evil.display())
            ]
        ));
        std::fs::write(dir.join("a.txt"), "three\n").unwrap();
        assert!(git_snapshot(&dir).is_none());
        assert!(!marker.exists());
    }

    /// Finding 2: an initialised submodule and a nested independent
    /// repository are never entered, by the walker (git refused because of
    /// the submodule's configuration) nor by the git listing (the gitlink
    /// entry); both are reported, and a whole build reads nothing in them.
    #[cfg(unix)]
    #[test]
    fn submodules_and_nested_repositories_are_never_entered() {
        let temp = tempfile::tempdir().unwrap();
        let lib = temp.path().join("lib");
        let dir = temp.path().join("repo");
        if repo(&lib, &[("private.rs", b"pub fn submodule_private() {}\n")]).is_none()
            || repo(&dir, &[("README.md", b"# Demo\n")]).is_none()
        {
            return;
        }
        let added = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["-c", "protocol.file.allow=always", "submodule", "add", "-q"])
            .arg(&lib)
            .arg("external")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(added, "the submodule was added");
        assert!(dir.join("external/private.rs").exists());
        // A nested repository that is not a submodule.
        assert!(
            repo(
                &dir.join("nested"),
                &[("inner.rs", b"pub fn nested_private() {}\n")]
            )
            .is_some()
        );

        // Git refused (submodule.* in its configuration): the walker.
        assert!(git_refusal(&dir).is_some());
        let walked = project_files(&dir, 100).unwrap();
        let paths: Vec<&str> = walked.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with("external/") || p.starts_with("nested/")),
            "{paths:?}"
        );
        assert_eq!(walked.nested, ["external", "nested"]);

        // Git usable (the submodule's configuration taken out): the listing.
        assert!(git_set(
            &dir,
            &["config", "--remove-section", "submodule.external"]
        ));
        assert_eq!(git_refusal(&dir), None, "git is used now");
        let listed = project_files(&dir, 100).unwrap();
        let paths: Vec<&str> = listed.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with("external") || p.starts_with("nested/")),
            "{paths:?}"
        );
        assert!(
            listed.nested.contains(&"external".to_owned()),
            "{:?}",
            listed.nested
        );

        // A whole build reads nothing in them, and says so.
        let host = Arc::new(RealReads {
            fake: connecto_memory::testing::FakeHost::new(),
        });
        for _ in 0..8 {
            host.fake.reply(r#"{"operations":[]}"#);
        }
        let data = temp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let writer = connecto_memory::Writer::spawn(
            &data.join("memory.db"),
            "p1",
            connecto_memory::WRITER_QUEUE,
        )
        .unwrap();
        let handle = writer.handle();
        let nothing = || {};
        let cx = connecto_memory::bootstrap::Context {
            host: Arc::clone(&host) as Arc<dyn Host>,
            writer: &handle,
            project_id: "p1",
            project: &dir,
            settings: MemorySettings::default(),
            stops: connecto_memory::bootstrap::Stops::default(),
            gate: Arc::default(),
            changed: &nothing,
        };
        let state = connecto_memory::bootstrap::run(
            &cx,
            connecto_memory::bootstrap::Start::New {
                id: connecto_memory::bootstrap::new_id(),
                options: connecto_core::memory::BootstrapOptions::default(),
            },
        )
        .unwrap();
        assert_eq!(state, connecto_core::memory::BootstrapState::Completed);
        let prompts: String = host.fake.jobs().iter().map(|j| j.prompt.clone()).collect();
        assert!(!prompts.contains("submodule_private"));
        assert!(!prompts.contains("nested_private"));
        let view = handle
            .call(|s| connecto_memory::bootstrap::view(s))
            .unwrap()
            .unwrap();
        assert!(
            view.gaps
                .contains(&connecto_core::memory::CoverageGap::NestedRepositories)
        );
    }

    // ---- review round 6

    /// Finding 2: index variants. A split index, an index in version 4, one
    /// with an untracked cache, a repository with no index yet, and one
    /// with an `index.lock` being written all work; a damaged index is
    /// refused with its reason.
    #[cfg(unix)]
    #[test]
    fn every_index_variant_works_or_is_refused_at_bind() {
        let temp = tempfile::tempdir().unwrap();
        let mut n = 0;
        let mut fresh = |files: &[(&str, &[u8])]| {
            n += 1;
            let dir = temp.path().join(format!("r{n}"));
            repo(&dir, files).map(|()| dir)
        };
        let Some(split) = fresh(&[("a.txt", b"a\n"), ("b.txt", b"b\n")]) else {
            return;
        };
        assert!(git_set(&split, &["update-index", "--split-index"]));
        assert!(
            std::fs::read_dir(split.join(".git"))
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("sharedindex."))
        );
        let v4 = fresh(&[("a.txt", b"a\n")]).unwrap();
        assert!(git_set(&v4, &["update-index", "--index-version", "4"]));
        let untracked = fresh(&[("a.txt", b"a\n")]).unwrap();
        let _ = git_set(&untracked, &["update-index", "--untracked-cache"]);
        let locked = fresh(&[("a.txt", b"a\n")]).unwrap();
        std::fs::write(locked.join(".git/index.lock"), "half written").unwrap();
        for dir in [&split, &v4, &untracked, &locked] {
            std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
            std::fs::write(dir.join("new.txt"), "new\n").unwrap();
            let snap =
                git_snapshot(dir).unwrap_or_else(|| panic!("{dir:?}: {:?}", git_refusal(dir)));
            let changed: Vec<&str> = snap.changed.iter().map(|f| f.path.as_str()).collect();
            assert_eq!(changed, ["a.txt", "new.txt"], "{dir:?}");
            let files: Vec<String> = project_files(dir, 100)
                .unwrap()
                .files
                .into_iter()
                .map(|f| f.path)
                .collect();
            assert!(files.contains(&"a.txt".to_owned()), "{dir:?}: {files:?}");
            assert_eq!(git_refusal(dir), None, "{dir:?}");
        }
        // No index yet (no commit, nothing added).
        let empty = temp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(git_set(&empty, &["init", "-q", "-b", "main"]));
        std::fs::write(empty.join("x.txt"), "x\n").unwrap();
        let snap = git_snapshot(&empty).unwrap();
        assert_eq!(snap.changed.len(), 1);
        assert_eq!(snap.head, None);
        // A damaged index: refused, with why.
        let damaged = fresh(&[("a.txt", b"a\n")]).unwrap();
        std::fs::write(damaged.join(".git/index"), "not an index").unwrap();
        assert!(git_snapshot(&damaged).is_none());
        assert!(git_refusal(&damaged).is_some_and(|r| r.contains("index")));
    }

    /// Finding 2 (b): each git command failing in turn after a successful
    /// bind: never "nothing" silently. The listing falls back to the folder,
    /// and the build declares that git was not fully used.
    #[cfg(unix)]
    #[test]
    fn a_git_command_that_fails_falls_back_and_is_declared() {
        let temp = tempfile::tempdir().unwrap();
        let mut n = 0;
        for what in [
            "ls-files",
            "status",
            "rev-list",
            "cat-file",
            "diff-tree",
            "diff-index",
            "ls-tree",
            "hash-object",
        ] {
            n += 1;
            let dir = temp.path().join(format!("r{n}"));
            if repo(
                &dir,
                &[
                    ("README.md", b"# Demo\n"),
                    ("src/lib.rs", b"pub fn a() {}\n"),
                ],
            )
            .is_none()
            {
                return;
            }
            std::fs::write(dir.join("src/lib.rs"), "pub fn b() {}\n").unwrap();
            FAIL.with(|f| f.set(Some(what)));
            let host = RealReads {
                fake: connecto_memory::testing::FakeHost::new(),
            };
            let inv = connecto_memory::bootstrap_inventory::collect(
                &host,
                "p1",
                &dir,
                &connecto_core::memory::BootstrapOptions::default(),
                0,
            );
            FAIL.with(|f| f.set(None));
            assert!(
                inv.gaps
                    .contains(&connecto_core::memory::CoverageGap::GitNotUsable),
                "{what}: {:?}",
                inv.gaps
            );
            assert!(
                inv.sources.iter().any(|s| s.key == "file:README.md"),
                "{what}: files are still listed"
            );
        }
    }

    /// Finding 3: the private git compares content as plain git does, for
    /// line-ending settings, file modes, case and attributes.
    #[cfg(unix)]
    #[test]
    fn private_git_reports_the_same_changes_as_plain_git() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let plain_status = |dir: &Path| -> Vec<String> {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
                .output()
                .unwrap();
            let mut paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split('\0')
                .filter(|e| e.len() > 3)
                .map(|e| e[3..].to_owned())
                .collect();
            paths.sort();
            paths
        };
        let cases: Vec<(&str, Vec<(&str, &str)>, &str)> = vec![
            ("autocrlf-true", vec![("core.autocrlf", "true")], ""),
            ("autocrlf-input", vec![("core.autocrlf", "input")], ""),
            ("autocrlf-false", vec![("core.autocrlf", "false")], ""),
            ("eol-crlf", vec![("core.eol", "crlf")], "* text=auto\n"),
            ("filemode-off", vec![("core.filemode", "false")], ""),
            ("ignorecase", vec![("core.ignorecase", "true")], ""),
            (
                "attributes",
                vec![],
                "*.txt text eol=crlf\n*.raw -text\n*.bin binary\n",
            ),
        ];
        let mut checked = 0;
        for (name, settings, attributes) in cases {
            let dir = temp.path().join(name);
            let mut files: Vec<(&str, &[u8])> = vec![
                ("crlf.txt", b"one\r\ntwo\r\n"),
                ("lf.txt", b"one\ntwo\n"),
                ("data.raw", b"a\r\nb\n"),
                ("blob.bin", b"\x00\x01\r\n"),
                ("run.sh", b"echo\n"),
            ];
            if !attributes.is_empty() {
                files.push((".gitattributes", attributes.as_bytes()));
            }
            if repo(&dir, &files).is_none() {
                return;
            }
            for (k, v) in &settings {
                assert!(git_set(&dir, &["config", k, v]));
            }
            // Touch every file without changing it, change one mode, and
            // write one line-ending variant.
            for (f, body) in &files {
                std::fs::write(dir.join(f), body).unwrap();
            }
            std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            std::fs::write(dir.join("lf.txt"), b"one\r\ntwo\r\n").unwrap();
            let ours: Vec<String> = git_snapshot(&dir)
                .unwrap_or_else(|| panic!("{name}: {:?}", git_refusal(&dir)))
                .changed
                .into_iter()
                .map(|f| f.path)
                .collect();
            assert_eq!(ours, plain_status(&dir), "{name}");
            checked += 1;
        }
        assert_eq!(checked, 7);
    }

    // ---- review round 7

    #[cfg(unix)]
    fn plain_changed(dir: &Path) -> Vec<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
            .output()
            .unwrap();
        let mut paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|e| e.len() > 3)
            .map(|e| e[3..].to_owned())
            .collect();
        paths.sort();
        paths
    }

    /// Finding 2: a worktree-local override (`extensions.worktreeConfig`
    /// with `core.filemode=false` in `config.worktree`), in the main
    /// worktree and in a linked one: private git reports what plain git
    /// does.
    #[cfg(unix)]
    #[test]
    fn worktree_local_settings_are_honoured_in_main_and_linked_worktrees() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        if repo(&main, &[("run.sh", b"echo\n"), ("a.txt", b"a\n")]).is_none() {
            return;
        }
        assert!(git_set(
            &main,
            &["config", "extensions.worktreeConfig", "true"]
        ));
        let linked = temp.path().join("linked");
        assert!(git_set(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                &linked.display().to_string()
            ]
        ));
        for dir in [&main, &linked] {
            assert!(git_set(
                dir,
                &["config", "--worktree", "core.filemode", "false"]
            ));
            std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
            let ours: Vec<String> = git_snapshot(dir)
                .unwrap_or_else(|| panic!("{dir:?}: {:?}", git_refusal(dir)))
                .changed
                .into_iter()
                .map(|f| f.path)
                .collect();
            assert_eq!(ours, plain_changed(dir), "{dir:?}");
            assert_eq!(ours, ["a.txt"], "{dir:?}: the mode-only change is not one");
        }
    }

    /// Finding 2, differential: for each copied key and a few values, set
    /// in the repository's `config` or in the active `config.worktree`, the
    /// private git's own value equals git's effective value for the real
    /// repository, computed from the layered `--file` reads (global, then
    /// `config`, then `config.worktree` when the extension is on).
    #[cfg(unix)]
    #[test]
    fn every_copied_key_has_git_s_effective_value_in_the_private_config() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        if repo(&dir, &[("a.txt", b"a\n")]).is_none() {
            return;
        }
        assert!(git_set(
            &dir,
            &["config", "extensions.worktreeConfig", "true"]
        ));
        let get_file = |file: &Path, key: &str| -> Option<String> {
            let out = Command::new("git")
                .args(["config", "--no-includes", "--file"])
                .arg(file)
                .args(["--get", key])
                .output()
                .ok()?;
            out.status.success().then(|| {
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .to_ascii_lowercase()
            })
        };
        let get_global = |key: &str| -> Option<String> {
            let out = Command::new("git")
                .args(["config", "--global", "--no-includes", "--get", key])
                .output()
                .ok()?;
            out.status.success().then(|| {
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .to_ascii_lowercase()
            })
        };
        let values: &[(&str, &[&str])] = &[
            ("core.autocrlf", &["true", "input", "false"]),
            ("core.eol", &["crlf", "lf"]),
            ("core.safecrlf", &["warn", "false"]),
            ("core.filemode", &["false", "true"]),
            ("core.symlinks", &["false"]),
            ("core.trustctime", &["false"]),
            ("core.checkstat", &["minimal"]),
            ("core.ignorecase", &["true", "false"]),
            ("core.precomposeunicode", &["true"]),
            ("core.protectntfs", &["true"]),
            ("core.protecthfs", &["false"]),
            ("core.longpaths", &["true"]),
            ("core.quotepath", &["false"]),
            ("core.abbrev", &["12"]),
        ];
        let env = memory_git::inherited();
        let mut checked = 0;
        for (key, vals) in values {
            for value in *vals {
                for worktree in [false, true] {
                    // Clear the key at both levels, then set it at one.
                    let _ = git_set(&dir, &["config", "--unset-all", key]);
                    let _ = git_set(&dir, &["config", "--worktree", "--unset-all", key]);
                    let level = if worktree { "--worktree" } else { "--local" };
                    assert!(git_set(&dir, &["config", level, key, value]), "{key}");
                    let expected = get_file(&dir.join(".git/config.worktree"), key)
                        .or_else(|| get_file(&dir.join(".git/config"), key))
                        .or_else(|| get_global(key));
                    let repo = memory_git::bind(&dir, &env)
                        .unwrap_or_else(|e| panic!("{key}={value}: {e:?}"));
                    let private = repo.shadow.as_ref().unwrap().path().join("config");
                    assert_eq!(get_file(&private, key), expected, "{key}={value} ({level})");
                    checked += 1;
                }
            }
        }
        assert!(checked >= 40, "{checked}");
    }

    /// Finding 3 (host side): once a project is stopped, nothing makes its
    /// private folder again, and git is not used for it, until it starts.
    #[cfg(unix)]
    #[test]
    fn a_stopped_project_gets_no_private_folder_until_it_starts() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        if repo(&dir, &[("a.txt", b"a\n")]).is_none() {
            return;
        }
        let private =
            crate::memory_private::PrivateGit::open(temp.path().join("memory-git")).unwrap();
        // Used directly (not installed): what the host calls.
        let repo_now = memory_git::locate(&dir, &memory_git::inherited()).unwrap();
        let fill = |d: &Path| crate::memory_private::write_new(d, "HEAD", b"x");
        assert!(private.folder(&repo_now.root, &dir, "k", fill).is_ok());
        private.release(&dir);
        assert!(private.folder(&repo_now.root, &dir, "k2", fill).is_err());
        private.start(&dir);
        assert!(private.folder(&repo_now.root, &dir, "k3", fill).is_ok());
        private.clear();
        assert!(private.folder(&repo_now.root, &dir, "k4", fill).is_err());
    }

    // ---- review round 8

    /// The reproduction: `extensions.worktreeConfig = 2` (git takes it as
    /// true) with a worktree-local `core.filemode = false`. Not understood
    /// here: git is refused, never used with the layer silently dropped.
    #[cfg(unix)]
    #[test]
    fn a_numeric_boolean_refuses_git_rather_than_dropping_a_layer() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        if repo(&dir, &[("run.sh", b"echo\n")]).is_none() {
            return;
        }
        assert!(git_set(&dir, &["config", "extensions.worktreeConfig", "2"]));
        assert!(git_set(
            &dir,
            &["config", "--worktree", "core.filemode", "false"]
        ));
        std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(
            plain_changed(&dir).is_empty(),
            "plain git honours the layer"
        );
        assert!(git_snapshot(&dir).is_none());
        assert!(
            git_refusal(&dir).is_some_and(|r| r.contains("not understood")),
            "{:?}",
            git_refusal(&dir)
        );
    }

    /// Every allowlisted key with the spellings git accepts besides the
    /// ones written here (numeric and word booleans, other cases of names
    /// and values, suffixes, leading zeros), duplicates with different
    /// values, and unknown extensions: each is refused, or, when accepted,
    /// private git takes exactly git's effective value.
    #[cfg(unix)]
    #[test]
    fn every_accepted_spelling_means_what_git_means_and_the_rest_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        if repo(&dir, &[("a.txt", b"a\n")]).is_none() {
            return;
        }
        let config = dir.join(".git/config");
        let base = "[core]\n\trepositoryformatversion = 0\n\tbare = false\n";
        let get = |file: &Path, key: &str, typed: bool| -> Option<String> {
            let mut cmd = Command::new("git");
            cmd.args(["config", "--no-includes", "--file"]).arg(file);
            if typed {
                cmd.arg("--type=bool");
            }
            let out = cmd.args(["--get", key]).output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        };
        let booleans: &[&str] = &[
            "true", "false", "yes", "no", "on", "off", "1", "0", "2", "-1", "TRUE", "True", "False",
        ];
        // (header, key as written, key as git names it, spellings, boolean)
        let cases: Vec<(&str, &str, &str, Vec<&str>, bool)> = vec![
            (
                "[core]",
                "filemode",
                "core.filemode",
                booleans.to_vec(),
                true,
            ),
            (
                "[Core]",
                "FileMode",
                "core.filemode",
                vec!["false", "No"],
                true,
            ),
            (
                "[core]",
                "ignorecase",
                "core.ignorecase",
                booleans.to_vec(),
                true,
            ),
            (
                "[core]",
                "precomposeunicode",
                "core.precomposeunicode",
                booleans.to_vec(),
                true,
            ),
            (
                "[core]",
                "symlinks",
                "core.symlinks",
                booleans.to_vec(),
                true,
            ),
            (
                "[core]",
                "trustctime",
                "core.trustctime",
                booleans.to_vec(),
                true,
            ),
            (
                "[core]",
                "quotepath",
                "core.quotepath",
                booleans.to_vec(),
                true,
            ),
            (
                "[core]",
                "longpaths",
                "core.longpaths",
                booleans.to_vec(),
                true,
            ),
            (
                "[core]",
                "protectntfs",
                "core.protectntfs",
                booleans.to_vec(),
                true,
            ),
            (
                "[core]",
                "protecthfs",
                "core.protecthfs",
                booleans.to_vec(),
                true,
            ),
            (
                "[core]",
                "logallrefupdates",
                "core.logallrefupdates",
                vec!["true", "always", "Always", "yes", "1"],
                false,
            ),
            (
                "[core]",
                "autocrlf",
                "core.autocrlf",
                vec!["true", "false", "input", "Input", "INPUT", "yes", "1", "on"],
                false,
            ),
            ("[core]", "AutoCRLF", "core.autocrlf", vec!["input"], false),
            (
                "[core]",
                "eol",
                "core.eol",
                vec!["lf", "crlf", "native", "CRLF", "Lf"],
                false,
            ),
            (
                "[core]",
                "safecrlf",
                "core.safecrlf",
                vec!["true", "warn", "Warn", "yes", "1"],
                false,
            ),
            (
                "[core]",
                "checkstat",
                "core.checkstat",
                vec!["default", "minimal", "Minimal"],
                false,
            ),
            (
                "[core]",
                "abbrev",
                "core.abbrev",
                vec!["12", "auto", "012", "1k", "3", "65", "no", "Auto"],
                false,
            ),
            (
                "[core]",
                "bare",
                "core.bare",
                vec!["false", "no", "0", "False"],
                true,
            ),
            (
                "[core]",
                "repositoryformatversion",
                "core.repositoryformatversion",
                vec!["0", "1", "00", "2"],
                false,
            ),
            (
                "[extensions]",
                "worktreeconfig",
                "extensions.worktreeconfig",
                booleans.to_vec(),
                true,
            ),
            (
                "[Extensions]",
                "WorktreeConfig",
                "extensions.worktreeconfig",
                vec!["true", "2"],
                true,
            ),
            (
                "[extensions]",
                "objectformat",
                "extensions.objectformat",
                vec!["sha1", "SHA1", "Sha256"],
                false,
            ),
            (
                "[extensions]",
                "partialclone",
                "extensions.partialclone",
                vec!["origin"],
                false,
            ),
            (
                "[extensions]",
                "refstorage",
                "extensions.refstorage",
                vec!["files"],
                false,
            ),
            (
                "[remote \"origin\"]",
                "prune",
                "remote.origin.prune",
                booleans.to_vec(),
                true,
            ),
            (
                "[remote \"origin\"]",
                "tagopt",
                "remote.origin.tagopt",
                vec!["--no-tags", "--NO-TAGS"],
                false,
            ),
            (
                "[branch \"main\"]",
                "rebase",
                "branch.main.rebase",
                vec!["true", "merges", "yes", "Merges", "interactive"],
                false,
            ),
            (
                "[pull]",
                "rebase",
                "pull.rebase",
                vec!["true", "merges", "1"],
                false,
            ),
            (
                "[pull]",
                "ff",
                "pull.ff",
                vec!["only", "Only", "yes"],
                false,
            ),
            (
                "[push]",
                "default",
                "push.default",
                vec!["simple", "Simple", "tracking"],
                false,
            ),
            (
                "[push]",
                "autosetupremote",
                "push.autosetupremote",
                booleans.to_vec(),
                true,
            ),
            (
                "[advice]",
                "detachedhead",
                "advice.detachedhead",
                booleans.to_vec(),
                true,
            ),
        ];
        let env = memory_git::inherited();
        let (mut refused, mut accepted) = (0, 0);
        let mut check = |text: String, key: &str, boolean: bool, what: &str| {
            std::fs::write(&config, &text).unwrap();
            match memory_git::bind(&dir, &env) {
                Err(_) => refused += 1,
                Ok(repo) => {
                    accepted += 1;
                    // Accepted: written exactly as git writes it...
                    let raw = get(&config, key, false).unwrap_or_default();
                    let (section, name) = key
                        .rsplit_once('.')
                        .map(|(s, n)| (s.split('.').next().unwrap_or(s), n))
                        .unwrap();
                    assert!(memory_git::value_accepted(section, name, &raw), "{what}");
                    // ... and a copied key means in private git what it means
                    // in git.
                    let private = repo.shadow.as_ref().unwrap().path().join("config");
                    let copied = [
                        "core.filemode",
                        "core.ignorecase",
                        "core.precomposeunicode",
                        "core.symlinks",
                        "core.trustctime",
                        "core.quotepath",
                        "core.longpaths",
                        "core.protectntfs",
                        "core.protecthfs",
                        "core.autocrlf",
                        "core.eol",
                        "core.safecrlf",
                        "core.checkstat",
                        "core.abbrev",
                        "extensions.objectformat",
                    ];
                    if copied.contains(&key) {
                        // `sha1` is git's default object format: not written.
                        let effective = |v: Option<String>| {
                            if key == "extensions.objectformat" {
                                v.or_else(|| Some("sha1".to_owned()))
                            } else {
                                v
                            }
                        };
                        assert_eq!(
                            effective(get(&private, key, boolean)),
                            effective(get(&config, key, boolean)),
                            "{what}"
                        );
                    }
                }
            }
        };
        for (header, written, key, spellings, boolean) in &cases {
            for value in spellings {
                let text = format!("{base}{header}\n\t{written} = {value}\n");
                check(text, key, *boolean, &format!("{key} = {value}"));
            }
        }
        // The same key twice with different values (any case of its name).
        check(
            format!("{base}[core]\n\tautocrlf = true\n[CORE]\n\tAUTOCRLF = false\n"),
            "core.autocrlf",
            false,
            "duplicate",
        );
        assert!(
            refused > 40 && accepted > 20,
            "refused {refused}, accepted {accepted}"
        );
        // Duplicates and unknown extensions are among the refused.
        for text in [
            format!("{base}[core]\n\tautocrlf = true\n[CORE]\n\tAUTOCRLF = false\n"),
            format!("{base}[extensions]\n\tpartialclone = origin\n"),
            format!("{base}[Extensions]\n\tWorktreeConfig = 2\n"),
            format!("{base}[core]\n\tabbrev = 1k\n"),
        ] {
            std::fs::write(&config, &text).unwrap();
            assert!(memory_git::bind(&dir, &env).is_err(), "{text:?}");
        }
    }

    // ---- review round 9: the user's global configuration

    /// An environment with its own home and, when given, its own
    /// `XDG_CONFIG_HOME` (the test process's own is never changed).
    fn env_with(home: &Path, xdg: Option<&str>) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        let mut env: Vec<_> = memory_git::inherited()
            .into_iter()
            .filter(|(k, _)| k != "HOME" && k != "XDG_CONFIG_HOME")
            .collect();
        env.push(("HOME".into(), home.as_os_str().to_owned()));
        if let Some(x) = xdg {
            env.push(("XDG_CONFIG_HOME".into(), x.into()));
        }
        env
    }

    /// Changed paths as plain git sees them with that environment (no
    /// system configuration, as on the private side).
    fn plain_changed_with(
        dir: &Path,
        env: &[(std::ffi::OsString, std::ffi::OsString)],
    ) -> Vec<String> {
        let mut cmd = Command::new("git");
        cmd.env_clear();
        for (k, v) in env {
            if k == "PATH" || k == "HOME" || k == "XDG_CONFIG_HOME" || k == "TMPDIR" {
                cmd.env(k, v);
            }
        }
        let out = cmd
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("-C")
            .arg(dir)
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
            .output()
            .unwrap();
        let mut paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|e| e.len() > 3)
            .map(|e| e[3..].to_owned())
            .collect();
        paths.sort();
        paths
    }

    /// Changed paths as private git sees them with that environment.
    fn private_changed_with(
        dir: &Path,
        env: &[(std::ffi::OsString, std::ffi::OsString)],
    ) -> Vec<String> {
        let repo = memory_git::bind(dir, env).unwrap();
        let (out, ok) = memory_git::run(
            memory_git::command(&repo, env).args([
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
            ]),
            1 << 20,
        )
        .unwrap();
        assert!(ok);
        let mut paths: Vec<String> = out
            .split('\0')
            .filter(|e| e.len() > 3)
            .map(|e| e[3..].to_owned())
            .collect();
        paths.sort();
        paths
    }

    /// Findings 1 and 2: both global files as layers in git's order, with
    /// `XDG_CONFIG_HOME` set, unset and empty; the last value per key first,
    /// then checked (a mixed-spelling duplicate never brings back an earlier
    /// value); includes declared. Private git always has git's effective
    /// value or git's default, and sees the same changes as plain git.
    #[cfg(unix)]
    #[test]
    fn global_files_are_layered_as_git_layers_them() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        if repo(
            &dir,
            &[("crlf.txt", b"one\r\ntwo\r\n"), ("lf.txt", b"one\ntwo\n")],
        )
        .is_none()
        {
            return;
        }
        let get = |file: &Path, key: &str| -> Option<String> {
            let out = Command::new("git")
                .args(["config", "--no-includes", "--file"])
                .arg(file)
                .args(["--get", key])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        };
        // (XDG text, ~/.gitconfig text, expected private autocrlf, a limit expected)
        let cases: Vec<(&str, Option<&str>, Option<&str>, Option<&str>, bool)> = vec![
            (
                "only the XDG file",
                Some("[core]\n\tautocrlf = true\n"),
                None,
                Some("true"),
                false,
            ),
            (
                "both: the home file wins",
                Some("[core]\n\tautocrlf = true\n"),
                Some("[core]\n\tautocrlf = false\n"),
                Some("false"),
                false,
            ),
            (
                "both, reversed",
                Some("[core]\n\tautocrlf = false\n"),
                Some("[core]\n\tautocrlf = input\n"),
                Some("input"),
                false,
            ),
            (
                "a duplicate with a mixed spelling",
                None,
                Some("[core]\n\tautocrlf = true\n\tautocrlf = no\n"),
                None,
                true,
            ),
            (
                "an earlier file's value, then an unsupported one",
                Some("[core]\n\tautocrlf = true\n"),
                Some("[core]\n\tautocrlf = Yes\n"),
                None,
                true,
            ),
            (
                "an include",
                None,
                Some("[include]\n\tpath = elsewhere\n[core]\n\tautocrlf = input\n"),
                Some("input"),
                true,
            ),
        ];
        let mut n = 0;
        for (what, xdg_text, home_text, expected, limit) in &cases {
            for mode in ["set", "unset", "empty"] {
                n += 1;
                let home = temp.path().join(format!("home{n}"));
                std::fs::create_dir_all(&home).unwrap();
                let xdg_dir = temp.path().join(format!("xdg{n}"));
                let (xdg_var, xdg_file) = match mode {
                    "set" => (
                        Some(xdg_dir.display().to_string()),
                        xdg_dir.join("git/config"),
                    ),
                    "unset" => (None, home.join(".config/git/config")),
                    _ => (Some(String::new()), home.join(".config/git/config")),
                };
                if let Some(t) = xdg_text {
                    std::fs::create_dir_all(xdg_file.parent().unwrap()).unwrap();
                    std::fs::write(&xdg_file, t).unwrap();
                }
                if let Some(t) = home_text {
                    std::fs::write(home.join(".gitconfig"), t).unwrap();
                }
                let env = env_with(&home, xdg_var.as_deref());
                let repo = memory_git::bind(&dir, &env)
                    .unwrap_or_else(|e| panic!("{what} ({mode}): {e:?}"));
                let private = repo.shadow.as_ref().unwrap().path().join("config");
                assert_eq!(
                    get(&private, "core.autocrlf").as_deref(),
                    *expected,
                    "{what} ({mode})"
                );
                assert_eq!(
                    !repo.facts.limits.is_empty(),
                    *limit,
                    "{what} ({mode}): {:?}",
                    repo.facts.limits
                );
                if what.contains("include") {
                    assert!(
                        repo.facts
                            .limits
                            .iter()
                            .any(|l| l.contains("includes are not followed"))
                    );
                }
                // Rewrite every file unchanged: what counts as a change is
                // what plain git says.
                std::fs::write(dir.join("crlf.txt"), "one\r\ntwo\r\n").unwrap();
                std::fs::write(dir.join("lf.txt"), "one\ntwo\n").unwrap();
                assert_eq!(
                    private_changed_with(&dir, &env),
                    plain_changed_with(&dir, &env),
                    "{what} ({mode})"
                );
            }
        }
        assert_eq!(n, 18);
    }

    /// The reproduction: canonical `core.autocrlf = true` only in the XDG
    /// file while `~/.gitconfig` exists: taken, as plain git takes it.
    #[cfg(unix)]
    #[test]
    fn a_setting_only_in_the_xdg_file_is_taken_while_the_home_file_exists() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        if repo(&dir, &[("crlf.txt", b"one\r\ntwo\r\n")]).is_none() {
            return;
        }
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".config/git")).unwrap();
        std::fs::write(home.join(".gitconfig"), "[user]\n\tname = T\n").unwrap();
        std::fs::write(
            home.join(".config/git/config"),
            "[core]\n\tautocrlf = true\n",
        )
        .unwrap();
        let env = env_with(&home, None);
        let repo = memory_git::bind(&dir, &env).unwrap();
        assert!(
            repo.facts
                .semantics
                .contains(&("core.autocrlf".to_owned(), "true".to_owned()))
        );
        std::fs::write(dir.join("crlf.txt"), "one\r\ntwo\r\n").unwrap();
        assert_eq!(
            private_changed_with(&dir, &env),
            plain_changed_with(&dir, &env)
        );
    }
}

/// The benchmark harness (spec §11.3): ignored tests with the real CLIs, run
/// by hand. See `docs/specs/project-memory.work/benchmark.md`.
#[cfg(test)]
#[path = "memory_bench.rs"]
mod bench;
