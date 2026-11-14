//! A manual check against a real, deployed relay. Ignored by default.
//!
//! Needs `RELAY_CHECK_TOKEN` (the relay's shared access token) in the
//! environment, and optionally `RELAY_CHECK_URL` (default
//! `https://relay.connecto.studio`). The token is never printed, logged or
//! written anywhere: it only goes into the endpoints' relay configuration.
//!
//! Run: `RELAY_CHECK_TOKEN=... cargo test -p connecto-net --test real_relay -- --ignored --nocapture --test-threads=1`
//!
//! Both endpoints run on this one machine. That proves the token check, the
//! address discovery and that a session works through the relay. It does not
//! prove what two machines behind separate NATs would see: no hole punching
//! across a real network, and the relayed latency here is the round trip to the
//! relay and back twice over one local link, not a real two-site path.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use connecto_audit::{Audit, Sink};
use connecto_core::{
    GridSize, InputRefused, OutputTap, PauseSwitch, RemoteGuard, ResizeOutcome, SessionStatus,
    SnapshotOutcome, TermSnapshot, TerminalSession,
};
use connecto_gate::Gate;
use connecto_net::auth::{AuthorizedKeySet, KeySigner};
use connecto_net::client::{RemoteClient, RemoteSession};
use connecto_net::host::{FixedTerminals, HostService, HostTerminal, RingTap};
use connecto_net::iroh_transport::{IrohConfig, IrohNode, PeerAddr};
use connecto_net::seq::Ring;
use connecto_net::transport::PathKind;
use connecto_net::wire::TerminalAddr;
use connecto_session::RemoteGate;
use connecto_term::Emulator;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, PrivateKey};

fn url() -> String {
    std::env::var("RELAY_CHECK_URL").unwrap_or_else(|_| "https://relay.connecto.studio".to_owned())
}

fn token() -> String {
    std::env::var("RELAY_CHECK_TOKEN").expect("RELAY_CHECK_TOKEN is not set")
}

/// A fresh identity each run: the relay keeps an endpoint's connection for a
/// while after it goes, and a second run with the same id would be turned away.
fn fresh_key(seed: u8) -> [u8; 32] {
    let mut key = [seed; 32];
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    key[..16].copy_from_slice(&nanos.to_le_bytes());
    key
}

