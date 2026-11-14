//! The host: lets one authenticated peer look at and type into terminals.
//!
//! One connection is served on the calling thread plus scoped threads: one for
//! the control stream and, per attached terminal, one reading the peer's input
//! while the serving thread streams output. Nothing is served before the
//! handshake ends: the first stream must say `Hello` and answer the challenge,
//! and only then are further streams accepted at all.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use connecto_core::{
    GridSize, OutputTap, ResizeOutcome, SessionStatus, TermSnapshot, TerminalSession,
};

use crate::auth::{AuthorizedKeySet, HostAuth};
use crate::guard::{Peer, RemoteGuard, deny_reason};
use crate::repaint::repaint;
use crate::seq::{Ring, RingRead};
use crate::transport::{Connection, Stream};
use crate::wire::{DenyReason, MAX_DATA, ManageOp, Message, RemoteStatus, TerminalInfo};

/// How long a peer gets to say something while the host waits for it.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);
/// Unacknowledged output the host sends before it waits. A peer that stays
/// behind falls off the ring and gets a repaint instead.
pub const SEND_WINDOW: u64 = 512 * 1024;

/// Who decides a terminal's size while a peer is attached. The one place this
/// is decided: the user has not chosen yet, so the peer drives the host's PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizePolicy {
    /// An attach and a resize from the peer resize the host's PTY.
    PeerDrivesHost,
    /// The host's grid stands; the peer adopts it.
    HostOnly,
}

impl ResizePolicy {
    const fn peer_may_resize(self) -> bool {
        matches!(self, Self::PeerDrivesHost)
    }
}

/// The policy in force.
pub const RESIZE_POLICY: ResizePolicy = ResizePolicy::PeerDrivesHost;

/// Feeds the emulator and numbers the bytes in one step.
pub struct RingTap {
    ring: Arc<Ring>,
    /// Whether output is kept. Off, bytes are only numbered.
    record: Arc<AtomicBool>,
}

impl RingTap {
    #[must_use]
    pub const fn new(ring: Arc<Ring>, record: Arc<AtomicBool>) -> Self {
        Self { ring, record }
    }

    /// A tap that always keeps output.
    #[must_use]
    pub fn recording(ring: Arc<Ring>) -> Self {
        Self::new(ring, Arc::new(AtomicBool::new(true)))
    }
}

impl OutputTap for RingTap {
    fn feed(&self, bytes: &[u8], emulator_feed: &mut dyn FnMut(&[u8])) {
        self.ring
            .append_with(bytes, emulator_feed, self.record.load(Ordering::Relaxed));
    }
}

/// A terminal the host can show: its session, and the ring its tap fills.
#[derive(Clone)]
pub struct HostTerminal {
    pub id: u32,
    pub name: String,
    /// See [`TerminalInfo::tool`].
    pub tool: u8,
    /// The terminal's attempt number, for the audit record.
    pub attempt: u64,
    /// The status dot, as [`TerminalInfo::activity`].
    pub activity: u8,
    pub session: Arc<dyn TerminalSession>,
    pub ring: Arc<Ring>,
}

impl HostTerminal {
    fn grid(&self) -> (u16, u16) {
        let mut snap = TermSnapshot::default();
        self.session.poll_snapshot(&mut snap);
        if snap.cols == 0 || snap.rows == 0 {
            (80, 24)
        } else {
            (snap.cols, snap.rows)
        }
    }

    fn info(&self) -> TerminalInfo {
        let (cols, rows) = self.grid();
        TerminalInfo {
            id: self.id,
            name: self.name.clone(),
            tool: self.tool,
            running: self.session.status() == SessionStatus::Running,
            activity: self.activity,
            cols,
            rows,
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        for _ in 0..50 {
            match self.session.resize_remote(GridSize { cols, rows }) {
                ResizeOutcome::Retry => thread::sleep(Duration::from_millis(2)),
                ResizeOutcome::Applied | ResizeOutcome::Failed => return,
            }
        }
    }

    /// The screen as of a known sequence number.
    fn capture(&self) -> Option<(u64, u16, u16, Vec<u8>)> {
        for _ in 0..200 {
            let got = self.ring.consistent(|end| {
                let mut snap = TermSnapshot::default();
                match self.session.poll_snapshot(&mut snap) {
                    connecto_core::SnapshotOutcome::Updated
                    | connecto_core::SnapshotOutcome::Unchanged => Some((end, snap)),
                    _ => None,
                }
            });
            if let Some((end, snap)) = got {
                return Some((end, snap.cols, snap.rows, repaint(&snap)));
            }
            thread::sleep(Duration::from_millis(1));
        }
        None
    }
}

/// The terminals a host offers, now.
pub trait HostTerminals: Send + Sync {
    fn list(&self) -> Vec<HostTerminal>;

