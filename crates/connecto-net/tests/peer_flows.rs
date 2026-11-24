//! What a peer sees and does when the host's terminals change, through the real
//! supervisor (`reconnect::spawn`) and client: its own restart, add and close,
//! and changes made on the host while it is connected. The peer never
//! reconnects for any of them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use connecto_audit::{Audit, Sink};
use connecto_core::{
    GridSize, InputRefused, OutputTap, PauseSwitch, RemoteGuard, ResizeOutcome, SessionStatus,
    SnapshotOutcome, TermSnapshot, TerminalSession,
};
use connecto_gate::Gate;
use connecto_net::auth::{AuthorizedKeySet, KeySigner, Signer};
use connecto_net::host::{
    HostService, HostTerminal, HostTerminals, NO_SUCH_PROJECT, Right, RingTap, Target,
};
use connecto_net::reconnect::TOO_OLD_FOR_OPENCODE;
use connecto_net::reconnect::{Backoff, Dialer, HostHandle, SignerSource, SupervisorConfig, spawn};
use connecto_net::seq::Ring;
use connecto_net::transport::loopback::{self, LoopbackConnection};
use connecto_net::transport::{Connection, EndpointId, NetError, Stream};
use connecto_net::wire::{FEATURE_OPENCODE, ManageOp, Message, ProjectInfo};
use connecto_session::RemoteGate;
use connecto_term::Emulator;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, PrivateKey};

const HOST_ID: [u8; 32] = [0xaa; 32];
const CLIENT_ID: [u8; 32] = [0xbb; 32];

/// A terminal on the host: output the test prints, and input it records.
struct FakeTerm {
    emulator: Emulator,
    ring: Arc<Ring>,
    typed: Mutex<Vec<u8>>,
}

impl FakeTerm {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            emulator: Emulator::new(
                GridSize { cols: 60, rows: 12 },
                Arc::new(|_| {}),
                Arc::new(|| {}),
            ),
            ring: Arc::new(Ring::new(1 << 20)),
            typed: Mutex::new(Vec::new()),
        })
    }

    fn print(&self, bytes: &[u8]) {
        RingTap::recording(Arc::clone(&self.ring)).feed(bytes, &mut |b| self.emulator.feed(b));
    }

    fn typed(&self) -> Vec<u8> {
        self.typed.lock().unwrap().clone()
    }
}

impl TerminalSession for FakeTerm {
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

struct Entry {
    id: u32,
    project: String,
    term: Arc<FakeTerm>,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    /// Projects open with no terminal yet.
    empty: Vec<String>,
    next_id: u32,
    /// What peers asked for, in order.
    asked: Vec<ManageOp>,
}

/// The host's terminals. Changes come from the peer (through `manage`) or from
/// the test, which plays the local user.
#[derive(Default)]
struct Registry {
    state: Mutex<State>,
    /// How many times the host read the registry's terminals.
    lists: AtomicUsize,
}

impl Registry {
    fn new(terminals: &[(u32, &str)], empty: &[&str]) -> Arc<Self> {
        let registry = Self::default();
        {
            let mut state = registry.state.lock().unwrap();
            state.next_id = 100;
            state.empty = empty.iter().map(|p| (*p).to_owned()).collect();
        }
        for (id, project) in terminals {
            registry.insert(*id, project);
        }
        Arc::new(registry)
    }

    fn insert(&self, id: u32, project: &str) {
        let mut state = self.state.lock().unwrap();
        state.entries.push(Entry {
            id,
            project: project.to_owned(),
            term: FakeTerm::new(),
        });
    }

    /// A new terminal in `project`, started on the host.
    fn local_add(&self, project: &str) -> u32 {
        let id = {
            let mut state = self.state.lock().unwrap();
            state.next_id += 1;
            state.next_id
        };
        self.insert(id, project);
        id
    }

    /// The host's own restart: a new session, a new ring, the old ring closed.
    fn local_restart(&self, id: u32) {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .expect("restart of a terminal that is not there");
        entry.term.ring.close();
        entry.term = FakeTerm::new();
    }

