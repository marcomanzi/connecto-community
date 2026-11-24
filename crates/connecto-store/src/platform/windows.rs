//! Windows side of [`crate::platform`].
//!
//! Windows has no Unix mode bits, so the store's copies are not made
//! read-only here: their integrity rests on the recorded SHA-256 digests,
//! which are checked before every use, as on macOS.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::hash::sha256_file;
use crate::store::Tool;

/// The helper's file name in the store.
pub const HOOK_NAME: &str = "connecto-hook.exe";
/// The `connecto` command beside it: a copy of the helper (a symbolic link
/// needs a privilege on Windows that a normal user does not have).
pub const CONNECTO_LINK: &str = "connecto.exe";
/// Claude's executable, relative to its version folder.
pub const CLAUDE_ENTRY: &str = "claude.exe";
/// Codex's executable, relative to its version folder (`bin\codex.exe` in the
/// npm platform package's release folder, as `@openai/codex`'s `bin/codex.js`
/// looks it up).
pub const CODEX_ENTRY: &str = "bin/codex.exe";

/// The publisher that must have signed Claude Code: its certificate's CN and
/// O are both this (`signature/windows.rs` holds the exact subject).
pub const CLAUDE_SIGNER: &str = "Anthropic, PBC";
/// The publisher that must have signed Codex's programs and libraries.
pub const CODEX_SIGNER: &str = "OpenAI OpCo, LLC";
/// The publisher of the Microsoft Visual C++ runtime files Codex ships
/// (CN `Microsoft Windows`, O `Microsoft Corporation`).
pub const MICROSOFT_RUNTIME_SIGNER: &str = "Microsoft Corporation";

/// The Microsoft Visual C++ runtime libraries, by file name (any case). In a
/// Codex copy, these must be validly signed by Microsoft
/// ([`MICROSOFT_RUNTIME_SIGNER`]) instead of OpenAI; every other program or
/// library must be signed by OpenAI. Codex 0.167.2 ships one of them,
/// `codex-resources\voice\bin\vcruntime140.dll`.
const MICROSOFT_RUNTIME_LIBRARIES: [&str; 6] = [
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "msvcp140.dll",
    "msvcp140_1.dll",
    "msvcp140_2.dll",
    "concrt140.dll",
];

/// The one file of a Codex release that is accepted without a signature,
/// by its exact path in the release: ripgrep, which OpenAI ships unsigned
/// (Codex 0.167.2, npm and standalone installer alike). It is still copied,
/// hashed and checked against its digest before every use like every other
/// file; only its origin is not proven by a signature. Decision D-3 in
/// docs/windows-port.md. Any other unsigned program refuses the release.
pub const CODEX_UNSIGNED_RG: &str = "codex-path/rg.exe";

/// The switch for [`CODEX_UNSIGNED_RG`]: `true` accepts that one unsigned
/// file (D-3, option a); `false` checks it like every other program, so a
/// release with an unsigned `rg.exe` (every release so far) is refused.
pub const ACCEPT_UNSIGNED_CODEX_RG: bool = true;

/// Whether the file is a program or library Windows would load (PE), judged
/// by its content.
pub fn is_native_binary(file: &Path) -> io::Result<bool> {
    is_pe(file)
}

/// Whether `file` is a PE image: `MZ` at its start, and the PE signature
/// `PE\0\0` where the DOS header's `e_lfanew` (at 0x3C) points. Windows
/// loads nothing else as a program or library (an old DOS `MZ` program
/// without a PE header does not run on 64-bit Windows).
pub fn is_pe(file: &Path) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut handle = fs::File::open(file)?;
    let mut header = [0_u8; 0x40];
    match handle.read_exact(&mut header) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(err) => return Err(err),
    }
    if !header.starts_with(b"MZ") {
        return Ok(false);
    }
    let Some(offset) = header
        .get(0x3c..0x40)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(u32::from_le_bytes)
    else {
        return Ok(false);
    };
    handle.seek(SeekFrom::Start(u64::from(offset)))?;
    let mut signature = [0_u8; 4];
    match handle.read_exact(&mut signature) {
        Ok(()) => Ok(&signature == b"PE\0\0"),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

/// The signer a native binary of `tool`'s copy (other than its entry) must
/// have, by its path in the copy; `None` when it is accepted unsigned.
#[must_use]
pub fn bundled_signer(tool: Tool, file: &Path) -> Option<&'static str> {
    bundled_signer_with(tool, file, ACCEPT_UNSIGNED_CODEX_RG)
}

