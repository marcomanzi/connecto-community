//! Storage for chat uploads, and for files sent to a host's terminals, under
//! the host's private data directory.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use cap_std::time::SystemTime;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
#[cfg(unix)]
use cap_std::fs::{DirBuilderExt, OpenOptionsExt};
use sha2::{Digest, Sha256};

use crate::wire::MAX_ATTACHMENT_BYTES;

/// One project's stored files, in all (chat attachments, terminal uploads
/// and the uploads under way), kept for [`RETENTION`]. Deliberately 512 MiB,
/// up from 128 MiB when a terminal upload took 10 MiB: a drop carries up to
/// [`connecto_core::terminals::MAX_UPLOAD_FILES`] files of up to 100 MiB,
/// and 128 MiB would take one such file at a time, a week long. This also
/// lets a project's chat alone keep up to 512 MiB.
const STORAGE_BYTES: u64 = 512 * 1024 * 1024;
const STORAGE_FILES: usize = 128;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const TEMP_RETENTION: Duration = Duration::from_secs(60 * 60);
pub(crate) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// The slowest average rate, in bytes a second (about 1 Mbit/s), at which a
/// large transfer still gets to finish: see [`transfer_timeout`].
const MIN_TRANSFER_RATE: u64 = 128 * 1024;
const STORAGE_ERROR: &str = "Could not save the attachment on the host.";

/// How long a transfer of `bytes` may take in all, on both ends:
/// [`TRANSFER_TIMEOUT`], or for a file too large to arrive in that at
/// [`MIN_TRANSFER_RATE`], as long as that rate needs (100 MiB: 13 min 20 s).
/// A transfer that stalls still ends after [`IDLE_TIMEOUT`].
pub(crate) fn transfer_timeout(bytes: usize) -> Duration {
    TRANSFER_TIMEOUT.max(Duration::from_secs(bytes as u64 / MIN_TRANSFER_RATE))
}

/// The largest file pasted or dropped on a local terminal as bytes only (a
/// screenshot): larger than a chat upload, as a full-screen capture can be.
pub const MAX_PASTED_BYTES: usize = 32 * 1024 * 1024;

use connecto_core::session::ATTACHMENTS_DIR;

pub(crate) fn valid_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// The display name can be Unicode; the stored basename is portable across
/// shells and filesystems and can never name a different directory.
fn stored_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .take(100)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_matches('.');
    if safe.is_empty() {
        "file".to_owned()
    } else {
        safe.to_owned()
    }
}

fn ensure_dir(parent: &Dir, name: &str) -> io::Result<Dir> {
    let mut options = DirBuilder::new();
    options.recursive(false);
    #[cfg(unix)]
    options.mode(0o700);
    match parent.create_dir_with(name, &options) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    parent.open_dir_nofollow(name)
}

fn prune(dir: &Dir, now: SystemTime) -> io::Result<()> {
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(text) = name.to_str() else { continue };
        let retention = if text.strip_prefix(".upload-").is_some_and(valid_token) {
            TEMP_RETENTION
        } else if text.split_once('-').is_some_and(|(digest, suffix)| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                && !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
        }) {
            RETENTION
        } else {
            continue;
        };
        let meta = dir.symlink_metadata(&name)?;
        if meta.is_file()
            && meta.modified().is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age >= retention)
            })
        {
            dir.remove_file(&name)?;
        }
    }
    Ok(())
}

