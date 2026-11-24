//! A PTY joined to an emulator: one terminal session.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use connecto_adapter::HookCtx;
use connecto_core::{
    GridSize, InputRefused, OutputTap, ResizeOutcome, SelectOp, SelectionText, SessionStatus,
    SnapshotOutcome, TermSnapshot, TerminalSession,
};
use connecto_pty::{
    OpenPty, OutputSink, PtyError, PtySession, PtySpec, SIGKILL_GRACE, Wake, child_env,
    input_channel,
};
use connecto_term::Emulator;

use crate::input_ledger::{InputGate, InputSource, Verdict};
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
/// The next run's [`LiveSession::input_generation`] epoch: never reused.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

pub struct LiveSession {
    /// This run of the program, told apart from every other (a restart is a
    /// new session): see [`Self::input_generation`].
    epoch: u64,
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
    /// The permission mode this session's agent was started with, set once
    /// at launch. None for a shell or a utility session.
    agent_mode: std::sync::OnceLock<connecto_core::AgentMode>,
    /// What parsing this session's hooks may consult.
    hook_ctx: Option<HookCtx>,
    /// Removed when the session goes away.
    _run: Option<RunFolder>,
    /// Keeps a login off this session's profile.
    _claim: Option<ProfileClaim>,
    /// Who decides the size: the local view, or a remote peer that has it.
    size: Mutex<SizeControl>,
    /// The size last applied, as `cols << 16 | rows`.
    applied: AtomicU32,
    /// The one way into the PTY: who wrote what, in the PTY's own order
    /// ([`crate::input_ledger`]).
    input_gate: Arc<InputGate>,
    /// The output's tap, through which the screen is resized too, so a
    /// remote host knows at which byte each size was taken (ADR 0017).
    tap: Option<Arc<dyn OutputTap>>,
}

