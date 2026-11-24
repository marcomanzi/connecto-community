//! The operations folder of a project, `.connecto/ops/<slug>/` (spec §3):
//! reading it, the approval hash, and keeping it out of git.

use std::fs;
#[cfg(not(unix))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::spec::{self, OpSpec};

/// The folder, relative to the project.
pub const OPS_DIR: &str = ".connecto/ops";
/// Where discovery's digest goes, inside [`OPS_DIR`] (not an operation: its
/// name is not a valid slug).
pub const DISCOVERY_DIR: &str = ".discovery";
/// Most operation folders read.
pub const MAX_OPS: usize = 100;

/// One operation folder as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub slug: String,
    pub loaded: Result<Loaded, String>,
}

/// An operation whose files passed the checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub spec: OpSpec,
    /// `op.json` exactly as read: with the body, what the hash covers, so a
    /// peer shown both can check that the hash it approves is theirs.
    pub spec_text: String,
    /// The script or the recipe.
    pub body: String,
    /// What an approval records ([`hash`]).
    pub hash: String,
}

#[must_use]
pub fn ops_dir(project: &Path) -> PathBuf {
    project.join(OPS_DIR)
}

/// `dir` exists as a real folder: not a link, not a file.
fn real_dir(dir: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "{} is a link; operations are read only from a real folder",
            dir.display()
        )),
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(format!("{} is not a folder", dir.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("{}: {err}", dir.display())),
    }
}

/// The operations folder, when it is there and confined to the project:
/// `.connecto` and `.connecto/ops` are real folders (no link on the way),
/// and the folder resolves inside the project. `Ok(None)` when it does not
/// exist yet.
///
/// # Errors
///
/// A link or a file where a folder should be.
pub fn checked_ops_dir(project: &Path) -> Result<Option<PathBuf>, String> {
    let dot = project.join(".connecto");
    if !real_dir(&dot)? {
        return Ok(None);
    }
    let ops = ops_dir(project);
    if !real_dir(&ops)? {
        return Ok(None);
    }
    let inside = match (fs::canonicalize(project), fs::canonicalize(&ops)) {
        (Ok(root), Ok(real)) => real.starts_with(&root),
        _ => false,
    };
    if !inside {
        return Err("the operations folder is not inside the project".to_owned());
    }
    Ok(Some(ops))
}

