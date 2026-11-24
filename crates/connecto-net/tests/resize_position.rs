//! A peer's screen ends equal to the host's when the terminal is resized while
//! it prints: each size reaches the peer at the byte where the host's screen
//! took it, never ahead of output the host drew at the size before.
//!
//! The case that showed it: Claude Code's slash menu, redrawn with cursor-up
//! and erase-line after every keystroke, while the peer's window was resized
//! nine times in four seconds. The peer piled up copies of menu rows.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use connecto_audit::{Audit, Sink};
use connecto_core::{
    GridSize, InputRefused, OutputTap, PauseSwitch, RemoteGuard, ResizeOutcome, SessionStatus,
    SnapshotOutcome, TermSnapshot, TerminalSession,
};
use connecto_gate::Gate;
use connecto_net::auth::{AuthorizedKeySet, KeySigner};
use connecto_net::client::{RemoteClient, RemoteSession};
use connecto_net::host::{FixedTerminals, HostService, HostTerminal, HostTerminals, RingTap};
use connecto_net::seq::Ring;
use connecto_net::transport::loopback;
use connecto_net::wire::TerminalAddr;
use connecto_session::RemoteGate;
use connecto_term::Emulator;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, PrivateKey};

const START: GridSize = GridSize { cols: 60, rows: 12 };

/// A held door: `hold` shuts it, the next caller of `pass` waits there until
/// `open`, and `entered` says someone is waiting.
#[derive(Default)]
struct Door {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Door {
    fn hold(&self) {
        *self.state.lock().unwrap() = (true, false);
    }

    fn pass(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.0 {
            return;
        }
        state.1 = true;
        self.changed.notify_all();
        while state.0 {
            state = self.changed.wait(state).unwrap();
        }
    }

    /// Whether someone came to the door within `wait`.
    fn entered(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let mut state = self.state.lock().unwrap();
        while !state.1 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
        true
    }

    fn open(&self) {
        self.state.lock().unwrap().0 = false;
        self.changed.notify_all();
    }
}

/// A terminal with no process, resized the way `LiveSession` resizes one.
struct Session {
    emulator: Arc<Emulator>,
    ring: Arc<Ring>,
    tap: RingTap,
    applied: Mutex<GridSize>,
    /// Where the host's stream reads the size it tells its peer.
    door: Door,
}

impl Session {
    fn new() -> Arc<Self> {
        let ring = Arc::new(Ring::new(1 << 20));
        let tap = RingTap::recording(Arc::clone(&ring));
        // As a real session starts: its first size marked before any output.
        assert!(tap.resize(&mut || Some(START)));
        Arc::new(Self {
            emulator: Arc::new(Emulator::new(START, Arc::new(|_| {}), Arc::new(|| {}))),
            tap,
            ring,
            applied: Mutex::new(START),
            door: Door::default(),
        })
    }

    /// Output from the "process", through the tap a real session uses.
    fn print(&self, bytes: &[u8]) {
        self.tap.feed(bytes, &mut |b| self.emulator.feed(b));
    }

    /// As `LiveSession::apply_size`: the screen, then the size it took,
    /// through the tap so no output is fed in between.
    fn apply(&self, size: GridSize) -> ResizeOutcome {
        let mut outcome = ResizeOutcome::Retry;
        let _ = self.tap.resize(&mut || {
            outcome = self.emulator.try_resize(size);
            (outcome == ResizeOutcome::Applied).then(|| {
                *self.applied.lock().unwrap() = size;
                size
            })
        });
        outcome
    }

