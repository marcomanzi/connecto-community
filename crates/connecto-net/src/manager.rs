//! Everything remote that the app owns, behind [`RemoteControl`]: this
//! machine's own hosting, the paired machines and their connections, and the
//! files that hold them (`<data>/remote/`).
//!
//! Nothing here runs on the UI thread. A call that has to wait (starting the
//! endpoint, the Keychain, a pairing) hands the work to a thread and shows the
//! result in the view.
//!
//! A paired machine is one record, spread over three places that are written
//! together and removed together (`commit_machine`, `disconnect_machine`):
//! - `known_hosts`: its name, its endpoint id (the pin), how to reach it, the
//!   date it was paired;
//! - `authorized_keys`: the SSH key it dials this machine with, its comment
//!   carrying its endpoint id;
//! - the Keychain: this machine's own SSH key for dialing it (`remote.pair.<id>`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use connecto_core::{
    AuthorizedKeyRow, HostLink, HostRow, HostingStatus, MachineRow, PairingPhase, PendingHost,
    RemoteControl, RemoteView, TerminalsControl,
};
use connecto_secrets::STORE_NAME;
use serde_json::{Value, json};
use ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey, PublicKey};

use crate::auth::{
    AuthorizedKeySet, CombinedSigners, KeyLine, PeerKeySigners, checked_key_line, key_lines,
    without_key,
};
use crate::guard::{GuardDenied, Peer, RemoteGuard};
use crate::host::{HostService, HostTerminals, KeySource};
use crate::iroh_transport::{IrohConfig, IrohNode, PAIR_ALPN, PeerAddr, endpoint_id_for_key};
use crate::pairing::{
    Code, KdfParams, NAMING_WAIT, Offer, PairError, Role, derive_keys, handshake, role_for,
};
use crate::reconnect::{Dialer, HostHandle, LinkState, SignerSource, SupervisorConfig, spawn};
use crate::relay_access::{self, RelayAccess};
use crate::remote_terminals::RemoteTerminals;
use crate::secrets::{RELAY_TOKEN, SecretStore, device_key, pair_key_name};
use crate::staging::{
    self, LINK_PREFIX, Status, fingerprints_for, link_id, raw_lines_for, staged_comment,
};
use crate::ticket::{Ticket, fingerprint};
use crate::transport::{Connection, EndpointId, PathKind};
use crate::trust::{
    KnownHost, KnownHosts, MAX_HOST_NAME_CHARS, clean_name, hex, unhex, write_private,
};
use zeroize::Zeroizing;

/// What the manager is built from.
pub struct ManagerConfig {
    /// `<data>/remote`.
    pub dir: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
    /// The host's policy; the Gate-backed one is built by the app.
    pub guard: Arc<dyn RemoteGuard>,
    /// The terminals a peer may see.
    pub terminals: Arc<dyn HostTerminals>,
    /// The keys to dial with when a machine has no key of its own (the device
    /// key of the older flow, then ssh-agent). A machine paired now uses its own.
    pub signers: Arc<dyn SignerSource>,
    pub wake: Arc<dyn Fn() + Send + Sync>,
    pub supervisor: SupervisorConfig,
    /// A fixed local address for the endpoint. `None` for the OS to choose.
    pub bind: Option<SocketAddr>,
    /// Put loopback addresses in the address this machine shares. They are no
    /// use to another machine; only a test on one machine wants them.
    pub share_loopback: bool,
    /// What this machine calls itself to the machine it pairs with.
    pub device_name: String,
    /// The cost of turning the two codes into a key. `PRODUCTION` in the app.
    pub kdf: KdfParams,
    /// How long a pairing code works. `CODE_LIFETIME` in the app.
    pub pairing_lifetime: Duration,
    /// The relay used when the user has not chosen one (or none).
    pub default_relay: Option<String>,
}

/// How long each pairing endpoint may take to reach the relay before it is
/// replaced by a fresh one; the last is long enough for a slow but healthy relay.
const ONLINE_WAITS: [Duration; 3] = [
    Duration::from_secs(4),
    Duration::from_secs(4),
    Duration::from_secs(15),
];

/// The relay a new install uses, until the user sets another or none.
pub const DEFAULT_RELAY_URL: &str = "https://relay.connecto.studio";

/// What an authorized key's comment carries when pairing by the older flow added it.
const PAIRED_MARK: &str = " (paired ";
/// What the comment of a key from a mutual pairing starts with: the other
/// machine's endpoint id, then the date.
/// The id of a machine paired by the older flow, which knows only its key.
const LEGACY_PREFIX: &str = "legacy:";

/// Codes a pairing may be given before it stops (a typo or two, no more).
const MAX_PEER_CODES: usize = 5;
/// Pairings that may be started in ten minutes, and the gap between two.
const MAX_STARTS: usize = 8;
const START_GAP: Duration = Duration::from_secs(3);

const EXPIRED: &str = "The code expired before the other machine connected. Start pairing again.";
const POLL: Duration = Duration::from_millis(200);
const DIAL_TRY: Duration = Duration::from_secs(8);
/// How often the startup endpoint's relay is checked, for `last_error`.
const RELAY_WATCH: Duration = Duration::from_secs(2);
const DIAL_PAUSE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Settings {
    enabled: bool,
    relay_url: Option<String>,
}

impl Settings {
    fn to_text(&self) -> String {
        // Always written: an empty one means "no relay", chosen on purpose.
        let v = json!({
            "version": 1,
            "enabled": self.enabled,
            "relay_url": self.relay_url.clone().unwrap_or_default(),
        });
        let mut text = serde_json::to_string_pretty(&v).unwrap_or_default();
        text.push('\n');
        text
    }

