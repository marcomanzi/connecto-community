//! Spawning a child in a PTY and owning its threads.

use std::ffi::OsString;
use std::io::{ErrorKind, Read, Write};
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use arc_swap::ArcSwap;
use connecto_core::{GridSize, SessionStatus};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};

use crate::input::{InputReceiver, InputSender};
use crate::process::{Killer, Stop};

/// The most bytes handed to the sink in one call, so that one read cannot hold
/// the emulator for long.
pub const MAX_READ_SLICE: usize = 64 * 1024;

/// How long a child gets to end after SIGTERM before SIGKILL.
pub const SIGKILL_GRACE: Duration = Duration::from_secs(5);

/// How many times to try opening a PTY before giving up.
const OPEN_ATTEMPTS: u32 = 3;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Serializes PTY creation within this process.
static OPEN_LOCK: Mutex<()> = Mutex::new(());

/// A callback that asks the UI for a new frame. Must not block.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Receives everything the child writes, on the reader thread.
pub trait OutputSink: Send + 'static {
    /// Consume one slice of at most [`MAX_READ_SLICE`] bytes.
    fn feed(&mut self, bytes: &[u8]);
}

/// What to run.
#[derive(Debug, Clone)]
pub struct PtySpec {
    /// Absolute path of the program. It is never looked up through `PATH`.
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    /// The complete child environment. Nothing else is inherited.
    pub env: Vec<(OsString, OsString)>,
    pub size: GridSize,
}

/// Failures while starting a session.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("program path must be absolute: {0}")]
    RelativeProgram(PathBuf),
    #[error("working directory does not exist: {0}")]
    MissingCwd(PathBuf),
    #[error("could not open a PTY: {0}")]
    Open(String),
    #[error("the PTY has no known device path")]
    UnknownTty,
    #[error("could not start the child: {0}")]
    Spawn(String),
    #[error("could not start a session thread: {0}")]
    Thread(#[from] std::io::Error),
}

/// An open PTY with no child yet.
pub struct OpenPty {
    pair: PtyPair,
    /// The slave device. A Windows pseudoconsole has none.
    #[cfg(unix)]
    tty: PathBuf,
}

impl OpenPty {
    /// Open a PTY. On Unix, fails if the slave device path is unknown, because
    /// callers must be able to name exactly this device and no other.
    pub fn open(size: GridSize) -> Result<Self, PtyError> {
        let pair = open_pty(pty_size(size))?;
        #[cfg(unix)]
        let tty = pair.master.tty_name().ok_or(PtyError::UnknownTty)?;
        Ok(Self {
            pair,
            #[cfg(unix)]
            tty,
        })
    }

    /// The path of the slave device. Unix only: ConPTY has no device path.
    #[cfg(unix)]
    #[must_use]
    pub fn tty_path(&self) -> &Path {
        &self.tty
    }
}

struct Shared {
    master: Mutex<Box<dyn MasterPty + Send>>,
    pid: Option<i32>,
    killer: Killer,
    reaped: AtomicBool,
    status: ArcSwap<SessionStatus>,
    /// Dropped by the waiter, which disconnects `exit_rx`.
    exit_rx: Receiver<()>,
}

impl Shared {
    /// Stop the child: on Unix, signal its process group and the terminal's
    /// foreground group (see [`Killer::stop`]).
    ///
    /// Nothing is signalled after the child is reaped, because its PID may be
    /// reused.
    fn signal_groups(&self, stop: Stop) {
        if self.reaped.load(Ordering::Acquire) {
            return;
        }
        self.killer.stop(self.pid, &self.master, stop);
    }
}

/// The body of one of a session's threads.
type ThreadBody = Box<dyn FnOnce() + Send>;