    /// The host window resizes the terminal, retrying a busy screen.
    fn resize_now(&self, size: GridSize) {
        while self.resize(size) == ResizeOutcome::Retry {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl TerminalSession for Session {
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
    fn send_input(&self, _bytes: Vec<u8>) -> Result<(), InputRefused> {
        Ok(())
    }
    fn resize(&self, size: GridSize) -> ResizeOutcome {
        self.apply(size)
    }
    fn resize_remote(&self, size: GridSize) -> ResizeOutcome {
        self.apply(size)
    }
    fn applied_size(&self) -> Option<GridSize> {
        self.door.pass();
        Some(*self.applied.lock().unwrap())
    }
    fn scroll(&self, lines: i32) -> bool {
        self.emulator.try_scroll(lines)
    }
    fn status(&self) -> SessionStatus {
        SessionStatus::Running
    }
}

#[derive(Clone, Default)]
struct Lines;

impl Sink for Lines {
    fn write_line(&mut self, _line: &str) -> std::io::Result<()> {
        Ok(())
    }
}

/// A host serving `session` as terminal 7, and a peer attached to it.
struct Rig {
    session: Arc<Session>,
    host: Arc<HostService>,
    key: PrivateKey,
    client: Mutex<RemoteClient>,
    remote: Arc<RemoteSession>,
}

fn connect(host: &Arc<HostService>, key: &PrivateKey) -> RemoteClient {
    let (client_end, host_end) = loopback::pair([0xbb; 32], [0xaa; 32], [1; 32]);
    let service = Arc::clone(host);
    thread::spawn(move || service.serve(&host_end));
    let client = RemoteClient::connect(
        Arc::new(client_end),
        "build-box",
        &[0xaa; 32],
        &KeySigner(key.clone()),
    )
    .unwrap();
    client.list().unwrap();
    client
}

impl Rig {
    fn new() -> Self {
        Self::after(|_| {})
    }

    /// A rig whose terminal printed `before` ahead of the peer's attach, which
    /// is then a repaint.
    fn after(before: impl FnOnce(&Session)) -> Self {
        let session = Session::new();
        before(&session);
        let list = vec![HostTerminal {
            entry: None,
            ran: true,
            owner: None,
            background: Vec::new(),
            id: 7,
            name: "claude-1".into(),
            tool: 1,
            project_id: "p".into(),
            project_name: "Project".into(),
            parent: None,
            attempt: 1,
            activity: 0,
            session: Arc::clone(&session) as Arc<dyn TerminalSession>,
            ring: Arc::clone(&session.ring),
        }];
        let allowed = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        let gate = Arc::new(Gate::new(
            Arc::new(PauseSwitch::default()),
            Audit::with_sink(Lines).unwrap(),
            "TEST",
        ));
        let guard = Arc::new(RemoteGate::with_gate(gate));
        guard.set_enabled(true);
        let text = format!("{}\n", allowed.public_key().to_openssh().unwrap());
        let keys = Arc::new(AuthorizedKeySet::parse(&text).unwrap());
        let host = Arc::new(HostService::new(
            Arc::new(FixedTerminals(Mutex::new(list))) as Arc<dyn HostTerminals>,
            Arc::new(move || Arc::clone(&keys)),
            guard as Arc<dyn RemoteGuard>,
        ));
        let client = connect(&host, &allowed);
        let remote = RemoteSession::new(
            TerminalAddr {
                host: client.host().to_owned(),
                terminal: 7,
            },
            START,
            Arc::new(|| {}),
        );
        remote.attach(&client, 7).unwrap();
        wait_until("attached", || remote.is_linked());
        Self {
            session,
            host,
            key: allowed,
            client: Mutex::new(client),
            remote,
        }
    }

    /// The link drops; `meanwhile` happens on the host; the peer reconnects
    /// and resumes from the byte it had.
    fn drop_link(&self, meanwhile: impl FnOnce(&Session)) {
        self.client.lock().unwrap().close();
        wait_until("the link is down", || !self.remote.is_linked());
        meanwhile(&self.session);
        // The host lets go of the old connection before it takes another.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let client = connect(&self.host, &self.key);
            if self.remote.attach(&client, 7).is_ok() {
                *self.client.lock().unwrap() = client;
                break;
            }
            assert!(Instant::now() < deadline, "no reattach");
            thread::sleep(Duration::from_millis(5));
        }
        wait_until("attached", || self.remote.is_linked());
    }

    /// The peer has every byte and the size the host ends at; then the two
    /// screens match, cell for cell.
    fn assert_converged(&self) {
        let end = self.session.ring.end();
        let size = *self.session.applied.lock().unwrap();
        wait_until("the peer caught up", || {
            self.remote.position() == end && self.remote.host_size() == size
        });
        let (host, peer) = (screen(&*self.session), screen(&*self.remote));
        assert_eq!((peer.cols, peer.rows), (host.cols, host.rows), "grid");
        assert_eq!(text(&peer), text(&host), "the peer's screen differs");
        for row in 0..host.rows {
            assert_eq!(peer.row(row), host.row(row), "row {row}");
        }
    }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for {what}");
}

fn screen(session: &dyn TerminalSession) -> TermSnapshot {
    let mut snap = TermSnapshot::default();
    for _ in 0..200 {
        if session.poll_snapshot(&mut snap) != SnapshotOutcome::Contended {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    snap
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

/// A menu drawn the way Claude Code draws its slash menu: up over the rows it
/// drew last time, then each row erased and drawn again, as wide as the
/// program believes the terminal is. `drawn` is how many rows the last frame
/// took, which is what the program moves up over.
fn menu(cols: u16, pick: usize, drawn: usize) -> Vec<u8> {
    let mut out = Vec::new();
    if drawn > 0 {
        out.extend(format!("\x1b[{drawn}A").bytes());
    }
    for item in 0..5 {
        out.extend(b"\r\x1b[2K");
        let mark = if item == pick { "> " } else { "  " };
        let line = format!("{mark}/command-{item} {}", "-".repeat(usize::from(cols)));
        // One column short of the width: the row never wraps where the
        // program thinks it is.
        out.extend(
            line.chars()
                .take(usize::from(cols) - 1)
                .collect::<String>()
                .bytes(),
        );
        out.extend(b"\r\n");
    }
    out
}

/// Output the host's screen took at one size, sent while the stream was
/// reading the size to tell: the peer must not draw it at the next size.
#[test]
fn output_drawn_before_a_resize_reaches_the_peer_before_the_size() {
    let rig = Rig::new();
    rig.session.print(b"$ claude\r\n");
    rig.session.print(&menu(START.cols, 0, 0));
    rig.assert_converged();

    let narrow = GridSize { cols: 40, rows: 8 };
    rig.session.door.hold();
    // The stream checks the size on every pass; once it is at the door (a
    // fixed stream may not come at all), the output and the resize happen.
    let _ = rig.session.door.entered(Duration::from_millis(500));
    rig.session.print(&menu(START.cols, 1, 5));
    rig.session.resize_now(narrow);
    rig.session.print(&menu(narrow.cols, 2, 5));
    rig.session.door.open();
    rig.assert_converged();
}

/// The burst that showed it: resizes back and forth while the menu redraws.
#[test]
fn a_burst_of_resizes_while_the_menu_redraws_leaves_the_peer_equal() {
    let rig = Rig::new();
    rig.session.print(b"$ claude\r\n");
    rig.session.print(&menu(START.cols, 0, 0));
    let sizes = [
        GridSize { cols: 90, rows: 16 },
        GridSize { cols: 70, rows: 10 },
        GridSize {
            cols: 100,
            rows: 20,
        },
        GridSize { cols: 50, rows: 12 },
        GridSize { cols: 80, rows: 14 },
    ];
    let stop = Arc::new(AtomicBool::new(false));
    let cols = Arc::new(Mutex::new(START.cols));
    let printer = {
        let (session, stop, cols) = (
            Arc::clone(&rig.session),
            Arc::clone(&stop),
            Arc::clone(&cols),
        );
        thread::spawn(move || {
            let mut pick = 0;
            while !stop.load(Ordering::SeqCst) {
                pick = (pick + 1) % 5;
                // Drawn at the size the program last heard of.
                let width = *cols.lock().unwrap();
                session.print(&menu(width, pick, 5));
                thread::sleep(Duration::from_micros(300));
            }
        })
    };
    for round in 0..40 {
        let size = sizes[round % sizes.len()];
        rig.session.resize_now(size);
        *cols.lock().unwrap() = size.cols;
        thread::sleep(Duration::from_millis(3));
    }
    stop.store(true, Ordering::SeqCst);
    printer.join().unwrap();
    let width = *cols.lock().unwrap();
    rig.session.print(&menu(width, 0, 5));
    rig.assert_converged();
}

/// A peer that attaches late is repainted from the host's screen, then
/// follows resizes from there: the repaint leaves the screen as the host's
/// own, soft-wrapped rows and blank cells included, so both reflow alike.
#[test]
fn a_repainted_peer_reflows_like_the_host_on_later_resizes() {
    let rig = Rig::after(|session| {
        session.print(b"$ cat notes\r\n");
        // Wraps at 60 columns: a soft-wrapped row the repaint must keep.
        session.print(&b"long line ".repeat(9));
        session.print(b"\r\n");
        session.print(&menu(START.cols, 0, 0));
    });
    rig.assert_converged();
    for size in [
        GridSize {
            cols: 100,
            rows: 16,
        },
        GridSize { cols: 40, rows: 8 },
        GridSize { cols: 75, rows: 12 },
    ] {
        rig.session.resize_now(size);
        rig.session.print(&menu(size.cols, 1, 5));
        rig.assert_converged();
    }
}

/// A peer away while the host shrinks and grows back with no output between:
/// the shrink lost cells the peer still has, so it is repainted, not resumed.
#[test]
fn a_peer_away_while_the_size_went_and_came_back_is_repainted() {
    let rig = Rig::new();
    // A full-screen program's alternate screen, as wide as the terminal.
    rig.session.print(b"\x1b[?1049h\x1b[H");
    for row in 0..START.rows {
        rig.session.print(format!("{row:<59}|").as_bytes());
        if row + 1 < START.rows {
            rig.session.print(b"\r\n");
        }
    }
    rig.assert_converged();
    let resyncs = rig.remote.resyncs();
    rig.drop_link(|session| {
        session.resize_now(GridSize { cols: 30, rows: 12 });
        session.resize_now(START);
    });
    rig.assert_converged();
    assert!(rig.remote.resyncs() > resyncs, "resumed over a lost size");
}

/// A peer away while nothing changed size resumes at its byte, no repaint.
#[test]
fn a_peer_away_while_only_output_came_resumes_exactly() {
    let rig = Rig::new();
    rig.session.print(&menu(START.cols, 0, 0));
    rig.assert_converged();
    let resyncs = rig.remote.resyncs();
    rig.drop_link(|session| session.print(&menu(START.cols, 1, 5)));
    rig.assert_converged();
    assert_eq!(rig.remote.resyncs(), resyncs, "a resume needs no repaint");
}
