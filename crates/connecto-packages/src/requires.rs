//! Looking for the tools packages need (ADR 0037): a program on `PATH` or in
//! the usual install folders, or a fixed path; then, when the package asks for
//! a minimum version, the found program run with its one version flag.
//! Nothing else is ever run here.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use connecto_core::{RequirementState, RequirementView};
use connecto_hook::requires::{Os, Requirement, version_in};

/// How long a program has to print its version.
const VERSION_WAIT: Duration = Duration::from_secs(15);
const MAX_OUTPUT: u64 = 64 * 1024;

/// Where a tool is looked for besides the folders named in `paths`.
#[derive(Debug, Clone)]
pub struct Search {
    pub os: Os,
    /// `PATH`, then folders installers put programs in that an app started
    /// from the Dock or Finder may not have on its `PATH`.
    pub dirs: Vec<PathBuf>,
    pub home: PathBuf,
    /// What a relative `PATH` entry is relative to (the app's working
    /// folder). A relative entry is never used as it is: the probe runs
    /// elsewhere, so it could start another program than the one checked.
    pub cwd: PathBuf,
}

impl Search {
    /// This machine's.
    #[must_use]
    pub fn here(home: &Path) -> Self {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        // A running app keeps the PATH it started with, and Windows
        // installers change the saved one (winget adds a package's folder
        // when it cannot make a link). Read that too, so a tool installed
        // since Connecto started is found when the user checks again.
        #[cfg(windows)]
        for dir in saved_windows_path() {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        let extra: &[&str] = match Os::CURRENT {
            Os::Macos => &["/opt/homebrew/bin", "/usr/local/bin", "~/.local/bin"],
            Os::Linux => &[
                "/usr/local/bin",
                "/usr/bin",
                "~/.local/bin",
                "/var/lib/flatpak/exports/bin",
                "~/.local/share/flatpak/exports/bin",
                "/snap/bin",
            ],
            Os::Windows => &["%LOCALAPPDATA%\\Microsoft\\WinGet\\Links"],
        };
        for dir in extra {
            if let Some(dir) = expand(dir, Os::CURRENT, home, &|name| std::env::var(name).ok())
                && !dirs.contains(&dir)
            {
                dirs.push(dir);
            }
        }
        Self {
            os: Os::CURRENT,
            dirs,
            home: home.to_owned(),
            cwd: std::env::current_dir().unwrap_or_default(),
        }
    }
}

/// The machine's and the user's `Path` as saved in the registry.
#[cfg(windows)]
fn saved_windows_path() -> Vec<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let keys = [
        (
            HKEY_LOCAL_MACHINE,
            r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
        ),
        (HKEY_CURRENT_USER, "Environment"),
    ];
    let mut out = Vec::new();
    for (root, key) in keys {
        if let Ok(value) = RegKey::predef(root)
            .open_subkey(key)
            .and_then(|k| k.get_value::<String, _>("Path"))
        {
            out.extend(split_saved_path(&value, &|name| std::env::var(name).ok()));
        }
    }
    out
}

/// A `Path` as Windows saves it: `;` between folders, `%VAR%` not yet
/// replaced. A folder naming a variable that is not set is left out.
#[cfg_attr(not(windows), allow(dead_code))]
fn split_saved_path(value: &str, var: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let expand = |text: &str| -> Option<String> {
        let mut out = String::new();
        let mut rest = text;
        while let Some(start) = rest.find('%') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let end = after.find('%')?;
            out.push_str(&var(&after[..end])?);
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Some(out)
    };
    value
        .split(';')
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .filter_map(expand)
        .map(PathBuf::from)
        .collect()
}

/// `~/x` under `home`; on Windows `%VAR%\x` with the variable's value.
fn expand(
    path: &str,
    os: Os,
    home: &Path,
    var: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return Some(home.join(rest));
    }
    if os == Os::Windows
        && let Some(rest) = path.strip_prefix('%')
    {
        let (name, rest) = rest.split_once("%\\")?;
        return Some(PathBuf::from(var(name)?).join(rest));
    }
    Some(PathBuf::from(path))
}

