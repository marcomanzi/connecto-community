//! Files offered to the other machines viewing this one: an agent runs
//! `connecto send-file <path>` in a terminal, and each machine connected to
//! this Connecto that has that terminal's project in its list may download
//! the file once, for a while.
//!
//! An offer is what this machine agreed to send: a peer can download only a
//! file offered here, never name another. It is checked when it is made (a
//! regular file, not too large, not in a folder of keys) and opened again
//! when it is sent, so a file replaced by a link meanwhile is refused.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// How long an offer stays open.
pub const OFFER_TTL: Duration = Duration::from_secs(10 * 60);
/// The largest file that can be offered.
pub const MAX_OFFER_BYTES: u64 = 64 * 1024 * 1024;
/// The most offers kept at once; the oldest goes first.
pub const MAX_OFFERS: usize = 32;

/// Folders under the home folder that hold keys and credentials: nothing in
/// them is offered, whoever asks.
const PRIVATE: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".docker",
    ".netrc",
    "Library/Keychains",
    ".config/gcloud",
];

/// One offered file.
#[derive(Debug, Clone)]
pub struct FileOffer {
    /// Unique for the life of the app.
    pub id: u64,
    /// The project of the terminal that offered it.
    pub project_id: String,
    /// The terminal that offered it (its id, such as `claude-3`).
    pub terminal: String,
    /// The file, as it was resolved when offered.
    pub path: PathBuf,
    /// Its file name, for the machine that saves it.
    pub name: String,
    pub bytes: u64,
    pub offered: Instant,
    /// The file, opened when it was offered and kept open: what is sent is
    /// read from this handle, so a file put at its path since is never sent.
    pub file: Arc<std::fs::File>,
}

impl PartialEq for FileOffer {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.path == other.path && self.bytes == other.bytes
    }
}

impl FileOffer {
    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.offered) >= OFFER_TTL
    }
}

#[derive(Default)]
pub struct FileOffers(Mutex<(u64, Vec<FileOffer>)>);

/// The offers of this app.
pub fn shared() -> &'static FileOffers {
    static OFFERS: OnceLock<FileOffers> = OnceLock::new();
    OFFERS.get_or_init(FileOffers::default)
}

/// Whether `path` (canonical) is in a folder of keys, named as it is or as
/// it resolves (such a folder may itself be a link elsewhere).
fn private(path: &Path, home: Option<&Path>) -> bool {
    home.is_some_and(|home| {
        PRIVATE.iter().any(|folder| {
            let named = home.join(folder);
            path.starts_with(&named)
                || std::fs::canonicalize(&named).is_ok_and(|real| path.starts_with(real))
        })
    })
}

/// Open `real` (a canonical path, so with no link in it) folder by folder
/// from its root, refusing any link met on the way, the file itself
/// included, and without waiting on a pipe or device put there: what opens
/// is the regular file at exactly that path, or nothing.
fn open_nofollow(real: &Path) -> std::io::Result<std::fs::File> {
    use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
    use std::path::Component;
    let not_there = || std::io::Error::from(std::io::ErrorKind::NotFound);
    let mut root = PathBuf::new();
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for part in real.components() {
        match part {
            Component::Prefix(_) | Component::RootDir => root.push(part.as_os_str()),
            Component::Normal(name) => parts.push(name),
            Component::CurDir | Component::ParentDir => return Err(not_there()),
        }
    }
    let (name, folders) = parts.split_last().ok_or_else(not_there)?;
    let mut dir = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())?;
    for folder in folders {
        dir = dir.open_dir_nofollow(folder)?;
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    Ok(dir.open_with(name, &options)?.into_std())
}

/// Whether two looks at a file are at the same file: its device and inode
/// on Unix; elsewhere its creation and change times and its length.
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.len() == b.len()
            && a.created().ok() == b.created().ok()
            && a.modified().ok() == b.modified().ok()
    }
}

/// The first id of this run: from the clock, so ids stay unique across
/// restarts and a viewer never takes a new offer for one it already had.
fn first_id() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX / 2))
        .max(1)
        << 8
}

