//! Finding where the user's own copy of an agent is.
//!
//! A program started from Finder has a minimal `PATH`, so the folders the
//! user's shell would search are asked of the login shell once (with a time
//! limit, and a command that is a constant: nothing from outside is put in it).
//! After those come the folders where the usual installers put things, then the
//! fixed ones. The order is fixed and a program found twice by different names
//! is listed once.
//!
//! What differs per platform (the login shell, the installers' folders, the
//! programs' file names) is in `discover/unix.rs` and `discover/windows.rs`.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as os;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

pub use os::login_path;

/// Ask for the login shell's `PATH` in the background, so that the first
/// lookup does not wait for it.
pub fn prefetch_login_path() {
    let _ = std::thread::Builder::new()
        .name("login-path".to_owned())
        .spawn(|| {
            let _ = login_path();
        });
}

/// Appends `dir` when it is absolute and not already in `out`.
fn push_absolute_unique(out: &mut Vec<PathBuf>, dir: PathBuf) {
    if dir.is_absolute() && !out.contains(&dir) {
        out.push(dir);
    }
}

/// The `PATH` that goes with the user's real home folder: the login shell's,
/// then this process's. Empty for any other `home` (a test, a trial build), so
/// that what is on the machine running it cannot change the answer.
#[must_use]
pub fn ambient_path(home: &Path) -> Vec<PathBuf> {
    let real = crate::platform::home_dir().is_some_and(|h| h == home);
    if !real {
        return Vec::new();
    }
    let mut dirs = login_path().to_vec();
    let process: OsString = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&process) {
        push_absolute_unique(&mut dirs, dir);
    }
    dirs
}

/// Whether `program` is a launcher script (its file starts with `#!`), such as
/// the npm shim of an agent. It runs only where the interpreter it names (`node`)
/// is on the `PATH`. On Windows a `.cmd`, `.bat` or `.ps1` file (npm's shims)
/// is one too; elsewhere only the `#!` counts.
#[must_use]
pub fn is_launcher(program: &Path) -> bool {
    let Ok(real) = std::fs::canonicalize(program) else {
        return false;
    };
    let mut head = [0_u8; 2];
    let shebang = std::fs::File::open(real)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok()
        && &head == b"#!";
    shebang || crate::platform::is_launcher_by_name(program)
}

/// The `PATH` to run `program` with. A launcher script gets the folders the
/// user's shell searches first (see [`ambient_path`]), so that its interpreter
/// is found even when Connecto was started from Finder. Then the system folders
/// (`/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`; on Windows those under
/// `%SystemRoot%`).
#[must_use]
pub fn run_path(program: &Path, home: &Path) -> OsString {
    let mut dirs = if is_launcher(program) {
        ambient_path(home)
    } else {
        Vec::new()
    };
    dirs.extend(os::system_dirs());
    std::env::join_paths(dirs).unwrap_or_else(|_| OsString::from(os::SYSTEM_PATH))
}

/// The subfolders of `dir`, newest version first.
fn newest_first(dir: &Path) -> Vec<PathBuf> {
    let key = |path: &Path| -> Vec<u64> {
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .trim_start_matches('v')
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|part| part.parse().ok())
            .collect()
    };
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    found.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| b.cmp(a)));
    found
}

