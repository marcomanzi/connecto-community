//! A PTY joined to an emulator: one terminal session.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use connecto_adapter::HookCtx;
use connecto_core::{
    GridSize, InputRefused, OutputTap, ResizeOutcome, SessionStatus, SnapshotOutcome, TermSnapshot,
    TerminalSession,
};
use connecto_pty::{
    OpenPty, OutputSink, PtyError, PtySession, PtySpec, SIGKILL_GRACE, Wake, child_env,
    input_channel,
};
use connecto_term::Emulator;
use crossbeam_channel::{RecvTimeoutError, Sender, bounded};

use crate::hook::HookHandle;
use crate::launch::{ProfileClaim, RunFolder};
use crate::sweep::sweep;

pub use crate::platform::SHELL;

/// Size used until the window reports the real one.
pub const INITIAL_SIZE: GridSize = GridSize { cols: 80, rows: 24 };

/// How often a held synchronized update is checked. vte holds one for 150 ms.
const SYNC_FLUSH_TICK: Duration = Duration::from_millis(50);

struct EmulatorSink {
    emulator: Arc<Emulator>,
    tap: Option<Arc<dyn OutputTap>>,
}

impl OutputSink for EmulatorSink {
    fn feed(&mut self, bytes: &[u8]) {
        match &self.tap {
            Some(tap) => tap.feed(bytes, &mut |b| self.emulator.feed(b)),
            None => self.emulator.feed(bytes),
        }
    }
}

/// Releases expired synchronized updates, so no session depends on a UI pass
/// (ADR 0011). Here because every PTY mode starts here, and a login or plain
/// shell has no relay tick. Ends when the session drops its sender.
fn spawn_sync_flush(emulator: Arc<Emulator>) -> Result<Sender<()>, PtyError> {
    let (stop, stopped) = bounded::<()>(0);
    thread::Builder::new()
        .name("sync-flush".into())
        .spawn(move || {
            while stopped.recv_timeout(SYNC_FLUSH_TICK) == Err(RecvTimeoutError::Timeout) {
                emulator.flush_expired();
            }
        })
        .map_err(|err| PtyError::Spawn(err.to_string()))?;
    Ok(stop)
}

/// A child in a PTY with its emulated screen.
pub struct LiveSession {
    emulator: Arc<Emulator>,
    /// Dropping this ends the flush thread.
    _sync_flush: Sender<()>,
    pty: Arc<PtySession>,
    terminating: AtomicBool,
    hook: Option<HookHandle>,
    /// The pane's attempt number for this session, set once before it is
    /// published. A caller reads id and attempt from one session, so a restart
    /// cannot mix them.
    attempt: AtomicU64,
    /// What parsing this session's hooks may consult.
    hook_ctx: Option<HookCtx>,
    /// Removed when the session goes away.
    _run: Option<RunFolder>,
    /// Keeps a login off this session's profile.
    _claim: Option<ProfileClaim>,
    /// Who decides the size: the local view, or a remote peer that has it.
    size: Mutex<SizeControl>,
}

struct SizeControl {
    remote: bool,
    /// The last size the local view asked for.
    local: GridSize,
}

impl LiveSession {
    /// Start a session. Replies to terminal queries go straight to the child.
    pub fn start(spec: PtySpec, wake: Wake) -> Result<Self, PtyError> {
        let pty = OpenPty::open(spec.size)?;
        Self::start_on(pty, spec, wake)
    }

    /// Start a session on a PTY that is already open.
    pub fn start_on(pty: OpenPty, spec: PtySpec, wake: Wake) -> Result<Self, PtyError> {
        Self::start_tapped(pty, spec, wake, None)
    }

    /// As [`LiveSession::start_on`], with a tap that sees the output as the
    /// emulator takes it. Remote hosting is the only user (ADR 0017).
    pub fn start_tapped(
        pty: OpenPty,
        spec: PtySpec,
        wake: Wake,
        tap: Option<Arc<dyn OutputTap>>,
    ) -> Result<Self, PtyError> {
        let initial_size = spec.size;
        let (input_tx, input_rx) = input_channel();
        let reply_tx = input_tx.clone();
        let emulator = Arc::new(Emulator::new(
            spec.size,
            // A reply dropped on a full queue only loses a query answer. The
            // alternative, waiting here, could deadlock reader and child.
            Arc::new(move |bytes| {
                let _ = reply_tx.try_send(bytes);
            }),
            Arc::clone(&wake),
        ));
        let sink = EmulatorSink {
            emulator: Arc::clone(&emulator),
            tap,
        };
        let sync_flush = spawn_sync_flush(Arc::clone(&emulator))?;
        let pty = PtySession::spawn_on(pty, spec, sink, (input_tx, input_rx), wake)?;
        Ok(Self {
            emulator,
            _sync_flush: sync_flush,
            pty: Arc::new(pty),
            terminating: AtomicBool::new(false),
            hook: None,
            attempt: AtomicU64::new(0),
            hook_ctx: None,
            _run: None,
            _claim: None,
            size: Mutex::new(SizeControl {
                remote: false,
                local: initial_size,
            }),
        })
    }