fn cfg(seed: u8, token: Option<String>, relay_only: bool) -> IrohConfig {
    IrohConfig {
        device_key: fresh_key(seed),
        relay_url: Some(url()),
        relay_token: token,
        bind: None,
        listen: true,
        relay_only,
        alpn: connecto_net::iroh_transport::ALPN,
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // 198.18.0.0/15 is the benchmarking range (often a local tunnel), and
            // 100.64.0.0/10 is carrier-grade NAT: neither is a public address.
            let benchmark = o[0] == 198 && (o[1] & 0xfe) == 18;
            let cgnat = o[0] == 100 && (o[1] & 0xc0) == 64;
            !(benchmark
                || cgnat
                || v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback() || v6.is_unspecified()) && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

/// An echoing terminal: what a peer types comes back as output.
struct Echo {
    emulator: Emulator,
    ring: Arc<Ring>,
}

impl TerminalSession for Echo {
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        self.emulator.poll_snapshot(out)
    }
    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        RingTap::recording(Arc::clone(&self.ring)).feed(&bytes, &mut |b| self.emulator.feed(b));
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

struct Lines;

impl Sink for Lines {
    fn write_line(&mut self, _line: &str) -> std::io::Result<()> {
        Ok(())
    }
}

/// A host with one echoing terminal, listening on `node`.
fn serve(node: &Arc<IrohNode>, allowed: &PrivateKey) {
    let ring = Arc::new(Ring::new(1 << 20));
    let session = Arc::new(Echo {
        emulator: Emulator::new(
            GridSize { cols: 80, rows: 24 },
            Arc::new(|_| {}),
            Arc::new(|| {}),
        ),
        ring: Arc::clone(&ring),
    });
    let gate = Arc::new(Gate::new(
        Arc::new(PauseSwitch::default()),
        Audit::with_sink(Lines).unwrap(),
        "TEST",
    ));
    let guard = Arc::new(RemoteGate::with_gate(gate));
    guard.set_enabled(true);
    let keys = Arc::new(
        AuthorizedKeySet::parse(&format!("{}\n", allowed.public_key().to_openssh().unwrap()))
            .unwrap(),
    );
    let host = Arc::new(HostService::new(
        Arc::new(FixedTerminals(Mutex::new(vec![HostTerminal {
            id: 1,
            name: "echo".into(),
            tool: 3,
            attempt: 1,
            activity: 0,
            session: session as Arc<dyn TerminalSession>,
            ring,
        }]))),
        Arc::new(move || Arc::clone(&keys)),
        guard as Arc<dyn RemoteGuard>,
    ));
    let node = Arc::clone(node);
    std::thread::spawn(move || {
        while let Ok(next) = node.accept(Duration::from_millis(100)) {
            if let Some(conn) = next {
                let host = Arc::clone(&host);
                std::thread::spawn(move || host.serve(&*conn));
            }
        }
    });
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Connect, attach, and time `n` keystrokes from typing to the echo arriving.
fn measure(
    host: &Arc<IrohNode>,
    client: &IrohNode,
    via_relay: bool,
    signer: &KeySigner,
    n: usize,
) -> (PathKind, Vec<Duration>) {
    let conn = client
        .connect(&PeerAddr {
            id: host.id(),
            relay: via_relay.then(url),
            direct: if via_relay {
                vec![]
            } else {
                host.direct_addrs()
                    .into_iter()
                    .filter(|a| a.ip().is_loopback() || a.ip().is_unspecified())
                    .map(|a| {
                        let mut a = a;
                        if a.ip().is_unspecified() {
                            a.set_ip(IpAddr::from([127, 0, 0, 1]));
                        }
                        a
                    })
                    .collect()
            },
        })
        .unwrap();
    let client = RemoteClient::connect(Arc::from(conn), "relay-check", &host.id(), signer).unwrap();
    let session = RemoteSession::new(
        TerminalAddr {
            host: "relay-check".into(),
            terminal: 1,
        },
        GridSize { cols: 80, rows: 24 },
        Arc::new(|| {}),
    );
    session.attach(&client, 1).unwrap();
    let wait = |ok: &dyn Fn() -> bool| {
        let end = Instant::now() + Duration::from_secs(20);
        while !ok() {
            assert!(Instant::now() < end, "timed out");
            std::thread::sleep(Duration::from_micros(200));
        }
    };
    wait(&|| session.is_linked());
    // Warm up so the path settles, then measure.
    let mut samples = Vec::with_capacity(n);
    for i in 0..n + 5 {
        let before = session.position();
        let started = Instant::now();
        session.send_input(vec![b'a' + (i % 26) as u8]).unwrap();
        wait(&|| session.position() > before);
        if i >= 5 {
            samples.push(started.elapsed());
        }
    }
    let path = client.path();
    client.close();
    (path, samples)
}

#[test]
#[ignore = "needs RELAY_CHECK_TOKEN and the network"]
fn the_relay_refuses_a_tokenless_endpoint_and_admits_one_with_the_token() {
    let denied = IrohNode::start(&cfg(31, None, false)).unwrap();
    assert!(
        !denied.wait_online(Duration::from_secs(10)),
        "the relay admitted an endpoint with no token"
    );
    let why = denied.relay_problem();
    eprintln!(
        "without the token: not admitted; status: {}",
        why.as_deref().unwrap_or("(no error reported yet)")
    );
    assert!(why.is_some(), "no refusal was reported");
    drop(denied);

    let admitted = IrohNode::start(&cfg(32, Some(token()), false)).unwrap();
    assert!(
        admitted.wait_online(Duration::from_secs(15)),
        "the relay did not admit an endpoint with the token"
    );
    eprintln!(
        "with the token: admitted; no relay error: {}",
        admitted.relay_problem().is_none()
    );
    assert!(admitted.relay_problem().is_none());
}

#[test]
#[ignore = "needs RELAY_CHECK_TOKEN and the network"]
fn address_discovery_reports_a_public_address() {
    let node = IrohNode::start(&cfg(33, Some(token()), false)).unwrap();
    assert!(node.wait_online(Duration::from_secs(15)));
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut addrs = node.known_addrs();
    while !addrs.iter().any(|a| is_public(a.ip())) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        addrs = node.known_addrs();
    }
    let public: Vec<_> = addrs.iter().filter(|a| is_public(a.ip())).collect();
    eprintln!(
        "addresses known: {} ; public ones: {:?}",
        addrs.len(),
        public
    );
    assert!(
        !public.is_empty(),
        "no public address was discovered: {addrs:?}"
    );
}

#[test]
#[ignore = "needs RELAY_CHECK_TOKEN and the network"]
fn a_session_forced_through_the_relay_carries_bytes_and_its_latency_is_measured() {
    let allowed = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let signer = KeySigner(allowed.clone());

    // Relayed: both endpoints have no direct paths at all.
    let host = Arc::new(IrohNode::start(&cfg(34, Some(token()), true)).unwrap());
    let client = IrohNode::start(&cfg(35, Some(token()), true)).unwrap();
    assert!(host.wait_online(Duration::from_secs(20)));
    assert!(client.wait_online(Duration::from_secs(20)));
    serve(&host, &allowed);
    let (path, mut relayed) = measure(&host, &client, true, &signer, 100);
    assert_eq!(
        path,
        PathKind::Relayed,
        "a relay-only session did not report a relayed path"
    );
    drop((host, client));

    // Direct: the same machine, loopback, no relay involved in the path.
    let host = Arc::new(
        IrohNode::start(&IrohConfig {
            relay_url: None,
            relay_token: None,
            ..cfg(36, None, false)
        })
        .unwrap(),
    );
    let client = IrohNode::start(&IrohConfig {
        relay_url: None,
        relay_token: None,
        ..cfg(37, None, false)
    })
    .unwrap();
    serve(&host, &allowed);
    let (path, mut direct) = measure(&host, &client, false, &signer, 100);
    assert_eq!(path, PathKind::Direct);

    relayed.sort();
    direct.sort();
    for (name, v) in [("relayed", &relayed), ("direct (loopback)", &direct)] {
        eprintln!(
            "input -> echo, {name}: n={} min={:?} p50={:?} p95={:?} max={:?}",
            v.len(),
            v[0],
            percentile(v, 0.5),
            percentile(v, 0.95),
            v[v.len() - 1]
        );
    }
}