fn runnable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Every place the tool is, best first: its programs on the search folders,
/// then its fixed paths for this system. Each is named by its real path
/// (links resolved), once. A place inside one of `untrusted` (Connecto Home,
/// where packages are kept; Connecto's data; the projects, where packages are
/// installed) is never one: a package cannot point detection at a program it
/// brought along.
#[must_use]
pub fn locate_all(req: &Requirement, search: &Search, untrusted: &[PathBuf]) -> Vec<PathBuf> {
    locate_with(req, search, untrusted, &|name| std::env::var(name).ok())
}

fn locate_with(
    req: &Requirement,
    search: &Search,
    untrusted: &[PathBuf],
    var: &dyn Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    let suffixes: &[&str] = if search.os == Os::Windows {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    // Every folder absolute, so what is checked is what runs.
    let dirs: Vec<PathBuf> = search
        .dirs
        .iter()
        .filter_map(|dir| {
            if dir.is_absolute() {
                Some(dir.clone())
            } else {
                search.cwd.is_absolute().then(|| search.cwd.join(dir))
            }
        })
        .collect();
    let mut places: Vec<PathBuf> = Vec::new();
    for program in &req.programs {
        for dir in &dirs {
            for suffix in suffixes {
                let candidate = dir.join(format!("{program}{suffix}"));
                if runnable(&candidate) {
                    places.push(candidate);
                }
            }
        }
    }
    places.extend(
        req.paths
            .get(search.os)
            .iter()
            .filter_map(|p| expand(p, search.os, &search.home, var))
            .filter(|p| p.exists()),
    );
    let untrusted: Vec<PathBuf> = untrusted
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let mut out: Vec<PathBuf> = Vec::new();
    for place in places {
        // Trusted by where it really is (links resolved; not found if that
        // cannot be told), but run as it was found: a launcher such as Snap's
        // tells its tools apart by the name it is started under.
        let Ok(real) = std::fs::canonicalize(&place) else {
            continue;
        };
        if untrusted.iter().any(|root| real.starts_with(root)) || out.contains(&place) {
            continue;
        }
        out.push(place);
    }
    out
}

/// The folder a version probe runs in: one no package can write to, so a
/// program that looks around its working folder finds nothing of a
/// package's there.
fn probe_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from("C:\\"), PathBuf::from)
    } else {
        PathBuf::from("/")
    }
}

/// Runs `program flag` with nothing on its input and returns what it printed,
/// or `None` if it could not start, failed, or did not finish in time.
#[must_use]
pub fn version_output(program: &Path, flag: &str) -> Option<String> {
    version_output_within(program, flag, VERSION_WAIT)
}

/// A private file a probe writes its output to, removed when dropped.
/// A file, not a pipe: nothing waits on it, so a helper the program leaves
/// running with its output open cannot hold anything here.
struct OutputFile {
    path: PathBuf,
    file: std::fs::File,
}

impl OutputFile {
    fn new() -> Option<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let path = std::env::temp_dir().join(format!(
            "connecto-version-probe-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = options.open(&path).ok()?;
        Some(Self { path, file })
    }

    /// The first [`MAX_OUTPUT`] bytes written.
    fn text(&self) -> String {
        let mut bytes = Vec::new();
        let mut file = &self.file;
        let _ = std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0));
        let _ = file.take(MAX_OUTPUT).read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Drop for OutputFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// End what the probe started, helpers too: on Unix the probe leads a
/// process group of its own.
fn end_probe(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.kill();
}

/// [`version_output`] with its own time limit.
fn version_output_within(program: &Path, flag: &str, wait: Duration) -> Option<String> {
    let deadline = Instant::now() + wait;
    let output = OutputFile::new()?;
    let mut command = Command::new(program);
    command
        .arg(flag)
        .current_dir(probe_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.file.try_clone().ok()?))
        .stderr(Stdio::from(output.file.try_clone().ok()?));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn().ok()?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                end_probe(&mut child);
                let _ = child.wait();
                return None;
            }
        }
    };
    // Helpers it left behind go too: a version probe has nothing to keep.
    end_probe(&mut child);
    // A program that failed says nothing to trust about its version.
    status.success().then(|| output.text())
}

