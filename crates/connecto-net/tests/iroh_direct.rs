//! The full remote path over real iroh endpoints in one process: QUIC on the
//! loopback interface, no relay, no outside network, no n0 services.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::path::Path;
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
use connecto_net::auth::{AuthorizedKeySet, KeySigner};
use connecto_net::client::{ClientError, RemoteClient};
use connecto_net::host::{FixedTerminals, HostService, HostTerminal, HostTerminals, RingTap};
use connecto_net::iroh_transport::{IrohConfig, IrohNode, PeerAddr};
use connecto_net::seq::Ring;
use connecto_net::transport::PathKind;
use connecto_net::wire::DenyReason;
use connecto_session::RemoteGate;
use connecto_term::Emulator;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, PrivateKey};

struct Fake {
    emulator: Emulator,
    ring: Arc<Ring>,
    typed: Mutex<Vec<u8>>,
}

impl TerminalSession for Fake {
    fn chat_history(&self) -> Option<Arc<connecto_core::session::ChatHistory>> {
        Some(Arc::new(connecto_core::session::ChatHistory::default()))
    }
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        self.emulator.poll_snapshot(out)
    }
    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        self.typed.lock().unwrap().extend(bytes);
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

fn config(seed: u8) -> IrohConfig {
    IrohConfig {
        device_key: [seed; 32],
        relay_url: None,
        relay_token: None,
        bind: Some("127.0.0.1:0".parse().unwrap()),
        listen: true,
        relay_only: false,
        alpn: connecto_net::iroh_transport::ALPN,
    }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn screen_text(s: &dyn TerminalSession) -> String {
    let mut snap = TermSnapshot::default();
    for _ in 0..100 {
        if s.poll_snapshot(&mut snap) == SnapshotOutcome::Updated {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    (0..snap.rows)
        .map(|r| snap.row(r).iter().map(|c| c.ch).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

struct Setup {
    host_node: Arc<IrohNode>,
    fake: Arc<Fake>,
    pause: Arc<PauseSwitch>,
    gate: Arc<Gate>,
    log: Lines,
    allowed: PrivateKey,
}

struct SharedTerminals {
    terminals: FixedTerminals,
    attachments: Option<tempfile::TempDir>,
}

impl HostTerminals for SharedTerminals {
    fn list(&self) -> Vec<HostTerminal> {
        self.terminals.list()
    }
    fn attachment_root(&self) -> Option<&Path> {
        self.attachments.as_ref().map(tempfile::TempDir::path)
    }
}

fn setup() -> Setup {
    setup_with_attachments(None)
}

fn setup_with_attachments(attachments: Option<tempfile::TempDir>) -> Setup {
    let allowed = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let ring = Arc::new(Ring::new(1 << 20));
    let fake = Arc::new(Fake {
        emulator: Emulator::new(
            GridSize { cols: 60, rows: 12 },
            Arc::new(|_| {}),
            Arc::new(|| {}),
        ),
        ring: Arc::clone(&ring),
        typed: Mutex::new(Vec::new()),
    });
    let log = Lines::default();
    let pause = Arc::new(PauseSwitch::default());
    let gate = Arc::new(Gate::new(
        Arc::clone(&pause),
        Audit::with_sink(log.clone()).unwrap(),
        "TEST",
    ));
    let guard = Arc::new(RemoteGate::with_gate(Arc::clone(&gate)));
    guard.set_enabled(true);
    let name = if attachments.is_some() {
        connecto_core::session::APP_SESSION
    } else {
        "claude-1"
    };
    let terminals = Arc::new(SharedTerminals {
        attachments,
        terminals: FixedTerminals(Mutex::new(vec![HostTerminal {
            entry: None,
            ran: true,
            owner: None,
            background: Vec::new(),
            id: 1,
            name: name.into(),
            tool: 1,
            project_id: "p".into(),
            project_name: "Project".into(),
            parent: None,
            attempt: 1,
            activity: 0,
            session: Arc::clone(&fake) as Arc<dyn TerminalSession>,
            ring,
        }])),
    });
    let keys = Arc::new(
        AuthorizedKeySet::parse(&format!("{}\n", allowed.public_key().to_openssh().unwrap()))
            .unwrap(),
    );
    let host = Arc::new(HostService::new(
        terminals,
        Arc::new(move || Arc::clone(&keys)),
        guard as Arc<dyn RemoteGuard>,
    ));
    let host_node = Arc::new(IrohNode::start(&config(1)).unwrap());
    {
        let node = Arc::clone(&host_node);
        thread::spawn(move || {
            while let Ok(next) = node.accept(Duration::from_millis(100)) {
                if let Some(conn) = next {
                    let host = Arc::clone(&host);
                    thread::spawn(move || host.serve(&*conn));
                }
            }
        });
    }
    Setup {
        host_node,
        fake,
        pause,
        gate,
        log,
        allowed,
    }
}

fn dial(setup: &Setup, node: &IrohNode) -> Arc<dyn connecto_net::transport::Connection> {
    let conn = node
        .connect(&PeerAddr {
            id: setup.host_node.id(),
            relay: None,
            direct: setup.host_node.direct_addrs(),
        })
        .unwrap();
    Arc::from(conn)
}

#[test]
fn two_iroh_endpoints_complete_the_whole_remote_session_directly() {
    let setup = setup();
    let client_node = IrohNode::start(&config(2)).unwrap();
    let conn = dial(&setup, &client_node);
    assert_eq!(conn.remote_id(), setup.host_node.id());
    assert_eq!(conn.local_id(), client_node.id());

    let signer = KeySigner(setup.allowed.clone());
    let client = RemoteClient::connect(conn, "build-box", &setup.host_node.id(), &signer).unwrap();
    let list = client.list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0.host, "build-box");

    {
        use connecto_core::OutputTap;
        let tap = RingTap::recording(Arc::clone(&setup.fake.ring));
        tap.feed(b"hello over quic\r\n", &mut |b| setup.fake.emulator.feed(b));
    }
    let remote = connecto_net::client::RemoteSession::new(
        connecto_net::wire::TerminalAddr {
            host: "build-box".into(),
            terminal: 1,
        },
        GridSize { cols: 60, rows: 12 },
        Arc::new(|| {}),
    );
    remote.attach(&client, 1).unwrap();
    wait_until("the screen", || {
        screen_text(&*remote).contains("hello over quic")
    });
    remote.send_input(b"whoami\r".to_vec()).unwrap();
    wait_until("typed bytes", || {
        *setup.fake.typed.lock().unwrap() == b"whoami\r"
    });
    assert_eq!(client.path(), PathKind::Direct, "loopback should be direct");

    // Pause reaches across the network too.
    setup.pause.pause();
    remote.send_input(b"x".to_vec()).unwrap();
    wait_until("the refusal", || {
        remote.last_denied() == Some(DenyReason::Paused)
    });
    assert_eq!(*setup.fake.typed.lock().unwrap(), b"whoami\r");

    // Input latency over real QUIC on loopback.
    setup.pause.resume();
    let mut worst = Duration::ZERO;
    for _ in 0..30 {
        let before = setup.fake.typed.lock().unwrap().len();
        let started = Instant::now();
        remote.send_input(b"k".to_vec()).unwrap();
        wait_until("a key", || setup.fake.typed.lock().unwrap().len() > before);
        worst = worst.max(started.elapsed());
    }
    eprintln!("worst input path over iroh loopback: {worst:?}");
    assert!(setup.gate.flush(Duration::from_secs(2)));
    assert!(
        setup
            .log
            .0
            .lock()
            .unwrap()
            .join("\n")
            .contains("\"actor\":\"remote\"")
    );
    client.close();
}

#[test]
fn an_unauthorized_key_gets_no_session_over_iroh() {
    let setup = setup();
    let client_node = IrohNode::start(&config(3)).unwrap();
    let conn = dial(&setup, &client_node);
    let stranger = KeySigner(PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap());
    let err = RemoteClient::connect(conn, "x", &setup.host_node.id(), &stranger)
        .err()
        .unwrap();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::NotAuthorized)),
        "{err}"
    );
}

#[test]
fn dialing_an_id_that_is_not_the_host_never_reaches_the_handshake() {
    let setup = setup();
    let client_node = IrohNode::start(&config(4)).unwrap();
    // The right address but another machine's id: TLS pins the id, so the
    // connection fails rather than reaching a stranger.
    let wrong = IrohNode::start(&config(9)).unwrap();
    let result = client_node.connect(&PeerAddr {
        id: wrong.id(),
        relay: None,
        direct: setup.host_node.direct_addrs(),
    });
    assert!(result.is_err());
}

#[test]
fn an_authenticated_iroh_peer_uploads_bytes_and_gets_a_host_reference() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().to_owned();
    let setup = setup_with_attachments(Some(root));
    let node = IrohNode::start(&config(5)).unwrap();
    let signer = KeySigner(setup.allowed.clone());
    let client = RemoteClient::connect(
        dial(&setup, &node),
        "build-box",
        &setup.host_node.id(),
        &signer,
    )
    .unwrap();
    client.list().unwrap();
    let remote = connecto_net::client::RemoteSession::new(
        connecto_net::wire::TerminalAddr {
            host: "build-box".into(),
            terminal: 1,
        },
        GridSize { cols: 60, rows: 12 },
        Arc::new(|| {}),
    );
    remote.attach(&client, 1).unwrap();
    wait_until("chat history", || remote.chat_history().is_some());
    wait_until("attachment support", || remote.chat_attachments_supported());
    let bytes = vec![0xE7; 300_000];
    let reference = remote
        .upload_chat_attachment("p", &"1".repeat(32), "photo.bin", &bytes)
        .unwrap();
    assert!(Path::new(&reference).starts_with(path.join("chat-attachments").join("p")));
    assert_eq!(std::fs::read(&reference).unwrap(), bytes);
    remote
        .send_chat_input(reference.as_bytes().to_vec())
        .unwrap();
    wait_until("host-side reference in terminal input", || {
        *setup.fake.typed.lock().unwrap() == reference.as_bytes()
    });
    client.close();
}
