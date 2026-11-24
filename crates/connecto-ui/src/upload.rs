//! Files dropped on, files copied in Finder and pasted into (once confirmed,
//! see `clipboard_files`), and images pasted into a remote host's terminal. The
//! host cannot read this computer's paths, so the files are sent to it first
//! (it keeps them in the terminal's project's attachments folder) and the
//! paths it saved them at are typed, as a dropped file's local path is on
//! this computer's terminals.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use connecto_core::TerminalsControl;
use connecto_core::terminals::{MAX_UPLOAD_BYTES, MAX_UPLOAD_FILES};
use egui::ColorImage;

/// What goes to the host.
pub(crate) enum Upload {
    /// Files dropped on the terminal, in drop order, or copied in Finder and
    /// pasted on it, in pasteboard order.
    Files(Vec<PathBuf>),
    /// An image pasted on it, sent as a PNG.
    Image(Arc<ColorImage>),
}

/// Why a drop of `count` files is refused before anything is read.
pub(crate) fn too_many(count: usize) -> Option<String> {
    (count > MAX_UPLOAD_FILES).then(|| {
        format!("At most {MAX_UPLOAD_FILES} files can be sent to a host's terminal at once.")
    })
}

fn limit_mib() -> usize {
    MAX_UPLOAD_BYTES / (1024 * 1024)
}

/// The most bytes of a name the host is told: well inside the wire's
/// string limit, and the host keeps fewer.
const MAX_NAME_BYTES: usize = 200;

/// The name the host is told: the file's own, with nothing it would refuse,
/// shortened to [`MAX_NAME_BYTES`] keeping its extension.
fn sent_name(path: &Path) -> Option<String> {
    let name: String = path
        .file_name()?
        .to_string_lossy()
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\') {
                '_'
            } else {
                ch
            }
        })
        .collect();
    if name.len() <= MAX_NAME_BYTES {
        return Some(name);
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, format!(".{ext}")),
        _ => (name.as_str(), String::new()),
    };
    let mut short = String::new();
    for ch in stem.chars() {
        if short.len() + ch.len_utf8() + ext.len() > MAX_NAME_BYTES {
            break;
        }
        short.push(ch);
    }
    short.push_str(&ext);
    Some(short)
}

/// A dropped file that passed the checks: the file itself, open, the name
/// the host is told, and the name a refusal shows.
#[derive(Debug)]
struct Dropped {
    file: std::fs::File,
    name: String,
    shown: String,
}

fn too_large(shown: &str) -> String {
    format!(
        "{shown} is larger than the {} MiB a host's terminal takes.",
        limit_mib()
    )
}

/// The dropped files, in drop order, each opened and checked on its open
/// handle, or why none is sent: too many, a folder, a file too large, or
/// one that cannot be opened. Every refusal comes before any file is read
/// or sent, and what is sent later is read from these handles: the files
/// checked, even if a path is replaced while earlier files go.
fn check_dropped(paths: &[PathBuf]) -> Result<Vec<Dropped>, String> {
    if let Some(why) = too_many(paths.len()) {
        return Err(why);
    }
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let shown = path.file_name().map_or_else(
            || path.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let unreadable = || format!("{shown} could not be read.");
        // The path's metadata first: a FIFO is refused without being opened
        // (opening one waits for a writer).
        let meta = std::fs::metadata(path).map_err(|_| unreadable())?;
        if meta.is_dir() {
            return Err(format!(
                "{shown} is a folder: only files can be sent to a host's terminal."
            ));
        }
        if !meta.is_file() {
            return Err(format!(
                "{shown} is not a regular file: only files can be sent to a host's terminal."
            ));
        }
        let name = sent_name(path).ok_or_else(unreadable)?;
        if meta.len() > MAX_UPLOAD_BYTES as u64 {
            return Err(too_large(&shown));
        }
        // Then the file itself, checked again on its handle: still a regular
        // file within the limit, and (on Unix) the one the path named.
        let file = open_without_waiting(path).map_err(|_| unreadable())?;
        let held = file.metadata().map_err(|_| unreadable())?;
        #[cfg(unix)]
        let same = {
            use std::os::unix::fs::MetadataExt;
            held.dev() == meta.dev() && held.ino() == meta.ino()
        };
        #[cfg(not(unix))]
        let same = true;
        if !held.is_file() || !same {
            return Err(unreadable());
        }
        if held.len() > MAX_UPLOAD_BYTES as u64 {
            return Err(too_large(&shown));
        }
        files.push(Dropped { file, name, shown });
    }
    Ok(files)
}