/// As [`checked_ops_dir`], creating the folders when they are missing.
///
/// # Errors
///
/// A link where a folder should be, or a folder that cannot be made.
pub fn prepare_ops_dir(project: &Path) -> Result<PathBuf, String> {
    for dir in [project.join(".connecto"), ops_dir(project)] {
        if !real_dir(&dir)? {
            fs::create_dir(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
    }
    checked_ops_dir(project)?.ok_or_else(|| "the operations folder is missing".to_owned())
}

/// SHA-256 of `op.json`, a zero byte and the body, in hex.
#[must_use]
pub fn hash(spec_bytes: &[u8], body: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(spec_bytes);
    digest.update([0_u8]);
    digest.update(body);
    digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The same file: what was inspected is what was opened.
#[cfg(not(unix))]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

/// A regular file (never a link) of at most `max` bytes. The opened file
/// must be the one inspected, so a link swapped in between is refused.
#[cfg(not(unix))]
fn read_file(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let meta = fs::symlink_metadata(path).map_err(|_| format!("{name} is missing"))?;
    if !meta.is_file() {
        return Err(format!("{name} is not a regular file"));
    }
    let file = fs::File::open(path).map_err(|err| format!("{name} cannot be read: {err}"))?;
    let opened = file
        .metadata()
        .map_err(|err| format!("{name} cannot be read: {err}"))?;
    if !opened.is_file() || !same_file(&meta, &opened) {
        return Err(format!("{name} changed while it was read"));
    }
    let mut bytes = Vec::new();
    let limit = u64::try_from(max).unwrap_or(u64::MAX) + 1;
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("{name} cannot be read: {err}"))?;
    if bytes.len() > max {
        return Err(format!("{name} is too large"));
    }
    Ok(bytes)
}

/// Check one operation from its two files, read by `read(name, max)`.
/// Reads one file of an operation folder, at most so many bytes.
type ReadFile<'a> = &'a dyn Fn(&str, usize) -> Result<Vec<u8>, String>;

fn check_files(read: ReadFile<'_>) -> Result<Loaded, String> {
    let spec_bytes = read(spec::FILE, spec::MAX_SPEC)?;
    let spec_text =
        std::str::from_utf8(&spec_bytes).map_err(|_| "op.json is not UTF-8".to_owned())?;
    let spec = spec::parse(spec_text)?;
    let body = read(spec.body_file(), body_max(&spec))?;
    spec::check_body(&spec, &body)?;
    let body = String::from_utf8(body).map_err(|_| "the body is not UTF-8".to_owned())?;
    checked(spec_text, &body)
}

/// The most bytes of the body of `spec`'s kind.
fn body_max(spec: &OpSpec) -> usize {
    match spec.kind {
        connecto_core::ops::OpKind::Recipe => spec::MAX_RECIPE,
        _ => spec::MAX_SCRIPT,
    }
}

/// Check an operation from the text of its two files, as [`load`] does once
/// it has read them: a peer sent the files checks them here, so what it
/// shows and the hash it approves both come from those exact bytes.
///
/// # Errors
///
/// The files do not pass the checks.
pub fn checked(spec_text: &str, body: &str) -> Result<Loaded, String> {
    let spec = spec::parse(spec_text)?;
    spec::check_body(&spec, body.as_bytes())?;
    Ok(Loaded {
        hash: hash(spec_text.as_bytes(), body.as_bytes()),
        spec,
        spec_text: spec_text.to_owned(),
        body: body.to_owned(),
    })
}

/// Read and check one operation folder. On Unix every step is taken from
/// the folder opened before it, never following a link, so nothing outside
/// the project is read even if a folder is swapped meanwhile.
///
/// # Errors
///
/// Why it cannot run, in one sentence.
pub fn load(project: &Path, slug: &str) -> Result<Loaded, String> {
    if !spec::valid_slug(slug) {
        return Err(format!("`{slug}` is not a valid operation name"));
    }
    #[cfg(unix)]
    {
        let ops = at::ops(project)?.ok_or_else(|| format!("there is no operation `{slug}`"))?;
        let dir = at::sub(&ops, slug)?.ok_or_else(|| format!("there is no operation `{slug}`"))?;
        check_files(&|name, max| at::read_file(&dir, name, max))
    }
    #[cfg(not(unix))]
    {
        let ops =
            checked_ops_dir(project)?.ok_or_else(|| format!("there is no operation `{slug}`"))?;
        let dir = ops.join(slug);
        let meta =
            fs::symlink_metadata(&dir).map_err(|_| format!("there is no operation `{slug}`"))?;
        if !meta.is_dir() {
            return Err(format!("`{slug}` is not a folder"));
        }
        check_files(&|name, max| read_file(&dir.join(name), max))
    }
}

/// The operation folders' names, sorted, at most [`MAX_OPS`].
fn slugs(project: &Path) -> Vec<String> {
    #[cfg(unix)]
    let mut slugs: Vec<String> = match at::ops(project) {
        Ok(Some(mut ops)) => at::folders(&mut ops),
        _ => return Vec::new(),
    };
    #[cfg(not(unix))]
    let mut slugs: Vec<String> = {
        let Ok(Some(ops)) = checked_ops_dir(project) else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(ops) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .collect()
    };
    slugs.retain(|s| spec::valid_slug(s));
    slugs.sort();
    slugs.truncate(MAX_OPS);
    slugs
}

/// Every operation folder of the project, sorted by slug. Entries that are
/// not valid slugs (such as `.discovery`) are skipped.
#[must_use]
pub fn scan(project: &Path) -> Vec<Found> {
    slugs(project)
        .into_iter()
        .map(|slug| Found {
            loaded: load(project, &slug),
            slug,
        })
        .collect()
}

/// Remove one operation's folder: on Unix relative to the operations folder
/// opened without following links, its contents removed the same way (a
/// link inside is removed as a link).
///
/// # Errors
///
/// Why it could not be removed.
pub fn delete(project: &Path, slug: &str) -> Result<(), String> {
    if !spec::valid_slug(slug) {
        return Err(format!("`{slug}` is not a valid operation name"));
    }
    #[cfg(unix)]
    {
        let ops = at::ops(project)?.ok_or_else(|| format!("there is no operation `{slug}`"))?;
        at::remove(&ops, slug)
    }
    // Without folder handles a deletion could be sent outside the project
    // by a folder swapped meanwhile: not done on this system.
    #[cfg(not(unix))]
    {
        let _ = project;
        Err(
            "Deleting operations is not available on this system; remove the folder by hand."
                .into(),
        )
    }
}

/// Write `name` in the operations folder's `sub` folder (made when
/// missing): a new file of our own under a unique name, renamed over
/// `name`, all relative to folders opened without following links.
///
/// # Errors
///
/// A link or file in the way, or a write that failed.
pub fn publish(project: &Path, sub: &str, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = format!(".{name}.{}.{nanos}.{n}.tmp", std::process::id());
    #[cfg(unix)]
    {
        let ops = at::ops_made(project)?;
        let dir = at::sub_made(&ops, sub)?;
        at::write_new(&dir, &tmp, name, bytes)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (sub, bytes, tmp);
        return Err("Writing in the operations folder is not available on this system.".into());
    }
    #[cfg(unix)]
    Ok(ops_dir(project).join(sub).join(name))
}

/// The folder calls of the Unix side: each relative to a folder already
/// open, never following a link.
#[cfg(unix)]
mod at {
    use std::io::Write as _;
    use std::os::fd::{AsRawFd, RawFd};
    use std::path::Path;

    use nix::dir::{Dir, Type};
    use nix::errno::Errno;
    use nix::fcntl::{OFlag, openat, renameat};
    use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat, mkdirat};
    use nix::unistd::{UnlinkatFlags, close, unlinkat};

    const DIR: OFlag = OFlag::O_RDONLY
        .union(OFlag::O_DIRECTORY)
        .union(OFlag::O_NOFOLLOW)
        .union(OFlag::O_CLOEXEC);

    /// A raw descriptor, closed on drop.
    struct Fd(RawFd);

    impl Drop for Fd {
        fn drop(&mut self) {
            let _ = close(self.0);
        }
    }

    fn kind(stat: &FileStat) -> SFlag {
        SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT
    }

    /// A folder inside `parent`; `None` when it is not there. A link or a
    /// file is an error.
    pub(super) fn sub(parent: &Dir, name: &str) -> Result<Option<Dir>, String> {
        match Dir::openat(Some(parent.as_raw_fd()), name, DIR, Mode::empty()) {
            Ok(dir) => Ok(Some(dir)),
            Err(Errno::ENOENT) => Ok(None),
            Err(Errno::ELOOP | Errno::ENOTDIR) => Err(format!(
                "{name} is a link or not a folder; operations are read only from real folders"
            )),
            Err(e) => Err(format!("{name}: {e}")),
        }
    }

    /// As [`sub`], made when missing.
    pub(super) fn sub_made(parent: &Dir, name: &str) -> Result<Dir, String> {
        if let Some(dir) = sub(parent, name)? {
            return Ok(dir);
        }
        match mkdirat(
            Some(parent.as_raw_fd()),
            name,
            Mode::from_bits_truncate(0o755),
        ) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(e) => return Err(format!("cannot create {name}: {e}")),
        }
        sub(parent, name)?.ok_or_else(|| format!("{name} could not be made"))
    }

    fn root(project: &Path) -> Result<Dir, String> {
        Dir::open(
            project,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| format!("{}: {e}", project.display()))
    }

    /// `.connecto/ops` of the project, opened through real folders only.
    pub(super) fn ops(project: &Path) -> Result<Option<Dir>, String> {
        let Some(dot) = sub(&root(project)?, ".connecto")? else {
            return Ok(None);
        };
        sub(&dot, "ops")
    }

    /// As [`ops`], made when missing.
    pub(super) fn ops_made(project: &Path) -> Result<Dir, String> {
        let dot = sub_made(&root(project)?, ".connecto")?;
        sub_made(&dot, "ops")
    }

    /// The folders in `dir`.
    pub(super) fn folders(dir: &mut Dir) -> Vec<String> {
        let fd = dir.as_raw_fd();
        let mut names = Vec::new();
        for entry in dir.iter().flatten() {
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if name == "." || name == ".." {
                continue;
            }
            let is_dir = match entry.file_type() {
                Some(Type::Directory) => true,
                Some(_) => false,
                None => fstatat(Some(fd), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW)
                    .is_ok_and(|st| kind(&st) == SFlag::S_IFDIR),
            };
            if is_dir {
                names.push(name.to_owned());
            }
        }
        names
    }

    /// A regular file in `dir`, at most `max` bytes. A link is refused.
    pub(super) fn read_file(dir: &Dir, name: &str, max: usize) -> Result<Vec<u8>, String> {
        let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
        let fd = match openat(Some(dir.as_raw_fd()), name, flags, Mode::empty()) {
            Ok(fd) => Fd(fd),
            Err(Errno::ENOENT) => return Err(format!("{name} is missing")),
            Err(Errno::ELOOP) => return Err(format!("{name} is not a regular file")),
            Err(e) => return Err(format!("{name} cannot be read: {e}")),
        };
        let stat = fstat(fd.0).map_err(|e| format!("{name} cannot be read: {e}"))?;
        if kind(&stat) != SFlag::S_IFREG {
            return Err(format!("{name} is not a regular file"));
        }
        let mut bytes = Vec::new();
        let mut buf = [0_u8; 16 * 1024];
        loop {
            let n = nix::unistd::read(fd.0, &mut buf)
                .map_err(|e| format!("{name} cannot be read: {e}"))?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            if bytes.len() > max {
                return Err(format!("{name} is too large"));
            }
        }
        Ok(bytes)
    }

    /// Remove `name` in `parent`: a file or link unlinked, a folder emptied
    /// the same way and removed.
    pub(super) fn remove(parent: &Dir, name: &str) -> Result<(), String> {
        let stat = fstatat(
            Some(parent.as_raw_fd()),
            name,
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .map_err(|e| match e {
            Errno::ENOENT => format!("there is no operation `{name}`"),
            e => format!("{name}: {e}"),
        })?;
        if kind(&stat) != SFlag::S_IFDIR {
            return unlinkat(Some(parent.as_raw_fd()), name, UnlinkatFlags::NoRemoveDir)
                .map_err(|e| format!("cannot remove {name}: {e}"));
        }
        let mut dir = sub(parent, name)?.ok_or_else(|| format!("{name} went away"))?;
        let mut entries: Vec<String> = Vec::new();
        for entry in dir.iter().flatten() {
            if let Ok(n) = entry.file_name().to_str()
                && n != "."
                && n != ".."
            {
                entries.push(n.to_owned());
            }
        }
        for entry in entries {
            remove(&dir, &entry)?;
        }
        unlinkat(Some(parent.as_raw_fd()), name, UnlinkatFlags::RemoveDir)
            .map_err(|e| format!("cannot remove {name}: {e}"))
    }

    /// A new file `tmp` in `dir` holding `bytes`, renamed over `name`.
    pub(super) fn write_new(dir: &Dir, tmp: &str, name: &str, bytes: &[u8]) -> Result<(), String> {
        let flags =
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let fd = Fd(openat(
            Some(dir.as_raw_fd()),
            tmp,
            flags,
            Mode::from_bits_truncate(0o644),
        )
        .map_err(|e| format!("cannot write {name}: {e}"))?);
        // The file just made, through its own descriptor.
        let written = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/dev/fd/{}", fd.0))
            .and_then(|mut file| {
                file.write_all(bytes)?;
                file.flush()
            });
        drop(fd);
        if let Err(e) = written {
            let _ = unlinkat(Some(dir.as_raw_fd()), tmp, UnlinkatFlags::NoRemoveDir);
            return Err(format!("cannot write {name}: {e}"));
        }
        renameat(Some(dir.as_raw_fd()), tmp, Some(dir.as_raw_fd()), name)
            .map_err(|e| format!("cannot write {name}: {e}"))
    }
}

fn git(project: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\n')
            .to_owned()
    })
}