fn existing_equals(dir: &Dir, name: &str, bytes: &[u8]) -> io::Result<bool> {
    match dir.symlink_metadata(name) {
        Ok(meta) if meta.is_file() && meta.len() == bytes.len() as u64 => {
            let mut opts = OpenOptions::new();
            opts.read(true).follow(FollowSymlinks::No);
            let file = dir.open_with(name, &opts)?;
            let mut existing = Vec::new();
            file.take(bytes.len() as u64 + 1)
                .read_to_end(&mut existing)?;
            Ok(existing == bytes)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Saves bytes pasted or dropped on a terminal of this machine's project
/// `project` (a workspace project id) beside the project's chat uploads, so
/// they follow the same limits, private permissions and seven-day cleanup.
/// The file is named after `name`, behind a unique prefix. Returns its path.
///
/// # Errors
/// In plain words, for the pane's notice: too large, storage full, or not
/// saved.
pub fn store_pasted(
    root: &Path,
    project: &str,
    name: &str,
    bytes: &[u8],
) -> Result<String, String> {
    if bytes.len() > MAX_PASTED_BYTES {
        return Err("The pasted image is larger than 32 MiB.".to_owned());
    }
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| PASTE_ERROR)?;
    let token = format!("{:032x}", u128::from_be_bytes(random));
    // A local paste has no peer: its own key space, apart from any peer's.
    store_within(
        root,
        project,
        &[0; 32],
        &token,
        name,
        bytes,
        MAX_PASTED_BYTES,
    )
    .map_err(|why| {
        if why == STORAGE_ERROR {
            PASTE_ERROR.to_owned()
        } else {
            why.replace(" on the host", "")
        }
    })
}

const PASTE_ERROR: &str = "Could not save the pasted image.";

pub(crate) fn store(
    root: &Path,
    project: &str,
    endpoint: &[u8; 32],
    token: &str,
    name: &str,
    bytes: &[u8],
) -> Result<String, String> {
    store_within(
        root,
        project,
        endpoint,
        token,
        name,
        bytes,
        MAX_ATTACHMENT_BYTES,
    )
}

/// Why [`Incoming::publish`] saved nothing: `allowed` said no.
pub(crate) const NOT_ALLOWED: &str = "The host no longer allows this transfer.";
const STORAGE_FULL: &str = "This project's attachment storage on the host is full.";

/// A terminal upload under way ([`Incoming`]): its project folder, its
/// temporary file and the bytes it announced. It counts against that
/// folder's quota as one file of its full size from before the peer sends a
/// byte until it is published or given up, so two uploads can never both be
/// let in for the same room.
struct Reservation {
    folder: PathBuf,
    temporary: String,
    bytes: u64,
}

/// Several peers (and a local paste) may store at once: the quota and token
/// checks and the publication run as one step under this, or two stores
/// could each pass the checks before either file exists. It holds the
/// uploads under way.
fn storing() -> std::sync::MutexGuard<'static, Vec<Reservation>> {
    static STORING: std::sync::Mutex<Vec<Reservation>> = std::sync::Mutex::new(Vec::new());
    STORING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The folder a reservation for `project` under the absolute `root` names.
fn folder(root: &Path, project: &str) -> PathBuf {
    root.join(ATTACHMENTS_DIR).join(project)
}

/// The absolute data root and project `project`'s attachments folder in it,
/// created if missing. Only the trusted application data root is opened as an
/// ambient path; everything below it is resolved relative to directory
/// handles.
fn project_dir(root: &Path, project: &str) -> Result<(PathBuf, Dir), String> {
    let root = std::path::absolute(root).map_err(|_| STORAGE_ERROR)?;
    let data = Dir::open_ambient_dir(&root, ambient_authority()).map_err(|_| STORAGE_ERROR)?;
    let uploads = ensure_dir(&data, ATTACHMENTS_DIR).map_err(|_| STORAGE_ERROR)?;
    let dir = ensure_dir(&uploads, project).map_err(|_| STORAGE_ERROR)?;
    Ok((root, dir))
}

fn prune_now(dir: &Dir) -> Result<(), String> {
    prune(dir, SystemTime::from_std(std::time::SystemTime::now())).map_err(|_| STORAGE_ERROR.into())
}

/// The prefix of every file stored for `endpoint`'s `token`.
fn key_prefix(endpoint: &[u8; 32], token: &str) -> String {
    let mut key = Sha256::new();
    key.update(endpoint);
    key.update(token);
    let digest = key.finalize();
    let mut prefix = String::with_capacity(65);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(prefix, "{byte:02x}");
    }
    prefix.push('-');
    prefix
}

/// The path the host reports for `stored` in `project`'s folder under the
/// absolute `root`.
fn stored_path(root: &Path, project: &str, stored: &str) -> Result<String, String> {
    let path = root
        .join(ATTACHMENTS_DIR)
        .join(project)
        .join(stored)
        .into_os_string()
        .into_string()
        .map_err(|_| STORAGE_ERROR)?;
    if path.len() > 4096 || path.chars().any(char::is_control) {
        return Err(STORAGE_ERROR.to_owned());
    }
    Ok(path)
}

/// Whether `dir` (folder `folder`) has room for `incoming` more bytes in one
/// more file, and (with a `prefix`) no file of that token yet. The uploads
/// under way in `reserved` count at their announced size, their temporaries
/// not again. `own` is the store's own temporary, counted neither way (its
/// bytes are `incoming`).
fn check_room(
    dir: &Dir,
    folder: &Path,
    reserved: &[Reservation],
    prefix: Option<&str>,
    own: Option<&str>,
    incoming: u64,
) -> Result<(), String> {
    let full = |count: usize, total: u64| count >= STORAGE_FILES || total > STORAGE_BYTES;
    let others: Vec<&Reservation> = reserved
        .iter()
        .filter(|r| r.folder == folder && Some(r.temporary.as_str()) != own)
        .collect();
    let mut count = others.len();
    let mut total = others
        .iter()
        .fold(incoming, |total, r| total.saturating_add(r.bytes));
    if full(count, total) {
        return Err(STORAGE_FULL.to_owned());
    }
    for entry in dir.entries().map_err(|_| STORAGE_ERROR)? {
        let entry = entry.map_err(|_| STORAGE_ERROR)?;
        let name = entry.file_name();
        let name = name.to_str();
        if prefix.is_some_and(|prefix| name.is_some_and(|name| name.starts_with(prefix))) {
            return Err("This attachment token was already used for a different file.".to_owned());
        }
        if name.is_some_and(|name| Some(name) == own || others.iter().any(|r| r.temporary == name))
        {
            continue;
        }
        let meta = entry.metadata().map_err(|_| STORAGE_ERROR)?;
        count += 1;
        total = total.saturating_add(meta.len());
        if full(count, total) {
            return Err(STORAGE_FULL.to_owned());
        }
    }
    Ok(())
}

/// A new private temporary file in `dir`, and its name. Pruned after
/// [`TEMP_RETENTION`] if it is ever left behind.
fn open_temporary(dir: &Dir) -> Result<(String, cap_std::fs::File), String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| STORAGE_ERROR)?;
    let temporary = format!(".upload-{:032x}", u128::from_be_bytes(random));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = dir
        .open_with(&temporary, &options)
        .map_err(|_| STORAGE_ERROR)?;
    Ok((temporary, file))
}

