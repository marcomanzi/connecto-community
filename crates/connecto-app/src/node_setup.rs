//! Node.js for Pi, which is a Node.js program: Pi's installer needs `node`
//! 22.19 or newer and `npm`, and stops when there is none and nobody is there
//! to answer its question (Connecto runs it with no terminal).
//!
//! So Connecto looks first, says what is missing, and, when the person agrees,
//! installs Node.js the way Pi's own installers do when they are asked to: the
//! official build of Node.js 22 from `https://nodejs.org/dist/latest-v22.x`,
//! checked against the `SHASUMS256.txt` published beside it, unpacked in the
//! user's own folder (no password, no administrator), in the folder Pi's
//! installers use for it:
//!
//! - Windows: `%LOCALAPPDATA%\pi-node\current` (`node.exe` in it).
//! - macOS: `~/.local/share/pi-node/current/bin` (`current` a link to the
//!   version's folder).
//!
//! Pi's installer is then run with that folder first on its `PATH`
//! (`crate::platform::installer_command`); it sees that its `node` is that
//! one and writes Pi's launcher to use it, so Pi keeps it even where an older
//! Node.js comes first on the user's own `PATH`. The user's own Node.js is
//! left as it is.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::agent_install::{Failure, version_of};
use crate::platform::THIS_COMPUTER;

/// The oldest Node.js Pi runs on.
pub const NEEDED: (u32, u32, u32) = (22, 19, 0);

/// Where the official builds of Node.js 22 are, with their checksums.
const DIST: &str = "https://nodejs.org/dist/latest-v22.x";

/// How long each download may take.
const DOWNLOAD_LIMIT_SECS: &str = "600";

/// Whether the Node.js Pi's installer would use will do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeCheck {
    /// Found, and new enough.
    Ready,
    /// No `node` where Pi's installer looks.
    Missing,
    /// Found, too old: what it says its version is.
    TooOld(String),
}

/// What `node --version` printed, as numbers (`v20.17.0` → `(20, 17, 0)`).
#[must_use]
pub fn parse_version(printed: &str) -> Option<(u32, u32, u32)> {
    let mut parts = printed.trim().trim_start_matches('v').splitn(3, '.');
    let mut next = || -> Option<u32> {
        let part = parts.next()?;
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    Some((next()?, next()?, next().unwrap_or(0)))
}

/// What a `node --version` line means for Pi.
#[must_use]
pub fn judge(printed: Option<&str>) -> NodeCheck {
    match printed {
        None => NodeCheck::Missing,
        Some(line) => match parse_version(line) {
            Some(version) if version >= NEEDED => NodeCheck::Ready,
            _ => NodeCheck::TooOld(line.trim().to_owned()),
        },
    }
}

/// The folders Pi's installer looks for `node` in, in its order: the one
/// Connecto installs Node.js in, then those of the user's own search path.
#[must_use]
pub fn search_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![crate::platform::standalone_node_dir(home)];
    for dir in std::env::split_paths(&connecto_store::launcher_path(home)) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// The `node` Pi's installer would run: the first in [`search_dirs`].
#[must_use]
pub fn find(home: &Path) -> Option<PathBuf> {
    search_dirs(home)
        .into_iter()
        .map(|dir| dir.join(crate::platform::NODE_PROGRAM))
        .find(|file| file.is_file())
}

/// Whether the Node.js Pi's installer would use will do. Runs it once.
#[must_use]
pub fn check(home: &Path) -> NodeCheck {
    let printed = find(home).and_then(|node| version_of(&node, home));
    judge(printed.as_deref())
}

/// What is said, and asked, when Pi cannot be installed yet; `None` when it can.
#[must_use]
pub fn prompt(check: &NodeCheck) -> Option<String> {
    let (major, minor, _) = NEEDED;
    let have = match check {
        NodeCheck::Ready => return None,
        NodeCheck::Missing => format!("Node.js is not on {THIS_COMPUTER}"),
        NodeCheck::TooOld(version) => format!("{THIS_COMPUTER} has Node.js {version}"),
    };
    Some(format!(
        "Pi runs on Node.js {major}.{minor} or newer, and {have}. Connecto can install the official Node.js {major} from nodejs.org for Pi, in your own folder: it needs no password, and any Node.js you already have stays as it is. Install it and go on with Pi?"
    ))
}

/// Said when the person does not want Node.js installed: Pi is not.
#[must_use]
pub fn declined() -> String {
    let (major, minor, _) = NEEDED;
    format!(
        "Pi was not installed: it needs Node.js {major}.{minor} or newer. Install Node.js yourself (nodejs.org), or choose Install and activate again to let Connecto do it."
    )
}

/// The archive for this computer in `SHASUMS256.txt`, and its checksum.
#[must_use]
pub fn pick_archive(shasums: &str, suffix: &str) -> Option<(String, String)> {
    shasums.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        let (hash, file) = (words.next()?, words.next()?);
        let plain = file.starts_with("node-v")
            && file.ends_with(suffix)
            && !file.contains(['/', '\\'])
            && hash.len() == 64
            && hash.chars().all(|c| c.is_ascii_hexdigit());
        plain.then(|| (file.to_owned(), hash.to_ascii_lowercase()))
    })
}

