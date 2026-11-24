//! A new project cloned from a git repository, and a project deleted with its
//! folder.
//!
//! Cloning: the repository goes into a new folder named after it, inside the
//! folder the person picked (as `git clone <url>` run there would do). The
//! folder must not exist yet, so nothing of the person's is ever written over
//! or mixed in, and a failed or cancelled clone removes the whole folder: no
//! half-made project is left. The system's `git` runs it, so the person's
//! credential helper and SSH agent work; it never prompts in a terminal and
//! Connecto never asks for or keeps a password.
//!
//! The clone goes into a folder Connecto itself made (it must not exist
//! before), whose identity on disk is kept: a failed or cancelled clone
//! removes that folder only while it is still the one Connecto made, and
//! says so when it cannot.
//!
//! Only https, ssh (`ssh://` or `user@host:path`), git and local folders
//! (an absolute path or `file://`) are cloned; git is told the same, so no
//! other transport or remote helper runs. An https address carries no user
//! name or token: the credential helper supplies them.
//!
//! Deleting: refused for a folder opened through a link, a disk or volume
//! root (a mount boundary), the home folder, system folders, the home
//! folder's standard folders and its CLI or system state, anything holding
//! Connecto's data, its source checkout or the running app, and anything
//! holding another project. Otherwise the folder goes to the system's trash
//! (the macOS Trash, or the freedesktop trash on Linux), from where it can be
//! put back. Nothing is ever deleted for good: a folder with no usable trash
//! (another disk on Linux, Windows for now) is refused, and the person
//! removes it in their file manager. The folder is identified on disk when
//! the person confirms and checked again just before it is moved.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use connecto_core::project_files::{Cloning, DeletePlan, ProjectTask, TaskState};
use connecto_session::launch::{Layout, validated};

/// The lines of git's output kept to explain a failure.
const KEPT_LINES: usize = 40;
/// How long the count of a folder's contents may take before it says "more than".
const COUNT_LIMIT: Duration = Duration::from_secs(2);
const COUNT_MAX_ENTRIES: u64 = 500_000;

// ---------- tasks ----------

/// A clone or a deletion under way.
pub(crate) struct Task {
    state: Mutex<TaskState>,
    cancelled: AtomicBool,
    /// The running git, so Cancel can stop it.
    child: Mutex<Option<Child>>,
    /// Whether Cancel applies (a clone) or is ignored (a deletion, which is
    /// not stopped half-way).
    cancellable: bool,
    /// Where a finished clone went.
    published: Mutex<Option<PathBuf>>,
}

impl Task {
    pub(crate) fn new(line: &str, cancellable: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(TaskState::Running(line.to_owned())),
            cancelled: AtomicBool::new(false),
            child: Mutex::new(None),
            cancellable,
            published: Mutex::new(None),
        })
    }

    pub(crate) fn set(&self, state: TaskState) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
    }

    /// A new progress line, while it is still running.
    pub(crate) fn progress(&self, line: &str) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.is_running() {
            *state = TaskState::Running(line.to_owned());
        }
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl ProjectTask for Task {
    fn published(&self) -> Option<PathBuf> {
        self.published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn state(&self) -> TaskState {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn cancel(&self) {
        if !self.cancellable || !self.state().is_running() {
            return;
        }
        self.cancelled.store(true, Ordering::SeqCst);
        self.progress("Cancelling…");
        if let Some(child) = self
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            kill_tree(child);
        }
    }
}

/// Stop git and what it started (the transport helper, ssh, index-pack).
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            // The clone runs in a process group of its own.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

// ---------- clone ----------

/// The folder name a repository address gives, as `git clone` picks it:
/// the last part of the path, without `.git`.
pub(crate) fn repo_name(url: &str) -> Result<String, String> {
    let url = check_url(url)?;
    let path = url.trim_end_matches(['/', '\\']);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches(['/', '\\']);
    let last = path
        .rsplit(['/', '\\', ':'])
        .next()
        .unwrap_or_default()
        .trim();
    if last.is_empty() || last == "." || last == ".." || last.starts_with('-') {
        return Err(format!(
            "No folder name can be taken from \"{url}\". Give the repository's address, such as https://github.com/owner/project.git or git@github.com:owner/project.git."
        ));
    }
    if last
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Err(format!(
            "The repository name \"{last}\" cannot be a folder name."
        ));
    }
    Ok(last.to_owned())
}

/// The address as given, trimmed, or why it cannot be one.
fn check_url(url: &str) -> Result<&str, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("Give the repository's address.".to_owned());
    }
    // A local folder may have spaces in its name; nothing has a control
    // character.
    if url.chars().any(char::is_control) {
        return Err(format!("\"{url}\" is not a repository address."));
    }
    if url.starts_with('-') {
        return Err(format!("\"{url}\" is not a repository address."));
    }
    transport(url)?;
    Ok(url)
}

const TRANSPORTS: &str = "Connecto clones from https, ssh, git or a local folder (a full path).";

/// Only the transports Connecto allows (see the module notes).
fn transport(url: &str) -> Result<(), String> {
    // `<helper>::<address>` runs a remote helper. An IPv6 host (`[::1]`)
    // has `::` too, but never right after a name at the start.
    if let Some((helper, _)) = url.split_once("::")
        && helper
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && helper
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return Err(TRANSPORTS.to_owned());
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        return match scheme.to_ascii_lowercase().as_str() {
            "https" | "http" => {
                let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
                if authority.contains('@') {
                    // A name or token here would be kept in the clone's
                    // settings (remote.origin.url).
                    Err("Leave the user name, password or token out of the address: git asks your credential helper for them, and Connecto never keeps one.".to_owned())
                } else {
                    Ok(())
                }
            }
            "ssh" | "git" | "file" => Ok(()),
            _ => Err(TRANSPORTS.to_owned()),
        };
    }
    let bytes = url.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if url.starts_with('/') || url.starts_with(r"\\") || drive {
        return Ok(());
    }
    // `[user@]host:path`, as git reads it: a colon before any slash.
    if let Some(colon) = url.find(':')
        && url.find('/').is_none_or(|slash| colon < slash)
        && colon > 0
    {
        return Ok(());
    }
    Err(TRANSPORTS.to_owned())
}

/// What git is told it may use: the same transports as [`transport`].
const PROTOCOLS: [&str; 12] = [
    "-c",
    "protocol.allow=never",
    "-c",
    "protocol.https.allow=always",
    "-c",
    "protocol.http.allow=always",
    "-c",
    "protocol.ssh.allow=always",
    "-c",
    "protocol.git.allow=always",
    "-c",
    "protocol.file.allow=always",
];

/// Where a clone of `url` into `parent` would go: the new folder in the
/// real parent folder. Refused when something is already there, and by the
/// rules for a project's folder (never Connecto's data, the CLIs' or the
/// system's state), checked on that folder before git writes anything.
pub(crate) fn clone_target(layout: &Layout, url: &str, parent: &Path) -> Result<PathBuf, String> {
    let name = repo_name(url)?;
    let parent = fs::canonicalize(parent)
        .map_err(|_| format!("The folder {} cannot be found.", parent.display()))?;
    if !parent.is_dir() {
        return Err(format!("{} is not a folder.", parent.display()));
    }
    let target = parent.join(&name);
    if fs::symlink_metadata(&target).is_ok() {
        return Err(format!(
            "{} already has something named \"{name}\". Pick another folder, or move that one away first: a clone goes into a new folder of its own.",
            parent.display()
        ));
    }
    let roots = connecto_session::project_folder::ForbiddenRoots {
        home: layout.home.clone(),
        data_root: layout.data_root.clone(),
        allowed: connecto_session::project_folder::allowed_root(&layout.data_root),
    };
    connecto_session::project_folder::validate_new_project(&target, &roots)
        .map_err(|err| format!("Nothing was cloned: {err}"))
}

/// Runs its action once when dropped: a reservation that is released however
/// the work holding it ends, a thread that never started included.
pub(crate) struct OnDrop(Option<Box<dyn FnOnce() + Send>>);

impl OnDrop {
    pub(crate) fn new(action: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(action)))
    }
}

impl Drop for OnDrop {
    fn drop(&mut self) {
        if let Some(action) = self.0.take() {
            action();
        }
    }
}