struct SizeControl {
    remote: bool,
    /// The last size the local view asked for.
    local: GridSize,
    /// How many times the local view took the size back from a peer.
    local_claims: u64,
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
        let input_gate = Arc::new(InputGate::default());
        let reply_gate = Arc::clone(&input_gate);
        let emulator = Arc::new(Emulator::new(
            spec.size,
            // A reply dropped on a full queue only loses a query answer. The
            // alternative, waiting here, could deadlock reader and child.
            // Through the gate like every other write, so the PTY's order is
            // the ledger's.
            Arc::new(move |bytes| {
                let _ = reply_gate.write(bytes, InputSource::Protocol, |b| reply_tx.try_send(b));
            }),
            Arc::clone(&wake),
        ));
        // The size the screen starts at, marked before any output, so a
        // host follows this screen's every size from the first byte.
        if let Some(tap) = &tap {
            let taken = connecto_term::effective_size(initial_size);
            // Nothing feeds it yet, so it is free; a retry costs nothing.
            while !tap.resize(&mut || Some(taken)) {
                thread::yield_now();
            }
        }
        let sink = EmulatorSink {
            emulator: Arc::clone(&emulator),
            tap: tap.clone(),
        };
        let sync_flush = spawn_sync_flush(Arc::clone(&emulator))?;
        let pty = PtySession::spawn_on(pty, spec, sink, (input_tx, input_rx), wake)?;
        Ok(Self {
            epoch: NEXT_EPOCH.fetch_add(1, Ordering::SeqCst),
            emulator,
            _sync_flush: sync_flush,
            pty: Arc::new(pty),
            terminating: AtomicBool::new(false),
            hook: None,
            attempt: AtomicU64::new(0),
            agent_mode: std::sync::OnceLock::new(),
            hook_ctx: None,
            _run: None,
            _claim: None,
            size: Mutex::new(SizeControl {
                remote: false,
                local: initial_size,
                local_claims: 0,
            }),
            applied: {
                let taken = connecto_term::effective_size(initial_size);
                AtomicU32::new((u32::from(taken.cols) << 16) | u32::from(taken.rows))
            },
            input_gate,
            tap,
        })
    }

    /// Write `bytes` to the child, as written by `source`. The only way into
    /// this session's PTY: provenance and the enqueue happen under one lock.
    pub fn write_input(&self, bytes: Vec<u8>, source: InputSource) -> Result<(), InputRefused> {
        self.input_gate
            .write(bytes, source, |b| self.pty.input().try_send(b))
    }

    /// [`Self::write_input`] only on input generation `expected` (when
    /// given): compared, enqueued and counted under the input gate's one
    /// lock, which every writer of this session goes through.
    ///
    /// # Errors
    /// `Stale` for another generation (nothing written), and as
    /// `write_input`.
    pub fn write_input_if(
        &self,
        expected: Option<connecto_core::InputGeneration>,
        output: Option<u64>,
        bytes: Vec<u8>,
        source: InputSource,
    ) -> Result<(), InputRefused> {
        if expected.is_some_and(|g| g.epoch != self.epoch) {
            return Err(InputRefused::Stale);
        }
        let write = |bytes: Vec<u8>| {
            self.input_gate.write_if(
                expected.map(|g| g.writes),
                bytes,
                source,
                |b| self.pty.input().try_send(b),
                || InputRefused::Stale,
            )
        };
        let Some(output) = output else {
            return write(bytes);
        };
        // The screen held from the comparison through the enqueue: a redraw
        // cannot come between them. A busy screen is waited for a little.
        let mut bytes = Some(bytes);
        for _ in 0..200 {
            let held = self.emulator.hold_generation(|now| {
                if now != output {
                    return Err(InputRefused::Stale);
                }
                write(bytes.take().unwrap_or_default())
            });
            if let Some(result) = held {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(InputRefused::Stale)
    }

    /// The screen's generation now (what a snapshot's `generation` says).
    #[cfg(test)]
    pub(crate) fn output_generation(&self) -> u64 {
        self.emulator.hold_generation(|g| g).unwrap_or(u64::MAX)
    }

    /// Every byte enqueued to this session's input, in order.
    #[cfg(test)]
    pub(crate) fn input_log(&self) -> Vec<u8> {
        self.input_gate.log()
    }

    /// The writes this session's input took, by anyone: the window, a peer,
    /// Connecto. A write conditional on what was read checks it has not moved.
    pub fn input_generation(&self) -> connecto_core::InputGeneration {
        connecto_core::InputGeneration {
            epoch: self.epoch,
            writes: self.input_gate.writes(),
        }
    }

    /// A prompt hook dequeued from this session's hook channel by an actor at
    /// `attempt`: who wrote that submission (see [`crate::input_ledger`]).
    pub(crate) fn prompt_verdict(&self, attempt: u64, text: &str, truncated: bool) -> Verdict {
        self.input_gate.prompt(attempt, text, truncated)
    }

    /// A turn end dequeued from this session's hook channel by an actor at
    /// `attempt`; `known` is false when the event could not be read.
    pub(crate) fn turn_ended(&self, attempt: u64, known: bool) {
        self.input_gate.turn_ended(attempt, known);
    }

    /// The tool was launched with a prompt on its command line: its hook is
    /// expected, so it cannot pass for a typed submission's.
    pub(crate) fn expect_startup_prompt(&self) {
        self.input_gate.expect_startup_prompt();
    }

    /// The tool (re)started, or hook events were lost: no prompt is the
    /// user's until the terminal is quiet again.
    pub(crate) fn taint_input(&self) {
        self.input_gate.taint();
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
        self.input_gate.set_attempt(attempt);
    }

    /// The pane's attempt number for this session.
    #[must_use]
    pub fn attempt(&self) -> u64 {
        self.attempt.load(Ordering::SeqCst)
    }

    /// Record the permission mode the agent was started with. Only the
    /// launch sets it, once.
    pub(crate) fn set_agent_mode(&self, mode: connecto_core::AgentMode) {
        let _ = self.agent_mode.set(mode);
    }

    /// The permission mode this session's agent was started with: what it
    /// may do without asking. None for a shell or a utility session.
    #[must_use]
    pub fn agent_mode(&self) -> Option<connecto_core::AgentMode> {
        self.agent_mode.get().copied()
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

    /// The visible screen as text, or None while it is busy. Never blocks.
    #[must_use]
    pub fn try_text_screen(&self) -> Option<String> {
        self.emulator.try_text_screen()
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
    /// Apply the local view's last wish, unless a peer took the size again
    /// meanwhile.
    fn give_back(&self, local: GridSize) {
        for _ in 0..50 {
            let held = self.size.lock().unwrap_or_else(PoisonError::into_inner);
            if held.remote {
                return;
            }
            if self.apply_size(local) != ResizeOutcome::Retry {
                return;
            }
            drop(held);
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn apply_size(&self, size: GridSize) -> ResizeOutcome {
        // As the emulator takes it, so a repaint and this agree.
        let taken = connecto_term::effective_size(size);
        let mut outcome = ResizeOutcome::Retry;
        let mut resize = || {
            outcome = self.emulator.try_resize(size);
            (outcome == ResizeOutcome::Applied).then(|| {
                // Kept before the PTY moves: a peer's stream that reads the
                // redraw this causes sends the size first.
                self.applied.store(
                    (u32::from(taken.cols) << 16) | u32::from(taken.rows),
                    Ordering::SeqCst,
                );
                taken
            })
        };
        // Through the tap, with no output fed meanwhile: a host marks the
        // size at the byte where the screen took it. A tap busy with output
        // leaves the outcome `Retry`, as a busy screen does: never waited for.
        match &self.tap {
            Some(tap) => {
                let _ = tap.resize(&mut resize);
            }
            None => {
                let _ = resize();
            }
        }
        if outcome == ResizeOutcome::Applied {
            // Fails only once the child is gone, when no one needs the size.
            // The child is given the size its viewers draw.
            self.pty.resize(taken);
        }
        outcome
    }
}

impl TerminalSession for LiveSession {
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        self.emulator.poll_snapshot(out)
    }

    fn poll_capture(
        &self,
        max_cells: usize,
        flush: bool,
        screen: &mut TermSnapshot,
        history: &mut TermSnapshot,
    ) -> SnapshotOutcome {
        if self.emulator.try_capture(max_cells, flush, screen, history) {
            SnapshotOutcome::Updated
        } else {
            SnapshotOutcome::Contended
        }
    }

    /// A direct write to a session is Connecto's (the relay actor, utility
    /// runs), never the user's; the window comes through `ManagedPane`.
    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        self.write_input(bytes, InputSource::Connecto)
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
            self.give_back(local);
        }
    }

    fn claim_local_size(&self) {
        let local = {
            let mut control = self.size.lock().unwrap_or_else(PoisonError::into_inner);
            if !control.remote {
                return;
            }
            control.remote = false;
            // Counted before the PTY moves, so a peer's stream that reads the
            // redraw this causes has already been told it lost the size.
            control.local_claims += 1;
            control.local
        };
        self.give_back(local);
    }

    fn applied_size(&self) -> Option<GridSize> {
        let packed = self.applied.load(Ordering::SeqCst);
        Some(GridSize {
            cols: (packed >> 16) as u16,
            rows: (packed & 0xffff) as u16,
        })
    }

    fn local_size_claims(&self) -> u64 {
        self.size
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .local_claims
    }

    fn scroll(&self, lines: i32) -> bool {
        self.emulator.try_scroll(lines)
    }

    fn select(&self, op: SelectOp) -> bool {
        self.emulator.try_select(op)
    }

    fn selection_text(&self) -> SelectionText {
        self.emulator.selection_text()
    }

    fn clean_selection_text(&self) -> SelectionText {
        self.emulator.clean_selection_text()
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
