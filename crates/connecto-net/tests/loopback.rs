//! The whole remote path over the in-memory transport: handshake, listing,
//! attaching, output, input, resume, repaint, Pause, and the refusals.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use connecto_audit::{Audit, Sink};
use connecto_core::RemoteGuard;
use connecto_core::{
    GridSize, InputRefused, PauseSwitch, ResizeOutcome, SessionStatus, SnapshotOutcome,
    TermSnapshot, TerminalSession,
};
use connecto_gate::Gate;
use connecto_net::auth::{AuthorizedKeySet, KeySigner, Signer};
use connecto_net::client::{ClientError, RemoteClient, RemoteSession};
use connecto_net::host::{FixedTerminals, HostService, HostTerminal, HostTerminals, RingTap};
use connecto_net::seq::Ring;
use connecto_net::transport::Connection;
use connecto_net::transport::loopback::{self, LoopbackConnection};
use connecto_net::wire::{DenyReason, ManageOp, Message, TerminalAddr};
use connecto_session::RemoteGate;
use connecto_term::Emulator;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, PrivateKey};

const HOST_ID: [u8; 32] = [0xaa; 32];
const CLIENT_ID: [u8; 32] = [0xbb; 32];

fn key() -> PrivateKey {
    PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap()
}

/// A terminal with no process: an emulator the test writes into, and a record
/// of what was typed.
struct FakeSession {
    emulator: Arc<Emulator>,
    ring: Arc<Ring>,
    typed: Mutex<Vec<u8>>,
    /// Mirrors a real session: `(peer has the size, last local wish)`.
    control: Mutex<(bool, GridSize)>,
}

impl FakeSession {
    fn new(ring_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            emulator: Arc::new(Emulator::new(
                GridSize { cols: 60, rows: 12 },
                Arc::new(|_| {}),
                Arc::new(|| {}),
            )),
            ring: Arc::new(Ring::new(ring_bytes)),
            typed: Mutex::new(Vec::new()),
            control: Mutex::new((false, GridSize { cols: 60, rows: 12 })),
        })
    }

    /// Output from the "process", through the same tap a real session uses.
    fn print(&self, bytes: &[u8]) {
        use connecto_core::OutputTap;
        RingTap::recording(Arc::clone(&self.ring)).feed(bytes, &mut |b| self.emulator.feed(b));
    }

    fn typed(&self) -> Vec<u8> {
        self.typed.lock().unwrap().clone()
    }
}