    /// A file that does not name a relay gets `default_relay`; one that names
    /// an empty relay means none, on purpose.
    fn load(path: &Path, default_relay: Option<&str>) -> Result<Self, String> {
        let default = default_relay.map(str::to_owned);
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                Ok(Self {
                    enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                    relay_url: match v.get("relay_url").and_then(Value::as_str) {
                        None => default,
                        Some("") => None,
                        Some(url) => Some(url.to_owned()),
                    },
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                enabled: false,
                relay_url: default,
            }),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// What the store says about the token the relay named in the settings gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokenFlags {
    /// The user's own token.
    stored: bool,
    /// Connecto's built-in token.
    built_in: bool,
    /// The store could not be read, so there is no telling.
    unreadable: bool,
}

fn token_flags(store: &dyn SecretStore, relay_url: Option<&str>) -> TokenFlags {
    let (stored, built_in, unreadable) = match relay_access::for_relay(store, relay_url) {
        Ok(RelayAccess::Stored(_)) => (true, false, false),
        Ok(RelayAccess::BuiltIn(_)) => (false, true, false),
        Ok(RelayAccess::None) => (false, false, false),
        Err(_) => (false, false, true),
    };
    TokenFlags {
        stored,
        built_in,
        unreadable,
    }
}

/// Stores the relay problem that a watch of endpoint `generation` found, unless
/// that endpoint is no longer the current one or the manager is shutting down.
/// `problem` is `None` when the relay accepts the endpoint. Returns whether the
/// stored value changed.
fn store_relay_problem(
    slot: &mut Option<(u64, String)>,
    current_epoch: u64,
    shutting_down: bool,
    generation: u64,
    problem: Option<String>,
) -> bool {
    if shutting_down || current_epoch != generation {
        return false;
    }
    let next = problem.map(|text| (generation, text));
    if *slot == next {
        return false;
    }
    *slot = next;
    true
}

/// The error the view shows: the general one, and the relay's problem beside
/// it when both are set. Neither is dropped.
fn shown_error(general: Option<&str>, relay: Option<&str>) -> Option<String> {
    match (general, relay) {
        (Some(general), Some(relay)) => Some(format!("{}. {relay}", general.trim_end_matches('.'))),
        (Some(general), None) => Some(general.to_owned()),
        (None, Some(relay)) => Some(relay.to_owned()),
        (None, None) => None,
    }
}

/// An endpoint with the generation and relay it was created for. Both are
/// fixed when it is created, under the lock that retires generations, so a
/// later change of settings never relabels it.
#[derive(Clone)]
struct Hosted {
    node: Arc<IrohNode>,
    generation: u64,
    relay_url: Option<String>,
}

/// The endpoint, started when first needed and again when its config changes.
struct NodeSlot {
    node: Mutex<Option<Hosted>>,
}

struct Slot {
    handle: Arc<HostHandle>,
    running: bool,
}

/// A connection a paired machine has opened to this one.
struct Inbound {
    seq: u64,
    endpoint: EndpointId,
    /// The SSH key it authenticated with; empty until it has.
    fingerprint: String,
    conn: Arc<dyn Connection>,
}

/// One pairing, from the moment its code is shown until it is done.
struct PairSession {
    /// The same for every attempt of one pairing.
    id: u64,
    /// The attempt in progress; an older attempt's threads stop reporting.
    attempt_id: u64,
    my_code: Code,
    /// When the code stops working.
    deadline: Instant,
    /// Set to end the attempt in progress.
    cancel: Arc<AtomicBool>,
    /// Codes entered so far.
    submits: usize,
    /// The connection of the attempt in progress, closed by a cancel.
    conn: Option<Arc<dyn Connection>>,
    /// Hands the name to the attempt once the other machine is connected.
    name_tx: Option<mpsc::Sender<String>>,
    /// The other machine, once connected.
    peer_id: Option<EndpointId>,
    /// Set once the commit has been sent and confirmed. From then the pairing
    /// is saved on both sides, and a cancel is not taken back.
    committed: bool,
}

struct Inner {
    settings: Settings,
    known: KnownHosts,
    slots: Vec<Slot>,
    pending: Option<(PendingHost, Ticket)>,
    last_error: Option<String>,
    hosting: HostingStatus,
    authorized_text: String,
    /// The user stored a relay token.
    relay_token_set: bool,
    /// No token is stored, and Connecto's built-in one is used.
    relay_builtin_token: bool,
    /// The stored token could not be read at all. Not the same as no token.
    relay_token_unreadable: bool,
    /// The relay's problem as the watch of one endpoint found it, tagged with
    /// that endpoint's generation (`listen_epoch`). Kept apart from
    /// `last_error`, and dropped whenever the endpoint is replaced.
    relay_problem: Option<(u64, String)>,
    /// The endpoint's identity and addresses once it is up.
    my_id: Option<[u8; 32]>,
    my_addrs: Vec<SocketAddr>,
    peer: Option<String>,
    pairing: PairingPhase,
    session: Option<PairSession>,
    next_id: u64,
    /// When pairings were started, for the limit on starting them.
    pair_starts: Vec<Instant>,
}

pub struct RemoteManager {
    cfg: ManagerConfig,
    inner: Mutex<Inner>,
    node: NodeSlot,
    guard: Arc<dyn RemoteGuard>,
    host_service: Arc<HostService>,
    inbound: Arc<Mutex<Vec<Inbound>>>,
    next_seq: AtomicU64,
    /// Serializes changes to the machine records (files and Keychain).
    records: Mutex<()>,
    /// Bumped to end the accept loop of an older endpoint.
    listen_epoch: Mutex<u64>,
    shutting_down: AtomicBool,
    me: Mutex<std::sync::Weak<Self>>,
}

/// Passes every decision to the host's policy, and remembers which connections
/// are live and under which key, so that a machine's session can be closed.
struct TrackingGuard {
    inner: Arc<dyn RemoteGuard>,
    inbound: Arc<Mutex<Vec<Inbound>>>,
}

impl RemoteGuard for TrackingGuard {
    fn enabled(&self) -> bool {
        self.inner.enabled()
    }
    fn set_enabled(&self, on: bool) {
        self.inner.set_enabled(on);
    }
    fn connected(&self, peer: &Peer, outcome: Result<(), &str>) {
        if outcome.is_ok() {
            let mut live = self.inbound.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(entry) = live
                .iter_mut()
                .find(|e| e.endpoint == peer.endpoint && e.fingerprint.is_empty())
            {
                entry.fingerprint.clone_from(&peer.fingerprint);
            }
        }
        self.inner.connected(peer, outcome);
    }
    fn attach(&self, peer: &Peer, terminal: &str, attempt: u64) -> Result<(), GuardDenied> {
        self.inner.attach(peer, terminal, attempt)
    }
    fn input(
        &self,
        peer: &Peer,
        terminal: &str,
        attempt: u64,
        bytes: &[u8],
    ) -> Result<(), GuardDenied> {
        self.inner.input(peer, terminal, attempt, bytes)
    }
    fn resize(&self, peer: &Peer, terminal: &str, attempt: u64) -> Result<(), GuardDenied> {
        self.inner.resize(peer, terminal, attempt)
    }
    fn manage(
        &self,
        peer: &Peer,
        action: &'static str,
        terminal: &str,
        attempt: u64,
    ) -> Result<(), GuardDenied> {
        self.inner.manage(peer, action, terminal, attempt)
    }
    fn detach(&self, peer: &Peer, terminal: &str, attempt: u64) {
        self.inner.detach(peer, terminal, attempt);
    }
}

struct IrohDialer {
    manager: Arc<RemoteManager>,
    peer: PeerAddr,
}

impl Dialer for IrohDialer {
    fn dial(&self) -> Result<Arc<dyn Connection>, String> {
        let node = self.manager.node()?;
        node.connect(&self.peer)
            .map(Arc::from)
            .map_err(|e| e.to_string())
    }
}

fn peer_of(host: &KnownHost) -> PeerAddr {
    PeerAddr {
        id: host.id,
        relay: host.relay.clone(),
        direct: host.addrs.iter().filter_map(|a| a.parse().ok()).collect(),
    }
}

/// Why an attempt stopped: quietly (it was cancelled or replaced), or with a
/// reason for the user.
enum Stop {
    Quiet,
    Fail(String),
}

impl From<PairError> for Stop {
    fn from(err: PairError) -> Self {
        Self::Fail(err.to_string())
    }
}

/// What a machine id names: a paired machine, or a device of the older flow.
enum Target {
    Machine([u8; 32]),
    Legacy(String),
}

impl Target {
    fn parse(id: &str) -> Result<Self, String> {
        if let Some(fp) = id.strip_prefix(LEGACY_PREFIX) {
            return Ok(Self::Legacy(fp.to_owned()));
        }
        unhex(id.trim())
            .map(Self::Machine)
            .ok_or_else(|| "that is not a paired machine".to_owned())
    }
}

/// The date in a comment's `(paired YYYY-MM-DD)`.
fn paired_date(comment: &str) -> Option<String> {
    comment
        .split_once(PAIRED_MARK)?
        .1
        .strip_suffix(')')
        .map(str::to_owned)
}

/// Whether a key line belongs to a paired machine (and so is not a hand-added key).
fn is_machine_key(comment: &str) -> bool {
    comment.starts_with(LINK_PREFIX) || comment.contains(PAIRED_MARK)
}

/// The keys that may open a session: the file's, without the staged ones.
fn authorized_for_sessions(text: &str, staged: &[String]) -> AuthorizedKeySet {
    let usable = staged
        .iter()
        .fold(text.to_owned(), |t, fp| without_key(&t, fp));
    AuthorizedKeySet::parse(&usable).unwrap_or_default()
}

/// Whether another machine (or a device of the older flow) already has `name`.
fn name_taken(known: &KnownHosts, authorized: &str, name: &str, except: &EndpointId) -> bool {
    known.get(name).is_some_and(|h| &h.id != except)
        || key_lines(authorized).iter().any(|k| {
            link_id(&k.comment).is_none()
                && k.comment.contains(PAIRED_MARK)
                && legacy_name(&k.comment).eq_ignore_ascii_case(name)
        })
}

fn legacy_name(comment: &str) -> String {
    comment
        .split_once(PAIRED_MARK)
        .map(|(name, _)| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Paired device".to_owned())
}

fn path_link(path: PathKind) -> HostLink {
    match path {
        PathKind::Direct => HostLink::Direct,
        PathKind::Relayed => HostLink::Relayed,
        PathKind::Unknown => HostLink::Connected,
    }
}

/// Every paired machine, as the view shows it: the saved hosts with their
/// keys and live links, then keys that have no saved host.
fn machine_rows(
    known: &KnownHosts,
    slots: &[Slot],
    authorized: &[KeyLine],
    inbound: &[(EndpointId, String, PathKind)],
) -> Vec<MachineRow> {
    let inbound_link = |endpoint: Option<&EndpointId>, fp: &str| {
        inbound
            .iter()
            .find(|(e, f, _)| endpoint == Some(e) || (!fp.is_empty() && f == fp))
            .map_or(HostLink::Disconnected, |(_, _, path)| path_link(*path))
    };
    let mut rows = Vec::new();
    for host in known.list() {
        let id = hex(&host.id);
        let line = authorized
            .iter()
            .find(|k| link_id(&k.comment).as_deref() == Some(id.as_str()));
        let paired_at = if host.paired_at.is_empty() {
            line.and_then(|k| paired_date(&k.comment))
                .unwrap_or_default()
        } else {
            host.paired_at.clone()
        };
        rows.push(MachineRow {
            name: host.name.clone(),
            paired_at,
            inbound: inbound_link(Some(&host.id), line.map_or("", |k| k.fingerprint.as_str())),
            outbound: slots
                .iter()
                .find(|s| s.handle.id() == host.id)
                .map_or(HostLink::Disconnected, |s| {
                    RemoteManager::link_of(&s.handle)
                }),
            id,
        });
    }
    for key in authorized.iter().filter(|k| is_machine_key(&k.comment)) {
        if let Some(id) = link_id(&key.comment) {
            if known.list().iter().any(|h| hex(&h.id) == id) {
                continue;
            }
            rows.push(MachineRow {
                name: format!("Machine {}", id.get(..8).unwrap_or(&id)),
                paired_at: paired_date(&key.comment).unwrap_or_default(),
                inbound: inbound_link(None, &key.fingerprint),
                outbound: HostLink::Disconnected,
                id,
            });
        } else {
            rows.push(MachineRow {
                id: format!("{LEGACY_PREFIX}{}", key.fingerprint),
                name: legacy_name(&key.comment),
                paired_at: paired_date(&key.comment).unwrap_or_default(),
                inbound: inbound_link(None, &key.fingerprint),
                outbound: HostLink::Disconnected,
            });
        }
    }
    rows
}

impl RemoteManager {
    /// Load the saved state. Hosting starts only if it was turned on.
    #[must_use]
    pub fn start(cfg: ManagerConfig) -> Arc<Self> {
        let settings_path = cfg.dir.join("settings.json");
        let mut last_error = None;
        match staging::recover(&cfg.dir, &*cfg.secrets) {
            Ok(None) => {}
            Ok(Some(recovery)) => last_error = Some(recovery_notice(&recovery)),
            Err(e) => last_error = Some(format!("an unfinished pairing could not be cleared: {e}")),
        }
        let settings =
            Settings::load(&settings_path, cfg.default_relay.as_deref()).unwrap_or_else(|e| {
                last_error = Some(format!("remote settings could not be read: {e}"));
                Settings::default()
            });
        let known = KnownHosts::load(&cfg.dir.join("known_hosts")).unwrap_or_else(|e| {
            last_error = Some(format!("the paired machines could not be read: {e}"));
            KnownHosts::default()
        });
        let authorized_text =
            std::fs::read_to_string(cfg.dir.join("authorized_keys")).unwrap_or_default();
        let guard = Arc::clone(&cfg.guard);
        guard.set_enabled(settings.enabled);
        let keys_path = cfg.dir.join("authorized_keys");
        // A key of a staged or sent record is never let in (see staging.rs). If
        // the staged records cannot be read, no key is let in at all.
        let dir_for_keys = cfg.dir.clone();
        let keys: KeySource = Arc::new(move || {
            let text = std::fs::read_to_string(&keys_path).unwrap_or_default();
            Arc::new(match staging::unusable_fingerprints(&dir_for_keys) {
                Ok(fps) => authorized_for_sessions(&text, &fps),
                Err(_) => AuthorizedKeySet::default(),
            })
        });
        let inbound = Arc::new(Mutex::new(Vec::new()));
        let tracking: Arc<dyn RemoteGuard> = Arc::new(TrackingGuard {
            inner: Arc::clone(&guard),
            inbound: Arc::clone(&inbound),
        });
        let host_service = Arc::new(HostService::new(Arc::clone(&cfg.terminals), keys, tracking));
        let slots = known
            .list()
            .iter()
            .map(|h| Slot {
                handle: HostHandle::new(&h.name, h.id, Arc::clone(&cfg.wake)),
                running: false,
            })
            .collect();
        let flags = token_flags(&*cfg.secrets, settings.relay_url.as_deref());
        let enabled = settings.enabled;
        cfg.terminals.set_recording(enabled);
        let manager = Arc::new(Self {
            inner: Mutex::new(Inner {
                settings,
                known,
                slots,
                pending: None,
                last_error,
                hosting: HostingStatus::Off,
                authorized_text,
                relay_token_set: flags.stored,
                relay_builtin_token: flags.built_in,
                relay_token_unreadable: flags.unreadable,
                relay_problem: None,
                my_id: None,
                my_addrs: Vec::new(),
                peer: None,
                pairing: PairingPhase::Idle,
                session: None,
                next_id: 1,
                pair_starts: Vec::new(),
            }),
            node: NodeSlot {
                node: Mutex::new(None),
            },
            guard,
            host_service,
            inbound,
            next_seq: AtomicU64::new(1),
            records: Mutex::new(()),
            listen_epoch: Mutex::new(0),
            shutting_down: AtomicBool::new(false),
            me: Mutex::new(std::sync::Weak::new()),
            cfg,
        });
        *manager.me.lock().unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&manager);
        if enabled {
            manager.begin_hosting();
        }
        manager
    }

    /// Stop every connection, any pairing, and the endpoint.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.cancel_pairing();
        for slot in &self.lock().slots {
            slot.handle.stop();
        }
        for live in self.inbound_conns() {
            live.close();
        }
        self.guard.set_enabled(false);
        self.retire_endpoint();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn arc(&self) -> Option<Arc<Self>> {
        self.me
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade()
    }

    fn fail(&self, why: impl Into<String>) {
        self.lock().last_error = Some(why.into());
        (self.cfg.wake)();
    }

    fn inbound_conns(&self) -> Vec<Arc<dyn Connection>> {
        self.inbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|e| Arc::clone(&e.conn))
            .collect()
    }