/// [`bundled_signer`] with the [`ACCEPT_UNSIGNED_CODEX_RG`] switch given.
pub(crate) fn bundled_signer_with(
    tool: Tool,
    file: &Path,
    accept_unsigned_rg: bool,
) -> Option<&'static str> {
    if tool != Tool::Codex {
        return Some(tool.team_id());
    }
    if accept_unsigned_rg && file == Path::new(CODEX_UNSIGNED_RG) {
        return None;
    }
    let runtime = file
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            MICROSOFT_RUNTIME_LIBRARIES
                .iter()
                .any(|library| name.eq_ignore_ascii_case(library))
        });
    Some(if runtime {
        MICROSOFT_RUNTIME_SIGNER
    } else {
        tool.team_id()
    })
}

/// Kept while a staged copy is hashed and checked: the file open without
/// write or delete sharing, so that nothing can change or replace it between
/// its digest, its signature check and its trial run. Afterwards the digests
/// are checked before every use.
#[derive(Debug)]
pub struct Held {
    _file: fs::File,
}

/// See [`Held`].
pub fn hold(file: &Path) -> io::Result<Held> {
    use std::os::windows::fs::OpenOptionsExt;
    /// `FILE_SHARE_READ`: others may read (and run) the file, not write,
    /// rename or delete it.
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    let file = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(file)?;
    Ok(Held { _file: file })
}

/// The npm package that npm's launcher script `codex.cmd` (or `codex.ps1`)
/// stands for: `<its folder>\node_modules\@openai\codex\bin\codex.js`, as
/// `npm install -g` lays it out (also in nvm-windows' and fnm's folders).
/// The script itself is never read; what is found there is checked as a
/// Codex package (its `package.json`, its platform package, the release's
/// files) and the copy's signatures are checked afterwards.
#[must_use]
pub fn codex_npm_shim_target(real: &Path) -> Option<PathBuf> {
    let named_codex = real
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("codex"));
    if !named_codex || !is_launcher_by_name(real) {
        return None;
    }
    let script = real
        .parent()?
        .join("node_modules")
        .join("@openai")
        .join("codex")
        .join("bin")
        .join("codex.js");
    script.is_file().then_some(script)
}

/// File extensions Windows runs as programs or scripts.
const EXECUTABLE_EXTENSIONS: [&str; 5] = ["exe", "com", "bat", "cmd", "ps1"];
/// Extensions of launcher scripts (npm and version manager shims).
const LAUNCHER_EXTENSIONS: [&str; 3] = ["bat", "cmd", "ps1"];

/// The user's home folder: `USERPROFILE`.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// The data root when it is not overridden: `%APPDATA%\connecto`.
#[must_use]
pub fn default_data_root(home: &Path) -> PathBuf {
    data_root_in(std::env::var_os("APPDATA"), home)
}

/// `<appdata>\connecto`; without a usable `APPDATA`, the default place of the
/// roaming application data folder under `home`.
pub(crate) fn data_root_in(appdata: Option<OsString>, home: &Path) -> PathBuf {
    appdata
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join("AppData").join("Roaming"))
        .join("connecto")
}

/// The variables a launcher script (Pi's `pi.cmd`) is run with besides
/// `PATH`, when the rest of the environment is cleared: Windows itself
/// (`SystemRoot`, without which `cmd.exe` and Node.js misbehave) and the
/// user's folders under `home`. Pi's launcher finds the Node.js installed for
/// it by `%LOCALAPPDATA%` (`pi-node\current\node.exe`); without it, it runs
/// the first `node` on the `PATH`, which may be too old for Pi.
#[must_use]
pub fn launcher_env(home: &Path) -> Vec<(OsString, OsString)> {
    let root = std::env::var_os("SystemRoot")
        .filter(|root| Path::new(root).is_absolute())
        .unwrap_or_else(|| OsString::from(r"C:\Windows"));
    let app_data = home.join("AppData");
    vec![
        ("SystemRoot".into(), root),
        ("USERPROFILE".into(), home.as_os_str().to_owned()),
        ("APPDATA".into(), app_data.join("Roaming").into_os_string()),
        (
            "LOCALAPPDATA".into(),
            app_data.join("Local").into_os_string(),
        ),
    ]
}