impl TerminalSession for FakeSession {
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        self.emulator.poll_snapshot(out)
    }
    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        self.typed.lock().unwrap().extend(bytes);
        Ok(())
    }
    fn resize(&self, size: GridSize) -> ResizeOutcome {
        let mut control = self.control.lock().unwrap();
        control.1 = size;
        if control.0 {
            return ResizeOutcome::Applied;
        }
        self.emulator.try_resize(size)
    }
    fn resize_remote(&self, size: GridSize) -> ResizeOutcome {
        self.emulator.try_resize(size)
    }
    fn set_remote_size_control(&self, on: bool) {
        let local = {
            let mut control = self.control.lock().unwrap();
            control.0 = on;
            control.1
        };
        if !on {
            // As a real session does, retry while the emulator is busy.
            for _ in 0..200 {
                if self.emulator.try_resize(local) != ResizeOutcome::Retry {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    fn scroll(&self, lines: i32) -> bool {
        self.emulator.try_scroll(lines)
    }
    fn status(&self) -> SessionStatus {
        SessionStatus::Running
    }
}

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

impl Sink for Lines {
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.0.lock().unwrap().push(line.to_owned());
        Ok(())
    }
}

/// The host's terminals, with a record of what peers asked to do to them.
struct Managed {
    inner: FixedTerminals,
    ops: Mutex<Vec<ManageOp>>,
    refuse: Mutex<Option<String>>,
}

impl HostTerminals for Managed {
    fn list(&self) -> Vec<HostTerminal> {
        self.inner.list()
    }
    fn manage(&self, op: &ManageOp) -> Result<Option<u32>, String> {
        self.ops.lock().unwrap().push(op.clone());
        if let Some(why) = self.refuse.lock().unwrap().clone() {
            return Err(why);
        }
        Ok(matches!(op, ManageOp::Add { .. }).then_some(99))
    }
}

struct Rig {
    managed: Arc<Managed>,
    host: Arc<HostService>,
    guard: Arc<RemoteGate>,
    pause: Arc<PauseSwitch>,
    gate: Arc<Gate>,
    log: Lines,
    session: Arc<FakeSession>,
    allowed: PrivateKey,
}

impl Rig {
    fn new(ring_bytes: usize) -> Self {
        let allowed = key();
        let session = FakeSession::new(ring_bytes);
        let log = Lines::default();
        let pause = Arc::new(PauseSwitch::default());
        let gate = Arc::new(Gate::new(
            Arc::clone(&pause),
            Audit::with_sink(log.clone()).unwrap(),
            "TEST",
        ));
        let guard = Arc::new(RemoteGate::with_gate(Arc::clone(&gate)));
        guard.set_enabled(true);
        let terminals = Arc::new(Managed {
            ops: Mutex::new(Vec::new()),
            refuse: Mutex::new(None),
            inner: FixedTerminals(Mutex::new(vec![HostTerminal {
                id: 7,
                name: "claude-1".into(),
                tool: 1,
                attempt: 1,
                activity: 0,
                session: Arc::clone(&session) as Arc<dyn TerminalSession>,
                ring: Arc::clone(&session.ring),
            }])),
        });
        let text = format!("{}\n", allowed.public_key().to_openssh().unwrap());
        let keys = Arc::new(AuthorizedKeySet::parse(&text).unwrap());
        let host = Arc::new(HostService::new(
            Arc::clone(&terminals) as Arc<dyn HostTerminals>,
            Arc::new(move || Arc::clone(&keys)),
            Arc::clone(&guard) as Arc<dyn RemoteGuard>,
        ));
        Self {
            managed: terminals,
            host,
            guard,
            pause,
            gate,
            log,
            session,
            allowed,
        }
    }

    /// A new connection with the host serving its end on a thread.
    fn connect(&self, binding: u8) -> (Arc<LoopbackConnection>, thread::JoinHandle<()>) {
        let (client, host) = loopback::pair(CLIENT_ID, HOST_ID, [binding; 32]);
        let service = Arc::clone(&self.host);
        let handle = thread::spawn(move || service.serve(&host));
        (Arc::new(client), handle)
    }

    fn client(&self, signer: &dyn Signer) -> Result<RemoteClient, ClientError> {
        let (conn, _handle) = self.connect(1);
        RemoteClient::connect(conn, "build-box", &HOST_ID, signer)
    }

    fn signer(&self) -> KeySigner {
        KeySigner(self.allowed.clone())
    }

    fn audit(&self) -> String {
        assert!(self.gate.flush(Duration::from_secs(2)));
        self.log.0.lock().unwrap().join("\n")
    }
}

fn text(snapshot: &TermSnapshot) -> String {
    (0..snapshot.rows)
        .map(|row| {
            let line: String = snapshot.row(row).iter().map(|c| c.ch).collect();
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

fn screen(session: &dyn TerminalSession) -> TermSnapshot {
    let mut snap = TermSnapshot::default();
    for _ in 0..100 {
        if session.poll_snapshot(&mut snap) == SnapshotOutcome::Updated {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    snap
}

fn wait_for_text(session: &dyn TerminalSession, needle: &str) {
    wait_until(needle, || text(&screen(session)).contains(needle));
}

/// A new client, once the host has freed its one slot.
fn reconnect(rig: &Rig) -> RemoteClient {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rig.client(&rig.signer()) {
            Ok(client) => return client,
            Err(ClientError::Denied(DenyReason::Busy)) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!("{err}"),
        }
    }
}

fn attach(client: &RemoteClient) -> Arc<RemoteSession> {
    let session = RemoteSession::new(
        TerminalAddr {
            host: client.host().to_owned(),
            terminal: 7,
        },
        GridSize { cols: 60, rows: 12 },
        Arc::new(|| {}),
    );
    session.attach(client, 7).unwrap();
    session
}

/// The visible screens match, cell for cell.
fn assert_screens_equal(a: &dyn TerminalSession, b: &dyn TerminalSession) {
    let (a, b) = (screen(a), screen(b));
    assert_eq!(text(&a), text(&b));
    for row in 0..a.rows {
        assert_eq!(a.row(row), b.row(row), "row {row}");
    }
}

#[test]
fn a_peer_with_an_allowed_key_lists_attaches_sees_output_and_types() {
    let rig = Rig::new(1 << 20);
    rig.session.print(b"before attach\r\n");
    let client = rig.client(&rig.signer()).unwrap();
    let list = client.list().unwrap();
    assert_eq!(list.len(), 1);
    // Terminals are addressed by host name and the host's own id.
    assert_eq!(list[0].0.host, "build-box");
    assert_eq!(list[0].0.terminal, 7);
    assert_eq!(list[0].1.name, "claude-1");
    assert!(list[0].1.running);

    let remote = attach(&client);
    // What was already on screen arrives as a repaint.
    wait_for_text(&*remote, "before attach");
    rig.session.print(b"live output\r\n");
    wait_for_text(&*remote, "live output");
    assert_screens_equal(&*rig.session, &*remote);

    remote.send_input(b"ls -l\r".to_vec()).unwrap();
    wait_until("typed bytes at the host", || {
        rig.session.typed() == b"ls -l\r"
    });
    let log = rig.audit();
    assert!(log.contains("\"actor\":\"remote\""), "{log}");
    assert!(log.contains("\"action\":\"attach\""));
    assert!(log.contains("\"action\":\"connect\""));
    client.close();
}

#[test]
fn input_reaches_the_host_in_well_under_a_frame_on_loopback() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    // Warm up, then time a key from the client until the host sees it.
    remote.send_input(b"a".to_vec()).unwrap();
    wait_until("warm up", || rig.session.typed() == b"a");
    let mut worst = Duration::ZERO;
    for i in 0..20_u8 {
        let before = rig.session.typed().len();
        let started = Instant::now();
        remote.send_input(vec![b'0' + i % 10]).unwrap();
        wait_until("key", || rig.session.typed().len() > before);
        worst = worst.max(started.elapsed());
    }
    eprintln!("worst input path on loopback: {worst:?}");
    assert!(worst < Duration::from_millis(100), "{worst:?}");
}

#[test]
fn an_unknown_key_is_refused_and_sees_nothing() {
    let rig = Rig::new(1 << 20);
    rig.session.print(b"secret screen\r\n");
    let (conn, _host) = rig.connect(1);
    let stranger = KeySigner(key());
    let err = RemoteClient::connect(
        Arc::clone(&conn) as Arc<dyn Connection>,
        "x",
        &HOST_ID,
        &stranger,
    )
    .err()
    .unwrap();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::NotAuthorized)),
        "{err}"
    );
    let log = rig.audit();
    assert!(log.contains("not authorized"), "{log}");
}

#[test]
fn no_terminal_stream_is_served_before_the_handshake_ends() {
    let rig = Rig::new(1 << 20);
    rig.session.print(b"secret screen\r\n");
    let (conn, _host) = rig.connect(1);
    let control = conn.open_stream().unwrap();
    // Opening a terminal stream first, ahead of any authentication.
    let early = conn.open_stream().unwrap();
    early
        .send(&Message::Attach {
            terminal: 7,
            resume_seq: 0,
            cols: 60,
            rows: 12,
        })
        .unwrap();
    // The control stream does not say Hello: it asks for the list instead.
    control.send(&Message::ListTerminals).unwrap();
    assert_eq!(
        control.recv(Duration::from_secs(5)).unwrap(),
        Some(Message::Denied(DenyReason::Protocol))
    );
    // The early stream gets nothing at all, and ends with the connection.
    let got = early.recv(Duration::from_secs(5));
    assert!(
        !matches!(got, Ok(Some(_))),
        "an unauthenticated stream was answered: {got:?}"
    );
}

#[test]
fn a_host_with_remote_access_off_drops_the_connection() {
    let rig = Rig::new(1 << 20);
    rig.guard.set_enabled(false);
    let started = Instant::now();
    let err = rig.client(&rig.signer()).err().unwrap();
    assert!(
        matches!(err, ClientError::Net(_) | ClientError::Protocol),
        "{err}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_different_machine_than_the_pinned_one_is_refused_before_signing() {
    let rig = Rig::new(1 << 20);
    let (conn, _host) = rig.connect(1);
    let err = RemoteClient::connect(conn, "build-box", &[0x11; 32], &rig.signer())
        .err()
        .unwrap();
    assert!(matches!(err, ClientError::HostMismatch));
}

#[test]
fn a_second_peer_is_refused_while_one_is_connected() {
    let rig = Rig::new(1 << 20);
    let first = rig.client(&rig.signer()).unwrap();
    let err = rig.client(&rig.signer()).err().unwrap();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::Busy)),
        "{err}"
    );
    first.close();
    // Once the first is gone, a new peer can connect.
    wait_until("the host to free up", || rig.client(&rig.signer()).is_ok());
}

