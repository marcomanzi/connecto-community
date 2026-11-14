//! One saved host, kept connected: dial, authenticate, follow the host's
//! terminals, and when the link drops, dial again with growing delays and pick
//! each terminal up where it left off.
//!
//! A [`HostHandle`] is what the rest of the app holds. The supervisor thread
//! behind it owns the connection. The terminals' [`RemoteSession`]s outlive
//! connections: their screens stay while the link is down, and each resumes at
//! the last byte it applied (the host repaints only if it no longer holds that
//! point).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use connecto_core::GridSize;

use crate::auth::Signer;
use crate::client::{ClientError, RemoteClient, RemoteSession};
use crate::transport::{Connection, EndpointId, PathKind};
use crate::wire::{DenyReason, ManageOp, TerminalAddr, TerminalInfo};

/// How the supervisor reaches the host. The iroh implementation dials the
/// pinned endpoint id; tests hand out loopback pairs.
pub trait Dialer: Send + Sync {
    /// # Errors
    /// With the reason, when no path to the host works now.
    fn dial(&self) -> Result<Arc<dyn Connection>, String>;
}

/// The SSH keys to try, in order, asked fresh for every connect so a key added
/// to the agent later is found.
pub trait SignerSource: Send + Sync {
    fn signers(&self) -> Vec<Arc<dyn Signer>>;
}

/// Delays between attempts: `initial`, doubling up to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}

impl Backoff {
    /// One second up to thirty.
    pub const DEFAULT: Self = Self {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(30),
    };

    #[must_use]
    pub fn next(self, current: Duration) -> Duration {
        (current * 2).min(self.max)
    }
}

/// How the supervisor runs.
#[derive(Debug, Clone, Copy)]
pub struct SupervisorConfig {
    pub backoff: Backoff,
    /// How often a connected host is asked for its terminals again.
    pub heartbeat: Duration,
    /// The grid a terminal's local screen starts with, until the host says.
    pub initial_size: GridSize,
}

impl SupervisorConfig {
    pub const DEFAULT: Self = Self {
        backoff: Backoff::DEFAULT,
        heartbeat: Duration::from_secs(3),
        initial_size: GridSize { cols: 80, rows: 24 },
    };
}

/// Where the connection to a host stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Idle,
    Connecting,
    Connected(PathKind),
    /// The last attempt failed; the next is at `retry_at`.
    Waiting {
        retry_at: Instant,
        error: String,
    },
    /// Refused for a reason waiting will not fix. It stays until the user asks
    /// to connect again.
    Refused(String),
}

/// A host's terminal as the supervisor last saw it.
#[derive(Clone)]
pub struct RemoteTerminal {
    pub info: TerminalInfo,
    pub session: Arc<RemoteSession>,
}

/// What the app holds for one host.
pub struct HostHandle {
    name: Mutex<String>,
    id: EndpointId,
    state: ArcSwap<LinkState>,
    terminals: Mutex<Vec<RemoteTerminal>>,
    stop: AtomicBool,
    kick: AtomicBool,
    wake: Arc<dyn Fn() + Send + Sync>,
    /// The connection while there is one, for management requests.
    client: Mutex<Option<Arc<RemoteClient>>>,
    /// Why the last management request failed, until the next one.
    last_error: Mutex<Option<String>>,
}