fn store_within(
    root: &Path,
    project: &str,
    endpoint: &[u8; 32],
    token: &str,
    name: &str,
    bytes: &[u8],
    limit: usize,
) -> Result<String, String> {
    if !crate::remote_terminals::is_canonical_project_id(project)
        || !valid_token(token)
        || bytes.len() > limit
        || name.is_empty()
    {
        return Err("The attachment is invalid.".to_owned());
    }
    let reserved = storing();
    let (root, dir) = project_dir(root, project)?;
    prune_now(&dir)?;
    let prefix = key_prefix(endpoint, token);
    let name = format!("{prefix}{}", stored_name(name));
    let path = stored_path(&root, project, &name)?;
    if existing_equals(&dir, &name, bytes).map_err(|_| STORAGE_ERROR)? {
        return Ok(path);
    }
    check_room(
        &dir,
        &folder(&root, project),
        &reserved,
        Some(&prefix),
        None,
        bytes.len() as u64,
    )?;
    let (temporary, mut file) = open_temporary(&dir)?;
    // A hard link publishes the complete file with no replacement semantics.
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| dir.hard_link(&temporary, &dir, &name));
    drop(file);
    let _ = dir.remove_file(&temporary);
    result.map_err(|_| STORAGE_ERROR)?;
    Ok(path)
}

/// A file a peer is sending to one of the host's terminals, on its way to the
/// terminal's project's attachments folder. Its bytes go to a private
/// temporary file there as they arrive, so the host holds one chunk of it at
/// a time, never the whole file. Its announced size is reserved in the
/// project's quota from [`Incoming::begin`] on (see [`Reservation`]).
/// Published whole by [`Incoming::publish`]; dropped, published or not, it
/// leaves no temporary and no reservation behind.
pub(crate) struct Incoming {
    root: PathBuf,
    project: String,
    folder: PathBuf,
    dir: Dir,
    temporary: String,
    file: Option<cap_std::fs::File>,
    expected: usize,
    written: usize,
}

