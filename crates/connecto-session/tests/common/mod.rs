//! Shared helpers for tests that run real processes in real PTYs.

#![allow(dead_code)]

use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use connecto_core::{GridSize, SessionStatus, SnapshotOutcome, TermSnapshot, TerminalSession};
use connecto_pty::Wake;
use connecto_session::{LiveSession, shell_spec, spec};

pub const TIMEOUT: Duration = Duration::from_secs(15);

/// A test deadline, stretched by `CONNECTO_TEST_TIME_SCALE` (a number of at
/// least 1) for a machine that is slower than usual.
pub fn scaled(base: Duration) -> Duration {
    let factor = std::env::var("CONNECTO_TEST_TIME_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|factor| factor.is_finite() && *factor >= 1.0)
        .unwrap_or(1.0);
    base.mul_f64(factor)
}

/// The real hook helper, built on first use. Tests are not given other
/// packages' binaries, and this keeps `cargo test -p` working.
pub fn hook_binary() -> PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-q", "-p", "connecto-hook"])
                .status()
                .unwrap();
            assert!(status.success());
            let test_exe = std::env::current_exe().unwrap();
            let profile_dir = test_exe.parent().unwrap().parent().unwrap();
            profile_dir.join(format!("connecto-hook{}", std::env::consts::EXE_SUFFIX))
        })
        .clone()
}

pub fn no_wake() -> Wake {
    Arc::new(|| {})
}

/// The folder the tests' shells start in.
#[cfg(unix)]
pub fn work_dir() -> PathBuf {
    PathBuf::from("/tmp")
}

/// The folder the tests' shells start in.
#[cfg(windows)]
pub fn work_dir() -> PathBuf {
    std::env::temp_dir()
}

/// Run `command` through the shell.
pub fn shell(command: &str) -> LiveSession {
    LiveSession::start(shell_spec(Some(command), &work_dir()), no_wake()).unwrap()
}

/// Run a program directly.
pub fn program(path: &str, args: &[&str], size: GridSize) -> LiveSession {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let mut pty_spec = spec(path, args, &work_dir());
    pty_spec.size = size;
    LiveSession::start(pty_spec, no_wake()).unwrap()
}

/// All visible text, one line per row.
pub fn screen_text(snapshot: &TermSnapshot) -> String {
    (0..snapshot.rows)
        .map(|row| {
            let line: String = snapshot.row(row).iter().map(|cell| cell.ch).collect();
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Poll until `predicate` holds on the screen, or fail with what was shown.
pub fn wait_for_screen(
    session: &impl TerminalSession,
    what: &str,
    predicate: impl Fn(&TermSnapshot) -> bool,
) -> TermSnapshot {
    let mut snapshot = TermSnapshot::default();
    let deadline = Instant::now() + scaled(TIMEOUT);
    while Instant::now() < deadline {
        let outcome = session.poll_snapshot(&mut snapshot);
        assert_ne!(outcome, SnapshotOutcome::Failed, "emulator failed");
        if !snapshot.cells.is_empty() && predicate(&snapshot) {
            return snapshot;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "timed out waiting for {what}; screen was:\n{}",
        screen_text(&snapshot)
    );
}

pub fn wait_for_text(session: &impl TerminalSession, needle: &str) -> TermSnapshot {
    wait_for_screen(session, needle, |snapshot| {
        screen_text(snapshot).contains(needle)
    })
}

/// Wait until the child is reaped and return its status.
pub fn wait_for_exit(session: &LiveSession) -> SessionStatus {
    assert!(
        session.wait_exit(scaled(TIMEOUT)),
        "child did not exit in time"
    );
    session.status()
}

#[cfg(unix)]
pub fn process_exists(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

#[cfg(windows)]
pub fn process_exists(pid: i32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let pid = Pid::from_u32(u32::try_from(pid).unwrap());
    let mut system = System::new().unwrap();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system.process(pid).is_some()
}

/// A child that opens a synchronized update, asks for the cursor position, never
/// closes the update, and blocks for the reply. Writes `<ms> <reply>` to argv[1].
pub const SYNC_QUERY_CHILD: &str = r#"import os,select,sys,time,tty
tty.setraw(0)
os.write(1,b'\x1b[?2026h\x1b[6n')
t=time.time()
r=select.select([0],[],[],5)[0]
reply=os.read(0,32) if r else b''
open(sys.argv[1]+'.tmp','w').write('%d %r'%((time.time()-t)*1000,reply))
os.rename(sys.argv[1]+'.tmp',sys.argv[1])
"#;

/// Hold back 150 ms, one 50 ms tick, and room for a loaded machine.
pub const SYNC_REPLY_LIMIT_MS: u64 = 400;

/// Wait for the child's report without touching the screen: no UI pass and no
/// snapshot poll may be what releases the update.
pub fn sync_query_result(path: &Path) -> (u64, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "the child never reported");
        thread::sleep(Duration::from_millis(10));
    }
    let text = std::fs::read_to_string(path).unwrap();
    let (ms, reply) = text.split_once(' ').unwrap();
    (ms.parse().unwrap(), reply.to_owned())
}

/// Scenes that run a real PTY and a scripted tool take a permit. Past a few at
/// once, the starved scripts (each spawns Python and the hook helper) make the
/// tests time out on readiness and on deliveries, which is a fault of the
/// load and not of the thing under test.
static PERMITS: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());
const MAX_SCENES: usize = 3;

pub struct Permit;

pub fn permit() -> Permit {
    let mut taken = PERMITS.0.lock().unwrap();
    while *taken >= MAX_SCENES {
        taken = PERMITS.1.wait(taken).unwrap();
    }
    *taken += 1;
    Permit
}

impl Drop for Permit {
    fn drop(&mut self) {
        *PERMITS
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) -= 1;
        PERMITS.1.notify_one();
    }
}

/// The shared copy of a fake program with this text, for this test process.
///
/// macOS checks a newly written executable the first time it runs, one at a
/// time for the whole machine, so a fresh file can take seconds on a loaded
/// machine. Each distinct program is written and run once, and every scene gets
/// a hard link to it instead of writing its own.
pub fn fake_program(body: &str) -> PathBuf {
    static MADE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, PathBuf>>> =
        OnceLock::new();
    let mut made = MADE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(path) = made.get(body) {
        return path.clone();
    }
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("connecto-session-fakes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    });
    let path = dir.join(format!("fake-{}", made.len()));
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The first run is the check; its outcome does not matter here.
    let _ = Command::new(&path)
        .arg("--version")
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    made.insert(body.to_owned(), path.clone());
    path
}

/// Makes `dest` a program with this text: a hard link to the shared copy.
pub fn link_fake(body: &str, dest: &Path) {
    let shared = fake_program(body);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(dest);
    std::fs::hard_link(&shared, dest).unwrap_or_else(|err| {
        panic!(
            "hard link {} -> {}: {err}",
            dest.display(),
            shared.display()
        )
    });
}