#[test]
fn pause_stops_typing_but_not_watching() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    remote.send_input(b"1".to_vec()).unwrap();
    wait_until("first key", || rig.session.typed() == b"1");

    rig.pause.pause();
    remote.send_input(b"2".to_vec()).unwrap();
    wait_until("the refusal", || {
        remote.last_denied() == Some(DenyReason::Paused)
    });
    thread::sleep(Duration::from_millis(100));
    assert_eq!(rig.session.typed(), b"1", "a paused host typed for a peer");
    // Output still flows.
    rig.session.print(b"still visible\r\n");
    wait_for_text(&*remote, "still visible");

    rig.pause.resume();
    remote.send_input(b"3".to_vec()).unwrap();
    wait_until("typing after resume", || rig.session.typed() == b"13");
    assert!(rig.audit().contains("relay is paused"));
}

#[test]
fn resume_after_a_dropped_link_continues_without_a_repaint() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    rig.session.print(b"one\r\ntwo\r\n");
    wait_for_text(&*remote, "two");
    assert_eq!(remote.position(), rig.session.ring.end());
    assert_eq!(remote.resyncs(), 1, "the first attach repaints");
    // The link drops. The host goes on printing.
    client.close();
    wait_until("the stream to end", || remote.is_closed());
    rig.session.print(b"three\r\nfour\r\n");

    // The same session joins a new connection and resumes. The host frees
    // its single slot a moment after the old link ends.
    let client = reconnect(&rig);
    remote.attach(&client, 7).unwrap();
    wait_for_text(&*remote, "four");
    assert_screens_equal(&*rig.session, &*remote);
    assert_eq!(remote.resyncs(), 1, "a resume must not repaint");
    assert_eq!(remote.position(), rig.session.ring.end());
}