    /// The wire id of a terminal right (the registry's entry ids are these).
    fn target_id(&self, right: &Right) -> Result<u32, String> {
        match right.target() {
            Target::Terminal { id, .. } => Ok(*id),
            Target::Project(_) => Err("no such terminal".to_owned()),
        }
    }

    /// The host closes an entry on its own (the user did it).
    fn local_close(&self, id: u32) {
        let mut state = self.state.lock().unwrap();
        if let Some(at) = state.entries.iter().position(|e| e.id == id) {
            let entry = state.entries.remove(at);
            entry.term.ring.close();
        }
    }

    /// The terminal as the host runs it now.
    fn current(&self, id: u32) -> Option<Arc<FakeTerm>> {
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| Arc::clone(&e.term))
    }

    fn asked(&self) -> Vec<ManageOp> {
        self.state.lock().unwrap().asked.clone()
    }
}

impl HostTerminals for Registry {
    fn list(&self) -> Vec<HostTerminal> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| HostTerminal {
                entry: None,
                ran: true,
                owner: None,
                background: Vec::new(),
                id: e.id,
                name: format!("t{}", e.id),
                tool: 1,
                project_id: e.project.clone(),
                project_name: format!("Name of {}", e.project),
                parent: None,
                attempt: 1,
                activity: 0,
                session: Arc::clone(&e.term) as Arc<dyn TerminalSession>,
                ring: Arc::clone(&e.term.ring),
            })
            .collect()
    }

    fn projects(&self) -> Vec<ProjectInfo> {
        let state = self.state.lock().unwrap();
        let mut ids: Vec<String> = Vec::new();
        for id in state
            .entries
            .iter()
            .map(|e| e.project.clone())
            .chain(state.empty.iter().cloned())
        {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids.into_iter()
            .map(|id| ProjectInfo {
                name: format!("Name of {id}"),
                id,
            })
            .collect()
    }

    fn active_project(&self) -> Option<String> {
        Some("p".to_owned())
    }

    /// This registry has no children, so a close is the target's alone.
    fn close_within(&self, right: &Right, _listed: &[u32]) -> Result<Option<u32>, String> {
        self.manage(
            &ManageOp::Close {
                terminal: self.target_id(right)?,
            },
            right,
        )
    }

    fn manage(&self, op: &ManageOp, right: &Right) -> Result<Option<u32>, String> {
        // The check and the action are one step under the registry's lock.
        let mut state = self.state.lock().unwrap();
        state.asked.push(op.clone());
        match (op, right.target()) {
            (ManageOp::Add { .. }, Target::Project(project)) => {
                let open = state.entries.iter().any(|e| &e.project == project)
                    || state.empty.contains(project);
                if !open {
                    return Err(NO_SUCH_PROJECT.to_owned());
                }
                state.next_id += 1;
                let id = state.next_id;
                state.entries.push(Entry {
                    id,
                    project: project.clone(),
                    term: FakeTerm::new(),
                });
                Ok(Some(id))
            }
            (
                ManageOp::Close { .. } | ManageOp::Restart { .. } | ManageOp::Rename { .. },
                Target::Terminal { id, project },
            ) => {
                let at = state
                    .entries
                    .iter()
                    .position(|e| e.id == *id && &e.project == project)
                    .ok_or("no such terminal")?;
                match op {
                    ManageOp::Close { .. } => {
                        let entry = state.entries.remove(at);
                        entry.term.ring.close();
                    }
                    ManageOp::Restart { .. } => {
                        let entry = &mut state.entries[at];
                        entry.term.ring.close();
                        entry.term = FakeTerm::new();
                    }
                    _ => {}
                }
                Ok(None)
            }
            _ => Err("no such terminal".to_owned()),
        }
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
    dials: AtomicUsize,
    /// The host answers as one from before OpenCode: it never grants
    /// [`FEATURE_OPENCODE`].
    before_opencode: AtomicBool,
}

impl Dialer for LoopDialer {
    fn dial(&self) -> Result<Arc<dyn Connection>, String> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        let (client, host) = loopback::pair(CLIENT_ID, HOST_ID, [1; 32]);
        let service = Arc::clone(&self.service);
        thread::spawn(move || service.serve(&host));
        let client: Arc<LoopbackConnection> = Arc::new(client);
        if self.before_opencode.load(Ordering::SeqCst) {
            return Ok(Arc::new(BeforeOpenCode(client)));
        }
        Ok(client)
    }
}