impl FileOffers {
    /// Offer `path` (absolute) from terminal `terminal` of project
    /// `project_id`: the offer, or why not, as the agent reads it.
    ///
    /// # Errors
    /// The file is missing, not a regular file, too large or private.
    pub fn offer(
        &self,
        project_id: &str,
        terminal: &str,
        path: &Path,
    ) -> Result<FileOffer, String> {
        self.offer_checked(
            project_id,
            terminal,
            path,
            std::env::home_dir().as_deref(),
            &|| {},
        )
    }

    /// As [`FileOffers::offer`]; `between` runs after the path is first
    /// checked and before the file is opened, for a test to change it there.
    fn offer_checked(
        &self,
        project_id: &str,
        terminal: &str,
        path: &Path,
        home: Option<&Path>,
        between: &dyn Fn(),
    ) -> Result<FileOffer, String> {
        if !path.is_absolute() {
            return Err("give the file's full path".to_owned());
        }
        let real = std::fs::canonicalize(path)
            .map_err(|_| format!("there is no file at {}", path.display()))?;
        // Only a regular file is opened (a pipe would hang the open).
        let first = std::fs::metadata(&real).map_err(|_| "that file cannot be read".to_owned())?;
        if !first.is_file() {
            return Err(format!(
                "{} is not a file (a folder can be sent as an archive: zip it first)",
                path.display()
            ));
        }
        between();
        let changed = || "that file changed while it was being offered: offer it again".to_owned();
        let file = open_nofollow(&real).map_err(|_| changed())?;
        // Every check from here is on what was opened: its type and size
        // from the handle, its place from the path as it resolves now, and
        // the file there must be the one opened.
        let meta = file.metadata().map_err(|_| changed())?;
        if !meta.is_file() {
            return Err(changed());
        }
        if std::fs::canonicalize(path).ok().as_ref() != Some(&real) {
            return Err(changed());
        }
        let at_path = std::fs::metadata(&real).map_err(|_| changed())?;
        if !same_file(&at_path, &meta) {
            return Err(changed());
        }
        if meta.len() > MAX_OFFER_BYTES {
            return Err(format!(
                "that file is {} MB, and at most {} MB are sent",
                meta.len().div_ceil(1024 * 1024),
                MAX_OFFER_BYTES / (1024 * 1024)
            ));
        }
        if private(&real, home) {
            return Err("files in folders of keys and credentials are never sent".to_owned());
        }
        let name = real
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| "that file has no name".to_owned())?;
        let mut inner = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.0 == 0 {
            inner.0 = first_id();
        }
        inner.0 = inner.0.wrapping_add(1).max(1);
        let offer = FileOffer {
            id: inner.0,
            project_id: project_id.to_owned(),
            terminal: terminal.to_owned(),
            path: real,
            name,
            bytes: meta.len(),
            offered: Instant::now(),
            file: Arc::new(file),
        };
        let now = Instant::now();
        inner.1.retain(|o| !o.expired(now));
        // The same file offered again replaces its older offer.
        inner
            .1
            .retain(|o| !(o.path == offer.path && o.project_id == offer.project_id));
        inner.1.push(offer.clone());
        let excess = inner.1.len().saturating_sub(MAX_OFFERS);
        inner.1.drain(..excess);
        Ok(offer)
    }

    /// The offers still open, oldest first.
    #[must_use]
    pub fn live(&self) -> Vec<FileOffer> {
        let now = Instant::now();
        let mut inner = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        inner.1.retain(|o| !o.expired(now));
        inner.1.clone()
    }

    /// The open offer `id`.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<FileOffer> {
        self.live().into_iter().find(|o| o.id == id)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_regular_file_is_offered_and_anything_else_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("report.html");
        std::fs::write(&file, b"<html>").unwrap();
        let offers = FileOffers::default();
        let offer = offers.offer("p", "claude-1", &file).unwrap();
        assert_eq!(offer.name, "report.html");
        assert_eq!(offer.bytes, 6);
        assert_eq!(offers.get(offer.id), Some(offer.clone()));
        // Offered again: one offer, the newest.
        let again = offers.offer("p", "claude-1", &file).unwrap();
        assert_eq!(offers.live(), vec![again]);

        assert!(offers.offer("p", "c", Path::new("report.html")).is_err());
        assert!(offers.offer("p", "c", &dir.path().join("missing")).is_err());
        assert!(offers.offer("p", "c", dir.path()).is_err());
    }

    #[test]
    fn ids_go_on_from_the_clock_not_from_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        std::fs::write(&file, b"x").unwrap();
        let offers = FileOffers::default();
        let first = offers.offer("p", "c", &file).unwrap().id;
        assert!(first > 1 << 40, "{first}");
        assert_eq!(offers.offer("p", "c", &file).unwrap().id, first + 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_key_folder_that_is_a_link_is_private_where_it_points() {
        let home = tempfile::tempdir().unwrap();
        let keys = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(keys.path(), home.path().join(".ssh")).unwrap();
        let real = std::fs::canonicalize(keys.path())
            .unwrap()
            .join("id_ed25519");
        assert!(private(&real, Some(home.path())));
    }

    /// The file swapped for a pipe while the offer is made: the open does not
    /// wait on it, and nothing is offered.
    #[cfg(unix)]
    #[test]
    fn a_file_swapped_for_a_pipe_while_it_is_offered_does_not_hang() {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::canonicalize(dir.path()).unwrap().join("r.html");
        std::fs::write(&file, b"report").unwrap();
        let offers = FileOffers::default();
        let swap = || {
            std::fs::remove_file(&file).unwrap();
            let made = std::process::Command::new("mkfifo")
                .arg(&file)
                .status()
                .unwrap();
            assert!(made.success());
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let got = offers.offer_checked("p", "c", &file, None, &swap);
                done_tx.send(got.is_err()).unwrap();
            });
            let refused = done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the offer waited on the pipe");
            assert!(refused);
        });
    }

    /// A link put in place of the file while the offer is made is not
    /// followed.
    #[cfg(unix)]
    #[test]
    fn a_file_swapped_for_a_link_while_it_is_offered_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (file, other) = (root.join("r.html"), root.join("other"));
        std::fs::write(&file, b"report").unwrap();
        std::fs::write(&other, b"other").unwrap();
        let offers = FileOffers::default();
        let swap = || {
            std::fs::remove_file(&file).unwrap();
            std::os::unix::fs::symlink(&other, &file).unwrap();
        };
        assert!(offers.offer_checked("p", "c", &file, None, &swap).is_err());
    }

    /// A folder on the path swapped for a link while the offer is made: the
    /// file opened is not where the path was checked, so nothing is offered.
    #[cfg(unix)]
    #[test]
    fn a_path_changed_while_it_is_offered_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let shown = root.path().join("out");
        let other = root.path().join("other");
        std::fs::create_dir(&shown).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::write(shown.join("r.html"), b"report").unwrap();
        std::fs::write(other.join("r.html"), vec![0u8; 10]).unwrap();
        let path = std::fs::canonicalize(&shown).unwrap().join("r.html");
        let offers = FileOffers::default();
        let swap = || {
            std::fs::rename(&shown, root.path().join("was")).unwrap();
            std::os::unix::fs::symlink(&other, &shown).unwrap();
        };
        let refused = offers.offer_checked("p", "c", &path, None, &swap);
        assert!(refused.is_err(), "{refused:?}");
        assert!(offers.live().is_empty());
    }

    #[test]
    fn key_folders_are_private() {
        let home = Path::new("/Users/me");
        assert!(private(Path::new("/Users/me/.ssh/id_ed25519"), Some(home)));
        assert!(private(
            Path::new("/Users/me/Library/Keychains/login.keychain-db"),
            Some(home)
        ));
        assert!(!private(
            Path::new("/Users/me/.claude/usage/report.html"),
            Some(home)
        ));
    }
}
