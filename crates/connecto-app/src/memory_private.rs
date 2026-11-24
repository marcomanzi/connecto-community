//! The private git folders memory runs git against ([`crate::memory_git`]),
//! and their lifecycle.
//!
//! # Where
//!
//! Under Connecto's own per-user data folder, `<data>/memory-git/`, never
//! in the shared system temporary folder and never in a project. On Unix
//! the folder is created owner-only (`0700`) and checked before each use:
//! a real folder (not a link), owned by the user, no access for group or
//! others, and no folder above it writable by group or others unless
//! sticky; otherwise git is not used. Each private folder inside it is
//! created `0700` with a fresh name (`p<pid>-<random>`) and every file in it
//! is written `0600`, created new. On Windows the data folder is in the
//! user's own profile, which is what protects it; no further check is made
//! there.
//!
//! # Lifecycle
//!
//! The host owns the [`PrivateGit`]; a private folder is reused while its
//! inputs are unchanged and removed when its project stops, when memory
//! shuts down, or when its process is gone (leftovers of a crash are
//! removed at the next start). Removal is never recursive: a private folder
//! holds a fixed set of files Connecto wrote, removed one by one by name
//! through handles that follow no link, then its (then empty) folders.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

/// The files a private folder may hold, besides `sharedindex.<hex>`.
const KNOWN_FILES: &[&str] = &["config", "HEAD", "index", "shallow"];
/// Its folders, removed empty after their files.
const KNOWN_DIRS: &[&str] = &["info", "refs", "objects"];
/// How long a recorded git failure counts for a project.
const FAILURE_FOR: Duration = Duration::from_secs(300);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One private git folder; removed (by name) when dropped.
#[derive(Debug)]
pub(crate) struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        if let (Some(parent), Some(name)) = (self.path.parent(), self.path.file_name()) {
            let _ = remove_private(parent, &name.to_string_lossy());
        }
    }
}

/// What a private folder was made from, to know when to remake it.
pub(crate) type CacheKey = String;

/// The host's private git folders.
#[derive(Debug)]
pub(crate) struct PrivateGit {
    base: PathBuf,
    /// The folders by repository root, and which projects are stopped (and
    /// whether everything is), under one lock: a folder is never made for
    /// a stopped project, even by a worker that raced the stop.
    state: Mutex<State>,
    failures: Mutex<BTreeMap<PathBuf, (Instant, String)>>,
    /// The user's global values of the semantic keys, and when read.
    global: Mutex<Option<(String, Instant, GlobalValues)>>,
}

/// The global values of the copied keys (`None`: not spelled as accepted)
/// and the limits of what was read.
pub(crate) type GlobalValues = (BTreeMap<String, Option<String>>, Vec<String>);

#[derive(Debug, Default)]
struct State {
    cache: BTreeMap<PathBuf, (CacheKey, Arc<PrivateDir>)>,
    /// Projects (real paths) whose memory stopped.
    stopped: std::collections::BTreeSet<PathBuf>,
    /// Memory shut down: nothing is made any more.
    closed: bool,
}

fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// The one the host installed (held weakly: the host owns it).
static ACTIVE: Mutex<Weak<PrivateGit>> = Mutex::new(Weak::new());

impl PrivateGit {
    /// Open (creating it owner-only) `base` and remove what crashed runs
    /// left there.
    pub fn open(base: PathBuf) -> std::io::Result<Arc<Self>> {
        create_owner_only(&base)?;
        check_secure(&base)?;
        remove_leftovers(&base);
        let this = Arc::new(Self {
            base,
            state: Mutex::new(State::default()),
            failures: Mutex::new(BTreeMap::new()),
            global: Mutex::new(None),
        });
        Ok(this)
    }

    /// Make this the one git runs with (the host does, once).
    pub fn install(self: &Arc<Self>) {
        *lock(&ACTIVE) = Arc::downgrade(self);
    }