/// Download `url` to `to` over HTTPS only, as the agents' installers are.
fn download(url: &str, to: &Path) -> Result<(), Failure> {
    let output = crate::platform::curl()
        .args([
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--connect-timeout",
            "20",
            "--max-time",
            DOWNLOAD_LIMIT_SECS,
            "--output",
        ])
        .arg(to)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| Failure {
            what: "Connecto could not start the download of Node.js.".to_owned(),
            next: "Try again.".to_owned(),
            details: err.to_string(),
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(Failure {
        what: format!(
            "Node.js could not be downloaded from nodejs.org. Is {THIS_COMPUTER} online?"
        ),
        next: "Check your internet connection, then try again.".to_owned(),
        details: format!(
            "curl exit {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    })
}

/// Install the official Node.js 22 for Pi, in `work` while it is fetched,
/// telling `say` which step it is on. Afterwards it must be the `node` Pi's
/// installer finds, and new enough.
///
/// # Errors
/// A [`Failure`], worded for a person.
pub fn install(home: &Path, work: &Path, say: &dyn Fn(&str)) -> Result<(), Failure> {
    let failed = |what: &str, details: String| Failure {
        what: what.to_owned(),
        next: "Try again, or install Node.js yourself from nodejs.org.".to_owned(),
        details,
    };
    let Some(suffix) = crate::platform::node_archive_suffix() else {
        return Err(failed(
            "There is no official Node.js build for this computer.",
            String::new(),
        ));
    };
    let _ = fs::remove_dir_all(work);
    fs::create_dir_all(work)
        .map_err(|e| failed("Connecto could not prepare a folder.", e.to_string()))?;
    crate::platform::make_private(work);
    let outcome = (|| {
        say("Downloading Node.js from nodejs.org…");
        let sums = work.join("SHASUMS256.txt");
        download(&format!("{DIST}/SHASUMS256.txt"), &sums)?;
        let text = fs::read_to_string(&sums).map_err(|e| {
            failed(
                "The list of Node.js builds could not be read.",
                e.to_string(),
            )
        })?;
        let (file, expected) = pick_archive(&text, suffix).ok_or_else(|| {
            failed(
                "There is no official Node.js build for this computer.",
                format!("no {suffix} in {DIST}/SHASUMS256.txt"),
            )
        })?;
        let archive = work.join(&file);
        download(&format!("{DIST}/{file}"), &archive)?;
        say("Checking the download…");
        let actual = connecto_store::sha256_file(&archive)
            .map_err(|e| failed("The Node.js download could not be read.", e.to_string()))?;
        if actual != expected {
            return Err(failed(
                "The Node.js download is not the one nodejs.org published.",
                format!("{file}: sha256 {actual}, expected {expected}"),
            ));
        }
        say("Installing Node.js…");
        crate::platform::unpack_node(&archive, &file, work, home)
            .map_err(|details| failed("Node.js could not be unpacked.", details))
    })();
    let _ = fs::remove_dir_all(work);
    outcome?;
    match check(home) {
        NodeCheck::Ready => Ok(()),
        other => Err(failed(
            "Node.js was installed, but it does not run.",
            format!(
                "{other:?} at {}",
                crate::platform::standalone_node_dir(home).display()
            ),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_and_judged_against_what_pi_needs() {
        assert_eq!(parse_version("v20.17.0"), Some((20, 17, 0)));
        assert_eq!(parse_version("v22.19.0\n"), Some((22, 19, 0)));
        assert_eq!(parse_version("v24.1"), Some((24, 1, 0)));
        assert_eq!(parse_version("node"), None);
        assert_eq!(judge(None), NodeCheck::Missing);
        assert_eq!(
            judge(Some("v20.17.0")),
            NodeCheck::TooOld("v20.17.0".into())
        );
        assert_eq!(
            judge(Some("v22.18.9")),
            NodeCheck::TooOld("v22.18.9".into())
        );
        assert_eq!(judge(Some("v22.19.0")), NodeCheck::Ready);
        assert_eq!(judge(Some("v24.0.0")), NodeCheck::Ready);
        assert_eq!(judge(Some("garbage")), NodeCheck::TooOld("garbage".into()));
    }

    #[test]
    fn the_prompt_says_what_is_missing_and_asks() {
        assert_eq!(prompt(&NodeCheck::Ready), None);
        let missing = prompt(&NodeCheck::Missing).unwrap();
        assert!(missing.contains("22.19") && missing.contains("is not on"));
        assert!(missing.contains("nodejs.org") && missing.ends_with('?'));
        let old = prompt(&NodeCheck::TooOld("v20.17.0".into())).unwrap();
        assert!(old.contains("has Node.js v20.17.0"), "{old}");
        assert!(declined().contains("Pi was not installed"));
    }

    #[test]
    fn the_archive_is_picked_from_the_published_checksums() {
        let hash = "a".repeat(64);
        let other = "b".repeat(64);
        let sums = format!(
            "{other}  node-v22.21.0-darwin-arm64.tar.gz\n{hash}  node-v22.21.0-win-x64.zip\n{other}  node-v22.21.0-win-x64.7z\n"
        );
        assert_eq!(
            pick_archive(&sums, "-win-x64.zip"),
            Some(("node-v22.21.0-win-x64.zip".into(), hash.clone()))
        );
        assert_eq!(
            pick_archive(&sums, "-darwin-arm64.tar.gz").map(|(f, _)| f),
            Some("node-v22.21.0-darwin-arm64.tar.gz".into())
        );
        assert_eq!(pick_archive(&sums, "-linux-x64.tar.gz"), None);
        // A name that would leave the folder, or a malformed checksum, is not taken.
        let bad = format!("{hash}  ../node-v1-win-x64.zip\nxyz  node-v1-win-x64.zip\n");
        assert_eq!(pick_archive(&bad, "-win-x64.zip"), None);
    }

    #[test]
    fn connectos_node_folder_is_looked_in_first() {
        let home = Path::new(if cfg!(windows) {
            r"C:\nobody"
        } else {
            "/nobody"
        });
        let dirs = search_dirs(home);
        assert_eq!(dirs[0], crate::platform::standalone_node_dir(home));
        // Another home than the real one sees nothing of this machine's PATH
        // (only the system folders, which on Windows hold no Node.js).
        if cfg!(windows) {
            assert!(find(home).is_none());
            assert_eq!(check(home), NodeCheck::Missing);
        }
    }
}
