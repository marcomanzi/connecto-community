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
    setup_with(fail_first, |fixed| fixed)
}

/// As [`setup`], with the host's terminals wrapped by `wrap`.
fn setup_with(
    fail_first: usize,
    wrap: impl FnOnce(Arc<FixedTerminals>) -> Arc<dyn connecto_net::host::HostTerminals>,
) -> Setup {
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
        entry: None,
        ran: true,
        owner: None,
        background: Vec::new(),
        id: 3,
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
    let keys = Arc::new(
        AuthorizedKeySet::parse(&format!("{}\n", allowed.public_key().to_openssh().unwrap()))
            .unwrap(),
    );
    let service = Arc::new(HostService::new(
        wrap(terminals),
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

/// A host whose project answers asks: it records each in order, refuses
/// settings that name the package `refused` and a Continue of `gone`, and is
/// slow to read the transcript `slow`.
struct AnsweringHost {
    fixed: Arc<FixedTerminals>,
    seen: Mutex<Vec<connecto_net::project_panels::ProjectAsk>>,
    /// Refuse a Refresh.
    refuse_refresh: std::sync::atomic::AtomicBool,
    /// Answer no settings, as a host that cannot give them.
    refuse_settings: std::sync::atomic::AtomicBool,
}

impl connecto_net::host::HostTerminals for AnsweringHost {
    fn list(&self) -> Vec<HostTerminal> {
        self.fixed.list()
    }
    fn project_ask(
        &self,
        _right: &connecto_net::host::Right,
        ask: &connecto_net::project_panels::ProjectAsk,
    ) -> Result<Vec<u8>, String> {
        use connecto_net::project_panels::{self as panels, ProjectAsk};
        self.seen.lock().unwrap().push(ask.clone());
        match ask {
            ProjectAsk::SetSettings(settings) if settings.packages == ["refused"] => {
                Err("that machine has no package refused".into())
            }
            ProjectAsk::Settings if self.refuse_settings.load(Ordering::SeqCst) => {
                Err("that machine keeps no settings for this project".into())
            }
            ProjectAsk::Refresh if self.refuse_refresh.load(Ordering::SeqCst) => {
                Err("that machine paused remote actions".into())
            }
            ProjectAsk::Continue { history } if history == "gone" => {
                Err("that closed terminal is no longer in the project's history".into())
            }
            ProjectAsk::Transcript { history } => {
                if history == "slow" {
                    thread::sleep(Duration::from_millis(400));
                }
                Ok(panels::transcript_json(
                    history,
                    &format!("text of {history}"),
                ))
            }
            // Idle until asked to update, then running.
            ProjectAsk::FunctionalMemory
            | ProjectAsk::FunctionalMemoryUpdate
            | ProjectAsk::FunctionalMemorySet { .. } => {
                use connecto_core::functional_memory::{FmFiles, FmState, FmStatus};
                let updating = self
                    .seen
                    .lock()
                    .unwrap()
                    .contains(&ProjectAsk::FunctionalMemoryUpdate);
                Ok(panels::functional_memory_json(
                    &FmFiles::default(),
                    &FmStatus {
                        state: if updating {
                            FmState::Running
                        } else {
                            FmState::Idle
                        },
                        commit: Some("abc1234".into()),
                        updated_ms: Some(7),
                        message: None,
                        auto: true,
                        ..Default::default()
                    },
                ))
            }
            ProjectAsk::Settings => Ok(panels::settings_answer_json(&panels::SettingsAnswer {
                project: Some(connecto_core::ProjectPackagesView {
                    project_id: "p".into(),
                    // A project of Codex and Pi.
                    settings: connecto_core::ProjectSettings {
                        tools: connecto_core::ProjectTools::from_tools([
                            connecto_core::ToolKind::Codex,
                            connecto_core::ToolKind::Pi,
                        ])
                        .unwrap(),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                ..Default::default()
            })),
            _ => Ok(panels::done_json()),
        }
    }
}

fn answering() -> (Setup, Arc<AnsweringHost>) {
    let mut host = None;
    let setup = setup_with(0, |fixed| {
        let answering = Arc::new(AnsweringHost {
            fixed,
            seen: Mutex::new(Vec::new()),
            refuse_refresh: std::sync::atomic::AtomicBool::new(false),
            refuse_settings: std::sync::atomic::AtomicBool::new(false),
        });
        host = Some(Arc::clone(&answering));
        answering
    });
    (setup, host.unwrap())
}

fn connected(setup: &Setup) -> Arc<HostHandle> {
    let handle = start(setup, config(20, 200));
    wait_until("the connection", || {
        handle.is_connected() && !handle.listed_projects().is_empty()
    });
    handle
}

/// A host project's new-terminal choices are the agents its Tools setting
/// there allows, once its settings are here; a shell always.
#[test]
fn a_host_projects_chooser_offers_only_the_agents_it_allows() {
    use connecto_core::{TerminalsControl, ToolKind};
    use connecto_net::remote_terminals::RemoteTerminals;
    let (setup, _host) = answering();
    let handle = connected(&setup);
    let terminals = RemoteTerminals::for_project(Arc::clone(&handle), "p");
    // Asked first, before the host has answered: the agents wait.
    assert!(terminals.agents_loading(), "loading on the first call");
    wait_until("the host's settings", || !terminals.agents_loading());
    assert!(!terminals.offers(ToolKind::Claude));
    assert!(terminals.offers(ToolKind::Codex));
    assert!(terminals.offers(ToolKind::Pi));
    assert!(!terminals.offers(ToolKind::Antigravity));
    assert!(terminals.offers(ToolKind::Shell));
}

/// A host that cannot give its project's settings: once it has said so,
/// every agent is offered (it refuses what it does not take).
#[test]
fn a_host_without_settings_offers_every_agent_once_it_has_said_so() {
    use connecto_core::{TerminalsControl, ToolKind};
    use connecto_net::remote_terminals::RemoteTerminals;
    let (setup, host) = answering();
    host.refuse_settings.store(true, Ordering::SeqCst);
    let handle = connected(&setup);
    let terminals = RemoteTerminals::for_project(Arc::clone(&handle), "p");
    assert!(terminals.agents_loading());
    wait_until("the refusal", || !terminals.agents_loading());
    assert!(terminals.offers(ToolKind::Claude));
    assert!(terminals.offers(ToolKind::Antigravity));
}

/// Changes reach the host in the order they were made, however quickly they
/// follow each other. A refusal of the last change is said, and stays said
/// after the view is fetched again; a later change that goes through clears
/// it.
#[test]
fn project_changes_reach_the_host_in_order_and_a_refusal_stays_said() {
    use connecto_net::project_cache::PanelKind;
    use connecto_net::project_panels::ProjectAsk;
    let (setup, host) = answering();
    let handle = connected(&setup);
    let settings = |package: &str| connecto_core::ProjectSettings {
        packages: vec![package.to_owned()],
        ..Default::default()
    };
    let sets = |host: &AnsweringHost| -> Vec<String> {
        host.seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|a| match a {
                ProjectAsk::SetSettings(s) => s.packages.first().cloned(),
                _ => None,
            })
            .collect()
    };
    let sent: Vec<String> = (0..8).map(|n| format!("pkg{n}")).collect();
    for package in &sent {
        handle.project_command("p", ProjectAsk::SetSettings(settings(package)));
    }
    wait_until("every change answered", || sets(&host).len() == sent.len());
    assert_eq!(sets(&host), sent, "in the order made");
    assert!(!handle.latest_save_refused("p"));

    handle.project_command("p", ProjectAsk::SetSettings(settings("refused")));
    wait_until("the refusal", || handle.latest_save_refused("p"));
    let refusal = Some("that machine has no package refused".to_owned());
    wait_until("the view fetched again", || {
        handle
            .panel("p", PanelKind::Settings, Duration::from_secs(60))
            .0
            .is_some()
    });
    thread::sleep(Duration::from_millis(100));
    assert_eq!(handle.peek_panel("p", PanelKind::Settings).1, refusal);
    assert!(handle.latest_save_refused("p"));

    // A refused save, then a Refresh that goes through: the save is still
    // the one refused.
    handle.project_command("p", ProjectAsk::Refresh);
    wait_until("the refresh", || {
        host.seen.lock().unwrap().last() == Some(&ProjectAsk::Refresh)
    });
    thread::sleep(Duration::from_millis(50));
    assert!(handle.latest_save_refused("p"), "a Refresh is not a save");
    assert_eq!(
        handle.peek_panel("p", PanelKind::Settings).1,
        refusal,
        "and why is still said"
    );

    // A save that goes through, then a refused Refresh: the save stands.
    handle.project_command("p", ProjectAsk::SetSettings(settings("kept")));
    host.refuse_refresh.store(true, Ordering::SeqCst);
    handle.project_command("p", ProjectAsk::Refresh);
    wait_until("the refused refresh", || {
        host.seen.lock().unwrap().last() == Some(&ProjectAsk::Refresh)
            && handle
                .peek_panel("p", PanelKind::Settings)
                .1
                .is_some_and(|e| e.contains("paused"))
    });
    assert!(!handle.latest_save_refused("p"), "the save went through");
    host.refuse_refresh.store(false, Ordering::SeqCst);

    // A refused change, then one that goes through: the later one stands.
    handle.project_command("p", ProjectAsk::SetSettings(settings("refused")));
    handle.project_command("p", ProjectAsk::SetSettings(settings("fine")));
    wait_until("both answered", || {
        sets(&host).last().map(String::as_str) == Some("fine")
    });
    wait_until("the later one stands", || {
        !handle.latest_save_refused("p") && handle.peek_panel("p", PanelKind::Settings).1.is_none()
    });
    handle.stop();
}

/// The transcript asked for last is the one kept, even when an earlier ask
/// is answered after it; a refused Continue is said as the list's error.
#[test]
fn the_transcript_asked_last_wins_and_a_refused_continue_is_said() {
    use connecto_net::project_panels::ProjectAsk;
    let (setup, _host) = answering();
    let handle = connected(&setup);
    handle.request_transcript("p", "slow");
    thread::sleep(Duration::from_millis(50));
    handle.request_transcript("p", "fast");
    wait_until("the fast transcript", || {
        handle
            .transcript("p")
            .is_some_and(|t| t.history_id == "fast")
    });
    thread::sleep(Duration::from_millis(600));
    let shown = handle.transcript("p").unwrap();
    assert_eq!(shown.history_id, "fast", "the slow answer was dropped");
    assert_eq!(*shown.text, "text of fast");

    handle.project_command(
        "p",
        ProjectAsk::Continue {
            history: "gone".into(),
        },
    );
    wait_until("the refusal", || handle.last_error().is_some());
    assert!(
        handle
            .last_error()
            .unwrap()
            .contains("no longer in the project's history")
    );
    handle.stop();
}

// ---------- a host project's secrets, as the window here keeps them

/// A host whose projects' secrets are in a vault (in memory), or one from
/// before secrets were shared (`old`), which knows no such ask.
struct VaultHost {
    fixed: Arc<FixedTerminals>,
    vault: Arc<connecto_vault::Vault>,
    old: bool,
}

impl connecto_net::host::HostTerminals for VaultHost {
    fn list(&self) -> Vec<HostTerminal> {
        self.fixed.list()
    }
    fn secret_asks(&self) -> Vec<connecto_core::SecretAsk> {
        connecto_core::VaultControl::view(&*self.vault).asks.clone()
    }
    fn project_ask(
        &self,
        right: &connecto_net::host::Right,
        ask: &connecto_net::project_panels::ProjectAsk,
    ) -> Result<Vec<u8>, String> {
        let connecto_net::host::Target::Project(project) = right.target() else {
            return Err("not a project".into());
        };
        if self.old {
            return Err(connecto_net::project_panels::UNKNOWN_ASK.into());
        }
        connecto_net::remote_secrets::answer_secrets(&*self.vault, project, "Project", ask)
    }
}

fn vault_host(old: bool) -> (Setup, Arc<connecto_vault::Vault>) {
    let vault = Arc::new(connecto_vault::Vault::open(
        Arc::new(connecto_secrets::Secrets::new(
            Arc::new(connecto_secrets::MemoryItems::default()),
            None,
        )),
        Arc::new(connecto_vault::FixedKey([5; 32])),
        None,
    ));
    let kept = Arc::clone(&vault);
    let setup = setup_with(0, move |fixed| {
        Arc::new(VaultHost {
            fixed,
            vault: kept,
            old,
        })
    });
    (setup, vault)
}

fn ask_secret(vault: &connecto_vault::Vault, name: &str) -> u64 {
    connecto_core::VaultBridge::ask(
        vault,
        &connecto_core::SecretCaller {
            terminal: "claude-1".into(),
            project: Some("p".into()),
            project_folder: None,
            owns_pid: Arc::new(|_| false),
        },
        connecto_core::AskRequest {
            name: name.into(),
            kind: connecto_core::AskKind::Token,
            reason: "to deploy the site to Fly.io".into(),
            proposed: Vec::new(),
        },
        Box::new(|_| {}),
    )
    .unwrap()
}

const VALUE: &str = "a-long-remote-value-123";

/// The window here learns of a host terminal's request, shows it as its
/// project's, and answers it through that project's secrets: the value goes
/// to the host's vault and the request leaves. One the host's person
/// answered first leaves too, and answering it from here is refused. A
/// secret added from here shows in the project's list, never its value; an
/// app-wide scope, or a value too short, is refused here and never sent.
#[test]
fn the_window_here_answers_a_host_terminals_request_and_keeps_a_projects_secrets() {
    use connecto_core::{AskAnswer, ProviderScope, VaultControl};
    use connecto_net::remote_secrets::{NOT_WAITING, RemoteVault};
    let (setup, vault) = vault_host(false);
    let first = ask_secret(&vault, "TOKEN");
    let handle = connected(&setup);
    let remote = RemoteVault::new(Arc::clone(&handle), "p".into(), "shown".into());
    let shown = ProviderScope::Project("shown".into());
    wait_until("the request told here", || remote.view().asks.len() == 1);
    let asked = remote.view().asks[0].clone();
    assert_eq!(asked.id, first);
    assert_eq!(asked.project.as_deref(), Some("shown"));

    assert!(
        remote
            .answer(
                first,
                AskAnswer::Provide {
                    scope: ProviderScope::Global,
                    value: VALUE.into(),
                    bindings: Vec::new(),
                },
            )
            .is_err(),
        "only the project's own"
    );
    assert!(
        remote
            .answer(
                first,
                AskAnswer::Provide {
                    scope: shown.clone(),
                    value: "short".into(),
                    bindings: Vec::new(),
                },
            )
            .is_err()
    );
    remote
        .answer(
            first,
            AskAnswer::Provide {
                scope: shown.clone(),
                value: VALUE.into(),
                bindings: Vec::new(),
            },
        )
        .unwrap();
    wait_until("the request gone here", || remote.view().asks.is_empty());
    assert!(vault.view().asks.is_empty(), "and on the host");
    assert!(
        connecto_core::VaultBridge::known(&*vault)
            .iter()
            .any(|k| k == VALUE)
    );

    // The host's person answers first: the request leaves here too, and an
    // answer from here finds nothing waiting.
    let second = ask_secret(&vault, "OTHER");
    wait_until("the second request", || remote.view().asks.len() == 1);
    vault.answer(second, AskAnswer::Decline).unwrap();
    wait_until("it leaves here", || remote.view().asks.is_empty());
    remote.answer(second, AskAnswer::Decline).unwrap();
    wait_until("the refusal said", || {
        remote.view().problem.as_deref() == Some(NOT_WAITING)
    });

    remote.put(&shown, "GH", "deploys", VALUE).unwrap();
    wait_until("the secret listed", || {
        remote.view().entries.iter().any(|e| e.name == "GH")
    });
    let view = remote.view();
    let gh = view.entries.iter().find(|e| e.name == "GH").unwrap();
    assert_eq!(gh.scope, shown, "under the id the window knows");
    assert!(
        vault
            .view()
            .entries
            .iter()
            .any(|e| e.name == "GH" && e.scope == ProviderScope::Project("p".into()))
    );
    assert!(remote.put(&ProviderScope::Global, "GH", "", VALUE).is_err());
    handle.stop();
}

/// A host from before secrets were shared: the section says to update
/// Connecto there, and a change is refused here, never kept anywhere else.
#[test]
fn a_host_too_old_for_secrets_says_to_update_it_and_nothing_is_kept() {
    use connecto_core::{ProviderScope, VaultControl};
    use connecto_net::remote_secrets::{RemoteVault, SECRETS_OLD_HOST};
    let (setup, vault) = vault_host(true);
    let handle = connected(&setup);
    let remote = RemoteVault::new(Arc::clone(&handle), "p".into(), "shown".into());
    wait_until("the update message", || {
        remote.view().problem.as_deref() == Some(SECRETS_OLD_HOST)
    });
    assert!(remote.view().entries.is_empty());
    let shown = ProviderScope::Project("shown".into());
    assert_eq!(
        remote.put(&shown, "GH", "", VALUE).unwrap_err(),
        SECRETS_OLD_HOST
    );
    assert_eq!(
        remote.put_generated_key(&shown, "KEY", "").unwrap_err(),
        SECRETS_OLD_HOST
    );
    assert!(vault.view().entries.is_empty());
    handle.stop();
}

/// An answer sent from here is followed by the command that carried it:
/// two refusals in the same words are each told, and a corrected answer
/// is taken.
#[test]
fn each_answer_sent_from_here_is_told_by_its_own_outcome() {
    use connecto_core::{AnswerState, AskAnswer, Binding, ProviderScope, VaultControl};
    use connecto_net::remote_secrets::RemoteVault;
    let (setup, vault) = vault_host(false);
    let id = ask_secret(&vault, "TOKEN");
    let handle = connected(&setup);
    let remote = RemoteVault::new(Arc::clone(&handle), "p".into(), "shown".into());
    let shown = ProviderScope::Project("shown".into());
    assert_eq!(remote.answer_state(id), None, "nothing sent yet");
    let answer = |base_url: &str| AskAnswer::Provide {
        scope: shown.clone(),
        value: VALUE.into(),
        bindings: vec![Binding::Http {
            base_url: base_url.into(),
            header: "Authorization".into(),
            prefix: String::new(),
        }],
    };
    let mut refusals = Vec::new();
    for _ in 0..2 {
        remote.answer(id, answer("ftp://elsewhere")).unwrap();
        assert!(remote.answer_state(id).is_some());
        wait_until("the refusal", || {
            matches!(remote.answer_state(id), Some(AnswerState::Refused(_)))
        });
        let Some(AnswerState::Refused(why)) = remote.answer_state(id) else {
            unreachable!()
        };
        refusals.push(why);
    }
    assert_eq!(refusals[0], refusals[1], "the same words, each told");
    remote
        .answer(id, answer("https://api.example.com"))
        .unwrap();
    wait_until("taken", || {
        remote.answer_state(id) == Some(AnswerState::Taken)
    });
    wait_until("the request gone", || remote.view().asks.is_empty());
    handle.stop();
}

/// A host project's secrets, many and long, in a list that records each
/// change of use.
struct ManySecrets {
    view: Mutex<connecto_core::VaultView>,
    uses_set: Mutex<Vec<String>>,
}

impl connecto_core::VaultControl for ManySecrets {
    fn view(&self) -> Arc<connecto_core::VaultView> {
        Arc::new(self.view.lock().unwrap().clone())
    }
    fn put(
        &self,
        _: &connecto_core::ProviderScope,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), String> {
        Err("not here".into())
    }
    fn set_bindings(
        &self,
        _: &connecto_core::ProviderScope,
        name: &str,
        _: Vec<connecto_core::Binding>,
    ) -> Result<(), String> {
        self.uses_set.lock().unwrap().push(name.to_owned());
        Ok(())
    }
    fn delete(&self, _: &connecto_core::ProviderScope, _: &str) -> Result<(), String> {
        Err("not here".into())
    }
}

struct ManyHost {
    fixed: Arc<FixedTerminals>,
    vault: Arc<ManySecrets>,
}

impl connecto_net::host::HostTerminals for ManyHost {
    fn list(&self) -> Vec<HostTerminal> {
        self.fixed.list()
    }
    fn project_ask(
        &self,
        right: &connecto_net::host::Right,
        ask: &connecto_net::project_panels::ProjectAsk,
    ) -> Result<Vec<u8>, String> {
        let connecto_net::host::Target::Project(project) = right.target() else {
            return Err("not a project".into());
        };
        connecto_net::remote_secrets::answer_secrets(&*self.vault, project, "Project", ask)
    }
}

/// More secrets than this machine reads at once: the window goes on from
/// where the list stopped, reaches the last one, and changes its use.
#[test]
fn a_secret_past_what_is_read_at_once_is_reached_and_changed() {
    use connecto_core::{Binding, ProviderScope, VaultControl};
    use connecto_net::remote_secrets::RemoteVault;
    let long = Binding::Command {
        programs: (0..32)
            .map(|n| format!("/{n}/{}", "x".repeat(1900)))
            .collect(),
        env: "TOKEN".into(),
    };
    let vault = Arc::new(ManySecrets {
        view: Mutex::new(connecto_core::VaultView {
            entries: (0..50)
                .map(|n| connecto_core::SecretEntry {
                    name: format!("K{n:02}"),
                    scope: ProviderScope::Project("p".into()),
                    description: String::new(),
                    bindings: vec![long.clone()],
                    updated_unix: 0,
                    last_used_unix: None,
                    public_key: None,
                })
                .collect(),
            ..connecto_core::VaultView::default()
        }),
        uses_set: Mutex::new(Vec::new()),
    });
    let kept = Arc::clone(&vault);
    let setup = setup_with(0, move |fixed| Arc::new(ManyHost { fixed, vault: kept }));
    let handle = connected(&setup);
    // One page of the host's at a time.
    handle.limit_secrets_read(1);
    let remote = RemoteVault::new(Arc::clone(&handle), "p".into(), "shown".into());
    let shown = ProviderScope::Project("shown".into());
    wait_until("the first part", || !remote.view().entries.is_empty());
    let first = remote.view();
    assert!(
        !first.entries.iter().any(|e| e.name == "K49"),
        "not all at once"
    );
    assert_eq!(first.pages.len(), 1);
    assert_eq!(first.pages[0].scope, shown);
    let mut steps = 0;
    while !remote.view().entries.iter().any(|e| e.name == "K49") {
        let next = remote.view().pages[0]
            .next
            .clone()
            .expect("more to go on to");
        remote.show_secrets_from(&shown, Some(next.clone()));
        wait_until("the next part", || {
            remote
                .view()
                .pages
                .first()
                .is_some_and(|p| p.from.as_ref() == Some(&next))
        });
        steps += 1;
        assert!(steps < 50);
    }
    assert!(steps >= 1);
    remote
        .set_bindings(&shown, "K49", vec![Binding::Ssh])
        .unwrap();
    wait_until("the change", || {
        vault.uses_set.lock().unwrap().as_slice() == ["K49"]
    });
    // And back to the first.
    remote.show_secrets_from(&shown, None);
    wait_until("the first again", || {
        remote.view().entries.iter().any(|e| e.name == "K00")
    });
    handle.stop();
}

/// A host with project groups: one named `name`, holding project `p`. Its
/// answer waits `slow` first when set.
struct GroupedHost {
    fixed: Arc<FixedTerminals>,
    name: Mutex<String>,
    slow: std::sync::atomic::AtomicBool,
}

impl connecto_net::host::HostTerminals for GroupedHost {
    fn list(&self) -> Vec<HostTerminal> {
        self.fixed.list()
    }
    fn groups(&self) -> connecto_core::RemoteGroups {
        if self.slow.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(600));
        }
        connecto_core::RemoteGroups {
            groups: vec![connecto_core::ProjectGroup {
                id: "group-1".into(),
                name: self.name.lock().unwrap().clone(),
            }],
            of: vec![("p".into(), "group-1".into())],
        }
    }
}

/// The window here learns of the host's groups once connected, and of
/// none once the link is gone.
#[test]
fn the_hosts_groups_are_known_here_while_connected() {
    let setup = setup_with(0, |fixed| {
        Arc::new(GroupedHost {
            fixed,
            name: Mutex::new("HARBOR".into()),
            slow: std::sync::atomic::AtomicBool::new(false),
        })
    });
    let handle = connected(&setup);
    wait_until("the groups", || {
        handle.groups().is_some_and(|g| {
            g.groups.len() == 1 && g.of == [("p".to_owned(), "group-1".to_owned())]
        })
    });
    handle.stop();
    wait_until("the link to go", || !handle.is_connected());
    assert_eq!(handle.groups(), None);
}

/// After a reconnect, the groups the last connection was told never show:
/// none until the host answers again, however slowly.
#[test]
fn a_reconnect_shows_no_groups_of_the_last_connection() {
    let mut host = None;
    let setup = setup_with(0, |fixed| {
        let grouped = Arc::new(GroupedHost {
            fixed,
            name: Mutex::new("HARBOR".into()),
            slow: std::sync::atomic::AtomicBool::new(false),
        });
        host = Some(Arc::clone(&grouped));
        grouped
    });
    let host = host.unwrap();
    let handle = connected(&setup);
    let named = |name: &str| {
        let handle = Arc::clone(&handle);
        let name = name.to_owned();
        move || handle.groups().is_some_and(|g| g.groups[0].name == name)
    };
    wait_until("the first groups", named("HARBOR"));
    *host.name.lock().unwrap() = "Renamed".into();
    host.slow.store(true, Ordering::SeqCst);
    let dials = setup.dialer.dial_count();
    setup.dialer.cut();
    wait_until("connected again", || {
        setup.dialer.dial_count() > dials && handle.is_connected()
    });
    // Until the slow answer comes: nothing, never the old name.
    let until = Instant::now() + Duration::from_millis(400);
    while Instant::now() < until && handle.is_connected() {
        let shown = handle.groups();
        assert!(
            shown.as_ref().is_none_or(|g| g.groups[0].name == "Renamed"),
            "{shown:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    wait_until("the new groups", named("Renamed"));
    handle.stop();
}

/// A host terminal that is stopped until `running` is set.
struct Switched {
    emulator: Emulator,
    running: std::sync::atomic::AtomicBool,
}

impl TerminalSession for Switched {
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
        if self.running.load(Ordering::SeqCst) {
            SessionStatus::Running
        } else {
            SessionStatus::Unknown {
                reason: "not started".into(),
            }
        }
    }
}

/// A stopped terminal that has not run since the host started is listed but
/// not attached: a stream for each of them costs threads on both machines.
/// A restored chat waiting for Start is still attached, and a stopped one is
/// attached once the host runs it.
#[test]
fn a_never_started_terminal_is_listed_but_attached_only_once_it_runs() {
    let stopped = |id: u32, name: &str| {
        let session = Arc::new(Switched {
            emulator: Emulator::new(
                GridSize { cols: 60, rows: 12 },
                Arc::new(|_| {}),
                Arc::new(|| {}),
            ),
            running: std::sync::atomic::AtomicBool::new(false),
        });
        let term = HostTerminal {
            entry: Some(format!("claude-{id}")),
            // Only the restored chat has a ring before it runs.
            ran: name == connecto_core::session::APP_SESSION,
            owner: None,
            background: Vec::new(),
            id,
            name: name.into(),
            tool: 1,
            project_id: "p".into(),
            project_name: "Project".into(),
            parent: None,
            attempt: 1,
            // NotStarted
            activity: 0,
            session: Arc::clone(&session) as Arc<dyn TerminalSession>,
            ring: Arc::new(Ring::new(1 << 20)),
        };
        (session, term)
    };
    let (plain_session, plain) = stopped(7, "Weekly report setup");
    let (_, chat) = stopped(12, connecto_core::session::APP_SESSION);
    let host_list: Arc<Mutex<Option<Arc<FixedTerminals>>>> = Arc::default();
    let keep = Arc::clone(&host_list);
    let setup = setup_with(0, move |fixed| {
        fixed.0.lock().unwrap().extend([plain, chat]);
        *keep.lock().unwrap() = Some(Arc::clone(&fixed));
        fixed
    });
    let handle = start(&setup, config(10, 50));
    let linked = |id: u32| {
        handle
            .terminals()
            .iter()
            .find(|t| t.info.id == id)
            .map(|t| t.session.is_linked())
    };
    wait_until("the running terminal and the chat to attach", || {
        linked(3) == Some(true) && linked(12) == Some(true)
    });
    // Several heartbeats and resync gaps later, still listed and unattached.
    thread::sleep(Duration::from_millis(600));
    assert_eq!(
        linked(7),
        Some(false),
        "a never-started terminal was attached"
    );

    // The host starts it: the next list attaches it.
    plain_session.running.store(true, Ordering::SeqCst);
    if let Some(fixed) = host_list.lock().unwrap().as_ref() {
        for term in fixed.0.lock().unwrap().iter_mut() {
            if term.id == 7 {
                term.activity = 5;
            }
        }
    }
    wait_until("the started terminal to attach", || linked(7) == Some(true));
    handle.stop();
}

/// A host project's functional memory as the window shows it: its status
/// from the host, and Update now asked of the host, which then says it is
/// running; the switch and the model are changed on the host.
#[test]
fn a_host_projects_functional_memory_shows_its_status_and_updates_on_the_host() {
    use connecto_core::functional_memory::{FmState, FunctionalMemoryControl};
    use connecto_net::project_panels::ProjectAsk;
    use connecto_net::remote_settings::RemoteFunctionalMemory;
    let (setup, host) = answering();
    let handle = connected(&setup);
    let memory = RemoteFunctionalMemory::new(Arc::clone(&handle), "p".into());
    wait_until("the host's status", || {
        memory.status("shown").state == FmState::Idle
    });
    let status = memory.status("shown");
    assert_eq!(status.commit.as_deref(), Some("abc1234"));
    assert_eq!(status.updated_ms, Some(7));
    // The switch and the model are the host's to change, asked of it.
    memory.set_auto("shown", false).unwrap();
    memory
        .set_model(
            "shown",
            Some(connecto_core::functional_memory::FmModel::new(
                connecto_core::memory::Provider::Codex,
                "gpt-6.2-sol",
            )),
        )
        .unwrap();
    wait_until("the changes asked of the host", || {
        let seen = host.seen.lock().unwrap();
        seen.contains(&ProjectAsk::FunctionalMemorySet {
            auto: Some(false),
            model: None,
        }) && seen.iter().any(|a| {
            matches!(a, ProjectAsk::FunctionalMemorySet { auto: None, model: Some(Some(m)) }
                if m.model == "gpt-6.2-sol")
        })
    });
    memory.update_now("shown").unwrap();
    wait_until("the update asked of the host", || {
        host.seen
            .lock()
            .unwrap()
            .contains(&ProjectAsk::FunctionalMemoryUpdate)
    });
    wait_until("the host's new status", || {
        memory.status("shown").state == FmState::Running
    });
    handle.stop();
    wait_until("the link to go", || !handle.is_connected());
    assert!(memory.update_now("shown").is_err());
}

/// A connection asks for large terminal uploads as it connects, so a file
/// past 10 MiB goes to the host rather than being refused here as for an
/// older host. (This host keeps no files: its answer says it got the ask.)
#[test]
fn a_connection_asks_for_large_uploads_so_a_file_past_ten_mib_reaches_the_host() {
    let setup = setup(0);
    let handle = connected(&setup);
    let large = vec![0; connecto_net::wire::LEGACY_UPLOAD_BYTES + 1];
    let why = handle.upload_file(3, "big.bin", &large).unwrap_err();
    assert!(
        why.contains("cannot keep files"),
        "refused before the host: {why}"
    );
    handle.stop();
}