    fn inbound_snapshot(&self) -> Vec<(EndpointId, String, PathKind)> {
        self.inbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|e| (e.endpoint, e.fingerprint.clone(), e.conn.path()))
            .collect()
    }

    fn save_settings(&self, settings: &Settings) -> Result<(), String> {
        write_private(
            &self.cfg.dir.join("settings.json"),
            settings.to_text().as_bytes(),
        )
        .map_err(|e| e.to_string())
    }

    /// The endpoint, started now if it is not up. Blocks while it binds.
    fn node(&self) -> Result<Arc<IrohNode>, String> {
        self.hosted().map(|hosted| hosted.node)
    }

    /// The endpoint with the generation and relay it was created for. The
    /// relay and the generation are read under the same lock, and a restart
    /// takes this slot only after retiring the generation, so the pair is
    /// never mixed.
    fn hosted(&self) -> Result<Hosted, String> {
        let mut slot = self
            .node
            .node
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(hosted) = slot.as_ref() {
            return Ok(hosted.clone());
        }
        let (relay_url, listen, generation) = {
            let inner = self.lock();
            (
                inner.settings.relay_url.clone(),
                inner.settings.enabled,
                self.generation(),
            )
        };
        let key = device_key(&*self.cfg.secrets)?;
        let access = self.relay_access_for(relay_url.as_deref())?;
        let node = Arc::new(
            IrohNode::start(&IrohConfig {
                device_key: key,
                relay_url: relay_url.clone(),
                relay_token: access.token().map(str::to_owned),
                bind: self.cfg.bind,
                listen,
                relay_only: false,
                alpn: crate::iroh_transport::ALPN,
            })
            .map_err(|e| e.to_string())?,
        );
        let mut inner = self.lock();
        inner.my_id = Some(node.id());
        inner.my_addrs = node.known_addrs();
        drop(inner);
        let hosted = Hosted {
            node,
            generation,
            relay_url,
        };
        *slot = Some(hosted.clone());
        Ok(hosted)
    }

    /// Retire the current endpoint: advance its generation, drop its relay
    /// problem, and empty the endpoint slot. A watch or accept loop still
    /// running for it can no longer publish. The lock order is the slot, then
    /// the inner state, then the generation, the same order `hosted()` uses. The
    /// slot is held across the bump, so an endpoint being created at the old
    /// generation cannot be stored after this one has been emptied.
    fn retire_endpoint(&self) {
        let mut slot = self
            .node
            .node
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        {
            let mut inner = self.lock();
            inner.relay_problem = None;
            *self
                .listen_epoch
                .lock()
                .unwrap_or_else(PoisonError::into_inner) += 1;
        }
        slot.take();
    }

    /// The generation of the endpoint now current.
    fn generation(&self) -> u64 {
        *self
            .listen_epoch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Drop the endpoint so the next use starts one with the current settings.
    fn restart_node(&self) {
        self.retire_endpoint();
    }

    /// Start the endpoint and the accept loop, off the calling thread.
    fn begin_hosting(&self) {
        let Some(me) = self.arc() else { return };
        let generation = self.generation();
        self.lock().hosting = HostingStatus::Starting;
        let spawned = thread::Builder::new()
            .name("remote-hosting".into())
            .spawn(move || match me.hosted() {
                Ok(hosted) => {
                    me.start_hosted(hosted);
                }
                Err(why) => me.hosting_failed(generation, why),
            });
        if let Err(err) = spawned {
            self.lock().hosting = HostingStatus::Error(err.to_string());
        }
    }

    /// Set the hosting status, but only while `generation` is still the current
    /// endpoint's. The check runs under the inner lock, which every retirement
    /// also takes, so a stale thread cannot overwrite a newer status.
    fn set_hosting_if_current(&self, generation: u64, status: HostingStatus) -> bool {
        let mut inner = self.lock();
        if !self.generation_is_current(generation) {
            return false;
        }
        inner.hosting = status;
        true
    }

    /// A hosting thread of endpoint `generation` failed. Reported only while
    /// that endpoint is still current: a retired one's failure says nothing
    /// about the settings now in force.
    fn hosting_failed(&self, generation: u64, why: String) {
        if self.set_hosting_if_current(generation, HostingStatus::Error(why)) {
            (self.cfg.wake)();
        }
    }

    /// Start the relay watch and the accept loop of an endpoint, unless that
    /// endpoint was already retired (hosting turned off, or the relay or the
    /// endpoint replaced) after it was obtained. Returns whether it started.
    fn start_hosted(self: &Arc<Self>, hosted: Hosted) -> bool {
        if !self.set_hosting_if_current(hosted.generation, HostingStatus::Listening) {
            return false;
        }
        (self.cfg.wake)();
        self.watch_relay(&hosted);
        self.accept_loop(&hosted);
        true
    }

    fn accept_loop(self: &Arc<Self>, hosted: &Hosted) {
        let generation = hosted.generation;
        loop {
            if !self.generation_is_current(generation) {
                return;
            }
            match hosted.node.accept(Duration::from_millis(200)) {
                Ok(Some(conn)) => {
                    let conn: Arc<dyn Connection> = Arc::from(conn);
                    let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
                    self.inbound
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(Inbound {
                            seq,
                            endpoint: conn.remote_id(),
                            fingerprint: String::new(),
                            conn: Arc::clone(&conn),
                        });
                    let me = Arc::clone(self);
                    let _ = thread::Builder::new()
                        .name("remote-peer".into())
                        .spawn(move || {
                            me.lock().peer = Some("a peer is connecting".to_owned());
                            (me.cfg.wake)();
                            me.host_service.serve(&*conn);
                            me.inbound
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .retain(|e| e.seq != seq);
                            me.lock().peer = None;
                            (me.cfg.wake)();
                        });
                }
                Ok(None) => {}
                Err(_) => return,
            }
        }
    }

    /// Keeps the relay problem of the endpoint `node` up to date while the
    /// endpoint lives: set while its relay cannot be reached, cleared once the
    /// relay accepts it. Every result is published only if this endpoint is
    /// still the current one and the manager is not shutting down, checked
    /// under the lock that stores it. Holds the endpoint only weakly, so a
    /// replaced endpoint closes at once.
    fn watch_relay(self: &Arc<Self>, hosted: &Hosted) {
        // The relay the endpoint was created with, not the current setting.
        let Some(relay_url) = hosted.relay_url.clone() else {
            return;
        };
        let generation = hosted.generation;
        let me = Arc::clone(self);
        let weak = Arc::downgrade(&hosted.node);
        let _ = thread::Builder::new()
            .name("remote-relay-watch".into())
            .spawn(move || {
                loop {
                    if !me.generation_is_current(generation) {
                        return;
                    }
                    let Some(node) = weak.upgrade() else {
                        return;
                    };
                    let online = node.wait_online(RELAY_WATCH);
                    let problem = if online {
                        None
                    } else {
                        let access = me
                            .relay_access_for(Some(&relay_url))
                            .unwrap_or(RelayAccess::None);
                        Some(relay_access::unreachable(
                            node.relay_problem(),
                            &access,
                            &relay_url,
                        ))
                    };
                    drop(node);
                    if me.publish_relay_problem(generation, problem) {
                        (me.cfg.wake)();
                    }
                    if online {
                        thread::sleep(RELAY_WATCH);
                    }
                }
            });
    }

    /// Whether the endpoint of `generation` is still the current one, and the
    /// manager is not shutting down.
    fn generation_is_current(&self, generation: u64) -> bool {
        !self.shutting_down.load(Ordering::SeqCst) && self.generation() == generation
    }

    /// Store a watch's result for the endpoint of `generation`, if it is still
    /// current. Returns whether the stored problem changed.
    fn publish_relay_problem(&self, generation: u64, problem: Option<String>) -> bool {
        let mut inner = self.lock();
        let epoch = self.generation();
        store_relay_problem(
            &mut inner.relay_problem,
            epoch,
            self.shutting_down.load(Ordering::SeqCst),
            generation,
            problem,
        )
    }

    /// The access an endpoint for `relay_url` gets. A default relay with no
    /// token fails here, at once, rather than after the wait for it to come online.
    fn relay_access_for(&self, relay_url: Option<&str>) -> Result<RelayAccess, String> {
        let access = relay_access::for_relay(&*self.cfg.secrets, relay_url)
            .map_err(|why| relay_access::unreadable(&why))?;
        if relay_access::lacks_required_token(relay_url, &access) {
            return Err(relay_access::NO_TOKEN.to_owned());
        }
        Ok(access)
    }

    fn start_slot(&self, index: usize) {
        let Some(me) = self.arc() else { return };
        let (handle, host) = {
            let mut inner = self.lock();
            let Some(slot) = inner.slots.get_mut(index) else {
                return;
            };
            if slot.running {
                slot.handle.kick();
                return;
            }
            slot.running = true;
            let handle = Arc::clone(&slot.handle);
            let name = handle.name();
            let Some(host) = inner.known.get(&name).cloned() else {
                return;
            };
            (handle, host)
        };
        // This machine's own key for the machine first, then the older keys.
        let signers: Arc<dyn SignerSource> = Arc::new(CombinedSigners(vec![
            Arc::new(PeerKeySigners {
                store: Arc::clone(&self.cfg.secrets),
                id: host.id,
            }),
            Arc::clone(&self.cfg.signers),
        ]));
        let dialer = Arc::new(IrohDialer {
            manager: me,
            peer: peer_of(&host),
        });
        if let Err(err) = spawn(handle, dialer, signers, self.cfg.supervisor) {
            self.fail(format!("could not start the connection: {err}"));
        }
    }

    fn index_of(inner: &Inner, name: &str) -> Option<usize> {
        inner
            .slots
            .iter()
            .position(|s| s.handle.name().eq_ignore_ascii_case(name.trim()))
    }

    fn link_of(handle: &HostHandle) -> HostLink {
        match handle.state() {
            LinkState::Idle => HostLink::Disconnected,
            LinkState::Connecting => HostLink::Connecting,
            LinkState::Connected(PathKind::Direct) => HostLink::Direct,
            LinkState::Connected(PathKind::Relayed) => HostLink::Relayed,
            LinkState::Connected(PathKind::Unknown) => HostLink::Connected,
            LinkState::Waiting { retry_at, .. } => HostLink::Reconnecting {
                retry_in_secs: retry_at
                    .saturating_duration_since(std::time::Instant::now())
                    .as_secs()
                    + 1,
            },
            LinkState::Refused(why) => HostLink::Refused(why),
        }
    }

    /// The addresses this machine can be reached at, as another machine should
    /// be told them.
    fn reachable(&self, node: &IrohNode) -> Vec<SocketAddr> {
        node.known_addrs()
            .into_iter()
            .filter(|a| {
                !a.ip().is_unspecified() && (self.cfg.share_loopback || !a.ip().is_loopback())
            })
            .collect()
    }
}