/// Whether the tool is there, in a version that will do. `run` prints a
/// found program's version ([`version_output`] outside tests). When only
/// some versions will do, a place counts only once its version is read and
/// in range; every place is tried before the tool is said to be the wrong
/// version, or one whose version cannot be read.
pub fn detect(
    req: &Requirement,
    search: &Search,
    untrusted: &[PathBuf],
    run: &dyn Fn(&Path, &str) -> Option<String>,
) -> RequirementState {
    let places = locate_all(req, search, untrusted);
    let Some(first) = places.first() else {
        return RequirementState::Missing;
    };
    let Some(flag) = &req.version_flag else {
        return RequirementState::Found { version: None };
    };
    let version_of = |place: &Path| run(place, flag).and_then(|out| version_in(&out));
    if !req.wants_a_version() {
        return RequirementState::Found {
            version: version_of(first),
        };
    }
    let wanted = req.wanted_versions();
    let mut wrong = None;
    for place in &places {
        match version_of(place) {
            Some(version) if req.version_fits(&version) => {
                return RequirementState::Found {
                    version: Some(version),
                };
            }
            Some(version) => {
                wrong.get_or_insert(version);
            }
            None => {}
        }
    }
    match wrong {
        Some(version) => RequirementState::WrongVersion { version, wanted },
        None => RequirementState::NoVersion { wanted },
    }
}