/// Start cloning `url` into `target` (from [`clone_target`]). Quick: git
/// runs on a thread of its own. `release` is dropped once the clone has
/// ended, or as soon as it cannot start.
///
/// git writes into a private staging folder beside the target, made by
/// Connecto (it must not exist before) and known by its identity on disk.
/// Only that folder is ever removed. The finished checkout is published by a
/// rename that never replaces anything, then checked before it is opened.
pub(crate) fn start_clone(
    layout: &Layout,
    url: &str,
    target: &Path,
    release: OnDrop,
    task: Arc<Task>,
) -> Result<Cloning, String> {
    let started = staged_clone(layout, url, target);
    match started {
        Ok((url, staging, owned)) => {
            task.progress("Starting git…");
            let spawned = thread::Builder::new().name("git-clone".into()).spawn({
                let task = Arc::clone(&task);
                let target = target.to_owned();
                let staging = staging.clone();
                move || {
                    run_clone(&task, &url, &staging, &target, owned, release);
                }
            });
            match spawned {
                Ok(_) => Ok(Cloning {
                    target: target.to_owned(),
                    task: task as Arc<dyn ProjectTask>,
                }),
                // The closure, and `release` in it, was dropped: released.
                Err(err) => Err(match remove_owned(&staging, owned) {
                    Ok(()) => format!("The clone could not start: {err}"),
                    Err(left) => format!(
                        "The clone could not start: {err}\nIts folder {} was not removed: {left} Remove it yourself.",
                        staging.display()
                    ),
                }),
            }
        }
        Err(why) => {
            drop(release);
            Err(why)
        }
    }
}

/// Check the address and make the staging folder: its path and identity.
fn staged_clone(
    layout: &Layout,
    url: &str,
    target: &Path,
) -> Result<(String, PathBuf, [u64; 2]), String> {
    let url = check_url(url)?.to_owned();
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(format!("{} cannot be cloned into.", target.display()));
    };
    let staging = make_staging(parent, &name.to_string_lossy())?;
    let owned = match opened_identity(&staging) {
        Some(owned) => owned,
        None => {
            let _ = fs::remove_dir(&staging);
            return Err(format!(
                "The folder {} could not be read back.",
                staging.display()
            ));
        }
    };
    // The same rules as opening a project: never into Connecto's data or the
    // CLIs' state (the staging folder sits where the clone will).
    if let Err(err) = validated(layout, &staging) {
        let _ = fs::remove_dir(&staging);
        return Err(err.to_string());
    }
    // Checked again now the staging folder is there: the final name must
    // still be free (on a case-insensitive volume `.CLAUDE` names an
    // existing `.claude`), and allowed by the rules for a new project.
    let roots = connecto_session::project_folder::ForbiddenRoots {
        home: layout.home.clone(),
        data_root: layout.data_root.clone(),
        allowed: connecto_session::project_folder::allowed_root(&layout.data_root),
    };
    if fs::symlink_metadata(target).is_ok() {
        let _ = fs::remove_dir(&staging);
        return Err(format!(
            "Nothing was cloned: {} names a folder that is already there.",
            target.display()
        ));
    }
    if let Err(err) = connecto_session::project_folder::validate_new_project(target, &roots) {
        let _ = fs::remove_dir(&staging);
        return Err(format!("Nothing was cloned: {err}"));
    }
    Ok((url, staging, owned))
}

/// A new, empty, hidden folder beside the target, made by this call alone.
fn make_staging(parent: &Path, name: &str) -> Result<PathBuf, String> {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    for _ in 0..16 {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let staging = parent.join(format!(
            ".{name}.connecto-clone-{}-{nanos:x}-{n}",
            std::process::id()
        ));
        match fs::create_dir(&staging) {
            Ok(()) => return Ok(staging),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(format!(
                    "A folder for the clone could not be made in {}: {err}",
                    parent.display()
                ));
            }
        }
    }
    Err(format!(
        "A folder for the clone could not be made in {}.",
        parent.display()
    ))
}

fn run_clone(
    task: &Task,
    url: &str,
    staging: &Path,
    target: &Path,
    owned: [u64; 2],
    release: OnDrop,
) {
    let result = clone_into(task, url, staging)
        .map_err(Published::Failed)
        .and_then(|()| {
            if task.cancelled() {
                Err(Published::Failed(String::new()))
            } else {
                publish(staging, target, owned)
            }
        });
    let state = match result {
        Ok(()) => {
            *task
                .published
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(target.to_owned());
            TaskState::Done
        }
        Err(Published::Kept(why)) => TaskState::Failed(why),
        Err(Published::Failed(why)) => match (task.cancelled(), remove_owned(staging, owned)) {
            (true, Ok(())) => TaskState::Cancelled,
            (false, Ok(())) => TaskState::Failed(why),
            (cancelled, Err(left)) => TaskState::Failed(format!(
                "{}\nThe partial clone in {} was not removed: {left} Remove it yourself.",
                if cancelled {
                    "The clone was stopped."
                } else {
                    why.as_str()
                },
                staging.display()
            )),
        },
    };
    drop(release);
    task.set(state);
}

/// Why a clone did not end in a published project.
enum Published {
    /// Nothing worth keeping: the staging folder goes.
    Failed(String),
    /// The finished checkout stays in the staging folder, which the message
    /// names.
    Kept(String),
}

/// Move the finished checkout to its place, never replacing anything there,
/// and check that what is there now is it.
fn publish(staging: &Path, target: &Path, owned: [u64; 2]) -> Result<(), Published> {
    if let Err(err) = rename_no_replace(staging, target) {
        return Err(Published::Kept(format!(
            "The clone finished, but {} could not be made ({err}): something may have been put there meanwhile, and it was left alone. The clone is kept in {}.",
            target.display(),
            staging.display()
        )));
    }
    if identity(target) != Some(owned) {
        return Err(Published::Kept(format!(
            "The clone finished, but what is at {} now is not it. Nothing was opened.",
            target.display()
        )));
    }
    Ok(())
}

/// Rename `from` to `to`, failing when anything is at `to`.
#[cfg(unix)]
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(std::io::Error::from)
}

/// .NET's `Directory.Move`, which fails when the destination exists (std's
/// rename may replace a file there).
#[cfg(windows)]
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[System.IO.Directory]::Move($env:CONNECTO_MOVE_FROM, $env:CONNECTO_MOVE_TO)",
        ])
        .env("CONNECTO_MOVE_FROM", from)
        .env("CONNECTO_MOVE_TO", to)
        .stdin(Stdio::null())
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ))
    }
}

/// Remove the private staging folder, only while it is still the one
/// Connecto made (`owned`): checked again before every attempt.
fn remove_owned(staging: &Path, owned: [u64; 2]) -> Result<(), String> {
    let mut last = None;
    for _ in 0..5 {
        match identity(staging) {
            None if fs::symlink_metadata(staging).is_err() => return Ok(()),
            Some(now) if now == owned => {}
            _ => {
                return Err("something else is at that path now, so it was left alone.".to_owned());
            }
        }
        match fs::remove_dir_all(staging) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => last = Some(err),
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(last.map_or_else(String::new, |err| format!("{err}.")))
}

/// A directory's identity on disk (device and inode; on Windows its creation
/// time), or `None` for a link or nothing.
fn identity(path: &Path) -> Option<[u64; 2]> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    of_meta(&meta)
}

#[cfg(unix)]
fn of_meta(meta: &fs::Metadata) -> Option<[u64; 2]> {
    use std::os::unix::fs::MetadataExt;
    Some([meta.dev(), meta.ino()])
}

#[cfg(windows)]
fn of_meta(meta: &fs::Metadata) -> Option<[u64; 2]> {
    use std::os::windows::fs::MetadataExt;
    Some([meta.creation_time(), 0])
}

/// The identity of the directory as opened (not looked up again by name).
fn opened_identity(path: &Path) -> Option<[u64; 2]> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS: needed to open a directory.
        options.custom_flags(0x0200_0000);
    }
    let meta = options.open(path).ok()?.metadata().ok()?;
    if !meta.is_dir() {
        return None;
    }
    of_meta(&meta)
}

