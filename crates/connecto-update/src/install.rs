//! Getting a release onto this machine: download, check, unpack, and the
//! checks on what was unpacked. Then [`crate::helper`] swaps it in.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::feed::{Artifact, sha256_file};

/// The `User-Agent` the updater sends. The site's download count leaves these
/// requests out: an update is not a new download.
#[must_use]
pub fn user_agent(current: &str) -> String {
    format!("Connecto-Updater/{current}")
}

#[cfg(windows)]
fn system32(program: &str) -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join(program)
}

/// The system `curl`, by absolute path, with no console window on Windows.
fn curl() -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = Command::new(system32("curl.exe"));
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    {
        Command::new("/usr/bin/curl")
    }
}

fn run(command: &mut Command, what: &str) -> Result<std::process::Output, String> {
    let output = command
        .output()
        .map_err(|err| format!("{what} could not run: {err}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        let why = String::from_utf8_lossy(&output.stderr);
        let why = why.trim();
        Err(if why.is_empty() {
            format!("{what} failed")
        } else {
            format!("{what} failed: {}", why.lines().last().unwrap_or(why))
        })
    }
}

/// Fetch a small file (the feed) over https.
///
/// # Errors
/// When it cannot be fetched.
pub fn fetch(url: &str, user_agent: &str) -> Result<Vec<u8>, String> {
    let output = run(
        curl()
            .args(["-fsSL", "--proto", "=https", "--max-time", "30", "-A"])
            .arg(user_agent)
            .arg(url),
        "the update check",
    )?;
    Ok(output.stdout)
}

/// What a conditional fetch of the feed found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// The feed, and the `ETag` to ask with next time, if the server gave one.
    Fresh { body: Vec<u8>, etag: Option<String> },
    /// `304`: the feed is the one fetched with that `ETag`.
    NotModified,
}

/// Fetch the feed over https, with `If-None-Match` when an `ETag` from the
/// last fetch is known, so an unchanged feed costs a `304` and no body.
///
/// # Errors
/// When it cannot be fetched.
pub fn fetch_conditional(
    url: &str,
    user_agent: &str,
    etag: Option<&str>,
) -> Result<Fetched, String> {
    let mut command = curl();
    // `-i`: the headers come first on stdout, for the status and the ETag.
    command
        .args(["-fsSLi", "--proto", "=https", "--max-time", "30", "-A"])
        .arg(user_agent);
    if let Some(etag) = etag.filter(|tag| usable_etag(tag)) {
        command.arg("-H").arg(format!("If-None-Match: {etag}"));
    }
    let output = run(command.arg(url), "the update check")?;
    read_response(&output.stdout)
}

/// An `ETag` worth sending back: short, printable, one line.
fn usable_etag(tag: &str) -> bool {
    !tag.is_empty() && tag.len() <= 200 && tag.chars().all(|c| c.is_ascii_graphic() || c == ' ')
}

/// Split what `curl -i` printed: one header block per response (a redirect
/// adds one), then the body of the last.
pub(crate) fn read_response(out: &[u8]) -> Result<Fetched, String> {
    let mut rest = out;
    let mut last: Option<(u16, Option<String>)> = None;
    while rest.starts_with(b"HTTP/") {
        let end = rest
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|at| (at, 4))
            .or_else(|| rest.windows(2).position(|w| w == b"\n\n").map(|at| (at, 2)))
            .ok_or("the update check got a cut-off answer")?;
        let head = String::from_utf8_lossy(&rest[..end.0]);
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or("the update check got an answer it cannot read")?;
        let etag = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("etag")
                .then(|| value.trim().to_owned())
                .filter(|tag| usable_etag(tag))
        });
        last = Some((status, etag));
        rest = &rest[end.0 + end.1..];
    }
    match last {
        Some((304, _)) => Ok(Fetched::NotModified),
        Some((200, etag)) => Ok(Fetched::Fresh {
            body: rest.to_vec(),
            etag,
        }),
        Some((status, _)) => Err(format!("the update check got HTTP {status}")),
        None => Err("the update check got an answer it cannot read".to_owned()),
    }
}