    /// Do what a peer asked to the host's terminals, through the same seam the
    /// window uses. The new terminal's id is returned when it is known at once.
    ///
    /// # Errors
    /// With the reason, as the peer will read it.
    fn manage(&self, op: &ManageOp) -> Result<Option<u32>, String> {
        let _ = op;
        Err("this machine does not let peers manage terminals".to_owned())
    }

    /// Whether terminal output is being kept for peers. Off, the ring only
    /// numbers the bytes, so nothing is retained while nobody can look.
    fn set_recording(&self, on: bool) {
        let _ = on;
    }
}

/// Where the host reads its authorized keys, each time a peer connects.
pub type KeySource = Arc<dyn Fn() -> Arc<AuthorizedKeySet> + Send + Sync>;

/// Serves remote peers, one at a time.
pub struct HostService {
    terminals: Arc<dyn HostTerminals>,
    keys: KeySource,
    guard: Arc<dyn RemoteGuard>,
    busy: AtomicBool,
}

/// Holds a terminal's size for a peer, and gives it back when dropped, whatever
/// way the stream ended.
struct SizeLease<'a>(&'a dyn TerminalSession);

impl<'a> SizeLease<'a> {
    fn take(session: &'a dyn TerminalSession) -> Self {
        session.set_remote_size_control(true);
        Self(session)
    }
}

impl Drop for SizeLease<'_> {
    fn drop(&mut self) {
        self.0.set_remote_size_control(false);
    }
}

struct BusyLease<'a>(&'a AtomicBool);

impl Drop for BusyLease<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl HostService {
    #[must_use]
    pub fn new(
        terminals: Arc<dyn HostTerminals>,
        keys: KeySource,
        guard: Arc<dyn RemoteGuard>,
    ) -> Self {
        Self {
            terminals,
            keys,
            guard,
            busy: AtomicBool::new(false),
        }
    }

    /// Serve `conn` until it ends. Blocks; run it on a thread of its own.
    pub fn serve(&self, conn: &dyn Connection) {
        if !self.guard.enabled() {
            conn.close();
            return;
        }
        self.serve_inner(conn);
        conn.close();
    }

    fn deny(&self, control: &dyn Stream, peer: &Peer, reason: DenyReason) {
        // Recorded first, so a peer that has seen the refusal has been logged.
        self.guard.connected(peer, Err(&reason.to_string()));
        let _ = control.send(&Message::Denied(reason));
    }

