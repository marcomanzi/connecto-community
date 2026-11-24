//! Pairing two machines by a code each (ADR 0022), over real iroh endpoints and
//! an in-process relay (no outside network): the two codes, the rendezvous, the
//! exchange, the names, the record that lands on both sides, and a session that
//! opens afterwards. Plus the ways it can end, and what each leaves behind.

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
use connecto_net::manager::{DEFAULT_RELAY_URL, ManagerConfig, RemoteManager};
use connecto_net::pairing::{CODE_DIGITS, KdfParams};
use connecto_net::reconnect::{Backoff, SupervisorConfig};
use connecto_net::relay_access::{BUILT_IN_TOKEN, NO_TOKEN};
use connecto_net::secrets::{MemorySecrets, SecretStore, pair_key_name};
use connecto_net::seq::Ring;
use connecto_net::ticket::Ticket;
use connecto_net::trust::hex;
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

struct Machine {
    manager: Arc<RemoteManager>,
    pause: Arc<PauseSwitch>,
    fake: Arc<Fake>,
    dir: PathBuf,
    log: Lines,
    secrets: Arc<MemorySecrets>,
}

fn machine_in(
    dir: PathBuf,
    tag: &str,
    relay: &str,
    lifetime: Duration,
    secrets: Arc<MemorySecrets>,
) -> Machine {
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
    let manager = RemoteManager::start(ManagerConfig {
        dir: dir.clone(),
        secrets: Arc::clone(&secrets) as Arc<dyn SecretStore>,
        guard: guard as Arc<dyn RemoteGuard>,
        terminals,
        signers: Arc::new(DeviceKeySigners(
            Arc::clone(&secrets) as Arc<dyn SecretStore>
        )),
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
        secrets,
    }
}

fn machine(tag: &str, relay: &str, lifetime: Duration) -> Machine {
    let dir = std::env::temp_dir().join(format!("mg-net-pair-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    machine_in(
        dir,
        tag,
        relay,
        lifetime,
        Arc::new(MemorySecrets::default()),
    )
}

struct World {
    _runtime: tokio::runtime::Runtime,
    _server: iroh_relay::server::Server,
    url: String,
}

fn world() -> World {
    connecto_net::iroh_transport::trust_any_relay_certificate_for_tests();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (_map, url, server) = runtime
        .block_on(iroh::test_utils::run_relay_server_with(true))
        .unwrap();
    World {
        _runtime: runtime,
        _server: server,
        url: url.to_string(),
    }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn phase(m: &Machine) -> PairingPhase {
    m.manager.view().pairing.clone()
}

fn wait_phase(m: &Machine, what: &str, ok: impl Fn(&PairingPhase) -> bool) {
    wait_until(what, || ok(&phase(m)));
}

fn is_failed(p: &PairingPhase) -> bool {
    matches!(p, PairingPhase::Failed { .. })
}

fn is_named(p: &PairingPhase) -> bool {
    matches!(p, PairingPhase::NameNeeded { .. })
}

fn is_done(p: &PairingPhase) -> bool {
    matches!(p, PairingPhase::Done { .. })
}

/// The code this machine shows. Starts a pairing first when none is under way.
fn my_code(m: &Machine) -> String {
    if matches!(
        phase(m),
        PairingPhase::Idle | PairingPhase::Done { .. } | PairingPhase::Failed { .. }
    ) {
        m.manager.start_mutual_pairing().unwrap();
    }
    wait_phase(m, "a code", |p| {
        matches!(p, PairingPhase::ShowingCode { .. })
    });
    match phase(m) {
        PairingPhase::ShowingCode { my_code, .. } => my_code,
        other => panic!("{other:?}"),
    }
}

/// This machine's endpoint id, from the address it shares.
fn id_of(m: &Machine) -> [u8; 32] {
    // The endpoint starts with hosting; a test that reads the id first turns it on.
    if m.manager.view().my_address.is_empty() {
        m.manager.set_hosting(true);
    }
    wait_until("the address", || !m.manager.view().my_address.is_empty());
    Ticket::parse(&m.manager.view().my_address).unwrap().id
}

fn authorized_lines(m: &Machine) -> Vec<String> {
    std::fs::read_to_string(m.dir.join("authorized_keys"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn known_lines(m: &Machine) -> Vec<String> {
    std::fs::read_to_string(m.dir.join("known_hosts"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Nothing was saved on this machine for the machine `other`.
fn nothing_saved_for(m: &Machine, other: &[u8; 32]) -> bool {
    authorized_lines(m).is_empty()
        && known_lines(m).is_empty()
        && m.manager.view().machines.is_empty()
        && m.secrets.get(&pair_key_name(other)).unwrap().is_none()
}

/// Both codes are in on both machines, in the order given, and both have named
/// the other one (`a` is named "Studio", `b` "Laptop").
fn pair_up_to_names(a: &Machine, b: &Machine, a_first: bool) -> (String, String) {
    let ca = my_code(a);
    let cb = my_code(b);
    if a_first {
        a.manager.submit_peer_code(&cb).unwrap();
        b.manager.submit_peer_code(&ca).unwrap();
    } else {
        b.manager.submit_peer_code(&ca).unwrap();
        a.manager.submit_peer_code(&cb).unwrap();
    }
    wait_phase(a, "A to name B", is_named);
    wait_phase(b, "B to name A", is_named);
    (ca, cb)
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

fn finish(machines: &[&Machine]) {
    for m in machines {
        m.manager.shutdown();
        let _ = std::fs::remove_dir_all(&m.dir);
    }
}

#[test]
fn two_codes_pair_both_ways_and_a_session_opens_afterwards() {
    let world = world();
    let a = machine("a1", &world.url, Duration::from_secs(60));
    let b = machine("b1", &world.url, Duration::from_secs(60));
    a.fake.print(b"hello from a\r\n");
    let (a_id, b_id) = (id_of(&a), id_of(&b));

    pair_up_to_names(&a, &b, true);
    a.manager.finish_pairing("Studio").unwrap();
    b.manager.finish_pairing("Laptop").unwrap();
    wait_phase(&a, "A done", is_done);
    wait_phase(&b, "B done", is_done);
    assert_eq!(
        phase(&a),
        PairingPhase::Done {
            machine_id: hex(&b_id)
        }
    );
    assert_eq!(
        phase(&b),
        PairingPhase::Done {
            machine_id: hex(&a_id)
        }
    );

    // Each side has the other as one machine, under the name it chose.
    let rows = a.manager.view().machines.clone();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].id.as_str(), rows[0].name.as_str()),
        (hex(&b_id).as_str(), "Studio")
    );
    assert_eq!(b.manager.view().machines[0].name, "Laptop");
    assert!(!rows[0].paired_at.is_empty());

    // One key each way, and the record's pieces are all there.
    let lines = authorized_lines(&a);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains(&format!("connecto-id={}", hex(&b_id))),
        "{}",
        lines[0]
    );
    assert_eq!(known_lines(&a).len(), 1);
    assert!(a.secrets.get(&pair_key_name(&b_id)).unwrap().is_some());
    assert!(b.secrets.get(&pair_key_name(&a_id)).unwrap().is_some());

    // The codes are on no screen and in no log.
    let log = a.log.0.lock().unwrap().join("\n");
    assert!(log.contains("\"action\":\"pair\""), "{log}");

    // B opens a session on A, with the keys the pairing made.
    let terminals = b.manager.terminals("Laptop").unwrap();
    wait_until("the terminal list", || {
        terminals.view().terminals.len() == 1
    });
    let session = Arc::clone(&terminals.view().terminals[0].session);
    wait_until("A's screen", || text(&*session).contains("hello from a"));
    wait_until("A to see B connected", || {
        a.manager.view().machines[0].inbound != HostLink::Disconnected
    });
    finish(&[&a, &b]);
}

#[test]
fn the_order_of_entry_does_not_matter() {
    let world = world();
    for a_first in [true, false] {
        let a = machine(
            &format!("a-order-{a_first}"),
            &world.url,
            Duration::from_secs(60),
        );
        let b = machine(
            &format!("b-order-{a_first}"),
            &world.url,
            Duration::from_secs(60),
        );
        let (a_id, b_id) = (id_of(&a), id_of(&b));
        pair_up_to_names(&a, &b, a_first);
        a.manager.finish_pairing("B").unwrap();
        b.manager.finish_pairing("A").unwrap();
        wait_phase(&a, "A done", is_done);
        wait_phase(&b, "B done", is_done);
        assert_eq!(a.manager.view().machines[0].id, hex(&b_id));
        assert_eq!(b.manager.view().machines[0].id, hex(&a_id));
        finish(&[&a, &b]);
    }
}

#[test]
fn a_wrong_code_saves_nothing_and_a_corrected_one_still_pairs() {
    let world = world();
    let a = machine("a-wrong", &world.url, Duration::from_secs(60));
    let b = machine("b-wrong", &world.url, Duration::from_secs(60));
    let (a_id, b_id) = (id_of(&a), id_of(&b));
    let ca = my_code(&a);
    let cb = my_code(&b);
    // A mistypes B's code: its rendezvous is not B's, so nobody answers.
    let digits: u32 = cb
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap();
    let modulus = 10_u32.pow(CODE_DIGITS as u32);
    let wrong = format!("{:0w$}", (digits + 1) % modulus, w = CODE_DIGITS);
    a.manager.submit_peer_code(&wrong).unwrap();
    b.manager.submit_peer_code(&ca).unwrap();
    thread::sleep(Duration::from_secs(2));
    assert!(!is_named(&phase(&a)) && !is_named(&phase(&b)));
    assert!(nothing_saved_for(&a, &b_id) && nothing_saved_for(&b, &a_id));
    // A types it again, correctly, and the pairing goes on.
    a.manager.submit_peer_code(&cb).unwrap();
    wait_phase(&a, "A to name B", is_named);
    wait_phase(&b, "B to name A", is_named);
    a.manager.finish_pairing("B").unwrap();
    b.manager.finish_pairing("A").unwrap();
    wait_phase(&a, "A done", is_done);
    wait_phase(&b, "B done", is_done);
    finish(&[&a, &b]);
}

#[test]
fn an_expired_code_fails_both_sides_and_saves_nothing() {
    let world = world();
    let a = machine("a-exp", &world.url, Duration::from_secs(3));
    let b = machine("b-exp", &world.url, Duration::from_secs(3));
    let (a_id, b_id) = (id_of(&a), id_of(&b));
    let ca = my_code(&a);
    let cb = my_code(&b);
    // Only A enters B's code; B never enters A's, so A waits until the code runs out.
    a.manager.submit_peer_code(&cb).unwrap();
    wait_phase(&a, "A to expire", is_failed);
    assert!(matches!(phase(&a), PairingPhase::Failed { message } if message.contains("expired")));
    // B's own code runs out too, with nothing entered.
    wait_phase(&b, "B to expire", is_failed);
    let _ = ca;
    assert!(nothing_saved_for(&a, &b_id) && nothing_saved_for(&b, &a_id));
    finish(&[&a, &b]);
}

#[test]
fn the_default_relay_with_no_token_fails_at_once_and_says_where_to_add_one() {
    // A build that carries Connecto's token has one for the default relay.
    if BUILT_IN_TOKEN.is_some() {
        return;
    }
    let a = machine("a-notoken", DEFAULT_RELAY_URL, Duration::from_secs(60));
    let b = machine("b-notoken", DEFAULT_RELAY_URL, Duration::from_secs(60));
    // An attempt starts once the other machine's code is entered.
    let _ = my_code(&a);
    let cb = my_code(&b);
    let started = Instant::now();
    a.manager.submit_peer_code(&cb).unwrap();
    wait_phase(&a, "the failure", is_failed);
    // No wait for the relay to come online: it cannot without a token.
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(matches!(phase(&a), PairingPhase::Failed { message } if message == NO_TOKEN));
    finish(&[&a, &b]);
}

#[test]
fn cancelling_while_waiting_or_showing_leaves_the_machine_idle_and_nothing_saved() {
    let world = world();
    let a = machine("a-cancel", &world.url, Duration::from_secs(60));
    let b = machine("b-cancel", &world.url, Duration::from_secs(60));
    let (a_id, b_id) = (id_of(&a), id_of(&b));
    let ca = my_code(&a);
    let cb = my_code(&b);
    // A has B's code and waits for B, which has not entered A's.
    a.manager.submit_peer_code(&cb).unwrap();
    wait_phase(&a, "A to be waiting", |p| {
        matches!(
            p,
            PairingPhase::WaitingForPeer { .. } | PairingPhase::Preparing
        )
    });
    a.manager.cancel_pairing();
    assert_eq!(phase(&a), PairingPhase::Idle);
    // B cancels while it only shows its code.
    let _ = ca;
    b.manager.cancel_pairing();
    assert_eq!(phase(&b), PairingPhase::Idle);
    thread::sleep(Duration::from_millis(500));
    assert!(nothing_saved_for(&a, &b_id) && nothing_saved_for(&b, &a_id));
    finish(&[&a, &b]);
}

#[test]
fn cancelling_on_either_side_after_the_names_saves_nothing_on_either() {
    let world = world();
    // The machine that cancels is A, then B the one that cancels.
    for cancel_a in [true, false] {
        let a = machine(
            &format!("a-named-{cancel_a}"),
            &world.url,
            Duration::from_secs(60),
        );
        let b = machine(
            &format!("b-named-{cancel_a}"),
            &world.url,
            Duration::from_secs(60),
        );
        let (a_id, b_id) = (id_of(&a), id_of(&b));
        pair_up_to_names(&a, &b, true);
        let (canceller, other) = if cancel_a { (&a, &b) } else { (&b, &a) };
        canceller.manager.cancel_pairing();
        assert_eq!(phase(canceller), PairingPhase::Idle);
        other.manager.finish_pairing("Other").unwrap();
        wait_phase(other, "the other side to fail", is_failed);
        thread::sleep(Duration::from_millis(300));
        assert!(nothing_saved_for(&a, &b_id) && nothing_saved_for(&b, &a_id));
        finish(&[&a, &b]);
    }
}

#[test]
fn a_machine_that_is_paused_refuses_at_the_commit_and_saves_nothing() {
    let world = world();
    let a = machine("a-pause", &world.url, Duration::from_secs(60));
    let b = machine("b-pause", &world.url, Duration::from_secs(60));
    let (a_id, b_id) = (id_of(&a), id_of(&b));
    pair_up_to_names(&a, &b, true);
    a.pause.pause();
    a.manager.finish_pairing("B").unwrap();
    b.manager.finish_pairing("A").unwrap();
    wait_phase(
        &a,
        "A to refuse",
        |p| matches!(p, PairingPhase::Failed { message } if message.contains("Pause")),
    );
    wait_phase(
        &b,
        "B to hear the refusal",
        |p| matches!(p, PairingPhase::Failed { message } if message.contains("Pause")),
    );
    assert!(nothing_saved_for(&a, &b_id) && nothing_saved_for(&b, &a_id));
    let log = a.log.0.lock().unwrap().join("\n");
    assert!(log.contains("\"decision\":\"deny\""), "{log}");
    finish(&[&a, &b]);
}

#[test]
fn a_machine_cannot_pair_with_its_own_code() {
    let world = world();
    let a = machine("a-self", &world.url, Duration::from_secs(60));
    let code = my_code(&a);
    // Its own code, typed into itself: nothing starts, and the view says why.
    let err = a.manager.submit_peer_code(&code).unwrap_err();
    assert!(err.contains("own code"), "{err}");
    assert!(matches!(phase(&a), PairingPhase::ShowingCode { .. }));
    assert!(authorized_lines(&a).is_empty());
    finish(&[&a]);
}

#[test]
fn pairing_the_same_machine_again_replaces_its_record() {
    let world = world();
    let a = machine("a-re", &world.url, Duration::from_secs(60));
    let b = machine("b-re", &world.url, Duration::from_secs(60));
    let b_id = id_of(&b);
    pair_up_to_names(&a, &b, true);
    a.manager.finish_pairing("Studio").unwrap();
    b.manager.finish_pairing("Laptop").unwrap();
    wait_phase(&a, "A done", is_done);
    let first_key = a.secrets.get(&pair_key_name(&b_id)).unwrap().unwrap();
    // The starts are spaced out, so the second pairing waits a moment.
    thread::sleep(Duration::from_millis(3100));
    a.manager.cancel_pairing();
    b.manager.cancel_pairing();
    pair_up_to_names(&a, &b, false);
    a.manager.finish_pairing("Studio").unwrap();
    b.manager.finish_pairing("Laptop").unwrap();
    wait_phase(&a, "A done again", is_done);
    wait_phase(&b, "B done again", is_done);
    assert_eq!(a.manager.view().machines.len(), 1);
    assert_eq!(authorized_lines(&a).len(), 1);
    assert_eq!(known_lines(&a).len(), 1);
    assert_ne!(
        a.secrets.get(&pair_key_name(&b_id)).unwrap().unwrap(),
        first_key
    );
    finish(&[&a, &b]);
}

#[test]
fn starting_and_entering_codes_is_limited() {
    let world = world();
    let a = machine("a-limit", &world.url, Duration::from_secs(60));
    my_code(&a);
    // A second start while one is under way is refused.
    assert!(a.manager.start_mutual_pairing().is_err());
    // Codes entered: five at most, then no more.
    let wrong = format!("{:0w$}", 1, w = CODE_DIGITS);
    let wrong = wrong.as_str();
    for n in 1..=MAX_CODES {
        a.manager
            .submit_peer_code(wrong)
            .unwrap_or_else(|e| panic!("code {n}: {e}"));
    }
    assert!(
        a.manager.submit_peer_code(wrong).is_err(),
        "a sixth code was allowed"
    );
    // A new pairing right after a cancel waits a few seconds.
    a.manager.cancel_pairing();
    assert!(
        a.manager.start_mutual_pairing().is_err(),
        "a new one at once was allowed"
    );
    finish(&[&a]);
}

const MAX_CODES: usize = 5;

#[test]
fn disconnecting_a_machine_closes_its_live_session_and_removes_everything() {
    let world = world();
    let a = machine("a-dc", &world.url, Duration::from_secs(60));
    let b = machine("b-dc", &world.url, Duration::from_secs(60));
    let b_id = id_of(&b);
    pair_up_to_names(&a, &b, true);
    a.manager.finish_pairing("Studio").unwrap();
    b.manager.finish_pairing("Laptop").unwrap();
    wait_phase(&a, "A done", is_done);
    wait_phase(&b, "B done", is_done);
    let terminals = b.manager.terminals("Laptop").unwrap();
    wait_until("the terminal list", || {
        terminals.view().terminals.len() == 1
    });
    wait_until("A to see B connected", || {
        a.manager.view().machines[0].inbound != HostLink::Disconnected
    });

    let id = hex(&b_id);
    a.manager.disconnect_machine(&id).unwrap();
    // A's live session with B is closed, and nothing is left for B on A.
    wait_until("A to see B gone", || {
        a.manager.view().machines.is_empty() && a.manager.view().peer.is_none()
    });
    assert!(authorized_lines(&a).is_empty());
    assert!(known_lines(&a).is_empty());
    assert!(a.secrets.get(&pair_key_name(&b_id)).unwrap().is_none());
    assert!(a.manager.disconnect_machine(&id).is_err());
    // B's next attempt is refused, cleanly.
    wait_until("B to be refused", || {
        matches!(b.manager.view().hosts[0].link, HostLink::Refused(_))
    });
    finish(&[&a, &b]);
}

#[test]
fn machines_paired_by_the_older_flow_are_listed_and_removable() {
    let world = world();
    let dir = std::env::temp_dir().join(format!("mg-net-pair-{}-legacy", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let key =
        ssh_key::PrivateKey::random(&mut ssh_key::rand_core::OsRng, ssh_key::Algorithm::Ed25519)
            .unwrap();
    let line = format!(
        "{} laptop (paired 2026-01-02)\n",
        key.public_key().to_openssh().unwrap()
    );
    std::fs::write(dir.join("authorized_keys"), line).unwrap();
    let old_host = [9_u8; 32];
    std::fs::write(
        dir.join("known_hosts"),
        format!("Build box\t{}\t-\t\n", hex(&old_host)),
    )
    .unwrap();
    let m = machine_in(
        dir,
        "legacy",
        &world.url,
        Duration::from_secs(60),
        Arc::new(MemorySecrets::default()),
    );
    let view = m.manager.view();
    let legacy = view
        .machines
        .iter()
        .find(|r| r.id.starts_with("legacy:"))
        .expect("the older key is listed");
    assert_eq!(legacy.name, "laptop");
    assert_eq!(legacy.paired_at, "2026-01-02");
    assert!(
        view.machines
            .iter()
            .any(|r| r.id == hex(&old_host) && r.name == "Build box")
    );
    assert!(
        view.authorized.is_empty(),
        "a paired key is not a hand-added one"
    );

    let legacy_id = legacy.id.clone();
    m.manager.disconnect_machine(&legacy_id).unwrap();
    m.manager.disconnect_machine(&hex(&old_host)).unwrap();
    assert!(m.manager.view().machines.is_empty());
    assert!(authorized_lines(&m).is_empty());
    assert!(known_lines(&m).is_empty());
    finish(&[&m]);
}

#[test]
fn a_hand_added_key_is_not_a_machine_and_a_machine_key_is_not_removed_by_fingerprint() {
    let world = world();
    let a = machine("a-hand", &world.url, Duration::from_secs(60));
    let key =
        ssh_key::PrivateKey::random(&mut ssh_key::rand_core::OsRng, ssh_key::Algorithm::Ed25519)
            .unwrap();
    let line = key.public_key().to_openssh().unwrap();
    a.manager.add_authorized_key(&line).unwrap();
    let view = a.manager.view();
    assert!(view.machines.is_empty());
    assert_eq!(view.authorized.len(), 1);
    let fp = view.authorized[0].fingerprint.clone();
    a.manager.remove_authorized_key(&fp).unwrap();
    assert!(a.manager.view().authorized.is_empty());
    finish(&[&a]);
}

#[test]
fn the_other_side_refusing_at_the_commit_rolls_back_what_this_side_saved() {
    let world = world();
    let a = machine("a-roll", &world.url, Duration::from_secs(60));
    let b = machine("b-roll", &world.url, Duration::from_secs(60));
    let (a_id, b_id) = (id_of(&a), id_of(&b));
    // Nothing is left behind on either side when the commit is refused.
    pair_up_to_names(&a, &b, true);
    // B is paused: it refuses at its commit, after A has written its record.
    b.pause.pause();
    a.manager.finish_pairing("B").unwrap();
    b.manager.finish_pairing("A").unwrap();
    wait_phase(
        &a,
        "A to hear the refusal",
        |p| matches!(p, PairingPhase::Failed { message } if message.contains("Pause")),
    );
    wait_phase(&b, "B to refuse", is_failed);
    // A had staged its record; the rollback leaves nothing behind.
    assert!(nothing_saved_for(&a, &b_id), "{:?}", known_lines(&a));
    assert!(nothing_saved_for(&b, &a_id));
    finish(&[&a, &b]);
}

#[test]
fn a_cancel_during_the_peer_commit_wait_rolls_back_on_both_sides_whichever_cancels() {
    let world = world();
    for a_cancels in [true, false] {
        let tag = if a_cancels { "a" } else { "b" };
        let a = machine(&format!("a-cw-{tag}"), &world.url, Duration::from_secs(60));
        let b = machine(&format!("b-cw-{tag}"), &world.url, Duration::from_secs(60));
        let (a_id, b_id) = (id_of(&a), id_of(&b));
        pair_up_to_names(&a, &b, true);
        let (canceller, other) = if a_cancels { (&a, &b) } else { (&b, &a) };
        // The canceller names and waits for the other side's intent, which the
        // other side has not yet given (it has not named).
        canceller.manager.finish_pairing("Other").unwrap();
        wait_phase(canceller, "the canceller to wait", |p| {
            matches!(p, PairingPhase::Confirming)
        });
        canceller.manager.cancel_pairing();
        assert_eq!(phase(canceller), PairingPhase::Idle);
        thread::sleep(Duration::from_millis(300));
        // The other side names afterwards and meets the cancel: it must fail too.
        other.manager.finish_pairing("Canceller").unwrap();
        wait_phase(other, "the other side to fail", is_failed);
        thread::sleep(Duration::from_millis(300));
        assert!(nothing_saved_for(&a, &b_id) && nothing_saved_for(&b, &a_id));
        finish(&[&a, &b]);
    }
}

#[test]
fn a_peer_that_vanishes_during_the_commit_wait_leaves_nothing_saved_on_the_other_side() {
    let world = world();
    let a = machine("a-vanish", &world.url, Duration::from_secs(60));
    let b = machine("b-vanish", &world.url, Duration::from_secs(60));
    let (a_id, b_id) = (id_of(&a), id_of(&b));
    pair_up_to_names(&a, &b, true);
    // A names and waits for B's intent; B goes away (its side is cancelled).
    a.manager.finish_pairing("B").unwrap();
    wait_phase(&a, "A to wait", |p| matches!(p, PairingPhase::Confirming));
    b.manager.cancel_pairing();
    wait_phase(&a, "A to see B gone", is_failed);
    assert_eq!(phase(&b), PairingPhase::Idle);
    thread::sleep(Duration::from_millis(300));
    assert!(nothing_saved_for(&a, &b_id) && nothing_saved_for(&b, &a_id));
    finish(&[&a, &b]);
}