/// `path` opened for reading. On Unix without waiting: a FIFO put in a
/// checked file's place opens at once (it is then refused on its handle)
/// instead of waiting for a writer. For a regular file the flag changes
/// nothing. The flag's value is the platform's own (no `libc` here).
fn open_without_waiting(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const O_NONBLOCK: i32 = 0o4000;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        const O_NONBLOCK: i32 = 0x0004; // macOS, iOS and the BSDs.
        options.custom_flags(O_NONBLOCK);
    }
    options.open(path)
}

/// `file`'s bytes, read from its open handle when its turn to be sent
/// comes, so one file at a time is held in memory (up to
/// [`MAX_UPLOAD_BYTES`]), not the whole drop.
fn read_dropped(file: &Dropped) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    (&file.file)
        .take(MAX_UPLOAD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| format!("{} could not be read.", file.shown))?;
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(too_large(&file.shown));
    }
    Ok(bytes)
}

/// Sends `upload` to the host of terminal `terminal`: the host's paths, in
/// order, once every file has arrived, or why not (then nothing is typed).
pub(crate) fn send(
    terminals: &dyn TerminalsControl,
    terminal: &str,
    upload: Upload,
) -> Result<Vec<String>, String> {
    match upload {
        Upload::Files(paths) => check_dropped(&paths)?
            .iter()
            .map(|file| terminals.upload_file(terminal, &file.name, &read_dropped(file)?))
            .collect(),
        Upload::Image(image) => {
            let bytes = crate::pasted::png(&image)?;
            if bytes.len() > MAX_UPLOAD_BYTES {
                return Err(format!(
                    "The pasted image is larger than the {} MiB a host's terminal takes.",
                    limit_mib()
                ));
            }
            let name = crate::pasted::name(chrono::Local::now());
            Ok(vec![terminals.upload_file(terminal, &name, &bytes)?])
        }
    }
}