#[test]
fn a_client_that_fell_off_the_ring_is_repainted_and_converges() {
    // A ring of 200 bytes cannot hold what was printed while away.
    let rig = Rig::new(200);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    rig.session.print(b"\x1b[1;31mred start\x1b[0m\r\n");
    wait_for_text(&*remote, "red start");
    let position = remote.position();
    client.close();
    wait_until("the stream to end", || remote.is_closed());
    for i in 0..60 {
        rig.session
            .print(format!("line {i:03} {}\r\n", "z".repeat(30)).as_bytes());
    }
    assert!(rig.session.ring.end() - position > 200);

    let client = reconnect(&rig);
    remote.attach(&client, 7).unwrap();
    wait_for_text(&*remote, "line 059");
    wait_until("converged", || remote.position() == rig.session.ring.end());
    assert_eq!(
        remote.resyncs(),
        2,
        "falling off the ring costs one repaint"
    );
    assert_screens_equal(&*rig.session, &*remote);
    // And it keeps following afterwards.
    rig.session.print(b"after repaint\r\n");
    wait_for_text(&*remote, "after repaint");
    assert_screens_equal(&*rig.session, &*remote);
}

#[test]
fn a_flood_larger_than_the_ring_ends_with_the_same_screen() {
    let rig = Rig::new(64 * 1024);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    let printer = {
        let session = Arc::clone(&rig.session);
        thread::spawn(move || {
            for i in 0..20_000 {
                session.print(format!("flood {i:06} {}\r\n", "y".repeat(60)).as_bytes());
            }
            session.print(b"\x1b[2J\x1b[Hdone flooding\r\n");
        })
    };
    printer.join().unwrap();
    wait_for_text(&*remote, "done flooding");
    wait_until("converged", || remote.position() == rig.session.ring.end());
    assert_screens_equal(&*rig.session, &*remote);
}

#[test]
fn the_host_grid_follows_a_resize_the_peer_asks_for() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    assert_eq!(
        remote.resize(GridSize { cols: 90, rows: 30 }),
        ResizeOutcome::Applied
    );
    wait_until("the host size", || {
        remote.host_size() == GridSize { cols: 90, rows: 30 }
    });
    assert_eq!(screen(&*rig.session).cols, 90);
    wait_until("the local grid", || screen(&*remote).cols == 90);
}

#[test]
fn attaching_to_a_terminal_that_does_not_exist_is_refused() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let session = RemoteSession::new(
        TerminalAddr {
            host: "x".into(),
            terminal: 99,
        },
        GridSize { cols: 60, rows: 12 },
        Arc::new(|| {}),
    );
    let err = session.attach(&client, 99).err().unwrap();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::NoSuchTerminal)),
        "{err}"
    );
}