/// Download `artifact` into `dir` and check its size and SHA-256 against the
/// signed feed. A file that does not match is removed.
///
/// # Errors
/// When the download fails or the file is not the one the feed names.
pub fn download(
    artifact: &Artifact,
    dir: &Path,
    name: &str,
    user_agent: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let path = dir.join(name);
    let part = dir.join(format!("{name}.part"));
    let _ = std::fs::remove_file(&part);
    run(
        curl()
            .args(["-fsSL", "--proto", "=https", "--max-time", "1800", "-A"])
            .arg(user_agent)
            .arg("-o")
            .arg(&part)
            .arg(&artifact.url),
        "the download",
    )?;
    let (sha256, size) = sha256_file(&part).map_err(|err| format!("the download: {err}"))?;
    if size != artifact.size || sha256 != artifact.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err("the download is not the file the update feed names".to_owned());
    }
    std::fs::rename(&part, &path).map_err(|err| format!("the download: {err}"))?;
    Ok(path)
}

/// Unpack `archive` into `dir` (a fresh folder each time).
///
/// # Errors
/// When it cannot be unpacked.
pub fn unpack(archive: &Path, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    #[cfg(target_os = "macos")]
    {
        run(
            Command::new("/usr/bin/ditto")
                .arg("-x")
                .arg("-k")
                .arg(archive)
                .arg(dir),
            "unpacking the update",
        )?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        run(
            Command::new(system32("tar.exe"))
                .creation_flags(CREATE_NO_WINDOW)
                .arg("-xf")
                .arg(archive)
                .arg("-C")
                .arg(dir),
            "unpacking the update",
        )?;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = archive;
        return Err("updates are not supported on this platform".to_owned());
    }
    Ok(())
}

// ---------- macOS ----------

/// The `.app` bundle a program inside it belongs to.
#[must_use]
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|dir| {
            dir.extension().is_some_and(|ext| ext == "app")
                && exe.starts_with(dir.join("Contents").join("MacOS"))
        })
        .map(Path::to_path_buf)
}

/// Why the bundle cannot replace itself, if it cannot.
#[must_use]
pub fn bundle_problem(bundle: &Path) -> Option<String> {
    let text = bundle.to_string_lossy();
    if text.contains("/AppTranslocation/") || text.starts_with("/Volumes/") {
        return Some("Connecto is running from the disk image or a quarantined copy. Move it to Applications to get updates.".to_owned());
    }
    let parent = bundle.parent()?;
    let probe = parent.join(format!(".connecto-update-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            None
        }
        Err(_) => Some(format!(
            "Connecto cannot write to {}, so it cannot update itself there.",
            parent.display()
        )),
    }
}

/// The `TeamIdentifier` line of `codesign -dv` output; `None` for an unsigned
/// or ad-hoc signed program.
#[must_use]
pub fn team_id_in(codesign_output: &str) -> Option<String> {
    codesign_output
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != "not set")
        .map(str::to_owned)
}

/// The Developer ID team that signed `app`.
#[must_use]
pub fn team_id(app: &Path) -> Option<String> {
    let output = Command::new("/usr/bin/codesign")
        .arg("-dv")
        .arg(app)
        .output()
        .ok()?;
    // codesign writes the details to stderr.
    team_id_in(&String::from_utf8_lossy(&output.stderr))
}

/// Check a new bundle before it may replace the running one: its signature is
/// intact, it is signed by `team` (the running app's), and Gatekeeper accepts
/// it (Developer ID and notarized).
///
/// # Errors
/// With the first check that fails.
pub fn check_mac_bundle(app: &Path, team: &str) -> Result<(), String> {
    if !app.join("Contents/MacOS/connecto-app").is_file() {
        return Err("the update does not hold the Connecto app".to_owned());
    }
    run(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app),
        "the update's signature check",
    )?;
    match team_id(app) {
        Some(id) if id == team => {}
        _ => {
            return Err(
                "the update is not signed by the same developer as this Connecto".to_owned(),
            );
        }
    }
    run(
        Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(app),
        "the update's Gatekeeper check (Developer ID and notarization)",
    )?;
    Ok(())
}

// ---------- Windows ----------

/// The fixed install folder, `%LOCALAPPDATA%\Programs\Connecto`.
#[must_use]
pub fn windows_install_dir(local_app_data: Option<&Path>, home: &Path) -> PathBuf {
    local_app_data
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join("AppData").join("Local"))
        .join("Programs")
        .join("Connecto")
}

/// The programs a Windows release must hold.
pub const WINDOWS_PROGRAMS: [&str; 3] = ["connecto-app.exe", "connecto-hook.exe", "connecto.exe"];