/// The client's side of a host from before OpenCode: what it grants never
/// holds [`FEATURE_OPENCODE`], as such a host only grants what it knows.
struct BeforeOpenCode(Arc<LoopbackConnection>);

struct BeforeOpenCodeStream(Box<dyn Stream>);

impl Stream for BeforeOpenCodeStream {
    fn send(&self, message: &Message) -> Result<(), NetError> {
        self.0.send(message)
    }
    fn recv(&self, timeout: Duration) -> Result<Option<Message>, NetError> {
        Ok(match self.0.recv(timeout)? {
            Some(Message::Features(granted)) => {
                Some(Message::Features(granted & !FEATURE_OPENCODE))
            }
            other => other,
        })
    }
}

impl Connection for BeforeOpenCode {
    fn local_id(&self) -> EndpointId {
        self.0.local_id()
    }
    fn remote_id(&self) -> EndpointId {
        self.0.remote_id()
    }
    fn channel_binding(&self) -> Result<[u8; 32], NetError> {
        self.0.channel_binding()
    }
    fn open_stream(&self) -> Result<Box<dyn Stream>, NetError> {
        Ok(Box::new(BeforeOpenCodeStream(self.0.open_stream()?)))
    }
    fn accept_stream(&self, timeout: Duration) -> Result<Option<Box<dyn Stream>>, NetError> {
        Ok(self
            .0
            .accept_stream(timeout)?
            .map(|stream| Box::new(BeforeOpenCodeStream(stream)) as Box<dyn Stream>))
    }
    fn close(&self) {
        self.0.close();
    }
}

struct Keys(Vec<Arc<dyn Signer>>);

impl SignerSource for Keys {
    fn signers(&self) -> Vec<Arc<dyn Signer>> {
        self.0.clone()
    }
}

struct Host {
    registry: Arc<Registry>,
    dialer: Arc<LoopDialer>,
    allowed: PrivateKey,
}

fn host(registry: Arc<Registry>) -> Host {
    let allowed = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let gate = Arc::new(Gate::new(
        Arc::new(PauseSwitch::default()),
        Audit::with_sink(Lines::default()).unwrap(),
        "TEST",
    ));
    let guard = Arc::new(RemoteGate::with_gate(gate));
    guard.set_enabled(true);
    let keys = Arc::new(
        AuthorizedKeySet::parse(&format!("{}\n", allowed.public_key().to_openssh().unwrap()))
            .unwrap(),
    );
    let service = Arc::new(HostService::new(
        Arc::clone(&registry) as Arc<dyn HostTerminals>,
        Arc::new(move || Arc::clone(&keys)),
        guard as Arc<dyn RemoteGuard>,
    ));
    Host {
        registry,
        dialer: Arc::new(LoopDialer {
            service,
            dials: AtomicUsize::new(0),
            before_opencode: AtomicBool::new(false),
        }),
        allowed,
    }
}

/// A supervisor that lists every 100 ms, as a stand-in for the heartbeat.
fn start(host: &Host) -> Arc<HostHandle> {
    start_with(host, Duration::from_millis(100))
}