#[cfg(unix)]
#[test]
fn a_real_pty_session_works_end_to_end_through_the_tap() {
    let spec = connecto_session::spec("/bin/cat", vec![], std::path::Path::new("/tmp"));
    // cat echoes the line back, once as the tty echo and once as its output.
    real_pty_end_to_end(spec, "hello from afar");
}

/// The same with PowerShell in a ConPTY: the line comes back as the echo of
/// `Read-Host` and then as its output.
#[cfg(windows)]
#[test]
fn a_real_pty_session_works_end_to_end_through_the_tap() {
    let spec = connecto_session::shell_spec(
        Some("$line = Read-Host; Write-Output ('got:' + $line); Start-Sleep -Seconds 30"),
        &std::env::temp_dir(),
    );
    real_pty_end_to_end(spec, "got:hello from afar");
}

/// Run `spec` in a real PTY shared through the tap, type a line from the
/// peer, wait for `needle` there, and compare the two screens.
fn real_pty_end_to_end(mut spec: connecto_pty::PtySpec, needle: &str) {
    use connecto_pty::OpenPty;
    use connecto_session::LiveSession;

    let size = GridSize { cols: 60, rows: 12 };
    let ring = Arc::new(Ring::new(1 << 20));
    let pty = OpenPty::open(size).unwrap();
    spec.size = size;
    let live = Arc::new(
        LiveSession::start_tapped(
            pty,
            spec,
            Arc::new(|| {}),
            Some(Arc::new(RingTap::recording(Arc::clone(&ring)))),
        )
        .unwrap(),
    );
    let rig = Rig::new(1 << 20);
    let terminals = Arc::new(FixedTerminals(Mutex::new(vec![HostTerminal {
        id: 7,
        name: "cat".into(),
        tool: 3,
        attempt: 1,
        activity: 0,
        session: Arc::clone(&live) as Arc<dyn TerminalSession>,
        ring: Arc::clone(&ring),
    }])));
    let text_keys = format!("{}\n", rig.allowed.public_key().to_openssh().unwrap());
    let keys = Arc::new(AuthorizedKeySet::parse(&text_keys).unwrap());
    let host = Arc::new(HostService::new(
        terminals,
        Arc::new(move || Arc::clone(&keys)),
        Arc::clone(&rig.guard) as Arc<dyn RemoteGuard>,
    ));
    let (client_conn, host_conn) = loopback::pair(CLIENT_ID, HOST_ID, [5; 32]);
    let service = Arc::clone(&host);
    thread::spawn(move || service.serve(&host_conn));
    let client =
        RemoteClient::connect(Arc::new(client_conn), "pty-host", &HOST_ID, &rig.signer()).unwrap();
    let remote = attach(&client);
    remote.send_input(b"hello from afar\r".to_vec()).unwrap();
    wait_for_text(&*remote, needle);
    assert_screens_equal(&*live, &*remote);
    live.terminate();
}

#[test]
fn the_host_view_follows_the_peer_while_attached_and_takes_its_size_back_after() {
    let rig = Rig::new(1 << 20);
    let host_view = GridSize { cols: 60, rows: 12 };
    let peer_view = GridSize {
        cols: 100,
        rows: 30,
    };
    let grid = || {
        let snap = screen(&*rig.session);
        GridSize {
            cols: snap.cols,
            rows: snap.rows,
        }
    };
    assert_eq!(grid(), host_view);

    let client = rig.client(&rig.signer()).unwrap();
    let session = RemoteSession::new(
        TerminalAddr {
            host: "x".into(),
            terminal: 7,
        },
        peer_view,
        Arc::new(|| {}),
    );
    session.attach(&client, 7).unwrap();
    wait_until("the host to take the peer's size", || grid() == peer_view);
    assert!(
        rig.session.control.lock().unwrap().0,
        "the peer has the size"
    );

    // The host window's layout asks for its own size: it does not move the terminal.
    assert_eq!(rig.session.resize(host_view), ResizeOutcome::Applied);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(grid(), peer_view);

    // A resize from the peer still goes through.
    assert_eq!(
        session.resize(GridSize { cols: 90, rows: 25 }),
        ResizeOutcome::Applied
    );
    wait_until("the peer's resize", || {
        grid() == GridSize { cols: 90, rows: 25 }
    });

    // The peer detaches: the host view takes the size back.
    client.close();
    wait_until("the host to get its size back", || grid() == host_view);
    assert!(!rig.session.control.lock().unwrap().0);
}

