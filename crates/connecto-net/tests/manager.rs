//! The remote manager the app owns: off by default, saved hosts with unique
//! names, the add-host fingerprint confirmation, authorized keys, and two
//! managers talking over real iroh endpoints on one machine.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use connecto_audit::{Audit, Sink};
use connecto_core::{
    GridSize, HostLink, HostingStatus, InputRefused, PauseSwitch, RemoteControl, RemoteGuard,
    ResizeOutcome, SessionStatus, SnapshotOutcome, TermSnapshot, TerminalSession,
};
use connecto_net::auth::{KeySigner, Signer};
use connecto_net::host::{FixedTerminals, HostTerminal, RingTap};
use connecto_net::manager::{DEFAULT_RELAY_URL, ManagerConfig, RemoteManager};
use connecto_net::pairing::KdfParams;
use connecto_net::reconnect::{Backoff, SignerSource, SupervisorConfig};
use connecto_net::relay_access::BUILT_IN_TOKEN;
use connecto_net::secrets::{MemorySecrets, RELAY_TOKEN, SecretStore};
use connecto_net::seq::Ring;
use connecto_session::RemoteGate;
use connecto_term::Emulator;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, PrivateKey};

struct Fake {
    emulator: Emulator,
    ring: Arc<Ring>,
    typed: Mutex<Vec<u8>>,
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

struct Keys(Vec<Arc<dyn Signer>>);

impl SignerSource for Keys {
    fn signers(&self) -> Vec<Arc<dyn Signer>> {
        self.0.clone()
    }
}

struct Machine {
    manager: Arc<RemoteManager>,
    guard: Arc<RemoteGate>,
    secrets: Arc<MemorySecrets>,
    pause: Arc<PauseSwitch>,
    fake: Arc<Fake>,
    dir: PathBuf,
    key: PrivateKey,
}

fn machine(tag: &str) -> Machine {
    let dir = std::env::temp_dir().join(format!("mg-net-mgr-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
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
    let terminals = Arc::new(FixedTerminals(Mutex::new(vec![HostTerminal {
        id: 1,
        name: "p: claude-1".into(),
        tool: 1,
        attempt: 1,
        activity: 5,
        session: Arc::clone(&fake) as Arc<dyn TerminalSession>,
        ring,
    }])));
    let pause = Arc::new(PauseSwitch::default());
    let guard = Arc::new(RemoteGate::new(
        Arc::clone(&pause),
        Audit::with_sink(Lines::default()).unwrap(),
    ));
    let secrets = Arc::new(MemorySecrets::default());
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let signer: Arc<dyn Signer> = Arc::new(KeySigner(key.clone()));
    let manager = RemoteManager::start(ManagerConfig {
        dir: dir.clone(),
        secrets: Arc::clone(&secrets) as Arc<dyn SecretStore>,
        guard: Arc::clone(&guard) as Arc<dyn RemoteGuard>,
        terminals,
        signers: Arc::new(Keys(vec![signer])),
        wake: Arc::new(|| {}),
        supervisor: SupervisorConfig {
            backoff: Backoff {
                initial: Duration::from_millis(50),
                max: Duration::from_millis(400),
            },
            heartbeat: Duration::from_millis(200),
            initial_size: GridSize { cols: 60, rows: 12 },
        },
        bind: Some("127.0.0.1:0".parse().unwrap()),
        share_loopback: true,
        device_name: format!("test-{tag}"),
        kdf: KdfParams::FAST,
        pairing_lifetime: Duration::from_secs(30),
        default_relay: None,
    });
    Machine {
        manager,
        guard,
        secrets,
        pause,
        fake,
        dir,
        key,
    }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
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
fn hosting_is_off_by_default_and_the_choice_is_saved() {
    let m = machine("off");
    let view = m.manager.view();
    assert!(!view.hosting_enabled);
    assert_eq!(view.hosting, HostingStatus::Off);
    assert!(!m.guard.enabled());
    assert!(view.hosts.is_empty() && view.authorized.is_empty());

    m.manager.set_hosting(true);
    assert!(m.guard.enabled());
    wait_until("listening", || {
        m.manager.view().hosting == HostingStatus::Listening
    });
    assert!(!m.manager.view().my_fingerprint.is_empty());
    assert!(m.manager.view().my_address.starts_with("connecto:"));
    let saved = std::fs::read_to_string(m.dir.join("settings.json")).unwrap();
    assert!(saved.contains("\"enabled\": true"));

    m.manager.set_hosting(false);
    assert!(!m.guard.enabled());
    assert_eq!(m.manager.view().hosting, HostingStatus::Off);
    m.manager.shutdown();
    // A new manager over the same folder starts with what was saved: off.
    let again = RemoteManager::start(ManagerConfig {
        dir: m.dir.clone(),
        secrets: Arc::clone(&m.secrets) as Arc<dyn SecretStore>,
        guard: Arc::new(RemoteGate::new(
            Arc::clone(&m.pause),
            Audit::with_sink(Lines::default()).unwrap(),
        )),
        terminals: Arc::new(FixedTerminals(Mutex::new(vec![]))),
        signers: Arc::new(Keys(vec![])),
        wake: Arc::new(|| {}),
        supervisor: SupervisorConfig::DEFAULT,
        bind: None,
        share_loopback: false,
        device_name: "x".into(),
        kdf: KdfParams::FAST,
        pairing_lifetime: Duration::from_secs(30),
        default_relay: None,
    });
    assert!(!again.view().hosting_enabled);
    again.shutdown();
    let _ = std::fs::remove_dir_all(&m.dir);
}

#[test]
fn authorized_keys_are_added_validated_listed_and_removed() {
    let m = machine("keys");
    assert!(m.manager.add_authorized_key("not a key").is_err());
    assert!(m.manager.add_authorized_key("a\nb").is_err());
    let line = format!("{} laptop", m.key.public_key().to_openssh().unwrap());
    m.manager.add_authorized_key(&line).unwrap();
    let view = m.manager.view();
    assert_eq!(view.authorized.len(), 1);
    assert_eq!(view.authorized[0].comment, "laptop");
    assert!(view.authorized_path.ends_with("authorized_keys"));
    // The file is the OpenSSH format, and owner-only (a mode on Unix; on
    // Windows the folder's ACL).
    let file = m.dir.join("authorized_keys");
    assert!(std::fs::read_to_string(&file).unwrap().contains("laptop"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    m.manager
        .remove_authorized_key(&view.authorized[0].fingerprint)
        .unwrap();
    assert!(m.manager.view().authorized.is_empty());
    m.manager.shutdown();
    let _ = std::fs::remove_dir_all(&m.dir);
}

#[test]
fn add_host_asks_for_a_unique_name_and_a_fingerprint_confirmation() {
    let m = machine("add");
    let ticket = format!("connecto:{}", "ab".repeat(32));
    assert!(m.manager.add_host("localhost", &ticket).is_err());
    assert!(m.manager.add_host("   ", &ticket).is_err());
    assert!(m.manager.add_host("x", "nonsense").is_err());
    m.manager.add_host("Build box", &ticket).unwrap();
    let pending = m.manager.view().pending.clone().unwrap();
    assert_eq!(pending.name, "Build box");
    assert_eq!(
        pending.fingerprint,
        connecto_net::ticket::fingerprint(&[0xab; 32])
    );
    // Nothing is saved until the fingerprint is confirmed.
    assert!(m.manager.view().hosts.is_empty());
    assert!(!m.dir.join("known_hosts").exists());
    m.manager.cancel_host();
    assert!(m.manager.view().pending.is_none());

    m.manager.add_host("Build box", &ticket).unwrap();
    m.manager.confirm_host();
    let view = m.manager.view();
    assert_eq!(view.hosts.len(), 1);
    assert_eq!(view.hosts[0].name, "Build box");
    assert!(m.dir.join("known_hosts").exists());
    // The same name or machine cannot be added again.
    assert!(
        m.manager
            .add_host("build BOX", &format!("connecto:{}", "cd".repeat(32)))
            .is_err()
    );
    assert!(m.manager.add_host("other", &ticket).is_err());
    // Rename keeps the pin; forget removes it.
    m.manager.rename_host("Build box", "Staging").unwrap();
    assert_eq!(m.manager.view().hosts[0].name, "Staging");
    assert!(m.manager.rename_host("Staging", "localhost").is_err());
    m.manager.forget_host("Staging").unwrap();
    assert!(m.manager.view().hosts.is_empty());
    m.manager.shutdown();
    let _ = std::fs::remove_dir_all(&m.dir);
}

#[test]
fn the_relay_token_is_a_secret_and_the_url_is_checked() {
    let m = machine("relay");
    assert!(
        m.manager
            .set_relay(Some("ftp://nope".into()), None)
            .is_err()
    );
    m.manager
        .set_relay(
            Some("https://relay.example:8443".into()),
            Some("s3cret".into()),
        )
        .unwrap();
    let view = m.manager.view();
    assert_eq!(
        view.relay_url.as_deref(),
        Some("https://relay.example:8443")
    );
    assert!(view.relay_token_set);
    // The token is in the secret store, and in no file or view.
    assert_eq!(m.secrets.get(RELAY_TOKEN).unwrap().unwrap(), b"s3cret");
    let settings = std::fs::read_to_string(m.dir.join("settings.json")).unwrap();
    assert!(!settings.contains("s3cret"));
    assert!(!format!("{view:?}").contains("s3cret"));
    m.manager.set_relay(None, Some(String::new())).unwrap();
    assert!(!m.manager.view().relay_token_set);
    assert!(m.manager.view().relay_url.is_none());
    m.manager.shutdown();
    let _ = std::fs::remove_dir_all(&m.dir);
}

#[test]
fn the_default_relay_uses_the_built_in_token_until_the_user_stores_one() {
    let m = machine("builtin");
    let built = BUILT_IN_TOKEN.is_some();
    // No token stored: the default relay has Connecto's access only if this build carries it.
    m.manager
        .set_relay(Some(DEFAULT_RELAY_URL.into()), None)
        .unwrap();
    let view = m.manager.view();
    assert!(!view.relay_token_set);
    assert_eq!(view.relay_builtin_token, built);
    // A token the user stores wins, and is never shown.
    m.manager
        .set_relay(Some(DEFAULT_RELAY_URL.into()), Some("mine".into()))
        .unwrap();
    let view = m.manager.view();
    assert!(view.relay_token_set);
    assert!(!view.relay_builtin_token);
    assert!(!format!("{view:?}").contains("mine"));
    // Clearing it falls back to the built-in access on the default relay.
    m.manager
        .set_relay(Some(DEFAULT_RELAY_URL.into()), Some(String::new()))
        .unwrap();
    let view = m.manager.view();
    assert!(!view.relay_token_set);
    assert_eq!(view.relay_builtin_token, built);
    // Any other relay never gets the built-in access.
    m.manager
        .set_relay(Some("https://relay.example:8443".into()), None)
        .unwrap();
    assert!(!m.manager.view().relay_builtin_token);
    m.manager.shutdown();
    let _ = std::fs::remove_dir_all(&m.dir);
}

fn relay_message(m: &Machine) -> bool {
    m.manager
        .view()
        .last_error
        .as_deref()
        .is_some_and(|e| e.starts_with("Could not reach the relay"))
}

/// A relay that failed is replaced by a working one (here, direct connections
/// only): its message is gone at once, and nothing of it comes back.
#[test]
fn replacing_a_failing_relay_clears_its_message() {
    let m = machine("replace");
    m.manager
        .set_relay(Some("https://127.0.0.1:9".into()), None)
        .unwrap();
    m.manager.set_hosting(true);
    wait_until("the relay message", || relay_message(&m));
    m.manager.set_relay(None, None).unwrap();
    wait_until("the message to clear", || {
        m.manager.view().last_error.is_none()
    });
    thread::sleep(Duration::from_secs(3));
    assert!(m.manager.view().last_error.is_none());
    m.manager.shutdown();
    let _ = std::fs::remove_dir_all(&m.dir);
}

/// Hosting turned off while the relay is being checked: the outstanding check
/// publishes nothing, so no message comes back after it is off.
#[test]
fn turning_hosting_off_during_a_relay_check_publishes_nothing() {
    let m = machine("hostoff");
    m.manager
        .set_relay(Some("https://127.0.0.1:9".into()), None)
        .unwrap();
    m.manager.set_hosting(true);
    wait_until("the relay message", || relay_message(&m));
    m.manager.set_hosting(false);
    assert!(m.manager.view().last_error.is_none());
    // Longer than one watch round, so a check that was in flight has finished.
    thread::sleep(Duration::from_secs(5));
    assert!(m.manager.view().last_error.is_none());
    m.manager.shutdown();
    let _ = std::fs::remove_dir_all(&m.dir);
}

#[test]
fn a_relay_that_cannot_be_reached_at_startup_is_said_in_last_error() {
    let m = machine("watch");
    // Nothing listens on this port, so the relay can never accept the endpoint.
    m.manager
        .set_relay(Some("https://127.0.0.1:9".into()), None)
        .unwrap();
    m.manager.set_hosting(true);
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let view = m.manager.view();
        if view
            .last_error
            .as_deref()
            .is_some_and(|e| e.starts_with("Could not reach the relay"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no relay message: {:?}",
            view.last_error
        );
        thread::sleep(Duration::from_millis(200));
    }
    m.manager.shutdown();
    let _ = std::fs::remove_dir_all(&m.dir);
}

#[test]
fn two_managers_connect_over_iroh_and_a_host_is_drawn_like_a_project() {
    let host = machine("e2e-host");
    let client = machine("e2e-client");
    host.manager
        .add_authorized_key(&client.key.public_key().to_openssh().unwrap())
        .unwrap();
    host.manager.set_hosting(true);
    wait_until("the host to listen", || {
        host.manager.view().hosting == HostingStatus::Listening
    });
    let host_view = host.manager.view();
    host.fake.print(b"hello from the host\r\n");

    client
        .manager
        .add_host("build-box", &host_view.my_address)
        .unwrap();
    // The fingerprint the client is asked to confirm is the one the host shows.
    assert_eq!(
        client.manager.view().pending.clone().unwrap().fingerprint,
        host_view.my_fingerprint
    );
    client.manager.confirm_host();
    wait_until("a direct link", || {
        client.manager.view().hosts[0].link == HostLink::Direct
    });
    wait_until("the terminal list", || {
        client.manager.view().hosts[0].terminals == 1
    });

    // The host's terminals come back as a registry the window already draws.
    let terminals = client.manager.terminals("build-box").unwrap();
    let list = terminals.view();
    assert_eq!(list.terminals.len(), 1);
    assert_eq!(list.terminals[0].name.as_deref(), Some("p: claude-1"));
    let session = Arc::clone(&list.terminals[0].session);
    wait_until("the screen", || {
        text(&*session).contains("hello from the host")
    });
    session.send_input(b"ls\r".to_vec()).unwrap();
    wait_until("typed bytes", || {
        *host.fake.typed.lock().unwrap() == b"ls\r"
    });
    assert!(host.manager.view().peer.is_some());

    // Pause on the host stops the typing.
    host.pause.pause();
    session.send_input(b"x".to_vec()).unwrap();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(*host.fake.typed.lock().unwrap(), b"ls\r");
    host.pause.resume();

    // Disconnecting goes back to disconnected; connecting again works.
    client.manager.disconnect("build-box");
    assert_eq!(client.manager.view().hosts[0].link, HostLink::Disconnected);
    client.manager.connect("build-box");
    wait_until("a link again", || {
        client.manager.view().hosts[0].link == HostLink::Direct
    });
    host.manager.shutdown();
    client.manager.shutdown();
    let _ = std::fs::remove_dir_all(&host.dir);
    let _ = std::fs::remove_dir_all(&client.dir);
}

#[test]
fn a_host_that_is_off_cannot_be_dialed() {
    let host = machine("off-host");
    let client = machine("off-client");
    // The client needs the host's id and address: start it, read them, stop it.
    host.manager.set_hosting(true);
    wait_until("listening", || {
        host.manager.view().hosting == HostingStatus::Listening
    });
    let address = host.manager.view().my_address.clone();
    host.manager.set_hosting(false);
    client.manager.add_host("gone", &address).unwrap();
    client.manager.confirm_host();
    thread::sleep(Duration::from_millis(1500));
    let link = client.manager.view().hosts[0].link.clone();
    assert!(
        !matches!(
            link,
            HostLink::Direct | HostLink::Relayed | HostLink::Connected
        ),
        "{link:?}"
    );
    host.manager.shutdown();
    client.manager.shutdown();
    let _ = std::fs::remove_dir_all(&host.dir);
    let _ = std::fs::remove_dir_all(&client.dir);
}

#[test]
fn a_new_install_uses_the_default_relay_and_a_choice_of_none_is_kept() {
    let dir = std::env::temp_dir().join(format!("mg-net-defrelay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let start = |default: Option<&str>| {
        RemoteManager::start(ManagerConfig {
            dir: dir.clone(),
            secrets: Arc::new(MemorySecrets::default()) as Arc<dyn SecretStore>,
            guard: Arc::new(RemoteGate::new(
                Arc::new(PauseSwitch::default()),
                Audit::with_sink(Lines::default()).unwrap(),
            )),
            terminals: Arc::new(FixedTerminals(Mutex::new(vec![]))),
            signers: Arc::new(Keys(vec![])),
            wake: Arc::new(|| {}),
            supervisor: SupervisorConfig::DEFAULT,
            bind: None,
            share_loopback: false,
            device_name: "x".into(),
            kdf: KdfParams::FAST,
            pairing_lifetime: Duration::from_secs(30),
            default_relay: default.map(str::to_owned),
        })
    };
    let first = start(Some("https://relay.example"));
    assert_eq!(
        first.view().relay_url.as_deref(),
        Some("https://relay.example")
    );
    // Choosing no relay on purpose is remembered, not replaced by the default.
    first.set_relay(None, None).unwrap();
    first.shutdown();
    let again = start(Some("https://relay.example"));
    assert_eq!(again.view().relay_url, None);
    // Choosing another one is remembered too.
    again
        .set_relay(Some("https://other.example".into()), None)
        .unwrap();
    again.shutdown();
    let third = start(Some("https://relay.example"));
    assert_eq!(
        third.view().relay_url.as_deref(),
        Some("https://other.example")
    );
    third.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