/// As [`start`], with the given heartbeat.
fn start_with(host: &Host, heartbeat: Duration) -> Arc<HostHandle> {
    let handle = HostHandle::new("build-box", HOST_ID, Arc::new(|| {}));
    let signer: Arc<dyn Signer> = Arc::new(KeySigner(host.allowed.clone()));
    spawn(
        Arc::clone(&handle),
        Arc::clone(&host.dialer) as Arc<dyn Dialer>,
        Arc::new(Keys(vec![signer])),
        SupervisorConfig {
            backoff: Backoff {
                initial: Duration::from_millis(20),
                max: Duration::from_millis(200),
            },
            heartbeat,
            initial_size: GridSize { cols: 60, rows: 12 },
        },
    )
    .unwrap();
    handle
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

/// The peer's session for a terminal it lists and has attached.
fn attached(handle: &HostHandle, id: u32) -> Option<Arc<connecto_net::client::RemoteSession>> {
    handle
        .terminals()
        .into_iter()
        .find(|t| t.info.id == id && t.session.is_linked())
        .map(|t| t.session)
}

fn screen(session: &dyn TerminalSession) -> String {
    let mut snap = TermSnapshot::default();
    for _ in 0..100 {
        if session.poll_snapshot(&mut snap) == SnapshotOutcome::Updated {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    (0..snap.rows)
        .map(|r| snap.row(r).iter().map(|c| c.ch).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Waits until the peer sees `text` on terminal `id`, then types `typed` and
/// waits until the host's current session of `id` has it.
fn view_and_type(host: &Host, handle: &HostHandle, id: u32, text: &str, typed: &str) {
    wait_until(&format!("terminal {id} attached with {text}"), || {
        attached(handle, id).is_some_and(|s| screen(&*s).contains(text))
    });
    let session = attached(handle, id).unwrap();
    session.send_input(typed.as_bytes().to_vec()).unwrap();
    wait_until(&format!("the host got {typed:?}"), || {
        host.registry
            .current(id)
            .is_some_and(|t| String::from_utf8_lossy(&t.typed()).contains(typed))
    });
}

#[test]
fn a_peer_restarts_a_terminal_it_views_and_types_into_the_new_one_without_reconnecting() {
    let host = host(Registry::new(&[(7, "p")], &[]));
    host.registry.current(7).unwrap().print(b"first screen\r\n");
    let handle = start(&host);
    wait_until("the first attach", || attached(&handle, 7).is_some());

    handle.manage(ManageOp::Restart { terminal: 7 }).unwrap();
    host.registry
        .current(7)
        .unwrap()
        .print(b"after the restart\r\n");
    view_and_type(&host, &handle, 7, "after the restart", "typed after\r");

    assert_eq!(
        host.dialer.dials.load(Ordering::SeqCst),
        1,
        "it reconnected"
    );
}

/// An OpenCode terminal goes only to a host that knows OpenCode. One from
/// before it would read the tool byte as Claude and start Claude: so it is
/// never asked for OpenCode's models or for an OpenCode terminal, and the
/// other agents work there as before.
#[test]
fn opencode_is_asked_only_of_a_host_that_knows_it() {
    use connecto_core::ToolKind;
    let add = |tool| ManageOp::Add {
        tool,
        name: None,
        project: Some("e".into()),
        model: None,
    };
    let opencode = connecto_net::hosting::tool_byte(ToolKind::OpenCode);

    let new = host(Registry::new(&[], &["e"]));
    let handle = start(&new);
    wait_until("the empty project is listed", || {
        handle.listed_projects().iter().any(|p| p.id == "e")
    });
    assert!(handle.knows(ToolKind::OpenCode));
    assert!(handle.manage(add(opencode)).unwrap().is_some());
    assert_eq!(new.registry.asked(), [add(opencode)]);

    let old = host(Registry::new(&[], &["e"]));
    old.dialer.before_opencode.store(true, Ordering::SeqCst);
    let handle = start(&old);
    wait_until("the empty project is listed", || {
        handle.listed_projects().iter().any(|p| p.id == "e")
    });
    assert!(!handle.knows(ToolKind::OpenCode));
    assert!(handle.knows(ToolKind::Claude));
    assert_eq!(
        handle.manage(add(opencode)).unwrap_err(),
        TOO_OLD_FOR_OPENCODE
    );
    assert_eq!(
        handle.models(ToolKind::OpenCode, "e").unwrap_err(),
        TOO_OLD_FOR_OPENCODE
    );
    assert!(old.registry.asked().is_empty(), "the old host was asked");
    // The other agents are added as before.
    assert!(handle.manage(add(1)).unwrap().is_some());
    assert_eq!(old.registry.asked(), [add(1)]);
}

#[test]
fn a_peer_adds_a_terminal_to_an_empty_project_and_types_into_it() {
    let host = host(Registry::new(&[(7, "p")], &["e"]));
    let handle = start(&host);
    wait_until("the empty project is listed", || {
        handle.listed_projects().iter().any(|p| p.id == "e")
    });

    let id = handle
        .manage(ManageOp::Add {
            tool: 1,
            name: None,
            project: Some("e".into()),
            model: None,
        })
        .unwrap()
        .expect("the new terminal's id");
    host.registry.current(id).unwrap().print(b"new in e\r\n");
    view_and_type(&host, &handle, id, "new in e", "typed in e\r");

    assert_eq!(
        host.dialer.dials.load(Ordering::SeqCst),
        1,
        "it reconnected"
    );
    assert!(
        handle
            .terminals()
            .iter()
            .any(|t| t.info.id == id && t.info.project_id == "e"),
        "the new terminal is not in its project"
    );
}

#[test]
fn a_terminal_added_or_restarted_on_the_host_while_the_peer_is_connected_is_seen_and_attachable() {
    let host = host(Registry::new(&[(7, "p")], &[]));
    let handle = start(&host);
    wait_until("the first attach", || attached(&handle, 7).is_some());

    // The host's own user restarts 7.
    host.registry.local_restart(7);
    host.registry
        .current(7)
        .unwrap()
        .print(b"restarted here\r\n");
    view_and_type(
        &host,
        &handle,
        7,
        "restarted here",
        "typed after local restart\r",
    );

    // And adds a terminal.
    let added = host.registry.local_add("p");
    host.registry
        .current(added)
        .unwrap()
        .print(b"added here\r\n");
    view_and_type(&host, &handle, added, "added here", "typed in local add\r");

    assert_eq!(
        host.dialer.dials.load(Ordering::SeqCst),
        1,
        "it reconnected"
    );
}

#[test]
fn a_peer_closes_a_terminal_and_its_id_is_not_usable_afterwards() {
    let host = host(Registry::new(&[(7, "p"), (8, "p")], &[]));
    let handle = start(&host);
    wait_until("both attached", || {
        attached(&handle, 7).is_some() && attached(&handle, 8).is_some()
    });

    handle.manage(ManageOp::Close { terminal: 7 }).unwrap();
    wait_until("7 gone from the peer's list", || {
        !handle.terminals().iter().any(|t| t.info.id == 7)
    });
    assert!(host.registry.current(7).is_none(), "the host kept 7");

    // Its id is refused now, whatever the peer asks.
    let err = handle
        .manage(ManageOp::Restart { terminal: 7 })
        .unwrap_err();
    assert!(err.contains("no such terminal"), "{err}");
    assert!(
        !host
            .registry
            .asked()
            .contains(&ManageOp::Restart { terminal: 7 })
    );
    // The other terminal is untouched.
    assert!(attached(&handle, 8).is_some());
    assert_eq!(
        host.dialer.dials.load(Ordering::SeqCst),
        1,
        "it reconnected"
    );
}

#[test]
fn a_terminal_restarted_on_the_host_reattaches_without_any_list() {
    // The heartbeat is far away: only the end of the restarted terminal's
    // stream can bring the peer back to it.
    let host = host(Registry::new(&[(7, "p")], &[]));
    let handle = start_with(&host, Duration::from_secs(60));
    wait_until("the first attach", || attached(&handle, 7).is_some());

    host.registry.local_restart(7);
    host.registry
        .current(7)
        .unwrap()
        .print(b"back after the host restart\r\n");
    wait_until("the restarted terminal attached again", || {
        attached(&handle, 7).is_some_and(|session| screen(&*session).contains("back after"))
    });
    assert_eq!(
        host.dialer.dials.load(Ordering::SeqCst),
        1,
        "it reconnected"
    );
}

#[test]
fn a_refused_reattachment_is_not_retried_until_the_next_list() {
    let host = host(Registry::new(&[(7, "p")], &[]));
    let handle = start_with(&host, Duration::from_secs(60));
    wait_until("the first attach", || attached(&handle, 7).is_some());
    // The host closes 7 on its own: the peer's stream ends, and the re-attach is
    // refused, because the list no longer has it.
    host.registry.local_close(7);
    thread::sleep(Duration::from_millis(600));
    let before = host.registry.lists.load(Ordering::SeqCst);
    thread::sleep(Duration::from_secs(1));
    let after = host.registry.lists.load(Ordering::SeqCst);
    assert_eq!(
        after, before,
        "a refused re-attach was sent again before a list"
    );
}