impl Incoming {
    /// Ready to receive a file of `bytes` bytes for project `project`, its
    /// size reserved. Refused at once when it is over `limit` or the
    /// project's storage, with the uploads already under way, has no room
    /// for it, so the peer does not send it in vain.
    ///
    /// # Errors
    /// In plain words, for the peer.
    pub(crate) fn begin(
        root: &Path,
        project: &str,
        bytes: usize,
        limit: usize,
    ) -> Result<Self, String> {
        if !crate::remote_terminals::is_canonical_project_id(project) || bytes > limit {
            return Err("The attachment is invalid.".to_owned());
        }
        let (root, dir) = project_dir(root, project)?;
        let folder = folder(&root, project);
        let mut reserved = storing();
        prune_now(&dir)?;
        check_room(&dir, &folder, &reserved, None, None, bytes as u64)?;
        let (temporary, file) = open_temporary(&dir)?;
        reserved.push(Reservation {
            folder: folder.clone(),
            temporary: temporary.clone(),
            bytes: bytes as u64,
        });
        Ok(Self {
            root,
            project: project.to_owned(),
            folder,
            dir,
            temporary,
            file: Some(file),
            expected: bytes,
            written: 0,
        })
    }

    /// The bytes still to come.
    pub(crate) fn remaining(&self) -> usize {
        self.expected - self.written
    }

    /// Writes the next `chunk`: refused past the size announced.
    ///
    /// # Errors
    /// In plain words, for the peer.
    pub(crate) fn write(&mut self, chunk: &[u8]) -> Result<(), String> {
        if chunk.len() > self.remaining() {
            return Err("The attachment is invalid.".to_owned());
        }
        let file = self.file.as_mut().ok_or(STORAGE_ERROR)?;
        file.write_all(chunk).map_err(|_| STORAGE_ERROR)?;
        self.written += chunk.len();
        Ok(())
    }

    /// Publishes the complete file under a name made from `name`, for
    /// `endpoint`'s `token`: its path. Only if `allowed` still holds once
    /// this store's turn has come (stores run one at a time, so it can
    /// wait), and again just before the file is published: a pause or a
    /// lost right meanwhile saves nothing, and says [`NOT_ALLOWED`].
    ///
    /// # Errors
    /// In plain words, for the peer.
    pub(crate) fn publish(
        mut self,
        endpoint: &[u8; 32],
        token: &str,
        name: &str,
        allowed: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        if self.written != self.expected || !valid_token(token) || name.is_empty() {
            return Err("The attachment is invalid.".to_owned());
        }
        // Flushed before the store's turn, which then stays short.
        let file = self.file.take().ok_or(STORAGE_ERROR)?;
        file.sync_all().map_err(|_| STORAGE_ERROR)?;
        drop(file);
        let mut reserved = storing();
        let published = self.publish_held(&mut reserved, endpoint, token, name, allowed);
        // Let go before `self` is dropped, which takes it again.
        drop(reserved);
        published
    }

    /// [`Incoming::publish`] once the store's turn has come. Published, the
    /// file stops being an upload under way in the same step: its
    /// reservation and temporary go before the lock does, so no other store
    /// ever counts it twice.
    fn publish_held(
        &self,
        reserved: &mut Vec<Reservation>,
        endpoint: &[u8; 32],
        token: &str,
        name: &str,
        allowed: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        if !allowed() {
            return Err(NOT_ALLOWED.to_owned());
        }
        prune_now(&self.dir)?;
        let prefix = key_prefix(endpoint, token);
        let stored = format!("{prefix}{}", stored_name(name));
        let path = stored_path(&self.root, &self.project, &stored)?;
        check_room(
            &self.dir,
            &self.folder,
            reserved,
            Some(&prefix),
            Some(&self.temporary),
            self.written as u64,
        )?;
        // Asked again just before publishing: the scans above take a while.
        if !allowed() {
            return Err(NOT_ALLOWED.to_owned());
        }
        // A hard link publishes the complete file with no replacement
        // semantics.
        self.dir
            .hard_link(&self.temporary, &self.dir, &stored)
            .map_err(|_| STORAGE_ERROR)?;
        self.retire(reserved);
        Ok(path)
    }

    /// Removes the temporary and the reservation, with the lock held.
    fn retire(&self, reserved: &mut Vec<Reservation>) {
        let _ = self.dir.remove_file(&self.temporary);
        reserved.retain(|r| !(r.folder == self.folder && r.temporary == self.temporary));
    }
}