    pub(crate) fn attach(
        &mut self,
        hook: Option<HookHandle>,
        hook_ctx: Option<HookCtx>,
        run: RunFolder,
        claim: Option<ProfileClaim>,
    ) {
        self.hook = hook;
        self.hook_ctx = hook_ctx;
        self._run = Some(run);
        self._claim = claim;
    }

    pub(crate) fn set_attempt(&self, attempt: u64) {
        self.attempt.store(attempt, Ordering::SeqCst);
    }

    /// The pane's attempt number for this session.
    #[must_use]
    pub fn attempt(&self) -> u64 {
        self.attempt.load(Ordering::SeqCst)
    }

    /// What parsing this session's hooks may consult.
    #[must_use]
    pub fn hook_ctx(&self) -> Option<&HookCtx> {
        self.hook_ctx.as_ref()
    }

    /// All the text the terminal holds, and whether history is full. Waits on
    /// the screen lock, so call it from a handler thread, never the UI.
    #[must_use]
    pub fn text_all(&self) -> (String, bool) {
        self.emulator.text_all()
    }

    /// The last `rows` rows of the screen and its history, for a caller that
    /// only looks at the bottom.
    #[must_use]
    pub fn text_tail(&self, rows: usize) -> String {
        self.emulator.text_tail(rows)
    }

    /// The session's hook channel, when it has one.
    #[must_use]
    pub fn hook(&self) -> Option<&HookHandle> {
        self.hook.as_ref()
    }

    pub(crate) fn pty(&self) -> Arc<PtySession> {
        Arc::clone(&self.pty)
    }

    /// The child's process ID.
    #[must_use]
    pub fn pid(&self) -> Option<i32> {
        self.pty.pid()
    }

    /// Send SIGTERM now and SIGKILL after the grace period, to the process
    /// groups and to every descendant found first. Never blocks.
    pub fn terminate(&self) {
        if self.terminating.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(root) = self.pty.pid().filter(|_| self.is_running()) else {
            self.pty.terminate();
            return;
        };
        let pty = Arc::clone(&self.pty);
        let swept = thread::Builder::new()
            .name("session-sweep".into())
            .spawn(move || sweep(root, SIGKILL_GRACE, || pty.terminate()));
        if swept.is_err() {
            self.pty.terminate();
        }
    }

    fn is_running(&self) -> bool {
        self.pty.status() == SessionStatus::Running
    }

    /// Wait up to `timeout` for the child to end.
    #[must_use]
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        self.pty.wait_exit(timeout)
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        self.terminate();
    }
}

impl LiveSession {
    fn apply_size(&self, size: GridSize) -> ResizeOutcome {
        let outcome = self.emulator.try_resize(size);
        if outcome == ResizeOutcome::Applied {
            // Fails only once the child is gone, when no one needs the size.
            self.pty.resize(size);
        }
        outcome
    }
}

impl TerminalSession for LiveSession {
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        self.emulator.poll_snapshot(out)
    }

    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        self.pty.input().try_send(bytes)
    }

    fn resize(&self, size: GridSize) -> ResizeOutcome {
        let mut control = self.size.lock().unwrap_or_else(PoisonError::into_inner);
        control.local = size;
        if control.remote {
            // A remote peer has the size: the local view is told it worked, and
            // gets its size back when the peer lets go.
            return ResizeOutcome::Applied;
        }
        self.apply_size(size)
    }

    fn resize_remote(&self, size: GridSize) -> ResizeOutcome {
        let _control = self.size.lock().unwrap_or_else(PoisonError::into_inner);
        self.apply_size(size)
    }

    fn set_remote_size_control(&self, on: bool) {
        let local = {
            let mut control = self.size.lock().unwrap_or_else(PoisonError::into_inner);
            control.remote = on;
            control.local
        };
        if !on {
            for _ in 0..50 {
                let held = self.size.lock().unwrap_or_else(PoisonError::into_inner);
                if held.remote {
                    // A peer took it again meanwhile.
                    return;
                }
                if self.apply_size(local) != ResizeOutcome::Retry {
                    return;
                }
                drop(held);
                thread::sleep(Duration::from_millis(2));
            }
        }
    }

    fn scroll(&self, lines: i32) -> bool {
        self.emulator.try_scroll(lines)
    }

    fn status(&self) -> SessionStatus {
        self.pty.status()
    }
}

/// A spec for `program` with the allowlisted environment.
#[must_use]
pub fn spec(program: impl Into<PathBuf>, args: Vec<OsString>, cwd: &Path) -> PtySpec {
    PtySpec {
        program: program.into(),
        args,
        cwd: crate::platform::child_cwd(cwd),
        env: child_env(|name| std::env::var_os(name)),
        size: INITIAL_SIZE,
    }
}

/// A spec for an interactive shell, or `command` run by that shell, without
/// the user's startup files (zsh `-f -c`; on Windows, PowerShell `-NoLogo
/// -NoProfile -Command`).
#[must_use]
pub fn shell_spec(command: Option<&str>, cwd: &Path) -> PtySpec {
    spec(
        crate::platform::shell_spec_program(),
        crate::platform::shell_spec_args(command),
        cwd,
    )
}
