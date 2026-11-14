//! Windows side of [`crate::platform`].
//!
//! A real Windows path is a verbatim one (`\\?\C:\Users\...`): that is what
//! `fs::canonicalize` returns, and it is what a project's id is made from, so
//! one folder has one id however it was typed (`/` or `\`, any letter case, a
//! short `PROGRA~1` name, a junction). The prefix is only taken off where a
//! person sees the path, by the same rule `connecto-session` uses for a
//! terminal's working folder.

use std::fs;
use std::io;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// A project's path as the sidebar, the recents and the other parts of the
/// app are given it: a drive path short enough not to need the `\\?\` prefix
/// loses it (`C:\Users\me\work\app`). Anything else (a share, a long path, a
/// name such as `CON` that only means itself with the prefix) is shown as it
/// is, so it still names the same folder.
pub(crate) fn shown(path: &Path) -> String {
    let Some(text) = path.to_str() else {
        return path.display().to_string();
    };
    let Some(rest) = text.strip_prefix(r"\\?\") else {
        return text.to_owned();
    };
    let bytes = rest.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    // MAX_PATH less the terminating NUL.
    let plain = drive
        && rest.len() < 260
        && !rest.contains('/')
        && rest
            .trim_end_matches('\\')
            .split('\\')
            .skip(1)
            .all(plain_component);
    if plain {
        rest.to_owned()
    } else {
        text.to_owned()
    }
}

/// A path component that means the same with and without `\\?\`.
fn plain_component(name: &str) -> bool {
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let numbered = |prefix: &str| {
        stem.len() == 4
            && stem
                .get(..3)
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            && stem.as_bytes()[3].is_ascii_digit()
    };
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !RESERVED.iter().any(|word| stem.eq_ignore_ascii_case(word))
        && !numbered("COM")
        && !numbered("LPT")
}

/// Whether a folder saved in the settings file is an absolute path: a drive
/// and a root (`C:\x`, `C:/x`), a share (`\\server\share\x`) or a verbatim
/// path (`\\?\C:\x`). `\x` and `C:x` depend on the current drive or folder,
/// so they are not a folder choice.
pub(crate) fn absolute(text: &str) -> bool {
    Path::new(text).is_absolute()
}

/// Tries, and the pause before each retry, when the target is held open.
const TRIES: u32 = 5;
const RETRY_PAUSE: Duration = Duration::from_millis(20);

/// `ERROR_SHARING_VIOLATION` and `ERROR_LOCK_VIOLATION`.
const SHARING: [i32; 2] = [32, 33];

/// Put `staged` in place of `target` in one step. Windows replaces an
/// existing file, but refuses while another program (a virus scanner, the
/// search indexer, an editor) holds the target open without sharing it; such a
/// hold is usually brief, so the move is tried again a few times.
pub(crate) fn replace(staged: &Path, target: &Path) -> io::Result<()> {
    let mut attempt = 1;
    loop {
        match fs::rename(staged, target) {
            Ok(()) => return Ok(()),
            Err(err) if attempt < TRIES && held(&err) => {
                thread::sleep(RETRY_PAUSE * attempt);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

fn held(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::PermissionDenied
        || err
            .raw_os_error()
            .is_some_and(|code| SHARING.contains(&code))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_plain_drive_path_loses_the_verbatim_prefix_and_nothing_else_does() {
        let cases = [
            (r"\\?\C:\Users\u\work\p", r"C:\Users\u\work\p"),
            (r"\\?\d:\", r"d:\"),
            (r"\\?\C:\Users\u\my project", r"C:\Users\u\my project"),
            (
                r"\\?\C:\Users\Jürgen\Ünïcødé ✓",
                r"C:\Users\Jürgen\Ünïcødé ✓",
            ),
            (r"C:\already\plain", r"C:\already\plain"),
            (r"\\?\UNC\server\share\p", r"\\?\UNC\server\share\p"),
            (r"\\?\C:\work\CON", r"\\?\C:\work\CON"),
            (r"\\?\C:\work\nul.txt", r"\\?\C:\work\nul.txt"),
            (r"\\?\C:\work\com1", r"\\?\C:\work\com1"),
            (r"\\?\C:\work\dot.", r"\\?\C:\work\dot."),
            (r"\\?\C:\work\space ", r"\\?\C:\work\space "),
            (r"\\?\C:\work\..\x", r"\\?\C:\work\..\x"),
            (r"\\?\C:\work\common", r"C:\work\common"),
        ];
        for (from, to) in cases {
            assert_eq!(shown(Path::new(from)), to, "{from}");
        }
        let long = format!(r"\\?\C:\{}", "x".repeat(300));
        assert_eq!(shown(Path::new(&long)), long);
    }

    #[test]
    fn only_paths_that_do_not_depend_on_the_current_folder_are_absolute() {
        for yes in [r"C:\x", "C:/x", r"\\server\share\x", r"\\?\C:\x", r"d:\"] {
            assert!(absolute(yes), "{yes}");
        }
        for no in ["relative", r"\x", "/x", "C:x", ""] {
            assert!(!absolute(no), "{no}");
        }
    }

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-ws-replace-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Opens `path` the way a scanner may: sharing nothing, so it can be
    /// neither replaced nor deleted while held.
    fn hold(path: &Path) -> fs::File {
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap()
    }

    #[test]
    fn a_file_held_open_for_a_moment_is_still_replaced() {
        let dir = dir("brief");
        let (staged, target) = (dir.join("a.json.tmp"), dir.join("a.json"));
        fs::write(&target, "old").unwrap();
        fs::write(&staged, "new").unwrap();
        let held = hold(&target);
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            drop(held);
        });
        replace(&staged, &target).unwrap();
        release.join().unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(!staged.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_held_open_for_good_is_an_error_and_both_files_stay() {
        let dir = dir("held");
        let (staged, target) = (dir.join("a.json.tmp"), dir.join("a.json"));
        fs::write(&target, "old").unwrap();
        fs::write(&staged, "new").unwrap();
        let held = hold(&target);
        assert!(replace(&staged, &target).is_err());
        drop(held);
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        assert_eq!(fs::read_to_string(&staged).unwrap(), "new");
        let _ = fs::remove_dir_all(&dir);
    }
}