/// The notice for a damaged pairing record (see [`staging::recover`]).
fn recovery_notice(recovery: &staging::Recovery) -> String {
    let who = if recovery.machines.is_empty() {
        "Pair again any machine you were pairing.".to_owned()
    } else {
        format!(
            "Pair these machines again: {}.",
            recovery.machines.join(", ")
        )
    };
    let kept = if recovery.kept.is_empty() {
        String::new()
    } else {
        let paths: Vec<String> = recovery
            .kept
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        format!(
            " The damaged files were kept for checking: {}.",
            paths.join(", ")
        )
    };
    format!(
        "A pending pairing could not be recovered, so every key that was not part of a finished pairing was removed. {who}{kept}"
    )
}

/// `YYYY-MM-DD`, in UTC, from the system clock.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}
impl RemoteManager {
    /// Stage a paired machine's record, durably (see [`crate::staging`]): the
    /// record's previous state is written first, then the staged key, the
    /// authorized key line and the host entry. A failure rolls back what was
    /// written. The record is not kept until the pairing is.
    fn stage_record(&self, peer: &Offer, pem: &str, name: &str) -> Result<(), String> {
        let _records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let name = clean_name(name).map_err(|e| e.to_string())?;
        let mut peer_key = PublicKey::from_openssh(&checked_key_line(&peer.key_line)?)
            .map_err(|e| e.to_string())?;
        let fp = peer_key.fingerprint(HashAlg::Sha256).to_string();
        let id = hex(&peer.id);
        let dir = self.cfg.dir.clone();
        let old_auth = std::fs::read_to_string(dir.join("authorized_keys")).unwrap_or_default();
        let (prev_known, relay, mut known) = {
            let inner = self.lock();
            (
                inner.known.by_id(&peer.id).cloned(),
                inner.settings.relay_url.clone(),
                inner.known.clone(),
            )
        };
        if name_taken(&known, &old_auth, &name, &peer.id) {
            return Err(format!("there is already a machine called \"{name}\""));
        }
        if let Some(previous) = &prev_known {
            known.remove(&previous.name).map_err(|e| e.to_string())?;
        }
        known
            .add(KnownHost {
                name: name.clone(),
                id: peer.id,
                relay: peer.relay.clone().or(relay),
                addrs: peer.addrs.iter().map(ToString::to_string).collect(),
                paired_at: today(),
            })
            .map_err(|e| e.to_string())?;

        // The machine's previous key lines stay valid until the commit, which
        // replaces them (see staging): a re-pairing never shuts the machine out.
        let mut auth = without_key(&old_auth, &fp);
        if !auth.is_empty() && !auth.ends_with('\n') {
            auth.push('\n');
        }
        peer_key.set_comment(staged_comment(&peer.id, &today()));
        auth.push_str(&peer_key.to_openssh().map_err(|e| e.to_string())?);
        auth.push('\n');

        let entry = staging::Entry {
            id: peer.id,
            fingerprint: fp,
            status: Status::Staged,
            prev_lines: raw_lines_for(&old_auth, &id),
            prev_known: prev_known.as_ref().map(staging::host_line).transpose()?,
        };
        let written = (|| -> Result<(), String> {
            staging::add(&dir, entry)?;
            self.cfg
                .secrets
                .set(&staging::staged_secret_name(&peer.id), pem.as_bytes())
                .map_err(|e| format!("the key could not be saved: {e}"))?;
            staging::write_auth(&dir, &auth)?;
            staging::write_known(&dir, &known)
        })();
        if let Err(e) = written {
            let _ = staging::rollback(&dir, &*self.cfg.secrets, &peer.id);
            return Err(e);
        }
        {
            let mut inner = self.lock();
            inner.known = known;
            inner.authorized_text = auth;
            if let Some(pos) = inner.slots.iter().position(|s| s.handle.id() == peer.id) {
                let old = inner.slots.remove(pos);
                old.handle.close_link();
            }
            inner.slots.push(Slot {
                handle: HostHandle::new(&name, peer.id, Arc::clone(&self.cfg.wake)),
                running: false,
            });
        }
        (self.cfg.wake)();
        Ok(())
    }

    /// Roll a staged record back: its key line, host entry and Keychain key
    /// are put back as they were, and this side's view follows the files.
    fn unstage(&self, id: &[u8; 32]) {
        let result = {
            let _records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
            staging::rollback(&self.cfg.dir, &*self.cfg.secrets, id)
        };
        if let Err(e) = result {
            self.fail(format!("a pairing could not be fully undone: {e}"));
        }
        self.reload_records();
    }

    /// The commit is confirmed: the staged key becomes the machine's key.
    fn keep_record(&self, id: &[u8; 32]) -> Result<(), String> {
        let _records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        staging::set_status(&self.cfg.dir, id, Status::Kept)?;
        staging::keep(&self.cfg.dir, &*self.cfg.secrets, id)
    }

    /// Re-read the record files into this side's view: the hosts, the keys
    /// and one outbound slot per host (running ones keep running).
    fn reload_records(&self) {
        let known = KnownHosts::load(&self.cfg.dir.join("known_hosts"))
            .unwrap_or_else(|_| self.lock().known.clone());
        let auth =
            std::fs::read_to_string(self.cfg.dir.join("authorized_keys")).unwrap_or_default();
        {
            let mut inner = self.lock();
            let mut slots = Vec::new();
            for host in known.list() {
                match inner.slots.iter().position(|s| s.handle.id() == host.id) {
                    Some(pos) => slots.push(inner.slots.remove(pos)),
                    None => slots.push(Slot {
                        handle: HostHandle::new(&host.name, host.id, Arc::clone(&self.cfg.wake)),
                        running: false,
                    }),
                }
            }
            for gone in inner.slots.drain(..) {
                gone.handle.close_link();
            }
            inner.slots = slots;
            inner.known = known;
            inner.authorized_text = auth;
        }
        (self.cfg.wake)();
    }