/// Starts one of a session's threads. Tests pass their own to make a start fail.
type StartThread = dyn Fn(&'static str, ThreadBody) -> std::io::Result<()>;

fn start_thread(name: &'static str, body: ThreadBody) -> std::io::Result<()> {
    thread::Builder::new()
        .name(name.into())
        .spawn(body)
        .map(drop)
}

/// End a child that no session will own and no waiter thread has: kill its
/// groups and reap it here, so neither a running process nor a zombie stays.
fn kill_and_reap(mut child: Box<dyn Child + Send + Sync>, shared: &Shared) {
    shared.signal_groups(Stop::Kill);
    let _ = child.kill();
    let _ = child.wait();
    shared.reaped.store(true, Ordering::Release);
}

/// A running child in a PTY. Dropping it terminates the child in the background.
pub struct PtySession {
    shared: Arc<Shared>,
    input: InputSender,
    terminating: AtomicBool,
}

impl PtySession {
    /// Start `spec`. The `sink` receives output on the reader thread, `input`
    /// feeds the writer thread and `wake` runs when the child's status changes.
    pub fn spawn(
        spec: PtySpec,
        sink: impl OutputSink,
        input: (InputSender, InputReceiver),
        wake: Wake,
    ) -> Result<Self, PtyError> {
        let pty = OpenPty::open(spec.size)?;
        Self::spawn_on(pty, spec, sink, input, wake)
    }

    /// Start `spec` on a PTY that is already open.
    ///
    /// Containment needs this: its profile names the exact slave device, which
    /// is only known once the PTY exists.
    pub fn spawn_on(
        pty: OpenPty,
        spec: PtySpec,
        sink: impl OutputSink,
        input: (InputSender, InputReceiver),
        wake: Wake,
    ) -> Result<Self, PtyError> {
        Self::spawn_with(pty, spec, sink, input, wake, &start_thread)
    }

    fn spawn_with(
        pty: OpenPty,
        spec: PtySpec,
        sink: impl OutputSink,
        input: (InputSender, InputReceiver),
        wake: Wake,
        start: &StartThread,
    ) -> Result<Self, PtyError> {
        if !spec.program.is_absolute() {
            return Err(PtyError::RelativeProgram(spec.program));
        }
        // portable-pty silently falls back to $HOME for a bad directory, which
        // would start the child somewhere the user did not choose.
        if !spec.cwd.is_dir() {
            return Err(PtyError::MissingCwd(spec.cwd));
        }

        let pair = pty.pair;

        let mut command = CommandBuilder::new(&spec.program);
        command.args(&spec.args);
        command.cwd(&spec.cwd);
        command.env_clear();
        for (name, value) in &spec.env {
            command.env(name, value);
        }

        // portable-pty calls setsid, so the child leads its own session and
        // process group, with this PTY as its controlling terminal.
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|err| PtyError::Spawn(err.to_string()))?;
        // The reader only sees EOF once every slave handle is closed.
        drop(pair.slave);

        let (exit_tx, exit_rx) = bounded::<()>(0);
        let (input_tx, input_rx) = input;
        let shared = Arc::new(Shared {
            pid: child.process_id().and_then(|pid| i32::try_from(pid).ok()),
            killer: Killer::new(&*child),
            master: Mutex::new(pair.master),
            reaped: AtomicBool::new(false),
            status: ArcSwap::from_pointee(SessionStatus::Running),
            exit_rx,
        });

        // The child runs from here on. Every error below ends it and reaps it
        // before returning: nothing else would, since no session is returned.
        let streams = {
            let master = shared.master.lock().unwrap_or_else(PoisonError::into_inner);
            master
                .try_clone_reader()
                .and_then(|reader| master.take_writer().map(|writer| (reader, writer)))
        };
        let (reader, writer) = match streams {
            Ok(streams) => streams,
            Err(err) => {
                kill_and_reap(child, &shared);
                return Err(PtyError::Open(err.to_string()));
            }
        };

        // The waiter first, and the child handed to it only once it runs: if
        // it cannot start, the child is still here to be reaped.
        let (child_tx, child_rx) = bounded::<Box<dyn Child + Send + Sync>>(1);
        let waiter = {
            let (shared, input_tx) = (Arc::clone(&shared), input_tx.clone());
            move || {
                if let Ok(child) = child_rx.recv() {
                    wait_for_exit(child, &shared, &input_tx, exit_tx, &wake);
                }
            }
        };
        if let Err(err) = start("pty-waiter", Box::new(waiter)) {
            kill_and_reap(child, &shared);
            return Err(err.into());
        }
        if let Err(back) = child_tx.send(child) {
            kill_and_reap(back.into_inner(), &shared);
            return Err(PtyError::Spawn("the waiter thread ended early".to_owned()));
        }

        let threads: [(&'static str, ThreadBody); 2] = [
            ("pty-reader", Box::new(move || read_until_eof(reader, sink))),
            (
                "pty-writer",
                Box::new(move || write_until_closed(writer, &input_rx)),
            ),
        ];
        for (name, body) in threads {
            if let Err(err) = start(name, body) {
                // The waiter has the child: kill it and wait for the reap.
                shared.signal_groups(Stop::Kill);
                let _ = shared.exit_rx.recv_timeout(SIGKILL_GRACE);
                return Err(err.into());
            }
        }

        Ok(Self {
            shared,
            input: input_tx,
            terminating: AtomicBool::new(false),
        })
    }

    /// The child's process ID, which on Unix is also its process group ID.
    #[must_use]
    pub fn pid(&self) -> Option<i32> {
        self.shared.pid
    }

    /// Current child status.
    #[must_use]
    pub fn status(&self) -> SessionStatus {
        self.shared.status.load().as_ref().clone()
    }

    /// Queue bytes for the child.
    pub fn input(&self) -> &InputSender {
        &self.input
    }

    /// Tell the kernel the new size, which raises SIGWINCH in the child.
    pub fn resize(&self, size: GridSize) -> bool {
        self.shared
            .master
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resize(pty_size(size))
            .is_ok()
    }

    /// Send SIGTERM now and SIGKILL after [`SIGKILL_GRACE`]. Never blocks.
    ///
    /// On Windows there is no SIGTERM: the child is terminated at once.
    pub fn terminate(&self) {
        if self.terminating.swap(true, Ordering::AcqRel) {
            return;
        }
        self.shared.signal_groups(Stop::Term);
        let shared = Arc::clone(&self.shared);
        let escalation = thread::Builder::new()
            .name("pty-kill".into())
            .spawn(move || kill_after_grace(&shared));
        if escalation.is_err() {
            // Without the timer thread, skip the grace period rather than
            // risk leaving the child running.
            self.shared.signal_groups(Stop::Kill);
        }
    }

    /// Wait up to `timeout` for the child to be reaped. Returns true if it was.
    #[must_use]
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        matches!(
            self.shared.exit_rx.recv_timeout(timeout),
            Err(RecvTimeoutError::Disconnected)
        )
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Open a PTY pair.
///
/// `openpty` was observed failing with the unexplained code -6 when many
/// threads called it at once on macOS, about once per several thousand opens.
/// The cause is unknown, so opens are serialized and briefly retried. This runs
/// only when a session starts, never on the input or render path.
fn open_pty(size: PtySize) -> Result<PtyPair, PtyError> {
    let _guard = OPEN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let mut attempt = 1;
    loop {
        match native_pty_system().openpty(size) {
            Ok(pair) => return Ok(pair),
            Err(err) if attempt >= OPEN_ATTEMPTS => return Err(PtyError::Open(err.to_string())),
            Err(_) => {
                attempt += 1;
                thread::sleep(OPEN_RETRY_DELAY);
            }
        }
    }
}

fn pty_size(size: GridSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn kill_after_grace(shared: &Shared) {
    if let Err(RecvTimeoutError::Timeout) = shared.exit_rx.recv_timeout(SIGKILL_GRACE) {
        shared.signal_groups(Stop::Kill);
    }
}

fn read_until_eof(mut reader: Box<dyn Read + Send>, mut sink: impl OutputSink) {
    let mut buffer = vec![0_u8; MAX_READ_SLICE];
    loop {
        match reader.read(&mut buffer) {
            // macOS reports a closed slave as either EOF or EIO.
            Ok(0) => return,
            Ok(len) => sink.feed(buffer.get(..len).unwrap_or_default()),
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn write_until_closed(mut writer: Box<dyn Write + Send>, input: &InputReceiver) {
    while let Some(bytes) = input.recv() {
        let result = writer.write_all(&bytes).and_then(|()| writer.flush());
        input.mark_written(bytes.len());
        if result.is_err() {
            input.close();
            return;
        }
    }
}

fn wait_for_exit(
    mut child: Box<dyn Child + Send + Sync>,
    shared: &Shared,
    input: &InputSender,
    exit_tx: Sender<()>,
    wake: &Wake,
) {
    let status = match child.wait() {
        Ok(status) => SessionStatus::Exited {
            code: status.exit_code(),
            signal: status.signal().map(str::to_owned),
        },
        Err(err) => SessionStatus::Unknown {
            reason: err.to_string(),
        },
    };
    shared.reaped.store(true, Ordering::Release);
    shared.status.store(Arc::new(status));
    input.close();
    drop(exit_tx);
    wake();
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::AtomicI32;

    use super::*;
    use crate::input::input_channel;

    #[cfg(unix)]
    use self::unix::{find_child, gone, sleeper, sleeper as failing_sleeper};
    #[cfg(windows)]
    use self::windows::{failing_sleeper, find_child, gone, sleeper};

    struct Discard;

    impl OutputSink for Discard {
        fn feed(&mut self, _: &[u8]) {}
    }

    #[cfg(unix)]
    mod unix {
        use std::path::{Path, PathBuf};

        use connecto_core::GridSize;
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        use crate::session::{OpenPty, PtySpec};

        pub(super) fn sleeper() -> PtySpec {
            PtySpec {
                program: PathBuf::from("/bin/sleep"),
                args: vec!["30".into()],
                cwd: std::env::temp_dir(),
                env: Vec::new(),
                size: GridSize { rows: 24, cols: 80 },
            }
        }

        /// Neither running nor a zombie: a zombie still answers signal 0.
        pub(super) fn gone(pid: i32) -> bool {
            matches!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH))
        }

        /// Finds the child started on `pty`, by its terminal.
        pub(super) fn find_child(pty: &OpenPty) -> impl Fn() -> Option<i32> + use<> {
            let tty = pty.tty_path().to_owned();
            move || pid_of_tty(&tty)
        }

        /// The process whose controlling terminal is `tty`, from `ps`.
        fn pid_of_tty(tty: &Path) -> Option<i32> {
            let name = tty.file_name()?.to_str()?;
            let out = std::process::Command::new("/bin/ps")
                .args(["-t", name, "-o", "pid="])
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        }
    }

    #[cfg(windows)]
    mod windows {
        use std::path::PathBuf;
        use std::process::Command;
        use std::thread;
        use std::time::{Duration, Instant};

        use connecto_core::GridSize;

        use crate::session::{OpenPty, PtySpec};

        /// The echo count that marks the failing-thread test's child, so it is
        /// told apart from the other test's child, which runs concurrently.
        const FAILING_COUNT: &str = "37";

        fn system32() -> PathBuf {
            std::env::var_os("SystemRoot")
                .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
                .join("System32")
        }

        /// `ping` sends one echo a second, so this runs for about `count`
        /// seconds, like `sleep`.
        fn pinger(count: &str) -> PtySpec {
            PtySpec {
                program: system32().join("PING.EXE"),
                args: vec!["-n".into(), count.into(), "127.0.0.1".into()],
                cwd: std::env::temp_dir(),
                // Not empty: portable-pty passes an empty environment as a
                // block CreateProcessW rejects (os error 87). The real child
                // environment is the realistic case anyway.
                env: crate::env::child_env(|name| std::env::var_os(name)),
                size: GridSize { rows: 24, cols: 80 },
            }
        }

        pub(super) fn sleeper() -> PtySpec {
            pinger("31")
        }

        pub(super) fn failing_sleeper() -> PtySpec {
            pinger(FAILING_COUNT)
        }

        /// The process has left the process list. A process ends shortly
        /// after `TerminateProcess` returns, so this polls briefly.
        pub(super) fn gone(pid: i32) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let listed = Command::new(system32().join("tasklist.exe"))
                    .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
                    .output()
                    .map(|out| {
                        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
                    });
                match listed {
                    Ok(false) => return true,
                    _ if Instant::now() >= deadline => return false,
                    _ => thread::sleep(Duration::from_millis(100)),
                }
            }
        }

        /// Finds the failing-thread test's child: a pseudoconsole has no device
        /// to look it up by, so it is found by its command line among this
        /// process's children.
        pub(super) fn find_child(_pty: &OpenPty) -> impl Fn() -> Option<i32> + use<> {
            || pid_of_ping(FAILING_COUNT)
        }

        /// A `ping` started with `-n count`, from the process list. Each test
        /// uses its own count.
        pub(super) fn pid_of_ping(count: &str) -> Option<i32> {
            let script = format!(
                "Get-CimInstance Win32_Process -Filter \"Name = 'PING.EXE'\" \
                 | Where-Object {{ $_.CommandLine -like '* -n {count} *' }} \
                 | ForEach-Object {{ $_.ProcessId }}"
            );
            let out = Command::new(system32().join(r"WindowsPowerShell\v1.0\powershell.exe"))
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        }

        /// `cmd.exe` running a `ping -n count`, so the ping is a grandchild
        /// attached to the same pseudoconsole.
        pub(super) fn shell_with_grandchild(count: &str) -> PtySpec {
            let mut spec = pinger(count);
            spec.program = system32().join("cmd.exe");
            spec.args = vec![
                "/d".into(),
                "/c".into(),
                format!(
                    "{} -n {count} 127.0.0.1",
                    system32().join("PING.EXE").display()
                )
                .into(),
            ];
            spec
        }
    }

    /// Start a session whose thread named `failing` cannot start. Returns the
    /// error and the child's pid.
    fn spawn_with_a_thread_that_fails(failing: &'static str) -> (PtyError, i32) {
        let spec = failing_sleeper();
        let pty = OpenPty::open(spec.size).unwrap();
        let find = find_child(&pty);
        let pid = Arc::new(AtomicI32::new(0));
        let start = {
            let pid = Arc::clone(&pid);
            move |name: &'static str, body: ThreadBody| {
                // The child runs by now. Its pid does not come back with an
                // error, so it is looked up by its terminal (Unix) or its
                // command line (Windows).
                if pid.load(Ordering::SeqCst) == 0 {
                    pid.store(find().unwrap_or(0), Ordering::SeqCst);
                }
                if name == failing {
                    Err(std::io::Error::other("no more threads"))
                } else {
                    start_thread(name, body)
                }
            }
        };
        let wake: Wake = Arc::new(|| {});
        let result = PtySession::spawn_with(pty, spec, Discard, input_channel(), wake, &start);
        (result.err().unwrap(), pid.load(Ordering::SeqCst))
    }

    #[test]
    fn a_child_is_killed_and_reaped_when_any_session_thread_cannot_start() {
        for failing in ["pty-waiter", "pty-reader", "pty-writer"] {
            let (err, pid) = spawn_with_a_thread_that_fails(failing);
            assert!(matches!(err, PtyError::Thread(_)), "{failing}: {err}");
            assert!(pid > 0, "{failing}: the child was started");
            assert!(gone(pid), "{failing}: pid {pid} is still there");
        }
    }

    #[test]
    fn a_session_whose_threads_all_start_runs_and_ends_as_before() {
        let wake: Wake = Arc::new(|| {});
        let session = PtySession::spawn(sleeper(), Discard, input_channel(), wake).unwrap();
        let pid = session.pid().unwrap();
        assert!(!gone(pid));
        assert!(matches!(session.status(), SessionStatus::Running));
        session.terminate();
        assert!(session.wait_exit(Duration::from_secs(10)));
        assert!(gone(pid));
        assert!(matches!(session.status(), SessionStatus::Exited { .. }));
    }

    /// ConPTY has no process groups: killing the child leaves its own
    /// children running, and closing the pseudoconsole, which dropping the
    /// session does, ends them.
    #[cfg(windows)]
    #[test]
    fn dropping_a_session_ends_what_else_runs_in_its_console() {
        let count = "43";
        let wake: Wake = Arc::new(|| {});
        let spec = windows::shell_with_grandchild(count);
        let session = PtySession::spawn(spec, Discard, input_channel(), wake).unwrap();
        // ConPTY first asks the terminal where the cursor is and holds the
        // console until it gets an answer, which the emulator gives in the app.
        session.input().try_send(b"\x1b[1;1R".to_vec()).unwrap();
        let shell = session.pid().unwrap();
        let mut grandchild = None;
        for _ in 0..50 {
            grandchild = windows::pid_of_ping(count);
            if grandchild.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let grandchild = grandchild.unwrap();
        session.terminate();
        assert!(session.wait_exit(Duration::from_secs(10)));
        assert!(gone(shell));
        // Still running: only the console's closing ends it below. A future
        // job object would end it here instead, and this check would change.
        assert!(!gone(grandchild), "the grandchild ended with the shell");
        drop(session);
        assert!(gone(grandchild), "pid {grandchild} outlived its console");
    }
}