/// The dialog's view of one tool on `os`.
#[must_use]
pub fn view(req: &Requirement, state: RequirementState, os: Os) -> RequirementView {
    RequirementView {
        id: req.id.clone(),
        name: req.name.clone(),
        why: req.why.clone(),
        state,
        install: req.install_on(os).map(str::to_owned),
        manual: req.manual.clone(),
        url: req.url.clone(),
        licence: req.licence,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mg-requires-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn program(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\necho 'Godot 4.7.2.stable'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn godot(min: Option<&str>) -> Requirement {
        Requirement {
            id: "godot".into(),
            name: "Godot".into(),
            programs: vec!["godot".into()],
            paths: connecto_hook::requires::PerOs {
                macos: vec!["~/Apps/Godot.app/Contents/MacOS/Godot".into()],
                linux: vec!["~/Apps/Godot.app/Contents/MacOS/Godot".into()],
                windows: vec!["~\\Apps\\Godot.app\\Contents\\MacOS\\Godot".into()],
            },
            version_flag: Some("--version".into()),
            min_version: min.map(str::to_owned),
            below_version: min.map(|_| "5".to_owned()),
            manual: "m".into(),
            ..Requirement::default()
        }
    }

    /// What winget writes when it cannot make a link: the package's folder,
    /// added to the user's saved `Path`.
    #[test]
    fn a_saved_windows_path_is_split_and_its_variables_filled_in() {
        let var =
            |name: &str| (name == "LOCALAPPDATA").then(|| r"C:\Users\u\AppData\Local".to_owned());
        let saved = r"C:\Windows;%LOCALAPPDATA%\Microsoft\WinGet\Packages\Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe\ffmpeg-9.1-full_build\bin; ;%NOPE%\x;C:\bad%";
        assert_eq!(
            split_saved_path(saved, &var),
            [
                PathBuf::from(r"C:\Windows"),
                PathBuf::from(
                    r"C:\Users\u\AppData\Local\Microsoft\WinGet\Packages\Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe\ffmpeg-9.1-full_build\bin"
                ),
            ]
        );
    }

    #[test]
    fn a_tool_is_found_on_the_search_folders_or_at_a_fixed_path() {
        let base = temp("find");
        let (bin, home) = (base.join("bin"), base.join("home"));
        std::fs::create_dir_all(&bin).unwrap();
        let search = Search {
            os: Os::CURRENT,
            cwd: PathBuf::from("/nonexistent-cwd"),
            dirs: vec![bin.clone()],
            home: home.clone(),
        };
        let ran = std::sync::atomic::AtomicBool::new(false);
        let never = |_: &Path, _: &str| -> Option<String> {
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
            None
        };
        let no_version = Requirement {
            version_flag: None,
            ..godot(None)
        };
        assert_eq!(
            detect(&no_version, &search, &[], &never),
            RequirementState::Missing
        );
        // A fixed path under the home folder.
        let app = home.join("Apps/Godot.app/Contents/MacOS");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("Godot"), "").unwrap();
        assert_eq!(
            detect(&no_version, &search, &[], &never),
            RequirementState::Found { version: None }
        );
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "nothing is run without a version flag"
        );
        std::fs::remove_dir_all(home.join("Apps")).unwrap();
        // A program on the search folders, run with its version flag only.
        let path = program(&bin, if cfg!(windows) { "godot.exe" } else { "godot" });
        let seen = std::sync::Mutex::new(Vec::new());
        let run = |p: &Path, flag: &str| {
            seen.lock().unwrap().push((p.to_owned(), flag.to_owned()));
            Some("Godot Engine v4.6.1.stable.official".to_owned())
        };
        let wanted = "4.7 or later, before 5".to_owned();
        assert_eq!(
            detect(&godot(Some("4.7")), &search, &[], &run),
            RequirementState::WrongVersion {
                version: "4.6.1".into(),
                wanted: wanted.clone(),
            }
        );
        assert_eq!(
            seen.lock().unwrap()[0],
            (path.clone(), "--version".to_owned())
        );
        let four = |_: &Path, _: &str| Some("4.7.2.stable".to_owned());
        assert_eq!(
            detect(&godot(Some("4.7")), &search, &[], &four),
            RequirementState::Found {
                version: Some("4.7.2".into())
            }
        );
        // Godot 5 is not Godot 4.
        let five = |_: &Path, _: &str| Some("5.0.stable".to_owned());
        assert_eq!(
            detect(&godot(Some("4.7")), &search, &[], &five),
            RequirementState::WrongVersion {
                version: "5.0".into(),
                wanted: wanted.clone(),
            }
        );
        // A version that cannot be read (the program failed, printed none)
        // is not one that will do.
        let silent = |_: &Path, _: &str| None;
        assert_eq!(
            detect(&godot(Some("4.7")), &search, &[], &silent),
            RequirementState::NoVersion { wanted }
        );
    }

    #[test]
    fn every_place_is_tried_before_the_version_is_wrong() {
        let base = temp("places");
        let (bin, home) = (base.join("bin"), base.join("home"));
        std::fs::create_dir_all(&bin).unwrap();
        // An old Godot on PATH, a good one at the fixed path.
        program(&bin, if cfg!(windows) { "godot.exe" } else { "godot" });
        let app = home.join("Apps/Godot.app/Contents/MacOS");
        std::fs::create_dir_all(&app).unwrap();
        let good = program(&app, "Godot");
        let search = Search {
            os: Os::CURRENT,
            cwd: PathBuf::from("/nonexistent-cwd"),
            dirs: vec![bin],
            home,
        };

        let run = |p: &Path, _: &str| {
            Some(
                if p == good {
                    "4.7.2.stable"
                } else {
                    "3.5.3.stable"
                }
                .to_owned(),
            )
        };
        assert_eq!(
            detect(&godot(Some("4.7")), &search, &[], &run),
            RequirementState::Found {
                version: Some("4.7.2".into())
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_program_a_package_brought_along_is_never_found_or_run() {
        let base = temp("untrusted");
        let (packages, bin) = (base.join("home/packages/evil/.claude"), base.join("bin"));
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let own = program(&packages, "godot");
        // Named at its own path, and reached through a link on PATH.
        std::os::unix::fs::symlink(&own, bin.join("godot")).unwrap();
        let mut req = godot(Some("4.7"));
        req.paths.macos = vec![own.display().to_string()];
        req.paths.linux = req.paths.macos.clone();
        let search = Search {
            os: Os::CURRENT,
            cwd: PathBuf::from("/nonexistent-cwd"),
            dirs: vec![bin],
            home: base.join("user"),
        };
        let ran = std::sync::atomic::AtomicBool::new(false);
        let run = |_: &Path, _: &str| {
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Some("4.7.2".to_owned())
        };
        let untrusted = [base.join("home")];
        assert_eq!(
            detect(&req, &search, &untrusted, &run),
            RequirementState::Missing
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_executable_is_not_the_program() {
        let base = temp("noexec");
        std::fs::write(base.join("godot"), "data").unwrap();
        let search = Search {
            os: Os::CURRENT,
            cwd: PathBuf::from("/nonexistent-cwd"),
            dirs: vec![base.clone()],
            home: base.join("home"),
        };
        let req = Requirement {
            paths: connecto_hook::requires::PerOs::default(),
            version_flag: None,
            ..godot(None)
        };
        assert_eq!(
            detect(&req, &search, &[], &|_, _| None),
            RequirementState::Missing
        );
        // At a fixed path, a file that does not run tells no version.
        let fixed = Requirement {
            paths: connecto_hook::requires::PerOs {
                macos: vec![base.join("godot").display().to_string()],
                linux: vec![base.join("godot").display().to_string()],
                windows: Vec::new(),
            },
            programs: Vec::new(),
            ..godot(Some("4.7"))
        };
        assert!(matches!(
            detect(&fixed, &search, &[], &version_output),
            RequirementState::NoVersion { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_version_is_read_from_the_real_program_only_when_it_succeeds() {
        let base = temp("run");
        let path = program(&base, "godot");
        let out = version_output(&path, "--version").unwrap();
        assert_eq!(version_in(&out).as_deref(), Some("4.7.2"));
        let failing = base.join("failing");
        std::fs::write(&failing, "#!/bin/sh\necho 4.7.2\nexit 3\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(version_output(&failing, "--version"), None);
    }

    #[test]
    fn windows_paths_expand_their_variable() {
        let var = |name: &str| {
            (name == "LOCALAPPDATA").then(|| "C:\\Users\\u\\AppData\\Local".to_owned())
        };
        let home = Path::new("/h");
        assert_eq!(
            expand("%LOCALAPPDATA%\\x\\godot.exe", Os::Windows, home, &var),
            Some(PathBuf::from("C:\\Users\\u\\AppData\\Local").join("x\\godot.exe"))
        );
        assert_eq!(expand("%NOPE%\\x", Os::Windows, home, &var), None);
        assert_eq!(expand("~/x", Os::Macos, home, &var), Some(home.join("x")));
    }

    #[cfg(unix)]
    #[test]
    fn a_launcher_is_run_under_the_name_it_was_found_by() {
        // Snap's way: one dispatcher, a link per tool, told apart by name.
        let base = temp("aliases");
        let (snap, bin) = (base.join("snap"), base.join("bin"));
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let dispatcher = program(&snap, "snap");
        std::os::unix::fs::symlink(&dispatcher, bin.join("godot")).unwrap();
        std::os::unix::fs::symlink(&dispatcher, bin.join("godot4")).unwrap();
        let mut req = godot(Some("4.7"));
        req.programs = vec!["godot".into(), "godot4".into()];
        // `bin` as a relative PATH entry: run as the absolute path that was
        // checked, under its own alias name.
        let search = Search {
            os: Os::CURRENT,
            cwd: base.clone(),
            dirs: vec![PathBuf::from("bin")],
            home: base.join("home"),
        };
        let seen = std::sync::Mutex::new(Vec::new());
        let run = |p: &Path, _: &str| {
            seen.lock().unwrap().push(p.to_owned());
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            Some(if name == "godot4" { "4.7.2" } else { "3.5.3" }.to_owned())
        };
        assert_eq!(
            detect(&req, &search, &[], &run),
            RequirementState::Found {
                version: Some("4.7.2".into())
            }
        );
        assert_eq!(
            *seen.lock().unwrap(),
            [bin.join("godot"), bin.join("godot4")],
            "both aliases, each by its own name"
        );
        // Trusted by where the link leads, all the same.
        assert_eq!(
            detect(&req, &search, &[snap], &run),
            RequirementState::Missing
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_helper_holding_the_output_open_does_not_hold_the_probe() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = temp("helper");
        let (path, pid_file) = (base.join("tool"), base.join("helper.pid"));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho 4.7.2\nsleep 30 &\necho $! > '{}'\nexit 0\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let out = version_output_within(&path, "--version", Duration::from_secs(3));
        // Nothing waited on the helper's open output: not even the deadline.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            out.as_deref().and_then(version_in).as_deref(),
            Some("4.7.2")
        );
        // And the helper itself is gone, so nothing it held stays open.
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let pid = nix::unistd::Pid::from_raw(pid);
        let gone_by = Instant::now() + Duration::from_secs(2);
        while nix::sys::signal::kill(pid, None).is_ok() && Instant::now() < gone_by {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            nix::sys::signal::kill(pid, None).is_err(),
            "the helper still runs"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_probe_runs_in_a_folder_no_package_writes_to() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = temp("cwd");
        let path = base.join("tool");
        std::fs::write(&path, "#!/bin/sh\npwd\necho 4.7.2\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = version_output(&path, "--version").unwrap();
        assert_eq!(out.lines().next(), Some("/"));
    }
}
