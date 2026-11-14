//! The reconnect supervisor over the in-memory transport: a link cut mid-stream
//! is redialed with growing delays and each terminal resumes where it was.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use connecto_net::host::{FixedTerminals, HostService, HostTerminal, RingTap};
use connecto_net::reconnect::{
    Backoff, Dialer, HostHandle, LinkState, SignerSource, SupervisorConfig, spawn,
};
use connecto_net::seq::Ring;
use connecto_net::transport::Connection;
use connecto_net::transport::loopback::{self, LoopbackConnection};
use connecto_session::RemoteGate;
use connecto_term::Emulator;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, PrivateKey};

const HOST_ID: [u8; 32] = [0xaa; 32];
const CLIENT_ID: [u8; 32] = [0xbb; 32];

struct Fake {
    emulator: Emulator,
    ring: Arc<Ring>,
}

impl Fake {
    fn print(&self, bytes: &[u8]) {
        use connecto_core::OutputTap;
        RingTap::recording(Arc::clone(&self.ring)).feed(bytes, &mut |b| self.emulator.feed(b));
    }
}

impl TerminalSession for Fake {
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        self.emulator.poll_snapshot(out)
    }
    fn send_input(&self, _bytes: Vec<u8>) -> Result<(), InputRefused> {
        Ok(())
    }
    fn resize(&self, size: GridSize) -> ResizeOutcome {
        self.emulator.try_resize(size)
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

struct LoopDialer {
    service: Arc<HostService>,
    fail_first: AtomicUsize,
    dials: Mutex<Vec<Instant>>,
    last: Mutex<Option<Arc<LoopbackConnection>>>,
}

impl LoopDialer {
    /// Cut the link that is up: both ends see it die, in the middle of a stream.
    fn cut(&self) {
        if let Some(conn) = self.last.lock().unwrap().as_ref() {
            conn.close();
        }
    }
    fn dial_count(&self) -> usize {
        self.dials.lock().unwrap().len()
    }
}

impl Dialer for LoopDialer {
    fn dial(&self) -> Result<Arc<dyn Connection>, String> {
        self.dials.lock().unwrap().push(Instant::now());
        if self.fail_first.load(Ordering::SeqCst) > 0 {
            self.fail_first.fetch_sub(1, Ordering::SeqCst);
            return Err("host unreachable".to_owned());
        }
        let (client, host) = loopback::pair(CLIENT_ID, HOST_ID, [1; 32]);
        let service = Arc::clone(&self.service);
        thread::spawn(move || service.serve(&host));
        let client = Arc::new(client);
        *self.last.lock().unwrap() = Some(Arc::clone(&client));
        Ok(client)
    }
}

struct Keys(Vec<Arc<dyn Signer>>);

impl SignerSource for Keys {
    fn signers(&self) -> Vec<Arc<dyn Signer>> {
        self.0.clone()
    }
}

struct Setup {
    fake: Arc<Fake>,
    dialer: Arc<LoopDialer>,
    allowed: PrivateKey,
    service: Arc<HostService>,
}

fn setup(fail_first: usize) -> Setup {
    let allowed = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let ring = Arc::new(Ring::new(1 << 20));
    let fake = Arc::new(Fake {
        emulator: Emulator::new(
            GridSize { cols: 60, rows: 12 },
            Arc::new(|_| {}),
            Arc::new(|| {}),
        ),
        ring: Arc::clone(&ring),
    });
    let pause = Arc::new(PauseSwitch::default());
    let gate = Arc::new(Gate::new(
        pause,
        Audit::with_sink(Lines::default()).unwrap(),
        "TEST",
    ));
    let guard = Arc::new(RemoteGate::with_gate(gate));
    guard.set_enabled(true);
    let terminals = Arc::new(FixedTerminals(Mutex::new(vec![HostTerminal {
        id: 3,
        name: "claude-1".into(),
        tool: 1,
        attempt: 1,
        activity: 0,
        session: Arc::clone(&fake) as Arc<dyn TerminalSession>,
        ring,
    }])));
    let keys = Arc::new(
        AuthorizedKeySet::parse(&format!("{}\n", allowed.public_key().to_openssh().unwrap()))
            .unwrap(),
    );
    let service = Arc::new(HostService::new(
        terminals,
        Arc::new(move || Arc::clone(&keys)),
        guard as Arc<dyn RemoteGuard>,
    ));
    let dialer = Arc::new(LoopDialer {
        service: Arc::clone(&service),
        fail_first: AtomicUsize::new(fail_first),
        dials: Mutex::new(Vec::new()),
        last: Mutex::new(None),
    });
    Setup {
        fake,
        dialer,
        allowed,
        service,
    }
}

fn config(initial_ms: u64, max_ms: u64) -> SupervisorConfig {
    SupervisorConfig {
        backoff: Backoff {
            initial: Duration::from_millis(initial_ms),
            max: Duration::from_millis(max_ms),
        },
        heartbeat: Duration::from_millis(100),
        initial_size: GridSize { cols: 60, rows: 12 },
    }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

fn text(s: &dyn TerminalSession) -> String {
    let mut snap = TermSnapshot::default();
    for _ in 0..100 {
        if s.poll_snapshot(&mut snap) == SnapshotOutcome::Updated {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    (0..snap.rows)
        .map(|r| {
            snap.row(r)
                .iter()
                .map(|c| c.ch)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn start(setup: &Setup, cfg: SupervisorConfig) -> Arc<HostHandle> {
    let handle = HostHandle::new("build-box", HOST_ID, Arc::new(|| {}));
    let signer: Arc<dyn Signer> = Arc::new(KeySigner(setup.allowed.clone()));
    spawn(
        Arc::clone(&handle),
        Arc::clone(&setup.dialer) as Arc<dyn Dialer>,
        Arc::new(Keys(vec![signer])),
        cfg,
    )
    .unwrap();
    handle
}

#[test]
fn a_link_cut_mid_stream_is_redialed_and_the_terminal_resumes_without_a_repaint() {
    let setup = setup(0);
    let handle = start(&setup, config(20, 200));
    wait_until("the first connection", || {
        handle
            .terminals()
            .first()
            .is_some_and(|t| t.session.is_linked())
    });
    let session = Arc::clone(&handle.terminals()[0].session);
    assert_eq!(handle.terminals()[0].info.name, "claude-1");

    setup.fake.print(b"before the cut\r\n");
    wait_until("output", || text(&*session).contains("before the cut"));
    assert_eq!(session.resyncs(), 1);

    // Cut it while the host is still printing.
    let printer = {
        let fake = Arc::clone(&setup.fake);
        thread::spawn(move || {
            for i in 0..200 {
                fake.print(format!("tick {i:03}\r\n").as_bytes());
                thread::sleep(Duration::from_millis(2));
            }
        })
    };
    thread::sleep(Duration::from_millis(40));
    setup.dialer.cut();
    wait_until("the session to notice", || !session.is_linked());
    printer.join().unwrap();

    wait_until("the link to come back", || session.is_linked());
    assert!(setup.dialer.dial_count() >= 2);
    wait_until("the screen to catch up", || {
        session.position() == setup.fake.ring.end() && text(&*session).contains("tick 199")
    });
    // Same screen as the host, and no repaint was needed.
    assert_eq!(text(&*session), text(&*setup.fake));
    assert_eq!(session.resyncs(), 1, "a resume must not repaint");
    // The same session object carried over: it is the one the handle lists.
    assert!(Arc::ptr_eq(&session, &handle.terminals()[0].session));
    handle.stop();
    let _ = &setup.service;
}

#[test]
fn failed_dials_back_off_with_growing_delays_and_then_connect() {
    let setup = setup(4);
    let started = Instant::now();
    let handle = start(&setup, config(30, 100));
    wait_until("a connection after four failures", || {
        matches!(handle.state(), LinkState::Connected(_))
    });
    let dials = setup.dialer.dials.lock().unwrap().clone();
    assert_eq!(dials.len(), 5);
    let gaps: Vec<u128> = dials
        .windows(2)
        .map(|w| (w[1] - w[0]).as_millis())
        .collect();
    // 30, 60, 100 (capped), 100: each wait at least what was asked, and the
    // third and fourth no longer than the cap plus slack.
    assert!(gaps[0] >= 30, "{gaps:?}");
    assert!(gaps[1] >= 60, "{gaps:?}");
    assert!(gaps[2] >= 100, "{gaps:?}");
    assert!(gaps[3] >= 100 && gaps[3] < 400, "{gaps:?}");
    assert!(started.elapsed() >= Duration::from_millis(290));
    handle.stop();
}

#[test]
fn a_host_that_does_not_allow_our_key_is_refused_once_and_not_hammered() {
    let setup = setup(0);
    let stranger: Arc<dyn Signer> = Arc::new(KeySigner(
        PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap(),
    ));
    let handle = HostHandle::new("build-box", HOST_ID, Arc::new(|| {}));
    spawn(
        Arc::clone(&handle),
        Arc::clone(&setup.dialer) as Arc<dyn Dialer>,
        Arc::new(Keys(vec![stranger])),
        config(10, 50),
    )
    .unwrap();
    wait_until("the refusal", || {
        matches!(handle.state(), LinkState::Refused(_))
    });
    let dials = setup.dialer.dial_count();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        setup.dialer.dial_count(),
        dials,
        "a refused host was redialed"
    );
    // Asking again tries again.
    handle.kick();
    wait_until("another try", || setup.dialer.dial_count() > dials);
    handle.stop();
}

#[test]
fn a_second_key_is_tried_when_the_first_is_not_allowed() {
    let setup = setup(0);
    let stranger: Arc<dyn Signer> = Arc::new(KeySigner(
        PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap(),
    ));
    let good: Arc<dyn Signer> = Arc::new(KeySigner(setup.allowed.clone()));
    let handle = HostHandle::new("build-box", HOST_ID, Arc::new(|| {}));
    spawn(
        Arc::clone(&handle),
        Arc::clone(&setup.dialer) as Arc<dyn Dialer>,
        Arc::new(Keys(vec![stranger, good])),
        config(10, 50),
    )
    .unwrap();
    wait_until("connected", || {
        matches!(handle.state(), LinkState::Connected(_))
    });
    handle.stop();
}

#[test]
fn no_key_in_the_agent_is_a_refusal_that_says_so() {
    let setup = setup(0);
    let handle = HostHandle::new("build-box", HOST_ID, Arc::new(|| {}));
    spawn(
        Arc::clone(&handle),
        Arc::clone(&setup.dialer) as Arc<dyn Dialer>,
        Arc::new(Keys(vec![])),
        config(10, 50),
    )
    .unwrap();
    wait_until(
        "the refusal",
        || matches!(handle.state(), LinkState::Refused(why) if why.contains("ssh-agent")),
    );
    assert_eq!(setup.dialer.dial_count(), 0);
    handle.stop();
}

#[test]
fn the_handle_manages_terminals_while_connected_and_says_so_when_it_is_not() {
    use connecto_net::wire::ManageOp;
    let setup = setup(0);
    let handle = start(&setup, config(20, 200));
    // Not connected yet: refused at once, with a reason.
    let early = HostHandle::new("x", HOST_ID, Arc::new(|| {}));
    assert!(
        early
            .manage(ManageOp::Restart { terminal: 3 })
            .unwrap_err()
            .contains("not connected")
    );
    assert!(early.last_error().is_some());
    wait_until("connected", || handle.is_connected());
    // The host in this setup does not manage terminals: its refusal is kept.
    let why = handle
        .manage(ManageOp::Restart { terminal: 3 })
        .unwrap_err();
    assert!(why.contains("does not let peers"), "{why}");
    assert_eq!(handle.last_error().as_deref(), Some(why.as_str()));
    // A request for a terminal that is not there never reaches the host.
    let why = handle.manage(ManageOp::Close { terminal: 99 }).unwrap_err();
    assert!(why.contains("no such terminal"), "{why}");
    // Asynchronously, as the window does it, the answer lands in last_error.
    handle.manage_async(ManageOp::Close { terminal: 98 });
    wait_until("the async answer", || {
        handle
            .last_error()
            .is_some_and(|e| e.contains("no such terminal"))
    });
    handle.stop();
    wait_until("the link to go", || !handle.is_connected());
}
