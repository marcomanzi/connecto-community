//! Helpers shared by connecto-app's unit tests.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

/// Every test deadline goes through here. `CONNECTO_TEST_TIME_SCALE` (a number
/// of at least 1) stretches them all for a machine that is slower than usual.
#[must_use]
pub fn scaled(base: Duration) -> Duration {
    let factor = std::env::var("CONNECTO_TEST_TIME_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|factor| factor.is_finite() && *factor >= 1.0)
        .unwrap_or(1.0);
    base.mul_f64(factor)
}

/// The shared copy of a fake program with this text, for this test process.
///
/// macOS checks a newly written executable the first time it runs, one at a
/// time for the whole machine (measured: 1.4 s p50 and 4.7 s max with twelve
/// threads, 0.01 s once checked). So each distinct program is written and run
/// once here, and every test gets a hard link to that copy: a hard link shares
/// the checked file, a copy does not. Tests that need different behaviour give
/// different text, never a new write of the same text.
pub fn fake_program(body: &str) -> PathBuf {
    static MADE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let mut made = MADE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(path) = made.get(body) {
        return path.clone();
    }
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("connecto-test-fakes-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(dir).unwrap()
    });
    let path = dir.join(format!("fake-{}", made.len()));
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    // The first run is the check; its outcome does not matter here.
    let _ = Command::new(&path)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    made.insert(body.to_owned(), path.clone());
    path
}

/// Makes `dest` a program with this text: a hard link to the shared copy.
pub fn link_fake(body: &str, dest: &Path) {
    let shared = fake_program(body);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let _ = fs::remove_file(dest);
    fs::hard_link(&shared, dest).unwrap_or_else(|err| {
        panic!(
            "hard link {} -> {}: {err}",
            dest.display(),
            shared.display()
        )
    });
}

/// A native (Mach-O) program built from Rust `source`, once per text, with the
/// toolchain that runs the tests (beside `CARGO`, else on the `PATH`). Claude is
/// only ever taken as a native program, so its fakes must be one too.
pub fn native_source(source: &str) -> PathBuf {
    static MADE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let mut made = MADE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(path) = made.get(source) {
        return path.clone();
    }
    let rustc = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .and_then(|cargo| cargo.parent().map(|dir| dir.join("rustc")))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("rustc"));
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("connecto-test-native-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(dir).unwrap()
    });
    let source_file = dir.join(format!("fake-{}.rs", made.len()));
    fs::write(&source_file, source).unwrap();
    // `.exe` on Windows, where a program is run by that name.
    let path = dir.join(format!(
        "fake-{}{}",
        made.len(),
        std::env::consts::EXE_SUFFIX
    ));
    let status = Command::new(rustc)
        .arg("-o")
        .arg(&path)
        .arg(&source_file)
        .status()
        .unwrap();
    assert!(status.success(), "compiling a fake program failed");
    // The first run is the check; its outcome does not matter here.
    let _ = Command::new(&path)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    made.insert(source.to_owned(), path.clone());
    path
}

/// The source of a native program that prints `line` for any arguments.
fn printing_source(line: &str) -> String {
    format!("fn main() {{ println!({line:?}); }}\n")
}

/// A native program that prints `line` for any arguments. (Used by the Unix
/// installer tests; Windows' run PowerShell installers.)
#[cfg(unix)]
pub fn native_program(line: &str) -> PathBuf {
    native_source(&printing_source(line))
}

/// Makes `dest` a native program built from `source`: a hard link to the shared copy.
pub fn link_native_source(source: &str, dest: &Path) {
    let shared = native_source(source);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let _ = fs::remove_file(dest);
    fs::hard_link(&shared, dest).unwrap_or_else(|err| {
        panic!(
            "hard link {} -> {}: {err}",
            dest.display(),
            shared.display()
        )
    });
}

/// Makes `dest` a native program that prints `line` for any arguments.
pub fn link_native(line: &str, dest: &Path) {
    link_native_source(&printing_source(line), dest);
}