fn clone_into(task: &Task, url: &str, target: &Path) -> Result<(), String> {
    let mut command = Command::new("git");
    command
        .args(PROTOCOLS)
        .args(["clone", "--progress", "--"])
        .arg(url)
        .arg(target)
        // Never wait for a password typed in a terminal: the credential
        // helper answers, or the clone fails and says so.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if task.cancelled() {
        return Err(String::new());
    }
    let mut child = command.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            "git is not installed, or Connecto cannot find it. Install git and try again."
                .to_owned()
        } else {
            format!("git could not be started: {err}")
        }
    })?;
    let stderr = child.stderr.take();
    *task.child.lock().unwrap_or_else(PoisonError::into_inner) = Some(child);
    if task.cancelled()
        && let Some(child) = task
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
    {
        kill_tree(child);
    }
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let read_done = Arc::new(AtomicBool::new(false));
    if let Some(mut stderr) = stderr {
        let lines = Arc::clone(&lines);
        let read_done = Arc::clone(&read_done);
        // The task is not 'static here: progress goes through the shared list
        // and is copied into the task by the waiting loop below.
        let _ = thread::Builder::new()
            .name("git-clone-output".into())
            .spawn(move || {
                let mut buffer = [0_u8; 4096];
                let mut pending = Vec::new();
                while let Ok(read) = stderr.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    for &byte in &buffer[..read] {
                        if byte == b'\r' || byte == b'\n' {
                            let line = String::from_utf8_lossy(&pending).trim().to_owned();
                            pending.clear();
                            if !line.is_empty() {
                                let mut kept = lines.lock().unwrap_or_else(PoisonError::into_inner);
                                kept.push(line);
                                let extra = kept.len().saturating_sub(KEPT_LINES);
                                kept.drain(..extra);
                            }
                        } else {
                            pending.push(byte);
                        }
                    }
                }
                let line = String::from_utf8_lossy(&pending).trim().to_owned();
                if !line.is_empty() {
                    lines
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(line);
                }
                read_done.store(true, Ordering::SeqCst);
            });
    } else {
        read_done.store(true, Ordering::SeqCst);
    }
    let status = loop {
        if let Some(last) = lines.lock().unwrap_or_else(PoisonError::into_inner).last() {
            task.progress(last);
        }
        let waited = task
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .map(Child::try_wait);
        match waited {
            Some(Ok(Some(status))) => break Some(status),
            Some(Ok(None)) => thread::sleep(Duration::from_millis(50)),
            Some(Err(_)) | None => break None,
        }
    };
    task.child
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    // The last words git wrote, if they come soon.
    let deadline = Instant::now() + Duration::from_secs(1);
    while !read_done.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if task.cancelled() {
        return Err(String::new());
    }
    match status {
        Some(status) if status.success() => Ok(()),
        _ => Err(clone_failure(
            &lines.lock().unwrap_or_else(PoisonError::into_inner),
        )),
    }
}

/// Why a clone failed, in words for the person, from what git wrote.
pub(crate) fn clone_failure(lines: &[String]) -> String {
    let all = lines.join("\n").to_lowercase();
    let detail = lines
        .iter()
        .rev()
        .find(|line| line.starts_with("fatal:") || line.starts_with("error:"))
        .or_else(|| lines.last())
        .cloned()
        .unwrap_or_default();
    let said = |text: &str| all.contains(text);
    let summary = if said("authentication failed")
        || said("could not read username")
        || said("could not read password")
        || said("terminal prompts disabled")
        || said("permission denied (publickey")
        || said("host key verification failed")
        || said("invalid username or password")
        || said("access denied")
    {
        "The repository refused access. Set up your access with git itself (a credential helper or an SSH key in your SSH agent), then try again. Connecto never asks for or keeps a password."
    } else if said("repository not found")
        || said("does not appear to be a git repository")
        || said("does not exist")
        || said("not found")
    {
        "No repository was found at that address, or your account cannot see it."
    } else if said("could not resolve host") || said("could not resolve hostname") {
        "The repository's host could not be found. Check the address and the network."
    } else if said("unable to access") || said("connection refused") || said("timed out") {
        "The repository's host could not be reached. Check the address and the network."
    } else {
        "git could not clone the repository."
    };
    if detail.is_empty() {
        summary.to_owned()
    } else {
        format!("{summary}\n{detail}")
    }
}

/// Take a folder away, trying again for a moment (a process that has just
/// been stopped may still hold a file).
pub(crate) fn remove_all(path: &Path) {
    for _ in 0..20 {
        match fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

// ---------- delete ----------

/// What a deletion must never reach.
pub(crate) struct Guards {
    pub(crate) home: PathBuf,
    pub(crate) data_root: PathBuf,
    /// Folders a deleted folder must not be or hold: Connecto Home, the
    /// Automations folder, Connecto's source checkout, the running app.
    pub(crate) connecto: Vec<PathBuf>,
    /// Folders a deleted project's folder must not be: Connecto's internal
    /// projects (the Automations and agentic jobs folders).
    pub(crate) internal: Vec<PathBuf>,
    /// The other projects, open or recent.
    pub(crate) others: Vec<PathBuf>,
    /// The project was opened through a link somewhere in its path.
    pub(crate) linked: bool,
}

/// A folder that passed [`refusal`]: its real path and identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checked {
    pub(crate) real: PathBuf,
    /// Its device and inode, then its parent's.
    pub(crate) identity: [u64; 4],
}

fn real_or(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// The trees of the system: never deleted, nor anything in them.
#[cfg(unix)]
const SYSTEM_TREES: &[&str] = &[
    "/System",
    "/Library",
    "/Applications",
    "/Network",
    "/cores",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc",
    "/var",
    "/tmp",
    "/opt",
    "/private",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/boot",
    "/root",
    "/srv",
    "/nix",
    "/snap",
];
#[cfg(windows)]
const SYSTEM_TREES: &[&str] = &[
    r"C:\Windows",
    r"C:\Program Files",
    r"C:\Program Files (x86)",
    r"C:\ProgramData",
];
/// Folders that hold other people's or many things: refused themselves (a
/// project inside one is fine).
#[cfg(unix)]
const SYSTEM_FOLDERS: &[&str] = &["/Users", "/home", "/Volumes", "/mnt", "/media"];
#[cfg(windows)]
const SYSTEM_FOLDERS: &[&str] = &[r"C:\Users", r"C:\Users\Public"];

/// The home folder's own folders: never deleted as a project.
const HOME_FOLDERS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Pictures",
    "Music",
    "Movies",
    "Videos",
    "Public",
    "Applications",
    "Sites",
    "OneDrive",
    "iCloud Drive",
];
/// The home folder's state: never deleted, nor anything in it.
const HOME_STATE: &[&str] = &[
    "Library", "AppData", ".claude", ".codex", ".local", ".pi", ".gemini", ".config", ".ssh",
    ".gnupg", ".cargo", ".rustup", ".Trash",
];

/// The device of a folder, for mount boundaries.
#[cfg(unix)]
fn device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).ok().map(|meta| meta.dev())
}

/// Whether any folder on the way to `path` (or `path` itself) is a link.
pub(crate) fn through_link(path: &Path) -> bool {
    let Ok(absolute) = std::path::absolute(path) else {
        return true;
    };
    absolute
        .ancestors()
        .any(|step| fs::symlink_metadata(step).is_ok_and(|meta| meta.file_type().is_symlink()))
}

/// `path` is `prefix` or inside it: for a folder that is there, by its
/// identity on disk (whatever spelling leads to it); otherwise by name, as a
/// case- and normalization-insensitive volume compares names.
fn within(path: &Path, prefix: &Path) -> bool {
    connecto_session::project_folder::within_protected(path, prefix)
}

fn same_folded(a: &Path, b: &Path) -> bool {
    within(a, b) && within(b, a)
}