impl HostHandle {
    #[must_use]
    pub fn new(name: &str, id: EndpointId, wake: Arc<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            name: Mutex::new(name.to_owned()),
            id,
            state: ArcSwap::from_pointee(LinkState::Idle),
            terminals: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            kick: AtomicBool::new(false),
            wake,
            client: Mutex::new(None),
            last_error: Mutex::new(None),
        })
    }

    /// Why the last add, close, restart or rename failed.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether a connection is up to send requests on.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Ask the host to change its terminals, waiting for its answer. The result
    /// is also kept for [`HostHandle::last_error`].
    ///
    /// # Errors
    /// With the reason: not connected, or the host's refusal.
    pub fn manage(&self, op: ManageOp) -> Result<Option<u32>, String> {
        let client = self
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let result = match client {
            Some(client) => client.manage(op).map_err(|e| e.to_string()),
            None => Err("not connected to that host".to_owned()),
        };
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = result.as_ref().err().cloned();
        (self.wake)();
        result
    }

    /// As [`HostHandle::manage`], off the calling thread, for the window.
    pub fn manage_async(self: &Arc<Self>, op: ManageOp) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        let handle = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("remote-manage".into())
            .spawn(move || {
                let _ = handle.manage(op);
            });
        if let Err(err) = spawned {
            *self
                .last_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(err.to_string());
        }
    }

    #[must_use]
    pub fn name(&self) -> String {
        self.name
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_name(&self, name: &str) {
        name.clone_into(&mut self.name.lock().unwrap_or_else(PoisonError::into_inner));
    }

    #[must_use]
    pub const fn id(&self) -> EndpointId {
        self.id
    }

    #[must_use]
    pub fn state(&self) -> LinkState {
        (**self.state.load()).clone()
    }

    #[must_use]
    pub fn terminals(&self) -> Vec<RemoteTerminal> {
        self.terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Ask the supervisor to stop. It ends within a moment.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Stop, and close the live connection now rather than at the next poll.
    pub fn close_link(&self) {
        self.stop();
        let client = self
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(client) = client {
            client.close();
        }
    }

    /// Ask for a connection now: ends a wait, and leaves a refusal.
    pub fn kick(&self) {
        self.kick.store(true, Ordering::SeqCst);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn set(&self, state: LinkState) {
        self.state.store(Arc::new(state));
        (self.wake)();
    }

    /// Wait up to `limit`; false when stopped, true on a kick or the time.
    fn pause(&self, limit: Duration) -> bool {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            if self.stopped() {
                return false;
            }
            if self.kick.swap(false, Ordering::SeqCst) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// Wait for a kick or a stop; false when stopped.
    fn wait_for_kick(&self) -> bool {
        loop {
            if self.stopped() {
                return false;
            }
            if self.kick.swap(false, Ordering::SeqCst) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

enum Failure {
    Retry(String),
    Fatal(String),
}

fn connect(
    handle: &HostHandle,
    dialer: &dyn Dialer,
    signers: &dyn SignerSource,
) -> Result<RemoteClient, Failure> {
    let signers = signers.signers();
    if signers.is_empty() {
        return Err(Failure::Fatal(
            "no SSH key is available: start ssh-agent and add a key".to_owned(),
        ));
    }
    let name = handle.name();
    for signer in &signers {
        let conn = dialer.dial().map_err(Failure::Retry)?;
        match RemoteClient::connect(conn, &name, &handle.id, &**signer) {
            Ok(client) => return Ok(client),
            Err(ClientError::Denied(DenyReason::NotAuthorized)) => {}
            Err(ClientError::HostMismatch) => {
                return Err(Failure::Fatal(
                    "a different machine answered than the one saved for this host".to_owned(),
                ));
            }
            Err(err) => return Err(Failure::Retry(err.to_string())),
        }
    }
    Err(Failure::Fatal(
        "the host does not allow any of this machine's SSH keys".to_owned(),
    ))
}

/// Make the sessions match the host's list, and attach any that lost their link.
fn sync(
    handle: &HostHandle,
    client: &RemoteClient,
    config: &SupervisorConfig,
) -> Result<(), String> {
    let listed = client.list().map_err(|e| e.to_string())?;
    let mut mine = handle
        .terminals
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let host = handle.name();
    let mut next = Vec::with_capacity(listed.len());
    for (_, info) in listed {
        let session = mine
            .iter()
            .find(|t| t.info.id == info.id)
            .map(|t| Arc::clone(&t.session))
            .unwrap_or_else(|| {
                RemoteSession::new(
                    TerminalAddr {
                        host: host.clone(),
                        terminal: info.id,
                    },
                    GridSize {
                        cols: info.cols.max(1),
                        rows: info.rows.max(1),
                    }
                    .clamp_to(config.initial_size),
                    Arc::clone(&handle.wake),
                )
            });
        if !session.is_linked() {
            session.attach(client, info.id).map_err(|e| e.to_string())?;
        }
        next.push(RemoteTerminal { info, session });
    }
    *mine = next;
    drop(mine);
    (handle.wake)();
    Ok(())
}

trait ClampSize {
    fn clamp_to(self, fallback: GridSize) -> GridSize;
}

impl ClampSize for GridSize {
    fn clamp_to(self, fallback: GridSize) -> GridSize {
        if self.cols == 0 || self.rows == 0 {
            fallback
        } else {
            self
        }
    }
}

/// Follow one connection until it fails or the handle stops.
fn serve_link(
    handle: &HostHandle,
    client: &RemoteClient,
    config: &SupervisorConfig,
) -> Result<(), String> {
    loop {
        sync(handle, client, config)?;
        let path = client.path();
        if matches!(handle.state(), LinkState::Connected(_)) {
            handle.state.store(Arc::new(LinkState::Connected(path)));
        }
        let until = Instant::now() + config.heartbeat;
        while Instant::now() < until {
            if handle.stopped() {
                return Ok(());
            }
            let terminals = handle.terminals();
            // Every terminal lost its stream: the link is gone, so redial now
            // instead of waiting for the next heartbeat to find out.
            if !terminals.is_empty() && terminals.iter().all(|t| t.session.is_closed()) {
                return Err("the link dropped".to_owned());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Start the supervisor for `handle`. It runs until the handle is stopped.
pub fn spawn(
    handle: Arc<HostHandle>,
    dialer: Arc<dyn Dialer>,
    signers: Arc<dyn SignerSource>,
    config: SupervisorConfig,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("remote-host-{}", handle.name()))
        .spawn(move || {
            let mut delay = config.backoff.initial;
            while !handle.stopped() {
                handle.set(LinkState::Connecting);
                let error = match connect(&handle, &*dialer, &*signers) {
                    Ok(client) => {
                        let client = Arc::new(client);
                        handle.set(LinkState::Connected(client.path()));
                        *handle.client.lock().unwrap_or_else(PoisonError::into_inner) =
                            Some(Arc::clone(&client));
                        delay = config.backoff.initial;
                        let result = serve_link(&handle, &client, &config);
                        *handle.client.lock().unwrap_or_else(PoisonError::into_inner) = None;
                        client.close();
                        if handle.stopped() {
                            break;
                        }
                        result.err().unwrap_or_default()
                    }
                    Err(Failure::Fatal(why)) => {
                        handle.set(LinkState::Refused(why));
                        if !handle.wait_for_kick() {
                            break;
                        }
                        delay = config.backoff.initial;
                        continue;
                    }
                    Err(Failure::Retry(why)) => why,
                };
                handle.set(LinkState::Waiting {
                    retry_at: Instant::now() + delay,
                    error,
                });
                if !handle.pause(delay) {
                    break;
                }
                delay = config.backoff.next(delay);
            }
            handle.set(LinkState::Idle);
        })
}