impl Drop for Incoming {
    /// Given up, or already retired by [`Incoming::publish`] (then there is
    /// nothing left to remove).
    fn drop(&mut self) {
        drop(self.file.take());
        self.retire(&mut storing());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_file_is_private_beside_the_uploads_and_kept_by_the_same_rules() {
        let root = tempfile::tempdir().unwrap();
        let path = store_pasted(root.path(), "0a1b2c", "pasted-20261119.png", b"\x89PNG").unwrap();
        let path = std::path::Path::new(&path);
        assert_eq!(
            path.parent().unwrap(),
            root.path().join(ATTACHMENTS_DIR).join("0a1b2c")
        );
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.ends_with("-pasted-20261119.png"), "{name}");
        assert_eq!(std::fs::read(path).unwrap(), b"\x89PNG");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Two pastes of the same name are two files.
        let again = store_pasted(root.path(), "0a1b2c", "pasted-20261119.png", b"x").unwrap();
        assert_ne!(std::path::Path::new(&again), path);
        // The owned-name rule prunes it after the retention.
        let dir = Dir::open_ambient_dir(path.parent().unwrap(), ambient_authority()).unwrap();
        prune(
            &dir,
            SystemTime::from_std(std::time::SystemTime::now()) + RETENTION,
        )
        .unwrap();
        assert!(!path.exists());
        assert!(store_pasted(root.path(), "../up", "a.png", b"x").is_err());
        assert!(
            store_pasted(
                root.path(),
                "0a1b2c",
                "a.png",
                &vec![0; MAX_PASTED_BYTES + 1]
            )
            .unwrap_err()
            .contains("32 MiB")
        );
    }