/// Check an unpacked Windows release.
///
/// # Errors
/// When a program is missing.
pub fn check_windows_folder(dir: &Path) -> Result<(), String> {
    for program in WINDOWS_PROGRAMS {
        if !dir.join(program).is_file() {
            return Err(format!("the update does not hold {program}"));
        }
    }
    Ok(())
}

// ---------- the hand-over ----------

/// Write `script` and start it, detached from this process so it outlives it.
///
/// # Errors
/// When it cannot be written or started.
pub fn start_hand_over(script: &str, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    #[cfg(windows)]
    let path = dir.join("hand-over.ps1");
    #[cfg(not(windows))]
    let path = dir.join("hand-over.sh");
    std::fs::write(&path, script).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut command;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command = Command::new(system32(r"WindowsPowerShell\v1.0\powershell.exe"));
        command
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-WindowStyle",
                "Hidden",
                "-File",
            ])
            .arg(&path)
            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        command = Command::new("/bin/sh");
        command.arg(&path).process_group(0);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("the installer could not start: {err}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_conditional_fetch_reads_the_status_the_etag_and_the_body_after_redirects() {
        let out = b"HTTP/1.1 301 Moved\r\nLocation: https://x/update.json\r\n\r\nHTTP/2 200 \r\ncontent-type: application/json\r\nETag: \"abc\"\r\n\r\n{\"a\":1}";
        assert_eq!(
            read_response(out).unwrap(),
            Fetched::Fresh {
                body: b"{\"a\":1}".to_vec(),
                etag: Some("\"abc\"".to_owned())
            }
        );
        let out = b"HTTP/2 304 \r\netag: \"abc\"\r\n\r\n";
        assert_eq!(read_response(out).unwrap(), Fetched::NotModified);
        let out = b"HTTP/2 200 \r\n\r\nbody";
        assert_eq!(
            read_response(out).unwrap(),
            Fetched::Fresh {
                body: b"body".to_vec(),
                etag: None
            }
        );
        assert!(read_response(b"HTTP/2 200 \r\nno end").is_err());
        assert!(read_response(b"garbage").is_err());
        assert!(!usable_etag("a\r\nX-Evil: 1"));
    }

    #[test]
    fn the_bundle_is_found_from_the_program_inside_it() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/Connecto.app/Contents/MacOS/connecto-app"
            )),
            Some(PathBuf::from("/Applications/Connecto.app"))
        );
        assert_eq!(bundle_of(Path::new("/usr/local/bin/connecto-app")), None);
        assert_eq!(
            bundle_of(Path::new("/x/Connecto.app/Contents/Resources/connecto-app")),
            None
        );
    }

    #[test]
    fn a_translocated_or_disk_image_copy_cannot_update_itself() {
        assert!(
            bundle_problem(Path::new(
                "/private/var/folders/x/AppTranslocation/ABC/d/Connecto.app"
            ))
            .is_some()
        );
        assert!(bundle_problem(Path::new("/Volumes/Connecto 0.1.8/Connecto.app")).is_some());
        let dir =
            std::env::temp_dir().join(format!("connecto-update-bundle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(bundle_problem(&dir.join("Connecto.app")), None);
    }

    #[test]
    fn the_team_comes_from_codesign_and_ad_hoc_has_none() {
        let signed = "Executable=/A.app/Contents/MacOS/connecto-app\nIdentifier=studio.connecto.app\nTeamIdentifier=ABCDE12345\n";
        assert_eq!(team_id_in(signed).as_deref(), Some("ABCDE12345"));
        assert_eq!(
            team_id_in("Signature=adhoc\nTeamIdentifier=not set\n"),
            None
        );
        assert_eq!(team_id_in("code object is not signed at all"), None);
    }

    #[test]
    fn the_windows_install_folder_is_fixed() {
        assert_eq!(
            windows_install_dir(
                Some(Path::new(r"C:\Users\a\AppData\Local")),
                Path::new(r"C:\Users\a")
            ),
            Path::new(r"C:\Users\a\AppData\Local")
                .join("Programs")
                .join("Connecto")
        );
        assert_eq!(
            windows_install_dir(None, Path::new("/home/a")),
            Path::new("/home/a/AppData/Local/Programs/Connecto")
        );
    }

    #[test]
    fn the_updater_names_itself() {
        assert_eq!(user_agent("0.1.8"), "Connecto-Updater/0.1.8");
    }
}