/// The folder a deletion would remove, or why it is refused.
pub(crate) fn refusal(path: &Path, guards: &Guards) -> Result<Checked, String> {
    let shown = path.display();
    let refuse = |why: &str| Err(format!("Connecto will not delete {shown}: {why}"));
    if guards.linked {
        return refuse(
            "the project was opened through a link, so its folder may be shared with another place. Remove it yourself if you mean to.",
        );
    }
    let meta = fs::symlink_metadata(path).map_err(|_| {
        format!("{shown} no longer exists: there is nothing to delete. Close the project instead.")
    })?;
    if meta.file_type().is_symlink() || through_link(path) {
        return refuse("it is reached through a link. Remove it yourself if you mean to.");
    }
    if !meta.is_dir() {
        return refuse("it is not a folder.");
    }
    let real = fs::canonicalize(path).map_err(|err| format!("{shown} cannot be read: {err}"))?;
    #[cfg(unix)]
    if real != path {
        return refuse("it is reached through a link. Remove it yourself if you mean to.");
    }
    let Some(parent) = real.parent() else {
        return refuse("it is the root of a disk.");
    };
    let normal = real
        .components()
        .filter(|part| matches!(part, Component::Normal(_)))
        .count();
    if normal < 2 {
        return refuse("it is a top-level folder of the disk.");
    }
    #[cfg(unix)]
    if device(&real) != device(parent) {
        return refuse("it is the top of a disk or volume.");
    }
    let home = real_or(&guards.home);
    if within(&home, &real) {
        return refuse("it is your home folder, or holds it.");
    }
    let tree =
        |root: &str| within(&real, Path::new(root)) || within(&real, &real_or(Path::new(root)));
    let same = |folder: &str| {
        same_folded(&real, Path::new(folder)) || same_folded(&real, &real_or(Path::new(folder)))
    };
    if SYSTEM_TREES.iter().any(|root| tree(root)) || SYSTEM_FOLDERS.iter().any(|f| same(f)) {
        return refuse("it is a system folder.");
    }
    // Each one as a whole folder, by identity when it is there: a project
    // inside Documents may go, Documents itself (however spelled) not.
    if HOME_FOLDERS
        .iter()
        .any(|name| same_folded(&real, &home.join(name)))
    {
        return refuse("it is one of your home folder's own folders.");
    }
    if HOME_STATE
        .iter()
        .any(|name| within(&real, &home.join(name)))
    {
        return refuse("it holds the settings of your system or your tools.");
    }
    let data = real_or(&guards.data_root);
    if within(&data, &real) || within(&real, &data) {
        return refuse("it holds Connecto's own data.");
    }
    if guards
        .connecto
        .iter()
        .any(|folder| within(&real_or(folder), &real))
    {
        return refuse(
            "it holds Connecto itself or its data (Connecto Home, the Automations folder, its source or the running app).",
        );
    }
    if guards
        .internal
        .iter()
        .any(|folder| same_folded(&real_or(folder), &real))
    {
        return refuse("it is one of Connecto's own folders.");
    }
    for other in &guards.others {
        let other = real_or(other);
        if !same_folded(&other, &real) && within(&other, &real) {
            return Err(format!(
                "Connecto will not delete {shown}: it holds another project, {}. Close or move that project first.",
                other.display()
            ));
        }
    }
    let (Some(own), Some(up)) = (identity(&real), identity(parent)) else {
        return refuse("it cannot be read.");
    };
    Ok(Checked {
        identity: [own[0], own[1], up[0], up[1]],
        real,
    })
}

/// What the confirmation shows about a checked folder, or why it is refused
/// (no usable trash).
pub(crate) fn plan(
    project: &str,
    checked: &Checked,
    bin: &Bin,
    token: u64,
) -> Result<DeletePlan, String> {
    let real = &checked.real;
    let trash = bin.name(real)?;
    Ok(DeletePlan {
        project: project.to_owned(),
        name: real.file_name().map_or_else(
            || real.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        ),
        path: real.display().to_string(),
        contents: contents(real),
        trash,
        warnings: git_warnings(real),
        identity: checked.identity.to_vec(),
        token,
    })
}

/// The confirmed folder and its parent, held open from the moment the
/// confirmation shows them until the deletion ends or the dialog closes.
/// While they are open their identities (device and inode) cannot be given
/// to another folder, so a folder put in their place never matches.
pub(crate) struct Held {
    _dir: fs::File,
    _parent: fs::File,
    /// The folder's device and inode, then its parent's, read from the open
    /// handles.
    pub(crate) identity: [u64; 4],
}

impl Held {
    /// Open `real` and its parent. `None` when either cannot be opened as a
    /// directory.
    pub(crate) fn open(real: &Path) -> Option<Self> {
        let (dir, own) = open_dir(real)?;
        let (parent, up) = open_dir(real.parent()?)?;
        Some(Self {
            _dir: dir,
            _parent: parent,
            identity: [own[0], own[1], up[0], up[1]],
        })
    }
}

/// A directory opened (never through a link at its last step) and its
/// identity as opened.
fn open_dir(path: &Path) -> Option<(fs::File, [u64; 2])> {
    if fs::symlink_metadata(path).ok()?.file_type().is_symlink() {
        return None;
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS: needed to open a directory.
        options.custom_flags(0x0200_0000);
    }
    let file = options.open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_dir() {
        return None;
    }
    Some((file, of_meta(&meta)?))
}

/// Whether the folder at `real` is still the one held: no link on the way,
/// the same parent, and, read last, the same directory.
fn still_the_same(real: &Path, held: &Held) -> bool {
    let Some(parent) = real.parent() else {
        return false;
    };
    let identity = held.identity;
    // The path checks first, so nothing but the folder's own identity is read
    // after them.
    !through_link(real)
        && fs::canonicalize(real).is_ok_and(|now| now == real)
        && self::identity(parent) == Some([identity[2], identity[3]])
        && self::identity(real) == Some([identity[0], identity[1]])
}

/// "1,204 files in 87 folders, 31.2 MB", without following links.
fn contents(root: &Path) -> String {
    let started = Instant::now();
    let (mut files, mut folders, mut bytes) = (0_u64, 0_u64, 0_u64);
    let mut stack = vec![root.to_owned()];
    let mut partial = false;
    'walk: while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if files + folders >= COUNT_MAX_ENTRIES || started.elapsed() > COUNT_LIMIT {
                partial = true;
                break 'walk;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                folders += 1;
                stack.push(entry.path());
            } else {
                files += 1;
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    let more = if partial { "more than " } else { "" };
    format!(
        "{more}{} {} in {} {}, {more}{}",
        grouped(files),
        if files == 1 { "file" } else { "files" },
        grouped(folders),
        if folders == 1 { "folder" } else { "folders" },
        size(bytes)
    )
}

fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[allow(clippy::cast_precision_loss)]
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// git's output, or `None` when it failed.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn lines(text: &str) -> usize {
    text.lines().filter(|l| !l.trim().is_empty()).count()
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Work in the folder's git repository that would be lost. A check git could
/// not make is said too, never taken for "nothing".
pub(crate) fn git_warnings(real: &Path) -> Vec<String> {
    const UNCHECKED: &str = "Git could not check this repository fully: there may be uncommitted or unpushed work that is not listed here.";
    let mut warnings = Vec::new();
    let Some(top) = git(real, &["rev-parse", "--show-toplevel"]) else {
        // A bare repository has no working tree and no `.git` folder.
        if git(real, &["rev-parse", "--is-bare-repository"]).is_some_and(|b| b.trim() == "true") {
            return bare_warnings(real);
        }
        if real.join(".git").exists() {
            warnings.push(UNCHECKED.to_owned());
        }
        return warnings;
    };
    let top = real_or(Path::new(top.trim()));
    // A repository of its own that git does not take as one (a damaged
    // `.git`): git answered for an enclosing repository instead.
    if top != real && real.join(".git").exists() {
        warnings.push(UNCHECKED.to_owned());
        return warnings;
    }
    let mut unchecked = false;
    // Only this folder's part of the repository: `.` is the folder itself.
    match git(
        real,
        &[
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--",
            ".",
        ],
    ) {
        Some(status) if lines(&status) > 0 => warnings.push(format!(
            "{} (changed or untracked files) will be lost.",
            plural(lines(&status), "uncommitted change", "uncommitted changes")
        )),
        Some(_) => {}
        None => unchecked = true,
    }
    if top != real {
        warnings.push(format!(
            "This folder is inside the git repository {}: deleting it shows there as a change.",
            top.display()
        ));
        if unchecked {
            warnings.push(UNCHECKED.to_owned());
        }
        return warnings;
    }
    let has_commits = git(real, &["rev-parse", "--verify", "-q", "HEAD"]).is_some();
    let remotes = git(real, &["remote"]).map(|r| lines(&r));
    // HEAD not on a branch: commits made there are on no branch.
    if has_commits && git(real, &["symbolic-ref", "-q", "HEAD"]).is_none() {
        match git(
            real,
            &[
                "rev-list",
                "--count",
                "HEAD",
                "--not",
                "--branches",
                "--remotes",
                "--tags",
            ],
        ) {
            Some(count) => {
                let count = count.trim().parse::<usize>().unwrap_or(0);
                warnings.push(if count > 0 {
                    format!(
                        "HEAD is detached: {} on no branch will be lost.",
                        plural(count, "commit", "commits")
                    )
                } else {
                    "HEAD is detached (not on a branch).".to_owned()
                });
            }
            None => unchecked = true,
        }
    }
    match remotes {
        None => unchecked = true,
        Some(0) => {
            if has_commits {
                warnings.push(
                    "The repository has no remote: its whole history exists only in this folder."
                        .to_owned(),
                );
            }
        }
        Some(_) => match git(
            real,
            &["rev-list", "--count", "--branches", "--not", "--remotes"],
        ) {
            Some(count) => {
                let count = count.trim().parse::<usize>().unwrap_or(0);
                if count > 0 {
                    warnings.push(format!(
                        "{} on local branches not pushed to any remote will be lost.",
                        plural(count, "commit", "commits")
                    ));
                }
            }
            None => unchecked = true,
        },
    }
    match git(real, &["stash", "list"]) {
        Some(stashes) if lines(&stashes) > 0 => warnings.push(format!(
            "{} will be lost.",
            plural(lines(&stashes), "stash", "stashes")
        )),
        Some(_) => {}
        None => unchecked = true,
    }
    if unchecked {
        warnings.push(UNCHECKED.to_owned());
    }
    warnings
}

