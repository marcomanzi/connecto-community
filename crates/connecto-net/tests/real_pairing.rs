//! Pairing through the live relay. Ignored by default.
//!
//! Needs `RELAY_CHECK_TOKEN` (the relay's shared access token) in the
//! environment, and optionally `RELAY_CHECK_URL` (default
//! `https://relay.connecto.studio`). The token is never printed or written: it
//! only goes into the two machines' secret stores. Both machines are in this
//! process, so this proves the relay path for pairing and the session that
//! follows, not two real networks.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use connecto_audit::{Audit, Sink};
use connecto_core::{
    GridSize, HostLink, InputRefused, PairingPhase, PauseSwitch, RemoteControl, RemoteGuard,
    ResizeOutcome, SessionStatus, SnapshotOutcome, TermSnapshot, TerminalSession,
};
use connecto_net::auth::DeviceKeySigners;
use connecto_net::host::{FixedTerminals, HostTerminal, RingTap};
use connecto_net::manager::{ManagerConfig, RemoteManager};
use connecto_net::pairing::KdfParams;
use connecto_net::reconnect::{Backoff, SupervisorConfig};
use connecto_net::secrets::{MemorySecrets, SecretStore};
use connecto_net::seq::Ring;
use connecto_session::RemoteGate;
use connecto_term::Emulator;

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

#[allow(dead_code)]
struct Machine {
    manager: Arc<RemoteManager>,
    pause: Arc<PauseSwitch>,
    fake: Arc<Fake>,
    dir: PathBuf,
    log: Lines,
}

fn machine(tag: &str, relay: &str, lifetime: Duration, token: &str) -> Machine {
    let dir = std::env::temp_dir().join(format!("mg-net-pair-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ring = Arc::new(Ring::new(1 << 20));
    let fake = Arc::new(Fake {
        emulator: Emulator::new(
            GridSize { cols: 60, rows: 12 },
            Arc::new(|_| {}),
            Arc::new(|| {}),
        ),
        ring: Arc::clone(&ring),
    });
    let terminals = Arc::new(FixedTerminals(Mutex::new(vec![HostTerminal {
        entry: None,
        ran: true,
        owner: None,
        background: Vec::new(),
        id: 1,
        name: "claude-1".into(),
        tool: 1,
        project_id: "p".into(),
        project_name: "Project".into(),
        parent: None,
        attempt: 1,
        activity: 0,
        session: Arc::clone(&fake) as Arc<dyn TerminalSession>,
        ring,
    }])));
    let pause = Arc::new(PauseSwitch::default());
    let log = Lines::default();
    let guard = Arc::new(RemoteGate::new(
        Arc::clone(&pause),
        Audit::with_sink(log.clone()).unwrap(),
    ));
    let secrets = Arc::new(MemorySecrets::default());
    secrets
        .set(connecto_net::secrets::RELAY_TOKEN, token.as_bytes())
        .unwrap();
    let manager = RemoteManager::start(ManagerConfig {
        dir: dir.clone(),
        secrets: Arc::clone(&secrets) as Arc<dyn SecretStore>,
        guard: guard as Arc<dyn RemoteGuard>,
        terminals,
        // Only the key made by pairing: no ssh-agent anywhere.
        signers: Arc::new(DeviceKeySigners(secrets)),
        wake: Arc::new(|| {}),
        supervisor: SupervisorConfig {
            backoff: Backoff {
                initial: Duration::from_millis(100),
                max: Duration::from_millis(800),
            },
            heartbeat: Duration::from_millis(200),
            initial_size: GridSize { cols: 60, rows: 12 },
        },
        bind: None,
        share_loopback: true,
        device_name: format!("device-{tag}"),
        kdf: KdfParams::FAST,
        pairing_lifetime: lifetime,
        default_relay: Some(relay.to_owned()),
    });
    Machine {
        manager,
        pause,
        fake,
        dir,
        log,
    }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
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
        .map(|r| snap.row(r).iter().map(|c| c.ch).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
#[ignore = "needs RELAY_CHECK_TOKEN and the network"]
fn two_codes_pair_two_machines_through_the_live_relay_and_a_session_follows() {
    let url = std::env::var("RELAY_CHECK_URL")
        .unwrap_or_else(|_| "https://relay.connecto.studio".to_owned());
    let token = std::env::var("RELAY_CHECK_TOKEN").expect("RELAY_CHECK_TOKEN is not set");
    let host = machine("rh", &url, Duration::from_secs(90), &token);
    let client = machine("rc", &url, Duration::from_secs(90), &token);
    host.fake.print(b"hello through the relay\r\n");
    let started = Instant::now();
    host.manager.start_mutual_pairing().unwrap();
    client.manager.start_mutual_pairing().unwrap();
    let host_code = code_of(&host);
    let client_code = code_of(&client);
    eprintln!("codes shown after {:?}", started.elapsed());
    client.manager.submit_peer_code(&host_code).unwrap();
    host.manager.submit_peer_code(&client_code).unwrap();
    let end = Instant::now() + Duration::from_secs(120);
    while !(named(&host) && named(&client)) && Instant::now() < end {
        if let PairingPhase::Failed { message } = host.manager.view().pairing.clone() {
            panic!("host failed: {message}");
        }
        if let PairingPhase::Failed { message } = client.manager.view().pairing.clone() {
            panic!("client failed: {message}");
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(named(&host) && named(&client), "no names in time");
    eprintln!("both connected after {:?}", started.elapsed());
    host.manager.finish_pairing("Client").unwrap();
    client.manager.finish_pairing("Host").unwrap();
    wait_until("both to be paired", || {
        matches!(host.manager.view().pairing, PairingPhase::Done { .. })
            && matches!(client.manager.view().pairing, PairingPhase::Done { .. })
    });
    wait_until("a connected link", || {
        matches!(
            client.manager.view().machines[0].outbound,
            HostLink::Direct | HostLink::Relayed | HostLink::Connected
        )
    });
    let terminals = client.manager.terminals("Host").unwrap();
    wait_until("the terminal list", || {
        terminals.view().terminals.len() == 1
    });
    let session = Arc::clone(&terminals.view().terminals[0].session);
    wait_until("the screen", || {
        text(&*session).contains("hello through the relay")
    });
    host.manager.shutdown();
    client.manager.shutdown();
    let _ = std::fs::remove_dir_all(&host.dir);
    let _ = std::fs::remove_dir_all(&client.dir);
}

fn named(m: &Machine) -> bool {
    matches!(m.manager.view().pairing, PairingPhase::NameNeeded { .. })
}

/// The code this machine shows; waits until it is on screen.
fn code_of(m: &Machine) -> String {
    let end = Instant::now() + Duration::from_secs(45);
    while Instant::now() < end {
        if let PairingPhase::ShowingCode { my_code, .. } = m.manager.view().pairing.clone() {
            return my_code;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("no code shown");
}
