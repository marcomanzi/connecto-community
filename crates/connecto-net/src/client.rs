//! The client: connects to a host, proves it holds an allowed key, and shows a
//! host's terminal as a [`TerminalSession`] the UI already knows how to draw.
//!
//! The terminal keeps its own emulator, fed with the host's output bytes, so
//! drawing, scrollback and selection are the local code. Its replies to
//! terminal queries are dropped: the host's emulator answers those, and a
//! second answer would reach the program twice.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use arc_swap::ArcSwap;
use connecto_core::{
    GridSize, InputRefused, ResizeOutcome, SessionStatus, SnapshotOutcome, TermSnapshot,
    TerminalSession,
};
use connecto_term::Emulator;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

use crate::auth::{SignError, Signer, client_response};
use crate::seq::{Accept, Tracker};
use crate::transport::{Connection, EndpointId, NetError, PathKind, Stream};
use crate::wire::{
    DenyReason, MAX_DATA, ManageOp, Message, PROTOCOL_VERSION, RemoteStatus, TerminalAddr,
    TerminalInfo,
};

const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);
/// Applied bytes between acknowledgements.
const ACK_EVERY: u64 = 32 * 1024;
const INPUT_QUEUE: usize = 1024;

/// Why connecting or attaching failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the machine that answered is not the one saved for this host")]
    HostMismatch,
    #[error("the host refused: {0}")]
    Denied(DenyReason),
    #[error("{0}")]
    Net(#[from] NetError),
    #[error("{0}")]
    Sign(#[from] SignError),
    #[error("the host sent something unexpected")]
    Protocol,
    #[error("{0}")]
    Remote(String),
}

/// A connection to one host, after the handshake.
pub struct RemoteClient {
    next_req: AtomicU32,
    host: String,
    conn: Arc<dyn Connection>,
    control: Mutex<Box<dyn Stream>>,
}

impl RemoteClient {
    /// Connect and authenticate. `expected_host` is the pinned identity: a
    /// connection to any other machine is refused before anything is signed.
    ///
    /// # Errors
    /// See [`ClientError`].
    pub fn connect(
        conn: Arc<dyn Connection>,
        host_name: &str,
        expected_host: &EndpointId,
        signer: &dyn Signer,
    ) -> Result<Self, ClientError> {
        if &conn.remote_id() != expected_host {
            conn.close();
            return Err(ClientError::HostMismatch);
        }
        let control = conn.open_stream()?;
        control.send(&Message::Hello {
            version: PROTOCOL_VERSION,
        })?;
        let nonce = match control.recv(REPLY_TIMEOUT)? {
            Some(Message::Challenge { nonce }) => nonce,
            Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
            _ => return Err(ClientError::Protocol),
        };
        let binding = conn.channel_binding()?;
        let response = client_response(
            signer,
            &nonce,
            &conn.local_id(),
            &conn.remote_id(),
            &binding,
        )?;
        control.send(&response)?;
        match control.recv(REPLY_TIMEOUT)? {
            Some(Message::AuthOk) => {}
            Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
            _ => return Err(ClientError::Protocol),
        }
        Ok(Self {
            next_req: AtomicU32::new(1),
            host: host_name.to_owned(),
            conn,
            control: Mutex::new(control),
        })
    }

    /// The display name this client knows the host by.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Whether traffic goes straight to the host or through the relay.
    #[must_use]
    pub fn path(&self) -> PathKind {
        self.conn.path()
    }

    /// The host's terminals, addressed as `(host, terminal)`.
    ///
    /// # Errors
    /// On a closed connection or an unexpected reply.
    pub fn list(&self) -> Result<Vec<(TerminalAddr, TerminalInfo)>, ClientError> {
        let control = self.control.lock().unwrap_or_else(PoisonError::into_inner);
        control.send(&Message::ListTerminals)?;
        loop {
            match control.recv(REPLY_TIMEOUT)? {
                Some(Message::TerminalList(list)) => {
                    return Ok(list
                        .into_iter()
                        .map(|info| {
                            (
                                TerminalAddr {
                                    host: self.host.clone(),
                                    terminal: info.id,
                                },
                                info,
                            )
                        })
                        .collect());
                }
                Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
                Some(_) => {}
                None => return Err(ClientError::Protocol),
            }
        }
    }

    /// Ask the host to add, close, restart or rename a terminal. Returns the new
    /// terminal's id when the host knows it at once.
    ///
    /// # Errors
    /// `Remote` with the host's reason (Pause, no such terminal...), or a
    /// transport error.
    pub fn manage(&self, op: ManageOp) -> Result<Option<u32>, ClientError> {
        let req = self.next_req.fetch_add(1, Ordering::SeqCst);
        let control = self.control.lock().unwrap_or_else(PoisonError::into_inner);
        control.send(&Message::Manage { req, op })?;
        let deadline = std::time::Instant::now() + REPLY_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match control.recv(Duration::from_millis(200))? {
                Some(Message::ManageResult { req: got, outcome }) if got == req => {
                    return outcome.map_err(ClientError::Remote);
                }
                Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
                _ => {}
            }
        }
        Err(ClientError::Protocol)
    }

    /// Say goodbye and close.
    pub fn close(&self) {
        let control = self.control.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = control.send(&Message::Bye);
        self.conn.close();
    }
}