/// Keep `.connecto/ops/` out of git: append the project-scoped pattern to
/// the file `git rev-parse --git-path info/exclude` names, once (as the
/// task board does, ADR 0029). Nothing happens outside a git repository.
/// Returns whether the pattern is there now.
pub fn exclude_from_git(project: &Path) -> bool {
    let Some(prefix) = git(project, &["rev-parse", "--show-prefix"]) else {
        return false;
    };
    let Some(exclude) = git(project, &["rev-parse", "--git-path", "info/exclude"]) else {
        return false;
    };
    let exclude = {
        let path = PathBuf::from(&exclude);
        if path.is_absolute() {
            path
        } else {
            project.join(path)
        }
    };
    let pattern = format!("/{prefix}{OPS_DIR}/");
    // An exclude file that exists but cannot be read is left as it is.
    let current = match fs::read_to_string(&exclude) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return false,
    };
    if current.lines().any(|line| line.trim() == pattern) {
        return true;
    }
    if let Some(parent) = exclude.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut text = current;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("# Connecto project operations (kept on this machine)\n");
    text.push_str(&pattern);
    text.push('\n');
    fs::write(&exclude, text).is_ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const SPEC: &str = r#"{"version":1,"name":"Tests","description":"Runs the tests.","phrases":["run the tests"],"kind":"script","danger":"none"}"#;

    pub(crate) fn write_op(project: &Path, slug: &str, spec: &str, body: &str) {
        let dir = ops_dir(project).join(slug);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("op.json"), spec).unwrap();
        fs::write(dir.join("run.sh"), body).unwrap();
    }

    #[test]
    fn scan_reads_every_folder_and_skips_others() {
        let dir = tempfile::tempdir().unwrap();
        write_op(dir.path(), "tests", SPEC, "cargo test\n");
        write_op(dir.path(), "broken", "{}", "x\n");
        fs::create_dir_all(ops_dir(dir.path()).join(DISCOVERY_DIR)).unwrap();
        let found = scan(dir.path());
        assert_eq!(
            found.iter().map(|f| f.slug.as_str()).collect::<Vec<_>>(),
            ["broken", "tests"]
        );
        assert!(found[0].loaded.is_err());
        let ok = found[1].loaded.as_ref().unwrap();
        assert_eq!(ok.body, "cargo test\n");
        assert_eq!(ok.hash.len(), 64);
    }

    #[test]
    fn any_change_changes_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        write_op(dir.path(), "tests", SPEC, "cargo test\n");
        let first = load(dir.path(), "tests").unwrap().hash;
        write_op(dir.path(), "tests", SPEC, "cargo test --all\n");
        let second = load(dir.path(), "tests").unwrap().hash;
        assert_ne!(first, second);
        // Moving bytes between the two files is a change too.
        assert_ne!(hash(b"ab", b"c"), hash(b"a", b"bc"));
    }

    #[cfg(unix)]
    #[test]
    fn links_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write_op(dir.path(), "tests", SPEC, "cargo test\n");
        let script = ops_dir(dir.path()).join("tests").join("run.sh");
        fs::remove_file(&script).unwrap();
        let elsewhere = dir.path().join("elsewhere.sh");
        fs::write(&elsewhere, "echo\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &script).unwrap();
        assert!(load(dir.path(), "tests").unwrap_err().contains("regular"));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_ops_folder_is_never_read_or_deleted_through() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        write_op(&outside, "tests", SPEC, "cargo test\n");
        let project = dir.path().join("project");
        fs::create_dir_all(project.join(".connecto")).unwrap();
        std::os::unix::fs::symlink(ops_dir(&outside), ops_dir(&project)).unwrap();
        assert!(scan(&project).is_empty());
        assert!(load(&project, "tests").is_err());
        assert!(delete(&project, "tests").is_err());
        assert!(
            ops_dir(&outside).join("tests").is_dir(),
            "nothing outside was removed"
        );
        assert!(prepare_ops_dir(&project).is_err());
        // A linked .connecto too.
        let other = dir.path().join("other");
        fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(outside.join(".connecto"), other.join(".connecto")).unwrap();
        assert!(scan(&other).is_empty());
        assert!(delete(&other, "tests").is_err());
    }

    #[test]
    fn delete_removes_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        write_op(dir.path(), "tests", SPEC, "cargo test\n");
        delete(dir.path(), "tests").unwrap();
        assert!(scan(dir.path()).is_empty());
        assert!(delete(dir.path(), "../x").is_err());
    }

    #[test]
    fn the_folder_is_kept_out_of_git_once() {
        let dir = tempfile::tempdir().unwrap();
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return; // no git here
        }
        let sub = dir.path().join("app");
        fs::create_dir_all(&sub).unwrap();
        assert!(exclude_from_git(&sub));
        assert!(exclude_from_git(&sub));
        let text = fs::read_to_string(dir.path().join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches("/app/.connecto/ops/").count(), 1, "{text}");
        assert!(!exclude_from_git(
            &tempfile::tempdir().unwrap().path().join("none")
        ));
    }
}