/// A bare repository (or a folder in one): its whole history is what goes.
fn bare_warnings(real: &Path) -> Vec<String> {
    let git_dir =
        git(real, &["rev-parse", "--absolute-git-dir"]).map(|dir| real_or(Path::new(dir.trim())));
    let Some(git_dir) = git_dir else {
        return vec!["This folder belongs to a bare git repository that git could not read fully: its history may be lost.".to_owned()];
    };
    if git_dir != real {
        return vec![format!(
            "This folder is inside the bare git repository {}: deleting it damages that repository.",
            git_dir.display()
        )];
    }
    let mut warnings = vec![
        "This folder is a bare git repository: the repository itself, with all its history, goes."
            .to_owned(),
    ];
    let has_refs = git(real, &["for-each-ref", "--count=1"]).map(|refs| lines(&refs) > 0);
    let remotes = git(real, &["remote"]).map(|r| lines(&r));
    match (has_refs, remotes) {
        (Some(false), _) => {}
        (Some(true), Some(0)) => {
            warnings.push("It has no remote: its history may exist only here.".to_owned())
        }
        (Some(true), Some(_)) => {
            match git(
                real,
                &["rev-list", "--count", "--branches", "--not", "--remotes"],
            ) {
                Some(count) if count.trim() != "0" => warnings.push(format!(
                    "{} on its branches not found on any remote will be lost.",
                    plural(count.trim().parse().unwrap_or(0), "commit", "commits")
                )),
                Some(_) => {}
                None => warnings
                    .push("Git could not check whether its history exists elsewhere.".to_owned()),
            }
        }
        _ => warnings.push("Git could not check whether its history exists elsewhere.".to_owned()),
    }
    warnings
}

// ---------- the trash ----------

/// Where deleted folders go.
#[derive(Clone)]
pub(crate) enum Bin {
    /// The system's trash.
    System,
    /// A folder standing in for it, for tests.
    Folder(PathBuf),
}

/// A move to the trash made ready (its name chosen, its record written), so
/// that only the move itself follows the last check.
enum Prepared {
    /// A test folder: the free name in it.
    Folder(PathBuf),
    System(system::Prepared),
}

impl Bin {
    /// The name of the trash `real` would go to, or why it has none.
    pub(crate) fn name(&self, real: &Path) -> Result<String, String> {
        match self {
            Self::Folder(_) => Ok("the Trash".to_owned()),
            Self::System => system::name(real),
        }
    }

    /// Move the checked folder to the trash, if it is still the one checked.
    /// Never deletes anything for good. The error says exactly what was and
    /// was not moved.
    ///
    /// Everything the move needs is made ready first, then the folder and its
    /// parent are checked against the held handles (the path checks first,
    /// the folder's own identity last), then the move is made. The move goes
    /// by path (the Finder's and the freedesktop trash's own way, which keeps
    /// "Put Back" working), so a folder swapped in between that last check
    /// and the move would be moved instead. On Linux that window is one
    /// `renameat2` call; on macOS it is one Foundation call
    /// (`trashItemAtURL`), whose own system calls Connecto does not control.
    /// It comes after every process of the project has stopped, and even then
    /// nothing is erased: the moved item is checked after the move, and the
    /// error names it and where it is in the Trash.
    pub(crate) fn remove(&self, real: &Path, held: &Held) -> Result<(), String> {
        self.remove_with(real, held, || {})
    }