struct Shared {
    tracker: Mutex<Tracker>,
    status: ArcSwap<SessionStatus>,
    denied: ArcSwap<Option<DenyReason>>,
    /// The grid the host last reported.
    size: Mutex<GridSize>,
    /// The link that may change this session. A reader of an older link
    /// finds it is not current and leaves everything alone.
    generation: AtomicU64,
    /// Repaints received, which a resume does not need.
    resyncs: AtomicU64,
    /// An attach succeeded once, so a later one resumes.
    attached_once: AtomicBool,
}

/// What a live link to the host holds.
struct Link {
    input: Sender<Vec<u8>>,
    resize: Sender<GridSize>,
    closed: Arc<AtomicBool>,
}

fn apply_size(emulator: &Emulator, shared: &Shared, cols: u16, rows: u16) {
    let size = GridSize { cols, rows };
    for _ in 0..50 {
        if emulator.try_resize(size) != ResizeOutcome::Retry {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    *shared.size.lock().unwrap_or_else(PoisonError::into_inner) = size;
}

fn read_loop(
    stream: &dyn Stream,
    emulator: &Emulator,
    shared: &Shared,
    generation: u64,
    closed: &AtomicBool,
) {
    let current = || shared.generation.load(Ordering::SeqCst) == generation;
    let mut unacked = 0_u64;
    loop {
        let message = match stream.recv(POLL) {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(_) => break,
        };
        if !current() {
            break;
        }
        match message {
            Message::Output { seq, bytes } => {
                let verdict = shared
                    .tracker
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .accept(seq, bytes.len());
                match verdict {
                    Accept::Apply { skip } => {
                        emulator.feed(bytes.get(skip..).unwrap_or_default());
                        unacked += (bytes.len() - skip) as u64;
                        if unacked >= ACK_EVERY {
                            unacked = 0;
                            let next = shared
                                .tracker
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .next();
                            let _ = stream.send(&Message::Ack { seq: next });
                        }
                    }
                    Accept::Duplicate => {}
                    Accept::Gap { .. } => {
                        // Bytes are missing: drop the link. The next attach
                        // resumes at `position()` and is repainted if need be.
                        break;
                    }
                }
            }
            Message::Resync {
                seq,
                cols,
                rows,
                repaint,
            } => {
                apply_size(emulator, shared, cols, rows);
                emulator.feed(&repaint);
                shared
                    .tracker
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .reset(seq);
                shared.resyncs.fetch_add(1, Ordering::SeqCst);
                unacked = 0;
                let _ = stream.send(&Message::Ack { seq });
            }
            Message::Size { cols, rows } => apply_size(emulator, shared, cols, rows),
            Message::Status(status) => {
                shared.status.store(Arc::new(match status {
                    RemoteStatus::Running => SessionStatus::Running,
                    RemoteStatus::Exited { code, signal } => SessionStatus::Exited { code, signal },
                    RemoteStatus::Unknown => SessionStatus::Unknown {
                        reason: "the host lost track of the process".to_owned(),
                    },
                }));
            }
            Message::Denied(reason) => shared.denied.store(Arc::new(Some(reason))),
            _ => {}
        }
    }
    closed.store(true, Ordering::SeqCst);
    if current() && matches!(**shared.status.load(), SessionStatus::Running) {
        shared.status.store(Arc::new(SessionStatus::Unknown {
            reason: "connection lost".to_owned(),
        }));
    }
}

fn write_loop(
    stream: &dyn Stream,
    input: &Receiver<Vec<u8>>,
    resize: &Receiver<GridSize>,
    closed: &AtomicBool,
) {
    loop {
        if closed.load(Ordering::SeqCst) {
            return;
        }
        while let Ok(size) = resize.try_recv() {
            if stream
                .send(&Message::Resize {
                    cols: size.cols,
                    rows: size.rows,
                })
                .is_err()
            {
                return;
            }
        }
        match input.recv_timeout(Duration::from_millis(20)) {
            Ok(mut bytes) => {
                // Whatever else is queued goes in the same message, but the
                // first bytes never wait for more.
                while bytes.len() < MAX_DATA {
                    let Ok(more) = input.try_recv() else { break };
                    bytes.extend_from_slice(&more);
                }
                for chunk in bytes.chunks(MAX_DATA) {
                    if stream.send(&Message::Input(chunk.to_vec())).is_err() {
                        return;
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                let _ = stream.send(&Message::Bye);
                return;
            }
        }
    }
}

/// A host's terminal, as a local terminal session. It outlives connections:
/// the screen stays, and [`RemoteSession::attach`] joins it to a new link,
/// resuming at the last byte it applied.
pub struct RemoteSession {
    addr: TerminalAddr,
    emulator: Arc<Emulator>,
    shared: Arc<Shared>,
    link: Mutex<Option<Link>>,
}

impl RemoteSession {
    /// A session with no link yet. Its status is unknown until it attaches.
    #[must_use]
    pub fn new(addr: TerminalAddr, size: GridSize, wake: Arc<dyn Fn() + Send + Sync>) -> Arc<Self> {
        let emulator = Arc::new(Emulator::new(size, Arc::new(|_| {}), wake));
        let shared = Arc::new(Shared {
            tracker: Mutex::new(Tracker::default()),
            status: ArcSwap::from_pointee(SessionStatus::Unknown {
                reason: "connecting".to_owned(),
            }),
            denied: ArcSwap::from_pointee(None),
            size: Mutex::new(size),
            generation: AtomicU64::new(0),
            resyncs: AtomicU64::new(0),
            attached_once: AtomicBool::new(false),
        });
        Arc::new(Self {
            addr,
            emulator,
            shared,
            link: Mutex::new(None),
        })
    }

    /// Join this session to `client`'s connection and start following the
    /// host's terminal `terminal`. After the first time it resumes at the last
    /// applied byte; the host repaints only if it no longer holds that point.
    ///
    /// # Errors
    /// When the host refuses or the stream fails.
    pub fn attach(&self, client: &RemoteClient, terminal: u32) -> Result<(), ClientError> {
        let resume = self
            .shared
            .attached_once
            .load(Ordering::SeqCst)
            .then(|| self.position())
            .filter(|p| *p > 0);
        let size = self.host_size();
        let stream: Arc<dyn Stream> = Arc::from(client.conn.open_stream()?);
        stream.send(&Message::Attach {
            terminal,
            resume_seq: resume.unwrap_or(0),
            cols: size.cols,
            rows: size.rows,
        })?;
        let (cols, rows) = match stream.recv(REPLY_TIMEOUT)? {
            Some(Message::Attached { cols, rows, .. }) => (cols, rows),
            Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
            _ => return Err(ClientError::Protocol),
        };
        apply_size(&self.emulator, &self.shared, cols, rows);
        let generation = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared.attached_once.store(true, Ordering::SeqCst);
        self.shared.status.store(Arc::new(SessionStatus::Running));
        let closed = Arc::new(AtomicBool::new(false));
        let (input_tx, input_rx) = bounded(INPUT_QUEUE);
        let (resize_tx, resize_rx) = bounded(8);
        {
            let (stream, emulator, shared, closed) = (
                Arc::clone(&stream),
                Arc::clone(&self.emulator),
                Arc::clone(&self.shared),
                Arc::clone(&closed),
            );
            thread::Builder::new()
                .name("remote-reader".into())
                .spawn(move || read_loop(&*stream, &emulator, &shared, generation, &closed))
                .map_err(|e| NetError::Transport(e.to_string()))?;
        }
        {
            let closed = Arc::clone(&closed);
            thread::Builder::new()
                .name("remote-writer".into())
                .spawn(move || write_loop(&*stream, &input_rx, &resize_rx, &closed))
                .map_err(|e| NetError::Transport(e.to_string()))?;
        }
        *self.link.lock().unwrap_or_else(PoisonError::into_inner) = Some(Link {
            input: input_tx,
            resize: resize_tx,
            closed,
        });
        Ok(())
    }

    /// Which terminal on which host.
    #[must_use]
    pub const fn addr(&self) -> &TerminalAddr {
        &self.addr
    }

    /// The next output byte this session wants.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.shared
            .tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next()
    }

    /// How many repaints this session has taken. A resume adds none.
    #[must_use]
    pub fn resyncs(&self) -> u64 {
        self.shared.resyncs.load(Ordering::SeqCst)
    }

    /// The host's grid, which the local view must match to draw it right.
    #[must_use]
    pub fn host_size(&self) -> GridSize {
        *self
            .shared
            .size
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The last refusal the host sent, such as Pause.
    #[must_use]
    pub fn last_denied(&self) -> Option<DenyReason> {
        **self.shared.denied.load()
    }

    /// Whether a link is carrying this session now.
    #[must_use]
    pub fn is_linked(&self) -> bool {
        self.link
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|link| !link.closed.load(Ordering::SeqCst))
    }

    /// Whether the link has ended (or there never was one).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        !self.is_linked()
    }
}

impl TerminalSession for RemoteSession {
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        self.emulator.poll_snapshot(out)
    }

    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        let guard = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(link) = guard.as_ref().filter(|l| !l.closed.load(Ordering::SeqCst)) else {
            return Err(InputRefused::Closed);
        };
        link.input.try_send(bytes).map_err(|err| match err {
            TrySendError::Full(_) => InputRefused::QueueFull,
            TrySendError::Disconnected(_) => InputRefused::Closed,
        })
    }

    fn resize(&self, size: GridSize) -> ResizeOutcome {
        // The host's grid decides the local one; this only asks.
        let guard = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(link) = guard.as_ref().filter(|l| !l.closed.load(Ordering::SeqCst)) else {
            return ResizeOutcome::Failed;
        };
        match link.resize.try_send(size) {
            Ok(()) => ResizeOutcome::Applied,
            Err(TrySendError::Full(_)) => ResizeOutcome::Retry,
            Err(TrySendError::Disconnected(_)) => ResizeOutcome::Failed,
        }
    }

    fn scroll(&self, lines: i32) -> bool {
        self.emulator.try_scroll(lines)
    }

    fn forwards_mouse(&self) -> bool {
        true
    }

    fn status(&self) -> SessionStatus {
        (**self.shared.status.load()).clone()
    }
}