#[test]
fn a_peer_adds_renames_restarts_and_closes_terminals_and_each_is_audited() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    assert_eq!(
        client
            .manage(ManageOp::Add {
                tool: 2,
                name: Some("review".into()),
            })
            .unwrap(),
        Some(99)
    );
    client
        .manage(ManageOp::Rename {
            terminal: 7,
            name: Some("main".into()),
        })
        .unwrap();
    client.manage(ManageOp::Restart { terminal: 7 }).unwrap();
    client.manage(ManageOp::Close { terminal: 7 }).unwrap();
    assert_eq!(
        *rig.managed.ops.lock().unwrap(),
        [
            ManageOp::Add {
                tool: 2,
                name: Some("review".into())
            },
            ManageOp::Rename {
                terminal: 7,
                name: Some("main".into())
            },
            ManageOp::Restart { terminal: 7 },
            ManageOp::Close { terminal: 7 },
        ]
    );
    let log = rig.audit();
    for action in ["add", "rename", "restart", "close"] {
        assert!(
            log.contains(&format!("\"action\":\"{action}\"")),
            "{action}: {log}"
        );
    }
    assert!(log.matches("\"actor\":\"remote\"").count() >= 4);
}

#[test]
fn pause_refuses_managing_terminals_and_the_host_never_acts() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    rig.pause.pause();
    for op in [
        ManageOp::Add {
            tool: 1,
            name: None,
        },
        ManageOp::Close { terminal: 7 },
        ManageOp::Restart { terminal: 7 },
        ManageOp::Rename {
            terminal: 7,
            name: None,
        },
    ] {
        let err = client.manage(op).unwrap_err();
        assert!(
            matches!(&err, ClientError::Remote(why) if why.contains("paused")),
            "{err}"
        );
    }
    assert!(
        rig.managed.ops.lock().unwrap().is_empty(),
        "the host acted while paused"
    );
    let log = rig.audit();
    assert_eq!(log.matches("\"decision\":\"deny\"").count(), 4, "{log}");
    // After Resume it works again.
    rig.pause.resume();
    client.manage(ManageOp::Restart { terminal: 7 }).unwrap();
    assert_eq!(rig.managed.ops.lock().unwrap().len(), 1);
}

#[test]
fn a_terminal_that_is_not_there_and_a_host_refusal_come_back_as_errors() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let err = client.manage(ManageOp::Close { terminal: 99 }).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("no such terminal")),
        "{err}"
    );
    assert!(rig.managed.ops.lock().unwrap().is_empty());
    *rig.managed.refuse.lock().unwrap() = Some("the last terminal cannot be closed".into());
    let err = client.manage(ManageOp::Close { terminal: 7 }).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("last terminal")),
        "{err}"
    );
}

#[test]
fn nothing_can_be_managed_before_the_handshake_ends() {
    let rig = Rig::new(1 << 20);
    let (conn, _host) = rig.connect(1);
    let control = conn.open_stream().unwrap();
    control
        .send(&Message::Manage {
            req: 1,
            op: ManageOp::Close { terminal: 7 },
        })
        .unwrap();
    assert_eq!(
        control.recv(Duration::from_secs(5)).unwrap(),
        Some(Message::Denied(DenyReason::Protocol))
    );
    assert!(rig.managed.ops.lock().unwrap().is_empty());
}

#[test]
fn a_terminal_that_ran_before_hosting_was_on_is_shared_from_a_repaint() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use connecto_core::OutputTap;
    let rig = Rig::new(1 << 20);
    // Output while nobody can look: numbered, not kept.
    let record = Arc::new(AtomicBool::new(false));
    let tap = RingTap::new(Arc::clone(&rig.session.ring), Arc::clone(&record));
    tap.feed(b"typed before hosting\r\n", &mut |b| {
        rig.session.emulator.feed(b)
    });
    assert!(rig.session.ring.end() > 0);

    // Hosting is turned on; a peer attaches without any restart.
    record.store(true, Ordering::Relaxed);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    wait_for_text(&*remote, "typed before hosting");
    tap.feed(b"and after\r\n", &mut |b| rig.session.emulator.feed(b));
    wait_for_text(&*remote, "and after");
    assert_screens_equal(&*rig.session, &*remote);
    assert_eq!(remote.resyncs(), 1);
}