    fn serve_inner(&self, conn: &dyn Connection) {
        let mut peer = Peer {
            fingerprint: String::new(),
            endpoint: conn.remote_id(),
        };
        let Ok(Some(control)) = conn.accept_stream(PEER_TIMEOUT) else {
            return;
        };
        let control: Arc<dyn Stream> = Arc::from(control);
        let Ok(binding) = conn.channel_binding() else {
            return self.deny(&*control, &peer, DenyReason::Protocol);
        };
        let mut auth = HostAuth::new(conn.local_id(), conn.remote_id(), binding, (self.keys)());
        match control.recv(PEER_TIMEOUT) {
            Ok(Some(Message::Hello { version })) => match auth.on_hello(version) {
                Ok(challenge) => {
                    if control.send(&challenge).is_err() {
                        return;
                    }
                }
                Err(reason) => return self.deny(&*control, &peer, reason),
            },
            Ok(_) => return self.deny(&*control, &peer, DenyReason::Protocol),
            Err(_) => return,
        }
        match control.recv(PEER_TIMEOUT) {
            Ok(Some(Message::AuthResponse {
                public_key,
                signature,
            })) => match auth.on_response(&public_key, &signature) {
                Ok(authed) => peer.fingerprint = authed.fingerprint,
                Err(reason) => return self.deny(&*control, &peer, reason),
            },
            Ok(_) => return self.deny(&*control, &peer, DenyReason::Protocol),
            Err(_) => return,
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return self.deny(&*control, &peer, DenyReason::Busy);
        }
        let _lease = BusyLease(&self.busy);
        if control.send(&Message::AuthOk).is_err() {
            return;
        }
        self.guard.connected(&peer, Ok(()));

        let stop = AtomicBool::new(false);
        let (peer, stop, this) = (&peer, &stop, self);
        thread::scope(|scope| {
            let control = Arc::clone(&control);
            scope.spawn(move || this.control_loop(peer, &*control, stop));
            while !stop.load(Ordering::SeqCst) {
                match conn.accept_stream(POLL) {
                    Ok(Some(stream)) => {
                        scope.spawn(move || this.terminal_stream(peer, &*stream, stop));
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
            stop.store(true, Ordering::SeqCst);
        });
    }

    fn control_loop(&self, peer: &Peer, control: &dyn Stream, stop: &AtomicBool) {
        while !stop.load(Ordering::SeqCst) {
            match control.recv(POLL) {
                Ok(Some(Message::ListTerminals)) => {
                    let list = self
                        .terminals
                        .list()
                        .iter()
                        .map(HostTerminal::info)
                        .collect();
                    if control.send(&Message::TerminalList(list)).is_err() {
                        break;
                    }
                }
                Ok(Some(Message::Manage { req, op })) => {
                    let outcome = self.manage(peer, &op);
                    if control
                        .send(&Message::ManageResult { req, outcome })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Some(Message::Bye)) | Err(_) => break,
                Ok(Some(_)) => {
                    let _ = control.send(&Message::Denied(DenyReason::Protocol));
                }
                Ok(None) => {}
            }
        }
        stop.store(true, Ordering::SeqCst);
    }

    /// One management request: the terminal must exist, the guard (the Gate and
    /// the audit log, as the `remote` actor, and Pause) must allow it, and then
    /// the host's own terminal registry does it.
    fn manage(&self, peer: &Peer, op: &ManageOp) -> Result<Option<u32>, String> {
        let (action, target) = match op {
            ManageOp::Add { .. } => ("add", None),
            ManageOp::Close { terminal } => ("close", Some(*terminal)),
            ManageOp::Restart { terminal } => ("restart", Some(*terminal)),
            ManageOp::Rename { terminal, .. } => ("rename", Some(*terminal)),
        };
        let (name, attempt) = match target {
            None => (String::new(), 0),
            Some(id) => {
                let found = self.terminals.list().into_iter().find(|t| t.id == id);
                match found {
                    Some(t) => (t.name, t.attempt),
                    None => return Err(DenyReason::NoSuchTerminal.to_string()),
                }
            }
        };
        self.guard
            .manage(peer, action, &name, attempt)
            .map_err(|denied| deny_reason(denied).to_string())?;
        self.terminals.manage(op)
    }

    fn terminal_stream(&self, peer: &Peer, stream: &dyn Stream, stop: &AtomicBool) {
        let Ok(Some(Message::Attach {
            terminal,
            resume_seq,
            cols,
            rows,
        })) = stream.recv(PEER_TIMEOUT)
        else {
            let _ = stream.send(&Message::Denied(DenyReason::Protocol));
            return;
        };
        let Some(term) = self.terminals.list().into_iter().find(|t| t.id == terminal) else {
            let _ = stream.send(&Message::Denied(DenyReason::NoSuchTerminal));
            return;
        };
        if let Err(reason) = self.guard.attach(peer, &term.name, term.attempt) {
            let _ = stream.send(&Message::Denied(deny_reason(reason)));
            return;
        }
        // While this peer is attached it has the terminal's size: the host's
        // own view follows it, and gets the size back when the peer detaches.
        let _lease = if RESIZE_POLICY.peer_may_resize() {
            let lease = SizeLease::take(&*term.session);
            if self.guard.resize(peer, &term.name, term.attempt).is_ok() {
                term.resize(cols, rows);
            }
            Some(lease)
        } else {
            None
        };

        // A first attach, or a position the ring no longer holds, gets a
        // repaint. A position it holds is resumed exactly.
        let resumable = resume_seq > 0
            && matches!(
                term.ring.read(resume_seq, 0),
                RingRead::Data { .. } | RingRead::Empty
            );
        let mut cursor = resume_seq;
        if resumable {
            let (c, r) = term.grid();
            if stream
                .send(&Message::Attached {
                    start_seq: resume_seq,
                    resync: false,
                    cols: c,
                    rows: r,
                })
                .is_err()
            {
                return;
            }
        } else if !self.send_repaint(stream, &term, true, &mut cursor) {
            return;
        }
        let _ = stream.send(&Message::Status(RemoteStatus::Running));

        let acked = AtomicU64::new(cursor);
        let ended = AtomicBool::new(false);
        let acked_ref = &acked;
        let ended_ref = &ended;
        let term_ref = &term;
        thread::scope(|scope| {
            scope.spawn(move || {
                self.read_peer(peer, stream, term_ref, stop, ended_ref, acked_ref);
            });
            self.write_peer(stream, &term, stop, &ended, &acked, &mut cursor);
            ended.store(true, Ordering::SeqCst);
        });
        self.guard.detach(peer, &term.name, term.attempt);
    }

    /// Send a screen. `first` makes it the `Attached` reply.
    fn send_repaint(
        &self,
        stream: &dyn Stream,
        term: &HostTerminal,
        first: bool,
        cursor: &mut u64,
    ) -> bool {
        let Some((seq, cols, rows, bytes)) = term.capture() else {
            let _ = stream.send(&Message::Denied(DenyReason::Protocol));
            return false;
        };
        if first
            && stream
                .send(&Message::Attached {
                    start_seq: seq,
                    resync: true,
                    cols,
                    rows,
                })
                .is_err()
        {
            return false;
        }
        *cursor = seq;
        stream
            .send(&Message::Resync {
                seq,
                cols,
                rows,
                repaint: bytes,
            })
            .is_ok()
    }

    fn read_peer(
        &self,
        peer: &Peer,
        stream: &dyn Stream,
        term: &HostTerminal,
        stop: &AtomicBool,
        ended: &AtomicBool,
        acked: &AtomicU64,
    ) {
        while !stop.load(Ordering::SeqCst) && !ended.load(Ordering::SeqCst) {
            match stream.recv(POLL) {
                Ok(Some(Message::Input(bytes))) => {
                    match self.guard.input(peer, &term.name, term.attempt, &bytes) {
                        Ok(()) => {
                            if term.session.send_input(bytes).is_err() {
                                let _ = stream.send(&Message::Denied(DenyReason::Busy));
                            }
                        }
                        Err(reason) => {
                            let _ = stream.send(&Message::Denied(deny_reason(reason)));
                        }
                    }
                }
                Ok(Some(Message::Resize { cols, rows })) => {
                    match self.guard.resize(peer, &term.name, term.attempt) {
                        Ok(()) => {
                            if RESIZE_POLICY.peer_may_resize() {
                                term.resize(cols, rows);
                            }
                            let (c, r) = term.grid();
                            let _ = stream.send(&Message::Size { cols: c, rows: r });
                        }
                        Err(reason) => {
                            let _ = stream.send(&Message::Denied(deny_reason(reason)));
                        }
                    }
                }
                Ok(Some(Message::Ack { seq })) => {
                    acked.fetch_max(seq, Ordering::SeqCst);
                }
                Ok(Some(Message::Bye)) | Err(_) => break,
                Ok(Some(_)) => {
                    let _ = stream.send(&Message::Denied(DenyReason::Protocol));
                }
                Ok(None) => {}
            }
        }
        ended.store(true, Ordering::SeqCst);
    }

    fn write_peer(
        &self,
        stream: &dyn Stream,
        term: &HostTerminal,
        stop: &AtomicBool,
        ended: &AtomicBool,
        acked: &AtomicU64,
        cursor: &mut u64,
    ) {
        let mut told_exit = false;
        while !stop.load(Ordering::SeqCst) && !ended.load(Ordering::SeqCst) {
            if cursor.saturating_sub(acked.load(Ordering::SeqCst)) > SEND_WINDOW {
                // The peer is behind. Hold off; the ring keeps moving, and if
                // it moves past the peer, the peer is repainted below.
                thread::sleep(Duration::from_millis(2));
                if matches!(
                    term.ring.read(*cursor, 0),
                    RingRead::Data { .. } | RingRead::Empty
                ) {
                    continue;
                }
            }
            match term.ring.read_wait(*cursor, MAX_DATA, POLL) {
                RingRead::Data { seq, bytes } => {
                    let len = bytes.len() as u64;
                    if stream.send(&Message::Output { seq, bytes }).is_err() {
                        return;
                    }
                    *cursor = seq + len;
                }
                RingRead::Empty => {
                    // The terminal restarted: its ring was closed and a new
                    // one made. This peer is let go and attaches again.
                    if term.ring.is_closed() {
                        return;
                    }
                    if !told_exit {
                        match term.session.status() {
                            SessionStatus::Running => {}
                            SessionStatus::Exited { code, signal } => {
                                told_exit = true;
                                let _ = stream
                                    .send(&Message::Status(RemoteStatus::Exited { code, signal }));
                            }
                            SessionStatus::Unknown { .. } => {
                                told_exit = true;
                                let _ = stream.send(&Message::Status(RemoteStatus::Unknown));
                            }
                        }
                    }
                }
                RingRead::Evicted { .. } | RingRead::Future { .. } => {
                    if !self.send_repaint(stream, term, false, cursor) {
                        return;
                    }
                    acked.store(*cursor, Ordering::SeqCst);
                }
            }
        }
    }
}

/// A [`HostTerminals`] over a fixed list, for tests and simple hosts.
pub struct FixedTerminals(pub Mutex<Vec<HostTerminal>>);

impl HostTerminals for FixedTerminals {
    fn list(&self) -> Vec<HostTerminal> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