    /// [`Self::remove`], running `between` after the last check and before
    /// the move: for tests that swap the folder there.
    fn remove_with(&self, real: &Path, held: &Held, between: impl FnOnce()) -> Result<(), String> {
        let identity = held.identity;
        let prepared = match self {
            Self::Folder(bin) => {
                let name = real.file_name().unwrap_or_default();
                let mut target = bin.join(name);
                let mut n = 1;
                while fs::symlink_metadata(&target).is_ok() {
                    n += 1;
                    target = bin.join(format!("{} {n}", name.to_string_lossy()));
                }
                Prepared::Folder(target)
            }
            Self::System => Prepared::System(
                system::prepare(real).map_err(|why| format!("Nothing was moved: {why}"))?,
            ),
        };
        // As late as can be: nothing but the move comes after it.
        if !still_the_same(real, held) {
            if let Prepared::System(prepared) = prepared {
                prepared.abandon();
            }
            return Err(format!(
                "The folder at {} changed since the deletion was confirmed, so nothing was moved.",
                real.display()
            ));
        }
        between();
        let moved =
            match prepared {
                Prepared::Folder(target) => rename_no_replace(real, &target)
                    .map(|()| target)
                    .map_err(|err| {
                        format!(
                            "Nothing was moved: the folder could not be moved to the Trash: {err}"
                        )
                    }),
                Prepared::System(prepared) => prepared.commit(real),
            }?;
        // What went must be what was confirmed (a move keeps the identity).
        if self::identity(&moved) != Some([identity[0], identity[1]]) {
            return Err(format!(
                "Something was moved to the Trash from {} and is now at {}, but it is not the folder that was confirmed: that folder was replaced at the last moment and was not moved. Put {} back from the Trash if it is not meant to go.",
                real.display(),
                moved.display(),
                moved.display()
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod system {
    use std::path::{Path, PathBuf};

    use objc2_foundation::{NSFileManager, NSURL};

    pub(super) fn name(_real: &Path) -> Result<String, String> {
        Ok("the Trash".to_owned())
    }

    /// Nothing to make ready: the Finder chooses the name in the Trash.
    pub(super) struct Prepared;

    pub(super) fn prepare(_real: &Path) -> Result<Prepared, String> {
        Ok(Prepared)
    }

    impl Prepared {
        pub(super) fn abandon(self) {}

        /// Through `NSFileManager`, in an autorelease pool of its own (this
        /// runs on a thread Rust made). Where it went in the Trash.
        pub(super) fn commit(self, real: &Path) -> Result<PathBuf, String> {
            objc2::rc::autoreleasepool(|_| {
                let url = NSURL::from_directory_path(real).ok_or_else(|| {
                    format!(
                        "Nothing was moved: {} cannot be named to the Finder.",
                        real.display()
                    )
                })?;
                let mut moved = None;
                NSFileManager::defaultManager()
                    .trashItemAtURL_resultingItemURL_error(&url, Some(&mut moved))
                    .map_err(|err| {
                        format!(
                            "Nothing was moved: the Finder could not move the folder to the Trash: {}",
                            err.localizedDescription()
                        )
                    })?;
                moved.and_then(|url| url.to_file_path()).ok_or_else(|| {
                    format!(
                        "{} was moved to the Trash, but the Trash did not say where it is. Check the Trash.",
                        real.display()
                    )
                })
            })
        }
    }
}

#[cfg(windows)]
mod system {
    use std::path::{Path, PathBuf};

    /// Windows does not promise a folder can be put back from the Recycle
    /// Bin (it depends on the drive and its settings), and Connecto cannot
    /// check it without unsafe code: refused for now.
    pub(super) fn name(_real: &Path) -> Result<String, String> {
        Err("Connecto cannot yet make sure a folder can be put back from the Recycle Bin. Close the project and delete the folder yourself in File Explorer.".to_owned())
    }

    pub(super) struct Prepared;

    pub(super) fn prepare(real: &Path) -> Result<Prepared, String> {
        Err(name(real).err().unwrap_or_default())
    }

    impl Prepared {
        pub(super) fn abandon(self) {}

        pub(super) fn commit(self, real: &Path) -> Result<PathBuf, String> {
            Err(name(real).err().unwrap_or_default())
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod system {
    //! The freedesktop.org trash in the home folder
    //! (`$XDG_DATA_HOME/Trash`). A folder on another disk cannot be moved
    //! there in one step and is refused (the trash of that disk is not
    //! used).

    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    pub(super) fn home_trash() -> Option<PathBuf> {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
        Some(data.join("Trash"))
    }

    /// The device of `path`, or of its nearest existing parent.
    fn device(path: &Path) -> Option<u64> {
        path.ancestors()
            .find_map(|p| fs::metadata(p).ok())
            .map(|m| m.dev())
    }

    pub(super) fn name(real: &Path) -> Result<String, String> {
        let no_trash = || {
            Err("It is on a disk without a trash Connecto can use. Close the project and delete the folder yourself in your file manager.".to_owned())
        };
        let Some(trash) = home_trash() else {
            return no_trash();
        };
        match (device(&trash), real.parent().and_then(device)) {
            (Some(a), Some(b)) if a == b => Ok("the Trash".to_owned()),
            _ => no_trash(),
        }
    }

    /// The name in `files/` and its record in `info/`, both chosen and the
    /// record written before the last check, so only the rename follows it.
    pub(super) struct Prepared {
        target: PathBuf,
        info: PathBuf,
    }

    pub(super) fn prepare(real: &Path) -> Result<Prepared, String> {
        let trash = home_trash().ok_or("There is no trash folder here.")?;
        prepare_in(&trash, real)
    }

    pub(super) fn prepare_in(trash: &Path, real: &Path) -> Result<Prepared, String> {
        let fail = |err: std::io::Error| format!("the Trash could not be prepared: {err}");
        let files = trash.join("files");
        let info = trash.join("info");
        fs::create_dir_all(&files).map_err(fail)?;
        fs::create_dir_all(&info).map_err(fail)?;
        let base = real
            .file_name()
            .map_or_else(|| "folder".to_owned(), |n| n.to_string_lossy().into_owned());
        let record = format!(
            "[Trash Info]\nPath={}\nDeletionDate={}\n",
            encode(real),
            now()
        );
        for n in 1..10_000 {
            let name = if n == 1 {
                base.clone()
            } else {
                format!("{base}.{n}")
            };
            let info_file = info.join(format!("{name}.trashinfo"));
            let mut file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&info_file)
            {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(fail(err)),
            };
            let target = files.join(&name);
            if fs::symlink_metadata(&target).is_ok() {
                let _ = fs::remove_file(&info_file);
                continue;
            }
            if let Err(err) = file.write_all(record.as_bytes()) {
                let _ = fs::remove_file(&info_file);
                return Err(fail(err));
            }
            return Ok(Prepared {
                target,
                info: info_file,
            });
        }
        Err("the Trash has no free name for this folder.".to_owned())
    }

    impl Prepared {
        pub(super) fn abandon(self) {
            let _ = fs::remove_file(&self.info);
        }

        /// The rename, never replacing anything in the Trash.
        pub(super) fn commit(self, real: &Path) -> Result<PathBuf, String> {
            match super::rename_no_replace(real, &self.target) {
                Ok(()) => Ok(self.target),
                Err(err) => {
                    let _ = fs::remove_file(&self.info);
                    Err(format!(
                        "Nothing was moved: the folder could not be moved to the Trash: {err}"
                    ))
                }
            }
        }
    }

    fn encode(path: &Path) -> String {
        use std::os::unix::ffi::OsStrExt;
        let mut out = String::new();
        for &b in path.as_os_str().as_bytes() {
            if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
                out.push(char::from(b));
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    /// Local time as `YYYY-MM-DDThh:mm:ss`, as the specification asks.
    pub(super) fn now() -> String {
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A scratch folder under the repository's `target/`: the system temp
    /// folder is under `/var` (or `/private/var`) on macOS, which a deletion
    /// refuses.
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/connecto-unit-tmp")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn the_folder_name_comes_from_the_address() {
        for (url, name) in [
            ("https://github.com/owner/project.git", "project"),
            ("https://github.com/owner/project", "project"),
            ("https://github.com/owner/project/", "project"),
            ("git@github.com:owner/project.git", "project"),
            ("git@host:project.git", "project"),
            ("ssh://git@host:22/owner/my-app.git", "my-app"),
            ("git://host/owner/served.git", "served"),
            ("/srv/repos/fixture.git", "fixture"),
            ("/srv/my repos/fixture.git", "fixture"),
            ("file:///srv/repos/fixture.git/", "fixture"),
            // IPv6 hosts have `::` in them.
            ("https://[::1]:8443/owner/repo.git", "repo"),
            ("git@[::1]:repo.git", "repo"),
            ("ssh://git@[2001:db8::1]/owner/served.git", "served"),
        ] {
            assert_eq!(repo_name(url).unwrap(), name, "{url}");
        }
    }

    #[test]
    fn addresses_outside_the_transport_policy_are_refused_before_git_runs() {
        for url in [
            "",
            "   ",
            "https://host/a\tb.git",
            "--upload-pack=touch /tmp/x",
            "https://host/..",
            // Remote helpers, whatever their name.
            "file::/srv/repos/fixture.git",
            "ext::sh -c touch% /tmp/pwned",
            "fd::7",
            "foo::https://host/x.git",
            // Schemes git knows that Connecto does not use.
            "ftp://host/x.git",
            "ftps://host/x.git",
            // User info of any kind in an https address is kept by git.
            "https://me:secret@github.com/owner/project.git",
            "https://ghp_token@github.com/owner/project.git",
            "https://me@github.com/owner/project.git",
            // A relative folder: relative to what?
            "repos/fixture.git",
            "~/repos/fixture.git",
        ] {
            assert!(repo_name(url).is_err(), "{url:?} was taken");
        }
    }

    #[test]
    fn git_is_told_the_same_transports() {
        assert_eq!(PROTOCOLS[1], "protocol.allow=never");
        let allowed: Vec<&str> = PROTOCOLS
            .iter()
            .filter_map(|c| c.strip_prefix("protocol."))
            .filter_map(|c| c.strip_suffix(".allow=always"))
            .collect();
        assert_eq!(allowed, ["https", "http", "ssh", "git", "file"]);
    }

    #[test]
    fn a_failure_says_what_kind_it_is() {
        let auth = clone_failure(&[
            "Cloning into 'x'...".into(),
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled"
                .into(),
        ]);
        assert!(auth.starts_with("The repository refused access"), "{auth}");
        assert!(auth.contains("never asks for or keeps a password"));
        let missing = clone_failure(&[
            "fatal: '/nope' does not appear to be a git repository".into(),
            "fatal: Could not read from remote repository.".into(),
        ]);
        assert!(missing.starts_with("No repository was found"), "{missing}");
        assert!(missing.ends_with("fatal: Could not read from remote repository."));
    }

    #[test]
    fn a_clone_cleanup_removes_only_the_folder_connecto_made() {
        let root = scratch("owned");
        let target = root.join("project");
        fs::create_dir(&target).unwrap();
        let owned = identity(&target).unwrap();
        fs::write(target.join("partial"), "x").unwrap();
        // Replaced by someone else's folder at the same path.
        fs::rename(&target, root.join("moved-away")).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("theirs.txt"), "keep me").unwrap();
        let why = remove_owned(&target, owned).unwrap_err();
        assert!(why.contains("left alone"), "{why}");
        assert!(target.join("theirs.txt").exists(), "not touched");
        // The folder Connecto made goes.
        let ours = root.join("moved-away");
        assert_eq!(remove_owned(&ours, owned), Ok(()));
        assert!(!ours.exists());
        // Already gone is fine.
        assert_eq!(remove_owned(&ours, owned), Ok(()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sizes_and_counts_read_well() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(1204), "1,204");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(size(12), "12 bytes");
        assert_eq!(size(31_200_000), "31.2 MB");
    }

    /// `home` (with Documents and .ssh), `data`, `work/app` (a project
    /// holding `work/app/inner`), and Connecto Home.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Self {
            let root = scratch(&format!("refuse-{name}"));
            for dir in [
                "home/Documents",
                "home/.ssh/keys",
                "home/code/site",
                "data/projects",
                "work/app/inner",
                "work/plain",
                "src-checkout/crates",
                "connecto-home/agentic jobs",
            ] {
                fs::create_dir_all(root.join(dir)).unwrap();
            }
            Self { root }
        }

        fn at(&self, path: &str) -> PathBuf {
            self.root.join(path)
        }

        fn guards(&self) -> Guards {
            Guards {
                home: self.at("home"),
                data_root: self.at("data"),
                connecto: vec![self.at("connecto-home"), self.at("src-checkout/crates")],
                internal: vec![self.at("connecto-home/agentic jobs")],
                others: vec![self.at("work/app/inner")],
                linked: false,
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn dangerous_folders_are_refused() {
        let tree = Tree::new("danger");
        let guards = tree.guards();
        let refused = |path: PathBuf, why: &str| {
            let err = refusal(&path, &guards).unwrap_err();
            assert!(err.contains(why), "{}: {err}", path.display());
        };
        refused(tree.at("home"), "home folder");
        refused(tree.root.clone(), "home folder");
        refused(tree.at("home/Documents"), "home folder's own folders");
        refused(tree.at("home/.ssh/keys"), "settings of your system");
        refused(tree.at("data"), "Connecto's own data");
        refused(tree.at("data/projects"), "Connecto's own data");
        refused(tree.at("connecto-home"), "Connecto itself");
        refused(tree.at("src-checkout"), "Connecto itself");
        refused(
            tree.at("connecto-home/agentic jobs"),
            "Connecto's own folders",
        );
        refused(tree.at("work/app"), "holds another project");
        refused(tree.at("work/missing"), "no longer exists");
        refused(PathBuf::from("/"), "root of a disk");
        // How the project was opened counts, not just where it is.
        let linked = Guards {
            linked: true,
            ..tree.guards()
        };
        let err = refusal(&tree.at("work/plain"), &linked).unwrap_err();
        assert!(err.contains("opened through a link"), "{err}");
        assert!(refusal(&tree.at("work/plain"), &guards).is_ok());
        assert!(refusal(&tree.at("home/code/site"), &guards).is_ok());
    }

    /// The home folder's own folders are whole folders, whatever their
    /// spelling: `Documentſ` stored is Documents to APFS (and to full case
    /// folding), so it is refused; a project inside Documents is not.
    #[test]
    fn a_home_folder_spelled_with_a_long_s_is_refused() {
        let tree = Tree::new("documents-long-s");
        fs::remove_dir_all(tree.at("home/Documents")).unwrap();
        let documents = tree.at("home/Document\u{17f}");
        fs::create_dir_all(documents.join("app")).unwrap();
        let err = refusal(&documents, &tree.guards()).unwrap_err();
        assert!(err.contains("home folder's own folders"), "{err}");
        assert!(refusal(&documents.join("app"), &tree.guards()).is_ok());
    }

    /// A settings folder stored as `.ſsh` is `~/.ssh` to APFS (and to
    /// full case folding), though lower-casing never makes it `.ssh`.
    #[test]
    fn a_protected_folder_spelled_with_a_long_s_is_refused() {
        let tree = Tree::new("long-s");
        // Only the long-s spelling is stored.
        fs::remove_dir_all(tree.at("home/.ssh")).unwrap();
        let keys = tree.at("home/.\u{17f}sh/keys");
        fs::create_dir_all(&keys).unwrap();
        let err = refusal(&keys, &tree.guards()).unwrap_err();
        assert!(err.contains("settings of your system"), "{err}");
    }

    /// Protected folders are compared without letter case: on a
    /// case-insensitive volume another spelling is the same folder.
    #[test]
    fn protected_folders_are_compared_without_letter_case() {
        let tree = Tree::new("case");
        let guards = Guards {
            home: tree.root.join("HOME"),
            data_root: tree.root.join("Data"),
            ..tree.guards()
        };
        for (path, why) in [
            ("home/.ssh/keys", "settings of your system"),
            ("home/Documents", "home folder's own folders"),
            ("data/projects", "Connecto's own data"),
        ] {
            let err = refusal(&tree.at(path), &guards).unwrap_err();
            assert!(err.contains(why), "{path}: {err}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn system_trees_are_prefixes_and_links_anywhere_are_refused() {
        let tree = Tree::new("unix");
        let guards = tree.guards();
        for path in [
            "/usr/local",
            "/var/lib",
            "/private/var/db",
            "/etc/ssh",
            "/opt/x",
        ] {
            let path = PathBuf::from(path);
            if path.exists() && fs::canonicalize(&path).is_ok_and(|real| real == path) {
                let err = refusal(&path, &guards).unwrap_err();
                assert!(err.contains("system folder"), "{}: {err}", path.display());
            }
        }
        std::os::unix::fs::symlink(tree.at("work/plain"), tree.at("work/link")).unwrap();
        let err = refusal(&tree.at("work/link"), &guards).unwrap_err();
        assert!(err.contains("link"), "{err}");
        // A folder reached through a linked parent is a link too.
        std::os::unix::fs::symlink(tree.at("work"), tree.at("linked-work")).unwrap();
        let err = refusal(&tree.at("linked-work/plain"), &guards).unwrap_err();
        assert!(err.contains("link"), "{err}");
        assert!(through_link(&tree.at("linked-work/plain")));
        assert!(!through_link(&tree.at("work/plain")));
    }

    #[cfg(unix)]
    #[test]
    fn a_volume_root_is_refused_by_its_mount_boundary() {
        // Any mounted volume's root, wherever it is mounted: here /dev,
        // which is its own filesystem on macOS and Linux.
        let guards = Tree::new("mount").guards();
        let dev = PathBuf::from("/dev");
        if device(&dev) != device(Path::new("/")) {
            let err = refusal(&dev, &guards).unwrap_err();
            assert!(
                err.contains("top of a disk") || err.contains("top-level"),
                "{err}"
            );
        }
        // The rule itself, at depth: a folder whose device differs from its
        // parent's. /Volumes/<disk> is one on macOS when a disk is mounted.
        if let Ok(volumes) = fs::read_dir("/Volumes") {
            for volume in volumes.flatten() {
                let path = volume.path();
                if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir())
                    && device(&path) != device(Path::new("/Volumes"))
                {
                    let err = refusal(&path, &guards).unwrap_err();
                    assert!(err.contains("top of a disk"), "{}: {err}", path.display());
                }
            }
        }
    }

    #[test]
    fn the_deletion_moves_only_the_folder_that_was_confirmed() {
        let tree = Tree::new("identity");
        let guards = tree.guards();
        let folder = tree.at("work/plain");
        fs::write(folder.join("a.txt"), "a").unwrap();
        let checked = refusal(&folder, &guards).unwrap();
        let held = Held::open(&checked.real).unwrap();
        assert_eq!(held.identity, checked.identity);
        let bin_dir = tree.at("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let bin = Bin::Folder(bin_dir.clone());
        // Swapped for another folder at the same path after the confirmation.
        fs::rename(&folder, tree.at("work/original")).unwrap();
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("other.txt"), "not this one").unwrap();
        let why = bin.remove(&folder, &held).unwrap_err();
        assert!(why.contains("changed since"), "{why}");
        assert!(folder.join("other.txt").exists(), "left alone");
        assert!(
            fs::read_dir(&bin_dir).unwrap().next().is_none(),
            "nothing moved"
        );
        // The confirmed one, back in place, goes.
        fs::remove_dir_all(&folder).unwrap();
        fs::rename(tree.at("work/original"), &folder).unwrap();
        bin.remove(&folder, &held).unwrap();
        assert!(bin_dir.join("plain/a.txt").exists());
    }

    #[test]
    fn the_plan_counts_what_goes_and_warns_of_unsaved_git_work() {
        let tree = Tree::new("plan");
        let guards = tree.guards();
        let app = tree.at("home/code/site");
        fs::write(app.join("index.html"), "<p>hi</p>").unwrap();
        fs::create_dir_all(app.join("css")).unwrap();
        fs::write(app.join("css/site.css"), "p{}").unwrap();
        let bin = Bin::Folder(tree.at("bin"));
        let plan_of = |path: &Path| plan("p1", &refusal(path, &guards).unwrap(), &bin, 0).unwrap();
        let first = plan_of(&app);
        assert_eq!(first.name, "site");
        assert_eq!(first.path, app.display().to_string());
        assert!(
            first.contents.starts_with("2 files in 1 folder"),
            "{}",
            first.contents
        );
        assert_eq!(first.trash, "the Trash");
        assert_eq!(first.identity.len(), 4);
        // Not a repository of its own (the scratch folder may sit inside
        // this checkout, where it is ignored).
        assert!(
            first
                .warnings
                .iter()
                .all(|w| w.contains("inside the git repository")),
            "{:?}",
            first.warnings
        );
        if git(&app, &["--version"]).is_none() {
            return;
        }
        let commit = |message: &str| {
            git(
                &app,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qam",
                    message,
                ],
            )
            .unwrap();
        };
        git(&app, &["init", "-q"]).unwrap();
        let fresh = plan_of(&app);
        assert!(
            fresh
                .warnings
                .iter()
                .any(|w| w.contains("2 uncommitted changes")),
            "{:?}",
            fresh.warnings
        );
        git(&app, &["add", "."]).unwrap();
        commit("one");
        assert_eq!(
            plan_of(&app).warnings,
            ["The repository has no remote: its whole history exists only in this folder."]
        );
        // A subfolder of a repository: its own changes, and that it is inside.
        fs::write(app.join("css/new.css"), "a{}").unwrap();
        let css = plan_of(&app.join("css"));
        assert!(
            css.warnings[0].contains("1 uncommitted change "),
            "{:?}",
            css.warnings
        );
        assert!(
            css.warnings[1].contains("inside the git repository"),
            "{:?}",
            css.warnings
        );
        fs::remove_file(app.join("css/new.css")).unwrap();
        // With a remote everything is pushed to, then a detached HEAD with a
        // commit of its own.
        let remote = tree.at("work/remote.git");
        git(&tree.at("work"), &["init", "-q", "--bare", "remote.git"]).unwrap();
        git(&app, &["remote", "add", "origin", remote.to_str().unwrap()]).unwrap();
        git(&app, &["push", "-q", "origin", "HEAD:refs/heads/main"]).unwrap();
        git(&app, &["fetch", "-q", "origin"]).unwrap();
        let pushed = plan_of(&app);
        assert!(
            !pushed.warnings.iter().any(|w| w.contains("not pushed")),
            "{:?}",
            pushed.warnings
        );
        git(&app, &["checkout", "-q", "--detach"]).unwrap();
        fs::write(app.join("index.html"), "<p>detached</p>").unwrap();
        commit("on no branch");
        let detached = plan_of(&app);
        assert!(
            detached
                .warnings
                .iter()
                .any(|w| w.contains("HEAD is detached: 1 commit on no branch")),
            "{:?}",
            detached.warnings
        );
        // A repository git cannot read is said, not taken for clean.
        fs::write(app.join(".git/HEAD"), "garbage").unwrap();
        let broken = plan_of(&app);
        assert!(
            broken
                .warnings
                .iter()
                .any(|w| w.contains("could not check")),
            "{:?}",
            broken.warnings
        );
    }

    #[test]
    fn a_finished_clone_never_replaces_what_is_at_its_place() {
        let root = scratch("publish");
        let staging = root.join(".project.connecto-clone-test");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("README.md"), "clone").unwrap();
        let owned = opened_identity(&staging).unwrap();
        // An empty folder put there meanwhile: a plain rename would replace
        // it on macOS and Linux.
        let target = root.join("project");
        fs::create_dir(&target).unwrap();
        let result = publish(&staging, &target, owned);
        assert!(
            matches!(result, Err(Published::Kept(_))),
            "published over an existing folder"
        );
        let Err(Published::Kept(why)) = result else {
            return;
        };
        assert!(why.contains("was left alone"), "{why}");
        assert!(why.contains(&staging.display().to_string()), "{why}");
        assert!(target.exists() && fs::read_dir(&target).unwrap().next().is_none());
        assert!(staging.join("README.md").exists(), "the clone is kept");
        // A folder of someone's, with files.
        fs::write(target.join("theirs.txt"), "keep").unwrap();
        assert!(publish(&staging, &target, owned).is_err());
        assert!(target.join("theirs.txt").exists());
        // Free: published, and checked.
        fs::remove_dir_all(&target).unwrap();
        assert!(publish(&staging, &target, owned).is_ok());
        assert!(target.join("README.md").exists() && !staging.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_swapped_at_the_handoff_to_the_trash_is_named_and_the_confirmed_one_stays() {
        let tree = Tree::new("handoff");
        let guards = tree.guards();
        let folder = tree.at("work/plain");
        fs::write(folder.join("confirmed.txt"), "mine").unwrap();
        let checked = refusal(&folder, &guards).unwrap();
        let held = Held::open(&checked.real).unwrap();
        assert_eq!(held.identity, checked.identity);
        let bin_dir = tree.at("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let bin = Bin::Folder(bin_dir.clone());
        let why = bin
            .remove_with(&folder, &held, || {
                // After the last check, before the move.
                fs::rename(&folder, tree.at("work/original")).unwrap();
                fs::create_dir(&folder).unwrap();
                fs::write(folder.join("swapped.txt"), "other").unwrap();
            })
            .unwrap_err();
        let moved = bin_dir.join("plain");
        assert!(
            why.contains(&format!("is now at {}", moved.display())),
            "{why}"
        );
        assert!(why.contains("was not moved"), "{why}");
        // Both folders afterwards: the swapped one is in the Trash (not
        // erased), the confirmed one is untouched where it went.
        assert!(moved.join("swapped.txt").exists());
        assert!(tree.at("work/original/confirmed.txt").exists());
    }

    #[test]
    fn a_bare_repository_is_warned_of() {
        let tree = Tree::new("bare");
        let guards = tree.guards();
        let bare = tree.at("work/plain");
        if git(&bare, &["--version"]).is_none() {
            return;
        }
        git(&bare, &["init", "-q", "--bare"]).unwrap();
        let bin = Bin::Folder(tree.at("bin"));
        let plan = plan("p", &refusal(&bare, &guards).unwrap(), &bin, 0).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("is a bare git repository")),
            "{:?}",
            plan.warnings
        );
        // With history and no remote.
        let source = tree.at("home/code/site");
        git(&source, &["init", "-q"]).unwrap();
        fs::write(source.join("a"), "a").unwrap();
        git(&source, &["add", "."]).unwrap();
        git(
            &source,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "a",
            ],
        )
        .unwrap();
        git(
            &source,
            &["push", "-q", bare.to_str().unwrap(), "HEAD:refs/heads/main"],
        )
        .unwrap();
        let plan = super::plan("p", &refusal(&bare, &guards).unwrap(), &bin, 0).unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("no remote")),
            "{:?}",
            plan.warnings
        );
        // A folder inside it.
        let refs = bare.join("refs");
        let inside = super::plan("p", &refusal(&refs, &guards).unwrap(), &bin, 0).unwrap();
        assert!(
            inside
                .warnings
                .iter()
                .any(|w| w.contains("inside the bare git repository")),
            "{:?}",
            inside.warnings
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_freedesktop_trash_records_where_and_when_before_the_move() {
        let root = scratch("fd-trash");
        let folder = root.join("my app");
        fs::create_dir_all(folder.join("src")).unwrap();
        fs::write(folder.join("src/main.rs"), "fn main() {}").unwrap();
        let trash = root.join("Trash");
        let prepared = system::prepare_in(&trash, &folder).unwrap();
        // The record is written before the move.
        let info = fs::read_to_string(trash.join("info/my app.trashinfo")).unwrap();
        assert!(
            info.contains("Path=") && info.contains("my%20app"),
            "{info}"
        );
        let date = info
            .lines()
            .find_map(|l| l.strip_prefix("DeletionDate="))
            .unwrap();
        assert_eq!(date.len(), 19, "{date}");
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[10..11], "T");
        let moved = prepared.commit(&folder).unwrap();
        assert_eq!(moved, trash.join("files/my app"));
        assert!(trash.join("files/my app/src/main.rs").exists());
        // A second one of the same name gets its own; an abandoned one
        // leaves no record.
        fs::create_dir_all(&folder).unwrap();
        let second = system::prepare_in(&trash, &folder).unwrap();
        assert!(trash.join("info/my app.2.trashinfo").exists());
        second.abandon();
        assert!(!trash.join("info/my app.2.trashinfo").exists());
        assert!(folder.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