/// Unix modes do not exist on Windows: nothing is changed.
pub const fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// `CREATE_NO_WINDOW`: a console program started by the app (which has no
/// console) would otherwise open a console window of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Run `command` without a console window: the app is a windowed program, and
/// the CLIs it asks (`--version`) are console programs.
pub fn no_console(command: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NO_WINDOW)
}

/// Whether the file is a program or script Windows runs, judged by its extension.
#[must_use]
pub fn is_executable(path: &Path, _meta: &fs::Metadata) -> bool {
    has_extension(path, &EXECUTABLE_EXTENSIONS)
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| extensions.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// Whether a process with this id exists. When the process list cannot be
/// read, it is taken to exist (the safe side: its folder is kept).
#[must_use]
pub fn pid_alive(pid: i32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let Ok(pid) = u32::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    let Ok(mut system) = System::new() else {
        return true;
    };
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system.process(pid).is_some()
}

/// Whether `link` already is the `connecto` command for the helper `hook`
/// (a file name in `dir`): a regular file with the same content.
#[must_use]
pub fn connecto_link_is_current(dir: &Path, link: &Path, hook: &str) -> bool {
    if !fs::symlink_metadata(link).is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    match (sha256_file(link), sha256_file(&dir.join(hook))) {
        (Ok(have), Ok(want)) => have == want,
        _ => false,
    }
}

/// Make `staged` the `connecto` command for the helper `hook`: a copy of it.
pub fn make_connecto_link(dir: &Path, hook: &str, staged: &Path) -> io::Result<()> {
    fs::copy(dir.join(hook), staged).map(|_| ())
}

/// What a replaced file is renamed to (after the file's own name and a dot).
const REPLACED_MARK: &str = ".old-";

/// Put `staged` in place of `target`. Windows does not let a running program
/// (or a file open without delete sharing) be replaced, but it lets it be
/// renamed: when the plain rename fails, `target` is first renamed aside to
/// `.<name>.old-<pid>-<n>` (a name `PATH` never runs: its extension is not a
/// program's), `staged` takes its place, and the old file is deleted when it
/// can be, else by a later [`sweep_replaced`]. If `staged` still cannot be put
/// in place, the old file is put back.
pub fn replace(staged: &Path, target: &Path) -> io::Result<()> {
    let refused = match fs::rename(staged, target) {
        Ok(()) => return Ok(()),
        Err(err) => err,
    };
    if !fs::symlink_metadata(target).is_ok_and(|meta| meta.is_file()) {
        return Err(refused);
    }
    let aside = aside_name(target).ok_or(refused)?;
    fs::rename(target, &aside)?;
    if let Err(err) = fs::rename(staged, target) {
        let _ = fs::rename(&aside, target);
        return Err(err);
    }
    let _ = fs::remove_file(&aside);
    Ok(())
}

/// A free name beside `target` for the file it replaces.
fn aside_name(target: &Path) -> Option<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = target.file_name()?.to_str()?;
    (0..64).find_map(|_| {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let aside =
            target.with_file_name(format!(".{name}{REPLACED_MARK}{}-{n}", std::process::id()));
        (!aside.exists() && fs::symlink_metadata(&aside).is_err()).then_some(aside)
    })
}

/// Delete the files [`replace`] renamed aside in `dir` that nothing runs any
/// more. One still running stays for a later call.
pub fn sweep_replaced(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let replaced = name.to_str().is_some_and(|name| {
            name.starts_with('.')
                && name.rsplit_once(REPLACED_MARK).is_some_and(|(_, tail)| {
                    !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit() || b == b'-')
                })
        });
        if replaced && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Whether the file's name says it is a launcher script (`.cmd`, `.bat`,
/// `.ps1`), as npm and the version managers make them.
#[must_use]
pub fn is_launcher_by_name(path: &Path) -> bool {
    has_extension(path, &LAUNCHER_EXTENSIONS)
}