    /// The private git folder for `project` in the repository at `root`
    /// with these inputs: the cached one when `key` is unchanged, else a
    /// new one that `fill` writes (through [`write_new`]). Refused for a
    /// stopped project and after shutdown.
    pub fn folder(
        &self,
        root: &Path,
        project: &Path,
        key: &str,
        fill: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<Arc<PrivateDir>, String> {
        check_secure(&self.base).map_err(|e| format!("the private git folder is not safe: {e}"))?;
        let mut state = lock(&self.state);
        if state.closed {
            return Err("memory has shut down".to_owned());
        }
        if state.stopped.contains(&real(project)) {
            return Err("memory is stopped for this project".to_owned());
        }
        if let Some((k, dir)) = state.cache.get(root)
            && k == key
        {
            return Ok(Arc::clone(dir));
        }
        let dir = Arc::new(new_private(&self.base).map_err(|e| e.to_string())?);
        fill(dir.path())?;
        state
            .cache
            .insert(root.to_owned(), (key.to_owned(), Arc::clone(&dir)));
        Ok(dir)
    }

    /// A project's memory starts (again): folders may be made for it.
    pub fn start(&self, project: &Path) {
        let mut state = lock(&self.state);
        state.stopped.remove(&real(project));
        state.closed = false;
    }

    /// A git command failed for this repository: remembered so the build
    /// declares it.
    pub fn record_failure(&self, root: &Path, what: &str) {
        lock(&self.failures).insert(root.to_owned(), (Instant::now(), what.to_owned()));
    }

    /// The latest failure for this repository, if recent.
    pub fn failure(&self, root: &Path) -> Option<String> {
        lock(&self.failures)
            .get(root)
            .filter(|(at, _)| at.elapsed() < FAILURE_FOR)
            .map(|(_, what)| what.clone())
    }

    /// The cached global values for `key` (which global files), or read
    /// them with `read`.
    pub fn global(&self, key: &str, read: impl FnOnce() -> GlobalValues) -> GlobalValues {
        let mut g = lock(&self.global);
        if let Some((k, at, values)) = g.as_ref()
            && k == key
            && at.elapsed() < Duration::from_secs(60)
        {
            return values.clone();
        }
        let values = read();
        *g = Some((key.to_owned(), Instant::now(), values.clone()));
        values
    }

    /// A project stopped (closed, memory off): its private folders go, and
    /// none is made for it until [`PrivateGit::start`].
    pub fn release(&self, project: &Path) {
        let project = real(project);
        let mut state = lock(&self.state);
        state.cache.retain(|root, _| !project.starts_with(root));
        state.stopped.insert(project);
    }

    /// Memory shuts down: every private folder goes, and none is made.
    pub fn clear(&self) {
        let mut state = lock(&self.state);
        state.cache.clear();
        state.closed = true;
    }
}

impl Drop for PrivateGit {
    fn drop(&mut self) {
        lock(&self.state).cache.clear();
    }
}

/// The active one, if the host installed it. In this crate's tests, a
/// shared one under the system temporary folder (its leftovers are removed
/// by the next test run, as after a crash).
pub(crate) fn active() -> Option<Arc<PrivateGit>> {
    if let Some(p) = lock(&ACTIVE).upgrade() {
        return Some(p);
    }
    #[cfg(test)]
    {
        static TESTS: std::sync::OnceLock<Option<Arc<PrivateGit>>> = std::sync::OnceLock::new();
        TESTS
            .get_or_init(|| {
                let base = std::env::temp_dir().join(test_base_name());
                let p = PrivateGit::open(base).ok()?;
                Some(p)
            })
            .clone()
    }
    #[cfg(not(test))]
    {
        None
    }
}

/// The shared test base's name: per user on Unix (the system temporary
/// folder may be shared), a fixed name elsewhere (the temporary folder is in
/// the user's own profile).
#[cfg(all(test, unix))]
fn test_base_name() -> String {
    format!("connecto-memory-git-tests-{}", nix::unistd::geteuid())
}

#[cfg(all(test, not(unix)))]
fn test_base_name() -> String {
    "connecto-memory-git-tests".to_owned()
}

// ---- the file system, owner-only and by name

#[cfg(unix)]
fn create_owner_only(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn create_owner_only(dir: &Path) -> std::io::Result<()> {
    match std::fs::create_dir(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// `dir` is a real folder owned by the user with no access for group or
/// others, and no folder above it is writable by group or others unless
/// sticky (Unix).
#[cfg(unix)]
pub(crate) fn check_secure(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let me = nix::unistd::geteuid().as_raw();
    let bad = |why: &str| std::io::Error::other(why.to_owned());
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(bad("not a real folder"));
    }
    if meta.uid() != me {
        return Err(bad("not owned by this user"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(bad("open to group or others"));
    }
    let mut at = dir.parent();
    while let Some(p) = at {
        let m = std::fs::metadata(p)?;
        let mode = m.mode();
        let writable = mode & 0o022 != 0;
        let sticky = mode & 0o1000 != 0;
        if writable && !sticky {
            return Err(bad("a folder above it is writable by others"));
        }
        if m.uid() != me && m.uid() != 0 && writable {
            return Err(bad("a folder above it belongs to someone else"));
        }
        at = p.parent();
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn check_secure(dir: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        Ok(())
    } else {
        Err(std::io::Error::other("not a real folder"))
    }
}

/// A new private folder in `base`: `p<pid>-<random>`, owner-only.
fn new_private(base: &Path) -> std::io::Result<PrivateDir> {
    for _ in 0..16 {
        let name = format!(
            "p{}-{}",
            std::process::id(),
            &connecto_memory::prompt::nonce()[..12]
        );
        let path = base.join(&name);
        #[cfg(unix)]
        let made = {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&path)
        };
        #[cfg(not(unix))]
        let made = std::fs::create_dir(&path);
        match made {
            Ok(()) => {
                for sub in KNOWN_DIRS {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::DirBuilderExt;
                        std::fs::DirBuilder::new()
                            .mode(0o700)
                            .create(path.join(sub))?;
                    }
                    #[cfg(not(unix))]
                    std::fs::create_dir(path.join(sub))?;
                }
                return Ok(PrivateDir { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free private folder name"))
}

/// Write a new file (`0600`, never over an existing one) in a private
/// folder; `name` is one of the known files, `info/exclude`, or
/// `sharedindex.<hex>`.
pub(crate) fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    write_new_at(dir, name, bytes, None)
}

/// [`write_new`], with the copy given the original's modification time (an
/// index's own time is part of git's check for racily clean entries).
pub(crate) fn write_new_at(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    modified: Option<std::time::SystemTime>,
) -> Result<(), String> {
    use std::io::Write as _;
    if !known_name(name) && name != "info/exclude" {
        return Err("not a private folder file".to_owned());
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(dir.join(name))
        .and_then(|mut f| {
            f.write_all(bytes)?;
            if let Some(t) = modified {
                f.set_modified(t)?;
            }
            Ok(())
        })
        .map_err(|e| format!("the private git folder could not be written: {e}"))
}

fn known_name(name: &str) -> bool {
    KNOWN_FILES.contains(&name)
        || name
            .strip_prefix("sharedindex.")
            .is_some_and(|h| matches!(h.len(), 40 | 64) && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A private folder's name: `p<pid>-<hex>`; its pid.
fn private_pid(name: &str) -> Option<u32> {
    let (pid, rest) = name.strip_prefix('p')?.split_once('-')?;
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| pid.parse().ok())
        .flatten()
}

/// Remove the private folders of processes that are gone (a crash left
/// them): only entries named as Connecto names them, owned by the user.
#[cfg(unix)]
fn remove_leftovers(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = private_pid(&name) else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let alive = i32::try_from(pid).is_ok_and(|p| {
            !matches!(
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), None),
                Err(nix::errno::Errno::ESRCH)
            )
        });
        if !alive {
            let _ = remove_private(base, &name);
        }
    }
}

/// Off Unix, whether a process is gone is not checked: leftovers stay. This
/// is documented in the spec's known limits only; nothing in the app
/// declares it.
#[cfg(not(unix))]
fn remove_leftovers(base: &Path) {
    let _ = base;
}

/// Remove one private folder `name` in `base`: its known files by name,
/// relative to a handle on the folder opened without following a link,
/// then its known folders, then itself. Nothing else is touched; an
/// unexpected entry leaves the folder in place.
#[cfg(unix)]
pub(crate) fn remove_private(base: &Path, name: &str) -> std::io::Result<()> {
    use nix::dir::Dir;
    use nix::fcntl::OFlag;
    use nix::sys::stat::{Mode, fstat};
    use nix::unistd::{UnlinkatFlags, unlinkat};
    use std::os::fd::AsRawFd;

    if private_pid(name).is_none() {
        return Err(std::io::Error::other("not a private folder name"));
    }
    let err = |e: nix::errno::Errno| std::io::Error::from_raw_os_error(e as i32);
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let parent = Dir::open(base, flags, Mode::empty()).map_err(err)?;
    let mut dir = Dir::openat(Some(parent.as_raw_fd()), name, flags, Mode::empty()).map_err(err)?;
    if fstat(dir.as_raw_fd()).map_err(err)?.st_uid != nix::unistd::geteuid().as_raw() {
        return Err(std::io::Error::other("not owned by this user"));
    }
    // Its files, by name.
    let names: Vec<String> = dir
        .iter()
        .flatten()
        .filter_map(|e| e.file_name().to_str().ok().map(str::to_owned))
        .filter(|n| known_name(n))
        .collect();
    for n in names {
        let _ = unlinkat(
            Some(dir.as_raw_fd()),
            n.as_str(),
            UnlinkatFlags::NoRemoveDir,
        );
    }
    if let Ok(info) = Dir::openat(Some(dir.as_raw_fd()), "info", flags, Mode::empty()) {
        let _ = unlinkat(
            Some(info.as_raw_fd()),
            "exclude",
            UnlinkatFlags::NoRemoveDir,
        );
    }
    for sub in KNOWN_DIRS {
        let _ = unlinkat(Some(dir.as_raw_fd()), *sub, UnlinkatFlags::RemoveDir);
    }
    unlinkat(Some(parent.as_raw_fd()), name, UnlinkatFlags::RemoveDir).map_err(err)
}

/// Off Unix: the same files and folders by name, through paths.
#[cfg(not(unix))]
pub(crate) fn remove_private(base: &Path, name: &str) -> std::io::Result<()> {
    if private_pid(name).is_none() {
        return Err(std::io::Error::other("not a private folder name"));
    }
    let dir = base.join(name);
    if std::fs::symlink_metadata(&dir)?.file_type().is_symlink() {
        return Err(std::io::Error::other("a link"));
    }
    for e in std::fs::read_dir(&dir)?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if known_name(&n) {
            let _ = std::fs::remove_file(dir.join(&n));
        }
    }
    let _ = std::fs::remove_file(dir.join("info").join("exclude"));
    for sub in KNOWN_DIRS {
        let _ = std::fs::remove_dir(dir.join(sub));
    }
    std::fs::remove_dir(&dir)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn names_are_recognised_strictly() {
        assert_eq!(private_pid("p123-abcdef012345"), Some(123));
        assert_eq!(private_pid("p123-"), None);
        assert_eq!(private_pid("x123-ab"), None);
        assert_eq!(private_pid("p12x-ab"), None);
        assert!(known_name("index"));
        assert!(known_name(&format!("sharedindex.{}", "a".repeat(40))));
        assert!(!known_name("sharedindex.x"));
        assert!(!known_name("../index"));
    }

    /// Finding 1: owner-only, even when the system temporary folder is
    /// shared: the folder is 0700, its files 0600, and a base open to
    /// others is refused.
    #[cfg(unix)]
    #[test]
    fn private_folders_are_owner_only() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("memory-git");
        let private = PrivateGit::open(base.clone()).unwrap();
        assert_eq!(std::fs::metadata(&base).unwrap().mode() & 0o777, 0o700);
        let dir = private
            .folder(Path::new("/r"), Path::new("/r"), "k", |d| {
                write_new(d, "index", b"DIRC")
            })
            .unwrap();
        assert_eq!(std::fs::metadata(dir.path()).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::metadata(dir.path().join("index")).unwrap().mode() & 0o777,
            0o600
        );
        assert!(
            write_new(dir.path(), "index", b"again").is_err(),
            "never over a file"
        );
        assert!(write_new(dir.path(), "../escape", b"x").is_err());
        // A base opened to others is refused.
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            private
                .folder(Path::new("/r2"), Path::new("/r2"), "k", |_| Ok(()))
                .is_err()
        );
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Under a world-writable, non-sticky folder: refused.
        let shared = temp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(PrivateGit::open(shared.join("memory-git")).is_err());
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(dir);
        drop(private);
    }

    /// Finding 4: folders go when their project stops, at shutdown, and
    /// when a crashed process left them (removed at the next start); a
    /// living process's and anything not named as ours are left alone.
    #[cfg(unix)]
    #[test]
    fn private_folders_have_a_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("memory-git");
        let private = PrivateGit::open(base.clone()).unwrap();
        let count = || std::fs::read_dir(&base).unwrap().count();
        let a = private
            .folder(Path::new("/proj/a"), Path::new("/proj/a"), "k", |d| {
                write_new(d, "HEAD", b"x")
            })
            .unwrap();
        let b = private
            .folder(Path::new("/proj/b"), Path::new("/proj/b"), "k", |d| {
                write_new(d, "config", b"x")
            })
            .unwrap();
        drop((a, b));
        assert_eq!(count(), 2, "kept while cached");
        private.release(Path::new("/proj/a"));
        assert_eq!(count(), 1, "removed when its project stops");
        // A worker still running cannot make it again...
        assert!(
            private
                .folder(Path::new("/proj/a"), Path::new("/proj/a"), "k", |_| Ok(()))
                .is_err()
        );
        assert_eq!(count(), 1);
        // ... until the project starts again.
        private.start(Path::new("/proj/a"));
        drop(
            private
                .folder(Path::new("/proj/a"), Path::new("/proj/a"), "k2", |_| Ok(()))
                .unwrap(),
        );
        assert_eq!(count(), 2);
        private.release(Path::new("/proj/a"));
        private.clear();
        assert_eq!(count(), 0, "removed at shutdown");
        assert!(
            private
                .folder(Path::new("/proj/b"), Path::new("/proj/b"), "k", |_| Ok(()))
                .is_err(),
            "nothing is made after shutdown"
        );
        assert_eq!(count(), 0);
        // A crashed process's folder (a pid that is gone) and a stranger.
        let dead = base.join("p999999-abc123abc123");
        std::fs::create_dir(&dead).unwrap();
        for sub in KNOWN_DIRS {
            std::fs::create_dir(dead.join(sub)).unwrap();
        }
        std::fs::write(dead.join("index"), "x").unwrap();
        std::fs::write(dead.join("info/exclude"), "x").unwrap();
        let mine = base.join(format!("p{}-abc123abc124", std::process::id()));
        std::fs::create_dir(&mine).unwrap();
        let stranger = base.join("not-ours");
        std::fs::create_dir(&stranger).unwrap();
        drop(private);
        let _again = PrivateGit::open(base.clone()).unwrap();
        assert!(!dead.exists(), "a crashed run's folder is removed");
        assert!(mine.exists(), "this process's is kept");
        assert!(stranger.exists(), "anything else is left alone");
    }
}