    #[test]
    fn a_store_no_longer_allowed_when_its_turn_comes_saves_nothing() {
        let root = tempfile::tempdir().unwrap();
        let token = "c".repeat(32);
        let incoming = || {
            let mut incoming = Incoming::begin(root.path(), "0a1b", 1, 1).unwrap();
            incoming.write(b"x").unwrap();
            incoming
        };
        let refused = incoming().publish(&[1; 32], &token, "a.png", &|| false);
        assert_eq!(refused.unwrap_err(), NOT_ALLOWED);
        let folder = root.path().join(ATTACHMENTS_DIR).join("0a1b");
        assert_eq!(
            std::fs::read_dir(&folder).unwrap().count(),
            0,
            "no file, no temporary"
        );
        // Allowed when its turn came, refused just before publishing.
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let once = || asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let late = incoming().publish(&[1; 32], &token, "a.png", &once);
        assert_eq!(late.unwrap_err(), NOT_ALLOWED);
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read_dir(&folder).unwrap().count(),
            0,
            "no file, no temporary"
        );
        let path = incoming()
            .publish(&[1; 32], &token, "a.png", &|| true)
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"x");
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
    }

    /// A file sent to a terminal goes to disk chunk by chunk, never past the
    /// size it announced, and is published only whole. One that cannot fit
    /// the project's storage is refused before a byte of it is sent.
    #[test]
    fn an_incoming_file_is_written_as_it_arrives_and_published_only_whole() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join(ATTACHMENTS_DIR).join("0a1b");
        let token = "d".repeat(32);

        let mut incoming = Incoming::begin(root.path(), "0a1b", 6, 6).unwrap();
        incoming.write(b"abc").unwrap();
        assert_eq!(incoming.remaining(), 3);
        // The bytes so far are on disk, in the private temporary only.
        let on_disk: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        assert_eq!(on_disk.len(), 1);
        assert!(
            on_disk[0]
                .file_name()
                .to_str()
                .unwrap()
                .starts_with(".upload-"),
            "{:?}",
            on_disk[0].file_name()
        );
        assert_eq!(std::fs::read(on_disk[0].path()).unwrap(), b"abc");
        assert!(incoming.write(b"defg").is_err(), "past the announced size");
        incoming.write(b"def").unwrap();
        let path = incoming
            .publish(&[2; 32], &token, "b.bin", &|| true)
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1, "{path}");

        // Cut short: dropped, nothing stays; short of its size, not published.
        let mut cut = Incoming::begin(root.path(), "0a1b", 6, 6).unwrap();
        cut.write(b"ab").unwrap();
        drop(cut);
        let mut short = Incoming::begin(root.path(), "0a1b", 6, 6).unwrap();
        short.write(b"ab").unwrap();
        let other = "e".repeat(32);
        assert!(short.publish(&[2; 32], &other, "c.bin", &|| true).is_err());
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);

        // Over the limit, a bad project, or no room left: refused at once.
        assert!(Incoming::begin(root.path(), "0a1b", 7, 6).is_err());
        assert!(Incoming::begin(root.path(), "../x", 1, 6).is_err());
        let full = Incoming::begin(root.path(), "0a1b", STORAGE_BYTES as usize, usize::MAX);
        assert_eq!(full.err().unwrap(), STORAGE_FULL);
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
    }

    /// Two 100 MiB uploads asked at once with room for one: the second is
    /// refused before either sends a byte, as is a chat attachment, since the
    /// first holds the room it announced. Given up, the room is free again.
    #[test]
    fn uploads_under_way_hold_the_room_they_announced() {
        const LARGE: usize = 100 * 1024 * 1024;
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join(ATTACHMENTS_DIR).join("0a1b");
        std::fs::create_dir_all(&folder).unwrap();
        // Sparse: the quota counts its length, nothing near it is written.
        std::fs::File::create(folder.join("earlier"))
            .unwrap()
            .set_len(STORAGE_BYTES - LARGE as u64)
            .unwrap();

        let first = Incoming::begin(root.path(), "0a1b", LARGE, LARGE).unwrap();
        let second = Incoming::begin(root.path(), "0a1b", LARGE, LARGE);
        assert_eq!(second.err().unwrap(), STORAGE_FULL);
        let chat = store(
            root.path(),
            "0a1b",
            &[3; 32],
            &"f".repeat(32),
            "a.txt",
            b"x",
        );
        assert_eq!(chat.unwrap_err(), STORAGE_FULL);
        // Another project's room is its own.
        drop(Incoming::begin(root.path(), "0a1c", LARGE, LARGE).unwrap());

        drop(first);
        let again = Incoming::begin(root.path(), "0a1b", LARGE, LARGE).unwrap();
        drop(again);
        assert!(storing().iter().all(|r| r.folder != folder), "released");
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
    }

    /// Two uploads that fit the quota exactly are both published. Once
    /// published, the first counts as its file alone, even before it is
    /// dropped (as when the second's turn comes in between).
    #[test]
    fn two_uploads_that_fit_exactly_are_both_published() {
        const SIZE: usize = 4;
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join(ATTACHMENTS_DIR).join("0a1b");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::File::create(folder.join("earlier"))
            .unwrap()
            .set_len(STORAGE_BYTES - 2 * SIZE as u64)
            .unwrap();
        let begin = || {
            let mut incoming = Incoming::begin(root.path(), "0a1b", SIZE, SIZE).unwrap();
            incoming.write(b"abcd").unwrap();
            incoming
        };
        let first = begin();
        let second = begin();
        let one = first
            .publish_held(
                &mut storing(),
                &[4; 32],
                &"1".repeat(32),
                "one.bin",
                &|| true,
            )
            .unwrap();
        let two = second
            .publish(&[4; 32], &"2".repeat(32), "two.bin", &|| true)
            .unwrap();
        drop(first);
        assert_eq!(std::fs::read(one).unwrap(), b"abcd");
        assert_eq!(std::fs::read(two).unwrap(), b"abcd");
        assert_eq!(
            std::fs::read_dir(&folder).unwrap().count(),
            3,
            "no temporaries"
        );
        assert!(
            storing().iter().all(|r| r.folder != folder),
            "none reserved"
        );
    }

    #[test]
    fn a_large_transfer_gets_longer_than_a_small_one() {
        assert_eq!(transfer_timeout(MAX_ATTACHMENT_BYTES), TRANSFER_TIMEOUT);
        let largest = transfer_timeout(crate::wire::MAX_UPLOAD_BYTES);
        assert_eq!(largest, Duration::from_secs(800));
    }

    #[test]
    fn prunes_only_the_owned_file_names_after_their_retention() {
        let root = tempfile::tempdir().unwrap();
        let dir = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        let upload = format!("{}-notes.txt", "a".repeat(64));
        let temporary = format!(".upload-{}", "b".repeat(32));
        let unrelated = "notes.txt";
        for name in [&upload, &temporary, unrelated] {
            dir.write(name, b"some content").unwrap();
        }
        let now = SystemTime::from_std(std::time::SystemTime::now());
        prune(&dir, now + TEMP_RETENTION).unwrap();
        assert!(!dir.exists(&temporary));
        assert!(dir.exists(&upload));
        prune(&dir, now + RETENTION).unwrap();
        assert!(!dir.exists(&upload));
        assert!(dir.exists(unrelated));
    }
}