/// The target triple folder npm's Codex package uses on this platform, and the
/// name of the optional package that carries its binaries. Checked against
/// `bin/codex.js` of `@openai/codex` 0.167.2 (its `PLATFORM_PACKAGE_BY_TARGET`).
#[must_use]
pub const fn codex_platform() -> (&'static str, &'static str) {
    if cfg!(target_arch = "aarch64") {
        ("aarch64-pc-windows-msvc", "codex-win32-arm64")
    } else {
        ("x86_64-pc-windows-msvc", "codex-win32-x64")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_data_root_is_under_appdata() {
        let home = Path::new(r"C:\Users\u");
        assert_eq!(
            data_root_in(Some(r"D:\Roaming".into()), home),
            PathBuf::from(r"D:\Roaming\connecto")
        );
        // Missing or relative: the default place under the home folder.
        for appdata in [None, Some("relative".into())] {
            assert_eq!(
                data_root_in(appdata, home),
                PathBuf::from(r"C:\Users\u\AppData\Roaming\connecto")
            );
        }
        let root = default_data_root(home);
        assert!(
            !root.components().any(|part| part.as_os_str() == "Library"),
            "{}",
            root.display()
        );
    }

    #[test]
    fn programs_and_launchers_are_told_by_extension() {
        assert!(is_launcher_by_name(Path::new(r"C:\npm\codex.cmd")));
        assert!(is_launcher_by_name(Path::new(r"C:\npm\codex.PS1")));
        assert!(!is_launcher_by_name(Path::new(r"C:\bin\claude.exe")));
        assert!(!is_launcher_by_name(Path::new(r"C:\bin\claude")));
        let meta = fs::metadata(std::env::current_exe().unwrap()).unwrap();
        assert!(is_executable(Path::new("a.EXE"), &meta));
        assert!(is_executable(Path::new("a.cmd"), &meta));
        assert!(!is_executable(Path::new("a.txt"), &meta));
        assert!(!is_executable(Path::new("a"), &meta));
    }

    /// A program that is running is replaced by renaming it aside; the aside
    /// copy goes once it no longer runs. Nothing else in the folder is touched.
    #[test]
    fn a_running_program_is_replaced_and_its_old_file_swept_later() {
        let dir = std::env::temp_dir().join(format!("connecto-replace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let system = std::env::var_os("SystemRoot").unwrap();
        let cmd = PathBuf::from(system).join("System32").join("cmd.exe");
        let target = dir.join("connecto.exe");
        fs::copy(&cmd, &target).unwrap();
        fs::write(dir.join("keep.txt"), "kept").unwrap();
        // The copy runs (waiting on a child), so its file is a running image.
        let mut running = std::process::Command::new(&target)
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let staged = dir.join(".connecto.exe-new");
        fs::write(&staged, "new build").unwrap();
        // A plain rename over the running program is what fails.
        let plain = dir.join(".plain");
        fs::write(&plain, "x").unwrap();
        assert!(fs::rename(&plain, &target).is_err());

        replace(&staged, &target).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new build");
        assert!(!staged.exists());
        let aside = || {
            fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".old-"))
                .count()
        };
        // Still running: kept aside, and a sweep leaves it.
        assert_eq!(aside(), 1);
        sweep_replaced(&dir);
        assert_eq!(aside(), 1);

        let _ = running.kill();
        running.wait().unwrap();
        drop(running);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while aside() > 0 && std::time::Instant::now() < deadline {
            sweep_replaced(&dir);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(aside(), 0);
        assert_eq!(fs::read_to_string(dir.join("keep.txt")).unwrap(), "kept");
        assert!(plain.exists());
        // Without a running program a plain rename does it.
        let staged = dir.join(".connecto.exe-newer");
        fs::write(&staged, "newer").unwrap();
        replace(&staged, &target).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "newer");
        assert_eq!(aside(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_process_is_alive_and_an_ended_one_is_not() {
        let me = i32::try_from(std::process::id()).unwrap();
        assert!(pid_alive(me));
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit"])
            .spawn()
            .unwrap();
        let ended = i32::try_from(child.id()).unwrap();
        child.wait().unwrap();
        // The handle keeps the ended process object; let it go first.
        drop(child);
        assert!(!pid_alive(ended));
        assert!(!pid_alive(0));
        assert!(!pid_alive(-5));
    }
}