    /// Remove a machine's record everywhere, and close its live sessions.
    fn remove_machine(&self, target: &Target) -> Result<(), String> {
        let _records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let auth_path = self.cfg.dir.join("authorized_keys");
        let known_path = self.cfg.dir.join("known_hosts");
        let old_auth = std::fs::read_to_string(&auth_path).unwrap_or_default();
        let (fps, known_entry) = match target {
            Target::Machine(id) => (
                fingerprints_for(&old_auth, &hex(id)),
                self.lock().known.by_id(id).cloned(),
            ),
            Target::Legacy(fp) => (vec![fp.clone()], None),
        };
        if fps.is_empty() && known_entry.is_none() {
            return Err("no paired machine has that id".to_owned());
        }
        // Its key goes first, so that no new connection can get in.
        let mut auth = old_auth.clone();
        for fp in &fps {
            auth = without_key(&auth, fp);
        }
        if auth != old_auth {
            write_private(&auth_path, auth.as_bytes()).map_err(|e| e.to_string())?;
        }
        // Then its live sessions, which the host would otherwise keep until they end.
        for entry in self
            .inbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|e| {
                let by_id = matches!(target, Target::Machine(id) if *id == e.endpoint);
                by_id || (!e.fingerprint.is_empty() && fps.contains(&e.fingerprint))
            })
        {
            entry.conn.close();
        }
        if let Some(host) = &known_entry {
            let mut known = self.lock().known.clone();
            known.remove(&host.name).map_err(|e| e.to_string())?;
            known
                .save(&known_path)
                .map_err(|e| format!("the machine list could not be saved: {e}"))?;
            let mut inner = self.lock();
            inner.known = known;
            if let Some(pos) = inner.slots.iter().position(|s| s.handle.id() == host.id) {
                let old = inner.slots.remove(pos);
                old.handle.close_link();
            }
        }
        if let Target::Machine(id) = target {
            self.cfg
                .secrets
                .clear(&pair_key_name(id))
                .map_err(|e| format!("{STORE_NAME} could not be changed: {e}"))?;
        }
        self.lock().authorized_text = auth;
        (self.cfg.wake)();
        Ok(())
    }

    fn is_current(inner: &Inner, attempt_id: u64) -> bool {
        inner
            .session
            .as_ref()
            .is_some_and(|s| s.attempt_id == attempt_id)
    }

    /// Report a phase of the attempt `attempt_id`, if it is still the one in charge.
    fn set_phase(&self, attempt_id: u64, phase: PairingPhase) {
        let mut inner = self.lock();
        if Self::is_current(&inner, attempt_id) {
            inner.pairing = phase;
            drop(inner);
            (self.cfg.wake)();
        }
    }

    /// Keep the connection so that a cancel can close it.
    fn hold(&self, attempt_id: u64, conn: &Arc<dyn Connection>) {
        let mut inner = self.lock();
        if let Some(session) = inner
            .session
            .as_mut()
            .filter(|s| s.attempt_id == attempt_id)
        {
            session.conn = Some(Arc::clone(conn));
        }
    }

    /// The pairing ends with a reason. Nothing it did is kept.
    fn fail_attempt(&self, attempt_id: u64, message: String) {
        let (conn, cancel) = {
            let mut inner = self.lock();
            if !Self::is_current(&inner, attempt_id) {
                return;
            }
            let Some(session) = inner.session.take() else {
                return;
            };
            inner.pairing = PairingPhase::Failed { message };
            (session.conn, session.cancel)
        };
        cancel.store(true, Ordering::SeqCst);
        if let Some(conn) = conn {
            conn.close();
        }
        (self.cfg.wake)();
    }

    /// The code ran out before anyone connected.
    fn expire(&self, id: u64) {
        let (conn, cancel) = {
            let mut inner = self.lock();
            let early = matches!(
                inner.pairing,
                PairingPhase::ShowingCode { .. }
                    | PairingPhase::WaitingForPeer { .. }
                    | PairingPhase::Preparing
            );
            let due = inner
                .session
                .as_ref()
                .is_some_and(|s| s.id == id && Instant::now() >= s.deadline);
            if !early || !due {
                return;
            }
            let Some(session) = inner.session.take() else {
                return;
            };
            inner.pairing = PairingPhase::Failed {
                message: EXPIRED.to_owned(),
            };
            (session.conn, session.cancel)
        };
        cancel.store(true, Ordering::SeqCst);
        if let Some(conn) = conn {
            conn.close();
        }
        (self.cfg.wake)();
    }

    /// A machine's name for the other: its own name, or the one it goes by,
    /// made unique.
    fn suggest_name(&self, offer: &Offer) -> String {
        let inner = self.lock();
        if let Some(host) = inner.known.by_id(&offer.id) {
            return host.name.clone();
        }
        let auth = inner.authorized_text.clone();
        let mut base: String = offer
            .device
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .chars()
            .take(MAX_HOST_NAME_CHARS)
            .collect();
        if clean_name(&base).is_err() {
            base = "Machine".to_owned();
        }
        let mut candidate = base.clone();
        let mut n = 2;
        while name_taken(&inner.known, &auth, &candidate, &offer.id) {
            candidate = format!("{base} {n}");
            n += 1;
        }
        candidate
    }

    /// The listening endpoint for the rendezvous, up and reachable.
    fn listen_node(&self, secret: &[u8; 32], cancel: &AtomicBool) -> Result<IrohNode, Stop> {
        let relay_url = self.lock().settings.relay_url.clone();
        let access = self
            .relay_access_for(relay_url.as_deref())
            .map_err(Stop::Fail)?;
        let token = access.token().map(str::to_owned);
        let has_relay = relay_url.is_some();
        // A connection to the relay can stall on its first try (iroh gives it 10 s
        // before it retries), so short waits come first: a fresh endpoint with the
        // same identity usually connects at once, and the last wait is generous.
        let mut problem = None;
        for wait in ONLINE_WAITS {
            if cancel.load(Ordering::SeqCst) {
                return Err(Stop::Quiet);
            }
            let node = IrohNode::start(&IrohConfig {
                device_key: *secret,
                relay_url: relay_url.clone(),
                relay_token: token.clone(),
                bind: self.cfg.bind,
                listen: true,
                relay_only: false,
                alpn: PAIR_ALPN,
            })
            .map_err(|e| Stop::Fail(format!("Could not start pairing: {e}")))?;
            if !has_relay || node.wait_online(wait) {
                return Ok(node);
            }
            problem = node.relay_problem();
        }
        Err(Stop::Fail(relay_access::unreachable(
            problem,
            &access,
            relay_url.as_deref().unwrap_or_default(),
        )))
    }

    /// One attempt at a pairing: reach the other machine through the rendezvous
    /// both codes give, run the exchange, take a name, and save.
    fn attempt(
        &self,
        attempt_id: u64,
        cancel: &AtomicBool,
        my: &Code,
        theirs: &Code,
        deadline: Instant,
        held: &mut Option<Arc<dyn Connection>>,
    ) -> Result<(), Stop> {
        let role = role_for(my, theirs)?;
        let keys = derive_keys(my, theirs, self.cfg.kdf)?;
        let rendezvous = endpoint_id_for_key(&keys.rendezvous_secret);
        let me = self.node().map_err(Stop::Fail)?;
        let my_id = me.id();
        let waiting = PairingPhase::WaitingForPeer {
            my_code: my.display(),
            expires_in_secs: 0,
        };

        // The listening endpoint stays up until the attempt ends: closing it
        // would close the connection the handshake runs on.
        let mut listener: Option<IrohNode> = None;
        let conn: Arc<dyn Connection> = match role {
            Role::Listen => {
                let node = self.listen_node(&keys.rendezvous_secret, cancel)?;
                self.set_phase(attempt_id, waiting);
                let conn = loop {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(Stop::Quiet);
                    }
                    if Instant::now() >= deadline {
                        return Err(Stop::Fail(EXPIRED.to_owned()));
                    }
                    match node.accept(POLL) {
                        Ok(Some(conn)) => break Arc::from(conn),
                        Ok(None) => {}
                        Err(err) => return Err(Stop::Fail(err.to_string())),
                    }
                };
                listener = Some(node);
                conn
            }
            Role::Dial => {
                let relay = self.lock().settings.relay_url.clone();
                self.set_phase(attempt_id, waiting);
                let target = PeerAddr {
                    id: rendezvous,
                    relay,
                    direct: Vec::new(),
                };
                loop {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(Stop::Quiet);
                    }
                    if Instant::now() >= deadline {
                        return Err(Stop::Fail(EXPIRED.to_owned()));
                    }
                    match me.connect_alpn_within(&target, PAIR_ALPN, DIAL_TRY) {
                        Ok(conn) => break Arc::from(conn),
                        Err(_) => thread::sleep(DIAL_PAUSE),
                    }
                }
            }
        };
        *held = Some(Arc::clone(&conn));
        if cancel.load(Ordering::SeqCst) {
            return Err(Stop::Quiet);
        }
        self.hold(attempt_id, &conn);
        self.set_phase(attempt_id, PairingPhase::Connecting);
        // Kept until the end of the attempt (see above).
        let _listener = listener;

        // This machine's own key for the other one, made now and kept only
        // if the pairing is saved.
        let key = PrivateKey::random(&mut ssh_key::rand_core::OsRng, Algorithm::Ed25519)
            .map_err(|e| Stop::Fail(e.to_string()))?;
        let pem = key
            .to_openssh(LineEnding::LF)
            .map_err(|e| Stop::Fail(e.to_string()))?;
        let offer = Offer {
            device: self.cfg.device_name.clone(),
            id: my_id,
            relay: self.lock().settings.relay_url.clone(),
            addrs: self.reachable(&me),
            key_line: key
                .public_key()
                .to_openssh()
                .map_err(|e| Stop::Fail(e.to_string()))?,
        };
        // This machine's endpoint key proves which machine it is.
        let identity = Zeroizing::new(device_key(&*self.cfg.secrets).map_err(Stop::Fail)?);
        let est = handshake(&*conn, role, &keys, &offer, &rendezvous, &identity)?;
        checked_key_line(&est.peer.key_line).map_err(Stop::Fail)?;

        // The name is the user's to choose. The connection waits for it.
        let suggested = self.suggest_name(&est.peer);
        let (name_tx, name_rx) = mpsc::channel::<String>();
        {
            let mut inner = self.lock();
            if let Some(session) = inner
                .session
                .as_mut()
                .filter(|s| s.attempt_id == attempt_id)
            {
                session.name_tx = Some(name_tx);
                session.peer_id = Some(est.peer.id);
                inner.pairing = PairingPhase::NameNeeded {
                    suggested_name: suggested,
                };
            } else {
                return Err(Stop::Quiet);
            }
        }
        (self.cfg.wake)();
        let naming_deadline = Instant::now() + NAMING_WAIT;
        let name = loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(Stop::Quiet);
            }
            if Instant::now() >= naming_deadline {
                return Err(Stop::Fail(
                    "No name was chosen in time. Start pairing again.".to_owned(),
                ));
            }
            match name_rx.recv_timeout(POLL) {
                Ok(name) => break name,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(Stop::Quiet),
            }
        };

        // The guard decides before anything is said to the other machine, and
        // the same check runs there.
        let peer_fp = PublicKey::from_openssh(&est.peer.key_line)
            .map(|k| k.fingerprint(HashAlg::Sha256).to_string())
            .unwrap_or_default();
        let device: String = est
            .peer
            .device
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        let peer = Peer {
            fingerprint: peer_fp,
            endpoint: est.peer.id,
        };
        if let Err(denied) = self.guard.manage(&peer, "pair", &device, 0) {
            let why = match denied {
                GuardDenied::Paused => "Pause is on",
                GuardDenied::Refused => "this machine does not allow pairing now",
            };
            let never = AtomicBool::new(false);
            let _ = est.exchange_intent(&never, Err(why.to_owned()));
            return Err(Stop::Fail(format!(
                "{why}, so this pairing was refused. Turn Pause off, then pair again."
            )));
        }
        // Staged first, durably: a crash from here on is finished or rolled back
        // at the next start (see staging.rs).
        let id = est.peer.id;
        self.stage_record(&est.peer, &pem, &name)
            .map_err(Stop::Fail)?;
        self.set_phase(attempt_id, PairingPhase::Confirming);
        match est.exchange_intent(cancel, Ok(())) {
            Ok(()) => {}
            Err(PairError::Cancelled) => {
                self.unstage(&id);
                return Err(Stop::Quiet);
            }
            Err(err) => {
                self.unstage(&id);
                return Err(err.into());
            }
        }
        // The commit point. Marked, and recorded as sent, before the final word
        // leaves: from here a cancel is not taken back.
        if !self.mark_committed(attempt_id, cancel) {
            est.decline("cancelled");
            self.unstage(&id);
            return Err(Stop::Quiet);
        }
        let sent = {
            let _records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
            staging::set_status(&self.cfg.dir, &id, Status::Sent)
        };
        if let Err(e) = sent {
            est.decline("could not be saved");
            self.unstage(&id);
            return Err(Stop::Fail(e));
        }
        self.set_phase(attempt_id, PairingPhase::Keeping);
        if let Err(err) = est.finish() {
            self.unstage(&id);
            return Err(err.into());
        }
        if let Err(e) = self.keep_record(&id) {
            // Past the commit point the record is never undone here: the other
            // side may have kept it. The entry is finished at the next start.
            self.reload_records();
            return Err(Stop::Fail(format!(
                "{e}. The pairing is finished when Connecto restarts."
            )));
        }
        self.succeed(attempt_id, hex(&id), &name);
        Ok(())
    }

    /// The commit point: from here the pairing is kept, and a cancel is not
    /// taken back. False when the attempt was cancelled first.
    fn mark_committed(&self, attempt_id: u64, cancel: &AtomicBool) -> bool {
        let mut inner = self.lock();
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        match inner
            .session
            .as_mut()
            .filter(|s| s.attempt_id == attempt_id)
        {
            Some(session) => {
                session.committed = true;
                true
            }
            None => false,
        }
    }

    fn succeed(&self, attempt_id: u64, machine_id: String, name: &str) {
        let index = {
            let mut inner = self.lock();
            if Self::is_current(&inner, attempt_id) {
                inner.session = None;
                inner.pairing = PairingPhase::Done { machine_id };
            }
            Self::index_of(&inner, name)
        };
        if let Some(index) = index {
            self.start_slot(index);
        }
        (self.cfg.wake)();
    }

    /// The thread for one attempt. It reports its end and closes its connection.
    fn run_attempt(
        self: Arc<Self>,
        attempt_id: u64,
        cancel: Arc<AtomicBool>,
        my: Code,
        theirs: Code,
        deadline: Instant,
    ) {
        let mut held = None;
        let result = self.attempt(attempt_id, &cancel, &my, &theirs, deadline, &mut held);
        if let Some(conn) = held {
            conn.close();
        }
        match result {
            Ok(()) | Err(Stop::Quiet) => {}
            Err(Stop::Fail(message)) => {
                // A cancel or a newer attempt has the say, not this one.
                if !cancel.load(Ordering::SeqCst) {
                    self.fail_attempt(attempt_id, message);
                }
            }
        }
    }

    /// Start an attempt on its own thread.
    fn spawn_attempt(
        self: &Arc<Self>,
        attempt_id: u64,
        cancel: Arc<AtomicBool>,
        my: Code,
        theirs: Code,
        deadline: Instant,
    ) -> Result<(), String> {
        let me = Arc::clone(self);
        thread::Builder::new()
            .name("remote-pairing".into())
            .spawn(move || me.run_attempt(attempt_id, cancel, my, theirs, deadline))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

impl RemoteControl for RemoteManager {
    fn view(&self) -> Arc<RemoteView> {
        let inbound = self.inbound_snapshot();
        let inner = self.lock();
        let hosts = inner
            .slots
            .iter()
            .map(|slot| HostRow {
                name: slot.handle.name(),
                fingerprint: fingerprint(&slot.handle.id()),
                link: Self::link_of(&slot.handle),
                terminals: slot.handle.terminals().len(),
            })
            .collect();
        // The addresses the endpoint knows now: ones learned from the relay's
        // address discovery appear after it started.
        let live_addrs = self
            .node
            .node
            .try_lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|hosted| hosted.node.known_addrs()))
            .unwrap_or_else(|| inner.my_addrs.clone());
        let my_address = inner.my_id.map(|id| {
            Ticket {
                id,
                relay: inner.settings.relay_url.clone(),
                addrs: live_addrs
                    .iter()
                    .copied()
                    .filter(|a| {
                        !a.ip().is_unspecified()
                            && (self.cfg.share_loopback || !a.ip().is_loopback())
                    })
                    .collect(),
            }
            .to_text()
        });
        let keys = key_lines(&inner.authorized_text);
        let machines = machine_rows(&inner.known, &inner.slots, &keys, &inbound);
        let pairing = match (&inner.pairing, inner.session.as_ref()) {
            (PairingPhase::ShowingCode { my_code, .. }, Some(s)) => PairingPhase::ShowingCode {
                my_code: my_code.clone(),
                expires_in_secs: s
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs(),
            },
            (PairingPhase::WaitingForPeer { my_code, .. }, Some(s)) => {
                PairingPhase::WaitingForPeer {
                    my_code: my_code.clone(),
                    expires_in_secs: s
                        .deadline
                        .saturating_duration_since(Instant::now())
                        .as_secs(),
                }
            }
            (phase, _) => phase.clone(),
        };
        Arc::new(RemoteView {
            hosting_enabled: inner.settings.enabled,
            hosting: inner.hosting.clone(),
            peer: inner.peer.clone(),
            machines,
            my_fingerprint: inner.my_id.map(|id| fingerprint(&id)).unwrap_or_default(),
            my_address: my_address.unwrap_or_default(),
            relay_url: inner.settings.relay_url.clone(),
            relay_token_set: inner.relay_token_set,
            relay_builtin_token: inner.relay_builtin_token,
            relay_token_unreadable: inner.relay_token_unreadable,
            authorized: keys
                .iter()
                .filter(|k| !is_machine_key(&k.comment))
                .map(|k| AuthorizedKeyRow {
                    fingerprint: k.fingerprint.clone(),
                    algorithm: k.algorithm.clone(),
                    comment: k.comment.clone(),
                    paired: false,
                })
                .collect(),
            authorized_path: self.cfg.dir.join("authorized_keys").display().to_string(),
            authorized_ignored: AuthorizedKeySet::parse(&inner.authorized_text)
                .map_or(0, |s| s.skipped_with_options()),
            hosts,
            pending: inner.pending.as_ref().map(|(p, _)| p.clone()),
            pairing,
            last_error: shown_error(
                inner.last_error.as_deref(),
                inner
                    .relay_problem
                    .as_ref()
                    .filter(|(generation, _)| *generation == self.generation())
                    .map(|(_, text)| text.as_str()),
            ),
        })
    }

    fn set_hosting(&self, enabled: bool) {
        let settings = {
            let mut inner = self.lock();
            if inner.settings.enabled == enabled {
                return;
            }
            inner.settings.enabled = enabled;
            inner.last_error = None;
            if !enabled {
                inner.hosting = HostingStatus::Off;
            }
            inner.settings.clone()
        };
        if let Err(why) = self.save_settings(&settings) {
            self.fail(format!("the setting could not be saved: {why}"));
        }
        self.guard.set_enabled(enabled);
        self.cfg.terminals.set_recording(enabled);
        // The endpoint listens only while hosting is on.
        self.restart_node();
        if enabled {
            self.begin_hosting();
        }
        (self.cfg.wake)();
    }

    fn set_relay(&self, url: Option<String>, token: Option<String>) -> Result<(), String> {
        let url = url.map(|u| u.trim().to_owned()).filter(|u| !u.is_empty());
        if let Some(url) = &url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return Err("the relay address must start with https://".to_owned());
        }
        if let Some(token) = token {
            if token.is_empty() {
                self.cfg.secrets.clear(RELAY_TOKEN)?;
            } else {
                self.cfg.secrets.set(RELAY_TOKEN, token.as_bytes())?;
            }
        }
        let (settings, enabled) = {
            let mut inner = self.lock();
            inner.settings.relay_url = url;
            let flags = token_flags(&*self.cfg.secrets, inner.settings.relay_url.as_deref());
            inner.relay_token_set = flags.stored;
            inner.relay_builtin_token = flags.built_in;
            inner.relay_token_unreadable = flags.unreadable;
            (inner.settings.clone(), inner.settings.enabled)
        };
        self.save_settings(&settings)?;
        self.restart_node();
        if enabled {
            self.begin_hosting();
        }
        (self.cfg.wake)();
        Ok(())
    }

    fn add_authorized_key(&self, line: &str) -> Result<(), String> {
        let _records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let line = checked_key_line(line)?;
        let path = self.cfg.dir.join("authorized_keys");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&line);
        text.push('\n');
        write_private(&path, text.as_bytes()).map_err(|e| e.to_string())?;
        self.lock().authorized_text = text;
        (self.cfg.wake)();
        Ok(())
    }

    fn remove_authorized_key(&self, fingerprint: &str) -> Result<(), String> {
        let _records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let path = self.cfg.dir.join("authorized_keys");
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        if key_lines(&text)
            .iter()
            .any(|k| k.fingerprint == fingerprint && is_machine_key(&k.comment))
        {
            return Err(
                "that key belongs to a paired machine; disconnect that machine instead".to_owned(),
            );
        }
        let next = without_key(&text, fingerprint);
        write_private(&path, next.as_bytes()).map_err(|e| e.to_string())?;
        self.lock().authorized_text = next;
        (self.cfg.wake)();
        Ok(())
    }

    fn add_host(&self, name: &str, address: &str) -> Result<(), String> {
        let name = clean_name(name).map_err(|e| e.to_string())?;
        let ticket = Ticket::parse(address)?;
        let mut inner = self.lock();
        if inner.known.get(&name).is_some() {
            return Err(format!("there is already a host called \"{name}\""));
        }
        if let Some(other) = inner.known.by_id(&ticket.id) {
            return Err(format!(
                "that machine is already saved as \"{}\"",
                other.name
            ));
        }
        inner.last_error = None;
        inner.pending = Some((
            PendingHost {
                name,
                fingerprint: fingerprint(&ticket.id),
                address: ticket.to_text(),
            },
            ticket,
        ));
        drop(inner);
        (self.cfg.wake)();
        Ok(())
    }

    fn start_mutual_pairing(&self) -> Result<(), String> {
        let Some(me) = self.arc() else {
            return Err("remote sessions are not running".to_owned());
        };
        let (id, deadline) = {
            let mut inner = self.lock();
            if inner.session.is_some() {
                return Err("a pairing is already under way".to_owned());
            }
            // Pairings are limited: one every few seconds, a handful in ten
            // minutes, so codes cannot be ground through by starting again.
            let now = Instant::now();
            inner
                .pair_starts
                .retain(|t| now.duration_since(*t) < Duration::from_secs(600));
            if inner
                .pair_starts
                .last()
                .is_some_and(|t| now.duration_since(*t) < START_GAP)
            {
                return Err("wait a few seconds before starting another pairing".to_owned());
            }
            if inner.pair_starts.len() >= MAX_STARTS {
                return Err("too many pairings in the last ten minutes; wait a while".to_owned());
            }
            inner.pair_starts.push(now);
            let my_code = Code::generate().map_err(|e| e.to_string())?;
            let deadline = now + self.cfg.pairing_lifetime;
            let id = inner.next_id;
            inner.next_id += 1;
            inner.pairing = PairingPhase::ShowingCode {
                my_code: my_code.display(),
                expires_in_secs: self.cfg.pairing_lifetime.as_secs(),
            };
            inner.session = Some(PairSession {
                id,
                attempt_id: id,
                my_code,
                deadline,
                cancel: Arc::new(AtomicBool::new(false)),
                submits: 0,
                conn: None,
                name_tx: None,
                peer_id: None,
                committed: false,
            });
            (id, deadline)
        };
        // Pairing ends with the other machine connecting to this one, so hosting
        // is turned on, and stays on.
        self.set_hosting(true);
        // A code that runs out before anyone enters the other one has no
        // attempt to end it, so this watches for that.
        let watch = Arc::clone(&me);
        let spawned = thread::Builder::new()
            .name("remote-pairing-expiry".into())
            .spawn(move || {
                while Instant::now() < deadline {
                    thread::sleep(POLL);
                    if !watch.lock().session.as_ref().is_some_and(|s| s.id == id) {
                        return;
                    }
                }
                watch.expire(id);
            });
        if let Err(err) = spawned {
            self.cancel_pairing();
            return Err(err.to_string());
        }
        (self.cfg.wake)();
        Ok(())
    }

    fn confirm_host(&self) {
        let _records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let added = {
            let mut inner = self.lock();
            let Some((pending, ticket)) = inner.pending.take() else {
                return;
            };
            let host = KnownHost {
                name: pending.name.clone(),
                id: ticket.id,
                relay: ticket.relay,
                addrs: ticket.addrs.iter().map(ToString::to_string).collect(),
                paired_at: String::new(),
            };
            let mut known = inner.known.clone();
            match known.add(host) {
                Ok(()) => match known.save(&self.cfg.dir.join("known_hosts")) {
                    Ok(()) => {
                        inner.known = known;
                        inner.slots.push(Slot {
                            handle: HostHandle::new(
                                &pending.name,
                                ticket.id,
                                Arc::clone(&self.cfg.wake),
                            ),
                            running: false,
                        });
                        Some(pending.name)
                    }
                    Err(e) => {
                        inner.last_error = Some(format!("the host list could not be saved: {e}"));
                        None
                    }
                },
                Err(e) => {
                    inner.last_error = Some(e.to_string());
                    None
                }
            }
        };
        if let Some(name) = added {
            self.connect(&name);
        }
        (self.cfg.wake)();
    }

    fn cancel_host(&self) {
        self.lock().pending = None;
        (self.cfg.wake)();
    }

    fn cancel_pairing(&self) {
        let (conn, cancel) = {
            let mut inner = self.lock();
            // Once the commit is confirmed the machine is being saved: a cancel
            // now would show Idle over a saved record, so it is not taken.
            if inner.session.as_ref().is_some_and(|s| s.committed) {
                return;
            }
            let session = inner.session.take();
            inner.pairing = PairingPhase::Idle;
            match session {
                Some(s) => (s.conn, Some(s.cancel)),
                None => (None, None),
            }
        };
        if let Some(cancel) = cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        if let Some(conn) = conn {
            conn.close();
        }
        (self.cfg.wake)();
    }

    fn submit_peer_code(&self, code: &str) -> Result<(), String> {
        let theirs = Code::parse(code).map_err(|e| e.to_string())?;
        let (attempt_id, my, deadline, cancel, old) = {
            let mut inner = self.lock();
            let fresh = inner.next_id;
            inner.next_id += 1;
            let phase_ok = matches!(
                inner.pairing,
                PairingPhase::ShowingCode { .. }
                    | PairingPhase::WaitingForPeer { .. }
                    | PairingPhase::Preparing
            );
            let Some(session) = inner.session.as_mut() else {
                return Err("start pairing on this machine first".to_owned());
            };
            if !phase_ok {
                return Err(
                    "the other machine's code is already in; wait for it to connect".to_owned(),
                );
            }
            if theirs == session.my_code {
                return Err(PairError::SameCode.to_string());
            }
            if session.submits >= MAX_PEER_CODES {
                return Err(
                    "too many codes were entered; cancel and start pairing again".to_owned(),
                );
            }
            session.submits += 1;
            // The attempt in progress, if any, is superseded.
            session.cancel.store(true, Ordering::SeqCst);
            session.attempt_id = fresh;
            session.cancel = Arc::new(AtomicBool::new(false));
            let old = session.conn.take();
            session.name_tx = None;
            session.peer_id = None;
            let attempt_id = session.attempt_id;
            let my = session.my_code.clone();
            let deadline = session.deadline;
            let cancel = Arc::clone(&session.cancel);
            inner.pairing = PairingPhase::Preparing;
            (attempt_id, my, deadline, cancel, old)
        };
        if let Some(old) = old {
            old.close();
        }
        let me = self
            .arc()
            .ok_or_else(|| "remote sessions are not running".to_owned())?;
        me.spawn_attempt(attempt_id, cancel, my, theirs, deadline)?;
        (self.cfg.wake)();
        Ok(())
    }

    fn finish_pairing(&self, name: &str) -> Result<(), String> {
        let name = clean_name(name).map_err(|e| e.to_string())?;
        let mut inner = self.lock();
        if !matches!(inner.pairing, PairingPhase::NameNeeded { .. }) {
            return Err("the other machine is not waiting to be named yet".to_owned());
        }
        let peer = inner
            .session
            .as_ref()
            .and_then(|s| s.peer_id)
            .ok_or_else(|| "the other machine is not waiting to be named yet".to_owned())?;
        let auth = inner.authorized_text.clone();
        if name_taken(&inner.known, &auth, &name, &peer) {
            return Err(format!("there is already a machine called \"{name}\""));
        }
        let tx = inner
            .session
            .as_mut()
            .and_then(|s| s.name_tx.take())
            .ok_or_else(|| "the pairing is no longer running".to_owned())?;
        tx.send(name)
            .map_err(|_| "the pairing is no longer running".to_owned())?;
        drop(inner);
        (self.cfg.wake)();
        Ok(())
    }

    fn disconnect(&self, name: &str) {
        let mut inner = self.lock();
        let Some(index) = Self::index_of(&inner, name) else {
            return;
        };
        let old = &inner.slots[index];
        old.handle.close_link();
        let fresh = HostHandle::new(
            &old.handle.name(),
            old.handle.id(),
            Arc::clone(&self.cfg.wake),
        );
        inner.slots[index] = Slot {
            handle: fresh,
            running: false,
        };
        drop(inner);
        (self.cfg.wake)();
    }

    fn disconnect_machine(&self, id: &str) -> Result<(), String> {
        let target = Target::parse(id)?;
        self.remove_machine(&target)
    }

    fn connect(&self, name: &str) {
        let index = Self::index_of(&self.lock(), name);
        if let Some(index) = index {
            self.start_slot(index);
        }
    }

    fn rename_host(&self, from: &str, to: &str) -> Result<(), String> {
        let _records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let mut inner = self.lock();
        let mut known = inner.known.clone();
        known.rename(from, to).map_err(|e| e.to_string())?;
        let new = clean_name(to).map_err(|e| e.to_string())?;
        known
            .save(&self.cfg.dir.join("known_hosts"))
            .map_err(|e| format!("the machine list could not be saved: {e}"))?;
        inner.known = known;
        if let Some(index) = Self::index_of(&inner, from) {
            inner.slots[index].handle.set_name(&new);
        }
        drop(inner);
        (self.cfg.wake)();
        Ok(())
    }

    /// Forget a saved host. It is removed as a whole: its key and its Keychain
    /// key too, as `disconnect_machine` does.
    fn forget_host(&self, name: &str) -> Result<(), String> {
        let id = self
            .lock()
            .known
            .get(name)
            .map(|h| h.id)
            .ok_or_else(|| format!("no host called \"{name}\""))?;
        self.remove_machine(&Target::Machine(id))
    }

    fn terminals(&self, name: &str) -> Option<Arc<dyn TerminalsControl>> {
        let inner = self.lock();
        let index = Self::index_of(&inner, name)?;
        Some(Arc::new(RemoteTerminals(Arc::clone(
            &inner.slots[index].handle,
        ))))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::ONLINE_WAITS;
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_stalled_relay_connection_is_replaced_quickly_but_a_slow_one_still_gets_time() {
        // iroh holds a stalled relay dial for 10 s; the early waits must be shorter.
        assert!(ONLINE_WAITS[0] < Duration::from_secs(10));
        assert!(ONLINE_WAITS[1] < Duration::from_secs(10));
        assert!(ONLINE_WAITS[2] >= Duration::from_secs(10));
    }

    #[test]
    fn a_staged_key_may_not_open_a_session_until_its_pairing_is_kept() {
        let key = |comment: &str| {
            let k = ssh_key::PrivateKey::random(
                &mut ssh_key::rand_core::OsRng,
                ssh_key::Algorithm::Ed25519,
            )
            .unwrap();
            let mut public = k.public_key().clone();
            public.set_comment(comment);
            (
                public.to_openssh().unwrap(),
                public.fingerprint(ssh_key::HashAlg::Sha256).to_string(),
            )
        };
        let (kept_line, kept_fp) = key("kept");
        let (staged_line, staged_fp) = key("staged");
        let text = format!("{kept_line}\n{staged_line}\n");
        let before = super::authorized_for_sessions(&text, &[]);
        assert!(before.contains(&ssh_key::PublicKey::from_openssh(&staged_line).unwrap()));
        let during = super::authorized_for_sessions(&text, std::slice::from_ref(&staged_fp));
        assert!(during.contains(&ssh_key::PublicKey::from_openssh(&kept_line).unwrap()));
        assert!(!during.contains(&ssh_key::PublicKey::from_openssh(&staged_line).unwrap()));
        assert_ne!(kept_fp, staged_fp);
    }

    #[test]
    fn link_ids_and_legacy_names_are_read_from_comments() {
        let id = "ab".repeat(32);
        let comment = format!("connecto-id={id} (paired 2026-11-12)");
        assert_eq!(super::link_id(&comment), Some(id.clone()));
        assert_eq!(super::paired_date(&comment).as_deref(), Some("2026-11-12"));
        assert_eq!(super::link_id("laptop (paired 2026-11-12)"), None);
        assert_eq!(super::legacy_name("laptop (paired 2026-11-12)"), "laptop");
        assert_eq!(super::legacy_name(" (paired 2026-11-12)"), "Paired device");
        assert!(super::is_machine_key(&comment));
        assert!(!super::is_machine_key("my laptop"));
    }

    const RELAY_DOWN: &str =
        "Could not reach the relay, so the other machine cannot find this one.";

    struct Quiet;

    impl connecto_audit::Sink for Quiet {
        fn write_line(&mut self, _line: &str) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct NoSigners;

    impl SignerSource for NoSigners {
        fn signers(&self) -> Vec<Arc<dyn crate::auth::Signer>> {
            Vec::new()
        }
    }

    /// A manager with hosting off, in its own folder.
    fn manager_for(tag: &str) -> Arc<RemoteManager> {
        let dir = std::env::temp_dir().join(format!("mg-net-hosted-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pause = Arc::new(connecto_core::PauseSwitch::default());
        let guard = connecto_session::RemoteGate::new(
            pause,
            connecto_audit::Audit::with_sink(Quiet).unwrap(),
        );
        RemoteManager::start(ManagerConfig {
            dir,
            secrets: Arc::new(crate::secrets::MemorySecrets::default()),
            guard: Arc::new(guard),
            terminals: Arc::new(crate::host::FixedTerminals(Mutex::new(Vec::new()))),
            signers: Arc::new(NoSigners),
            wake: Arc::new(|| {}),
            supervisor: SupervisorConfig {
                backoff: crate::reconnect::Backoff {
                    initial: Duration::from_millis(50),
                    max: Duration::from_millis(400),
                },
                heartbeat: Duration::from_millis(200),
                initial_size: connecto_core::GridSize { cols: 60, rows: 12 },
            },
            bind: Some("127.0.0.1:0".parse().unwrap()),
            share_loopback: true,
            device_name: "test".to_owned(),
            kdf: crate::pairing::KdfParams::FAST,
            pairing_lifetime: Duration::from_secs(30),
            default_relay: None,
        })
    }

    /// An endpoint is obtained for relay A. Before its watch is started, the
    /// relay is changed to another that also fails. That change retires the
    /// obtained endpoint, so starting it does nothing: no message of its relay
    /// appears under the new settings.
    #[test]
    fn an_endpoint_retired_before_its_watch_starts_starts_nothing() {
        use connecto_core::RemoteControl;
        let m = manager_for("retired");
        m.set_relay(Some("https://127.0.0.1:9".into()), None)
            .unwrap();
        // The hosting thread has this endpoint; nothing is registered yet.
        let hosted = m.hosted().unwrap();
        // The barrier: the settings change and its restart happen here.
        m.set_relay(Some("https://127.0.0.1:10".into()), None)
            .unwrap();
        assert!(!m.start_hosted(hosted), "a retired endpoint was started");
        thread::sleep(Duration::from_secs(3));
        assert!(m.view().last_error.is_none(), "{:?}", m.view().last_error);
        m.shutdown();
    }

    /// A hosting thread holds the endpoint slot while it creates an endpoint.
    /// A restart must wait for that thread before it advances the generation,
    /// so the endpoint it creates is retired, not stored under a new generation.
    #[test]
    fn a_restart_waits_for_the_endpoint_slot_before_it_retires_the_generation() {
        let m = manager_for("order");
        let before = m.generation();
        let held = m.node.node.lock().unwrap_or_else(PoisonError::into_inner);
        let restart = {
            let m = Arc::clone(&m);
            thread::spawn(move || m.restart_node())
        };
        thread::sleep(Duration::from_millis(300));
        assert_eq!(
            m.generation(),
            before,
            "the generation moved while the slot was held"
        );
        drop(held);
        restart.join().unwrap();
        assert_eq!(m.generation(), before + 1);
        m.shutdown();
    }

    /// The failure of a hosting thread whose endpoint was retired is not
    /// reported. The current endpoint's failure is.
    #[test]
    fn a_failure_of_a_retired_endpoint_is_not_reported() {
        let m = manager_for("failed");
        m.lock().hosting = HostingStatus::Starting;
        let old = m.generation();
        m.restart_node();
        m.hosting_failed(old, "bind failed".to_owned());
        assert_eq!(
            m.lock().hosting,
            HostingStatus::Starting,
            "a retired endpoint overwrote the status"
        );
        assert!(!m.set_hosting_if_current(old, HostingStatus::Listening));
        m.hosting_failed(m.generation(), "bind failed".to_owned());
        assert_eq!(
            m.lock().hosting,
            HostingStatus::Error("bind failed".to_owned())
        );
        m.shutdown();
    }

    #[test]
    fn a_watch_of_an_endpoint_that_was_replaced_publishes_nothing() {
        // Endpoint 1 was replaced by endpoint 2 while its check was outstanding.
        let mut slot = None;
        let changed =
            super::store_relay_problem(&mut slot, 2, false, 1, Some(RELAY_DOWN.to_owned()));
        assert!(!changed);
        assert_eq!(slot, None);
        // Nor can its "online" result clear the current endpoint's problem.
        let mut slot = Some((2, "B is down".to_owned()));
        assert!(!super::store_relay_problem(&mut slot, 2, false, 1, None));
        assert_eq!(slot, Some((2, "B is down".to_owned())));
    }

    #[test]
    fn a_watch_that_finishes_during_shutdown_or_after_hosting_is_off_publishes_nothing() {
        // Shutdown: the epoch is still the watch's, but nothing may be published.
        let mut slot = None;
        assert!(!super::store_relay_problem(
            &mut slot,
            1,
            true,
            1,
            Some(RELAY_DOWN.to_owned())
        ));
        assert_eq!(slot, None);
        // Hosting turned off retires the endpoint: its generation no longer
        // matches, so a late result is refused, and the old problem is gone.
        let mut slot = Some((1, RELAY_DOWN.to_owned()));
        let retired = 2;
        assert!(!super::store_relay_problem(
            &mut slot,
            retired,
            false,
            1,
            Some(RELAY_DOWN.to_owned())
        ));
        assert_eq!(
            slot,
            Some((1, RELAY_DOWN.to_owned())),
            "not cleared by a stale watch"
        );
    }

    #[test]
    fn the_current_watch_sets_and_clears_only_its_own_problem() {
        let mut slot = None;
        assert!(super::store_relay_problem(
            &mut slot,
            3,
            false,
            3,
            Some(RELAY_DOWN.to_owned())
        ));
        // The same text again changes nothing.
        assert!(!super::store_relay_problem(
            &mut slot,
            3,
            false,
            3,
            Some(RELAY_DOWN.to_owned())
        ));
        // A relay that accepts the endpoint clears it.
        assert!(super::store_relay_problem(&mut slot, 3, false, 3, None));
        assert_eq!(slot, None);
    }

    #[test]
    fn the_shown_error_keeps_the_general_error_and_the_relay_problem() {
        assert_eq!(super::shown_error(None, None), None);
        assert_eq!(
            super::shown_error(Some("the setting could not be saved: disk full"), None),
            Some("the setting could not be saved: disk full".to_owned())
        );
        assert_eq!(
            super::shown_error(None, Some(RELAY_DOWN)),
            Some(RELAY_DOWN.to_owned())
        );
        assert_eq!(
            super::shown_error(
                Some("the paired machines could not be read: bad"),
                Some(RELAY_DOWN)
            ),
            Some(format!(
                "the paired machines could not be read: bad. {RELAY_DOWN}"
            ))
        );
    }

    #[test]
    fn a_token_the_store_refuses_is_unreadable_and_never_no_token() {
        use crate::secrets::SecretStore;
        struct Denied;
        impl SecretStore for Denied {
            fn get(&self, _name: &str) -> Result<Option<Vec<u8>>, String> {
                Err("denied".to_owned())
            }
            fn set(&self, _name: &str, _value: &[u8]) -> Result<(), String> {
                Err("denied".to_owned())
            }
            fn clear(&self, _name: &str) -> Result<(), String> {
                Err("denied".to_owned())
            }
        }
        let flags = super::token_flags(&Denied, Some("https://relay.example:8443"));
        assert!(flags.unreadable);
        assert!(!flags.stored && !flags.built_in);
        let store = crate::secrets::MemorySecrets::default();
        let flags = super::token_flags(&store, Some("https://relay.example:8443"));
        assert!(!flags.unreadable);
        assert_eq!(
            flags,
            super::TokenFlags {
                stored: false,
                built_in: false,
                unreadable: false
            }
        );
    }
}