/// [`send`] off the UI thread. `done` gets the outcome, then the window is
/// asked to draw again.
pub(crate) fn send_in_background(
    terminals: Arc<dyn TerminalsControl>,
    terminal: String,
    upload: Upload,
    ctx: egui::Context,
    done: impl FnOnce(Result<Vec<String>, String>) + Send + 'static,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("terminal-upload".into())
        .spawn(move || {
            done(send(&*terminals, &terminal, upload));
            ctx.request_repaint();
        })
        .map(drop)
        .map_err(|_| "The file could not be sent to the host.".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn dropped_files_are_read_in_order_and_a_folder_or_too_many_refuses_all() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a b.txt");
        let b = dir.path().join("b.png");
        std::fs::write(&a, b"first").unwrap();
        std::fs::write(&b, b"second").unwrap();
        let read: Vec<_> = check_dropped(&[b.clone(), a.clone()])
            .unwrap()
            .iter()
            .map(|file| (file.name.clone(), read_dropped(file).unwrap()))
            .collect();
        assert_eq!(
            read,
            [
                ("b.png".to_owned(), b"second".to_vec()),
                ("a b.txt".to_owned(), b"first".to_vec())
            ]
        );
        let folder = check_dropped(&[a.clone(), dir.path().to_path_buf()]).unwrap_err();
        assert!(folder.contains("folder"), "{folder}");
        let many = vec![a.clone(); MAX_UPLOAD_FILES + 1];
        assert!(check_dropped(&many).unwrap_err().contains("At most"));
        let missing = check_dropped(&[dir.path().join("gone.txt")]).unwrap_err();
        assert!(missing.contains("gone.txt"), "{missing}");
    }

    #[test]
    fn a_long_name_is_shortened_on_a_character_boundary_keeping_its_extension() {
        let long = format!("{}.png", "é".repeat(300));
        let name = sent_name(Path::new(&long)).unwrap();
        assert!(name.len() <= MAX_NAME_BYTES, "{}", name.len());
        assert!(name.ends_with("é.png"), "{name}");
        // A backslash is a separator on Windows, part of a name elsewhere.
        #[cfg(not(windows))]
        assert_eq!(sent_name(Path::new("/x/a\\b.txt")).unwrap(), "a_b.txt");
    }

    /// A later file of a drop replaced while earlier ones are sent: what
    /// goes is the file that was checked, not what its path now names.
    #[test]
    fn a_dropped_file_is_sent_as_checked_even_if_its_path_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, b"first").unwrap();
        std::fs::write(&b, b"checked").unwrap();
        let files = check_dropped(&[a, b.clone()]).unwrap();
        std::fs::remove_file(&b).unwrap();
        std::fs::write(&b, b"swapped in").unwrap();
        assert_eq!(read_dropped(&files[0]).unwrap(), b"first");
        assert_eq!(read_dropped(&files[1]).unwrap(), b"checked");
    }

    /// One file of a drop that cannot be opened refuses the whole drop,
    /// before any file is read or sent.
    #[cfg(unix)]
    #[test]
    fn a_drop_with_a_file_that_cannot_be_opened_is_refused_whole() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let locked = dir.path().join("locked.txt");
        std::fs::write(&a, b"first").unwrap();
        std::fs::write(&locked, b"secret").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&locked).is_ok() {
            return; // Run as root: nothing can be locked.
        }
        let why = check_dropped(&[a, locked]).unwrap_err();
        assert_eq!(why, "locked.txt could not be read.");
    }

    /// A FIFO put where a checked file was opens at once, with no writer,
    /// and is no regular file on its handle: the drop is refused, not hung.
    #[cfg(unix)]
    #[test]
    fn a_fifo_swapped_in_opens_without_waiting_and_is_refused_on_its_handle() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let held = open_without_waiting(&fifo).and_then(|file| file.metadata());
            let _ = tx.send(held.map(|meta| meta.is_file()));
        });
        let opened = rx.recv_timeout(std::time::Duration::from_secs(2));
        assert!(opened.is_ok(), "the open waited for a writer");
        assert!(!opened.unwrap().unwrap(), "a FIFO is no regular file");
    }

    #[cfg(unix)]
    #[test]
    fn a_dropped_fifo_is_refused_without_opening_it() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let why = check_dropped(&[fifo]).unwrap_err();
        assert!(why.contains("not a regular file"), "{why}");
    }

    /// 100 MiB goes; a byte more is refused, before anything is read or
    /// sent. Sparse files: nothing near that size is written.
    #[test]
    fn a_file_over_the_limit_is_refused_before_it_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let sized = |name: &str, len: u64| {
            let path = dir.path().join(name);
            std::fs::File::create(&path).unwrap().set_len(len).unwrap();
            path
        };
        let big = sized("big.bin", MAX_UPLOAD_BYTES as u64 + 1);
        let why = check_dropped(&[big]).unwrap_err();
        assert!(why.contains("larger than the 100 MiB"), "{why}");
        let largest = sized("largest.bin", MAX_UPLOAD_BYTES as u64);
        let mid = sized("mid.bin", 50 * 1024 * 1024);
        let ok = check_dropped(&[largest, mid]).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].name, "largest.bin");
    }
}