/// Every folder to look in, in the order of priority.
#[must_use]
pub fn search_dirs(home: &Path, path: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let fixed = os::fixed_dirs(home);
    for dir in path
        .iter()
        .cloned()
        .chain(os::known_dirs(home))
        .chain(fixed)
    {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// The files called `name` in those folders, in order, each real program once.
/// On Windows `name.exe`, then `name.cmd`, in each folder.
#[must_use]
pub fn candidates(name: &str, home: &Path, path: &[PathBuf]) -> Vec<PathBuf> {
    let names = os::executable_names(name);
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut found = Vec::new();
    for dir in search_dirs(home, path) {
        for name in &names {
            let file = dir.join(name);
            if !file.is_file() {
                continue;
            }
            let real = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
            if !seen.contains(&real) {
                seen.push(real);
                found.push(file);
            }
        }
    }
    found
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn tree(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("connecto-discover-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::canonicalize(root).unwrap()
    }

    fn file(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn path_comes_first_then_installer_folders_then_the_fixed_ones_without_repeats() {
        let home = tree("order");
        let on_path = home.join("custom/bin");
        file(&on_path.join("codex"));
        file(&home.join(".bun/bin/codex"));
        file(&home.join(".local/bin/codex"));
        let found = candidates("codex", &home, &[on_path.clone(), on_path.clone()]);
        assert_eq!(
            found,
            [
                on_path.join("codex"),
                home.join(".bun/bin/codex"),
                home.join(".local/bin/codex"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn two_names_for_one_program_are_one_candidate() {
        let home = tree("dedupe");
        file(&home.join("real/codex"));
        std::fs::create_dir_all(home.join(".bun/bin")).unwrap();
        symlink(home.join("real/codex"), home.join(".bun/bin/codex")).unwrap();
        let found = candidates("codex", &home, &[home.join("real")]);
        assert_eq!(found, [home.join("real/codex")]);
    }

    #[cfg(unix)]
    #[test]
    fn node_version_managers_are_searched_newest_first() {
        let home = tree("nvm");
        file(&home.join(".nvm/versions/node/v18.9.0/bin/codex"));
        file(&home.join(".nvm/versions/node/v20.11.1/bin/codex"));
        file(&home.join(".nvm/versions/node/v9.0.0/bin/codex"));
        file(
            &home.join(
                "Library/Application Support/fnm/node-versions/v22.0.0/installation/bin/codex",
            ),
        );
        let found = candidates("codex", &home, &[]);
        assert_eq!(
            found,
            [
                home.join(".nvm/versions/node/v20.11.1/bin/codex"),
                home.join(".nvm/versions/node/v18.9.0/bin/codex"),
                home.join(".nvm/versions/node/v9.0.0/bin/codex"),
                home.join(
                    "Library/Application Support/fnm/node-versions/v22.0.0/installation/bin/codex"
                ),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn path_comes_first_then_installer_folders_then_the_fixed_ones_on_windows() {
        let home = tree("order-windows");
        let on_path = home.join(r"custom\bin");
        file(&on_path.join("codex.exe"));
        file(&home.join(r"AppData\Roaming\npm\codex.cmd"));
        file(&home.join(r".bun\bin\codex.exe"));
        file(&home.join(r".local\bin\codex.exe"));
        // Not a Windows program: never a candidate.
        file(&home.join(r".local\bin\codex"));
        let found = candidates("codex", &home, &[on_path.clone(), on_path.clone()]);
        assert_eq!(
            found,
            [
                on_path.join("codex.exe"),
                home.join(r"AppData\Roaming\npm\codex.cmd"),
                home.join(r".bun\bin\codex.exe"),
                home.join(r".local\bin\codex.exe"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_program_comes_before_its_shim_in_one_folder() {
        let home = tree("exe-cmd");
        let dir = home.join("bin");
        file(&dir.join("claude.cmd"));
        file(&dir.join("claude.exe"));
        assert_eq!(
            candidates("claude", &home, std::slice::from_ref(&dir)),
            [dir.join("claude.exe"), dir.join("claude.cmd")]
        );
    }

    #[cfg(windows)]
    #[test]
    fn node_version_managers_are_searched_newest_first_on_windows() {
        let home = tree("fnm-windows");
        let fnm = home.join(r"AppData\Roaming\fnm\node-versions");
        file(&fnm.join(r"v18.9.0\installation\codex.cmd"));
        file(&fnm.join(r"v20.11.1\installation\codex.cmd"));
        let nvm = home.join(r"AppData\Roaming\nvm");
        file(&nvm.join(r"v22.0.0\codex.cmd"));
        assert_eq!(
            candidates("codex", &home, &[]),
            [
                fnm.join(r"v20.11.1\installation\codex.cmd"),
                fnm.join(r"v18.9.0\installation\codex.cmd"),
                nvm.join(r"v22.0.0\codex.cmd"),
            ]
        );
        assert!(
            search_dirs(&home, &[])
                .iter()
                .all(|dir| !dir.components().any(|part| part.as_os_str() == "Library")),
        );
    }

    #[test]
    fn only_the_real_home_gets_the_machines_path() {
        assert!(ambient_path(Path::new("/nonexistent/fake-home")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_launcher_script_is_told_apart_from_a_program_and_gets_the_system_path() {
        let root = tree("launcher");
        let script = root.join("codex.js");
        std::fs::write(&script, "#!/usr/bin/env node\n").unwrap();
        let program = root.join("codex");
        std::fs::write(&program, [0x7f, b'E', b'L', b'F', 0]).unwrap();
        assert!(is_launcher(&script));
        assert!(!is_launcher(&program));
        assert!(!is_launcher(&root.join("missing")));
        // Not the real home, so no login-shell folders: the system's own only.
        let path = run_path(&script, Path::new("/nonexistent/fake-home"));
        assert_eq!(path.to_str(), Some("/usr/bin:/bin:/usr/sbin:/sbin"));
    }

    #[cfg(windows)]
    #[test]
    fn a_launcher_script_or_shim_is_told_apart_from_a_program_and_gets_the_system_path() {
        let root = tree("launcher");
        let script = root.join("codex.js");
        std::fs::write(&script, "#!/usr/bin/env node\n").unwrap();
        let shim = root.join("codex.cmd");
        std::fs::write(&shim, "@ECHO off\r\n").unwrap();
        let program = root.join("codex.exe");
        std::fs::write(&program, [b'M', b'Z', 0x90, 0]).unwrap();
        assert!(is_launcher(&script));
        assert!(is_launcher(&shim));
        assert!(!is_launcher(&program));
        assert!(!is_launcher(&root.join("missing.cmd")));
        // Not the real home, so no ambient folders: the system's own only.
        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let path = run_path(&shim, Path::new(r"C:\nonexistent\fake-home"));
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(dirs.first(), Some(&system.join("System32")));
        assert!(dirs.iter().all(|dir| dir.starts_with(&system)), "{dirs:?}");
        assert_eq!(
            run_path(&program, Path::new(r"C:\nonexistent\fake-home")),
            path
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_real_home_gets_the_process_path_on_windows() {
        let Some(home) = crate::platform::home_dir() else {
            return;
        };
        let process: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .filter(|dir| dir.is_absolute())
            .collect();
        let ambient = ambient_path(&home);
        assert!(process.iter().all(|dir| ambient.contains(dir)));
    }
}
