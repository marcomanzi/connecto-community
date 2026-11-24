//! One saved host, kept connected: dial, authenticate, follow the host's
//! terminals, and when the link drops, dial again with growing delays and pick
//! each terminal up where it left off.
//!
//! A [`HostHandle`] is what the rest of the app holds. The supervisor thread
//! behind it owns the connection. The terminals' [`RemoteSession`]s outlive
//! connections: their screens stay while the link is down, and each resumes at
//! the last byte it applied (the host repaints only if it no longer holds that
//! point).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use connecto_core::{GridSize, TermActivity, ToolKind};

use crate::auth::Signer;
use crate::client::{ClientError, RemoteClient, RemoteSession};
use crate::transport::{Connection, EndpointId, PathKind};
use crate::wire::{
    DenyReason, FEATURE_BACKGROUND, FEATURE_TERMINAL_IDS, ManageOp, ProjectInfo, TerminalAddr,
    TerminalInfo, ToolModels,
};

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
    /// When the list that sent `info` arrived: the background ages count from it.
    pub listed_at: Instant,
}

/// What the app holds for one host.
/// Files being read from hosts now, across every host.
static READING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Counts one read of a file from a host while it lasts.
pub(crate) struct Reading;

impl Reading {
    pub(crate) fn start() -> Self {
        READING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        READING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// How many files this machine is reading from hosts now.
#[must_use]
pub fn files_being_read() -> usize {
    READING.load(std::sync::atomic::Ordering::SeqCst)
}

pub struct HostHandle {
    chat_only: AtomicBool,
    name: Mutex<String>,
    id: EndpointId,
    state: ArcSwap<LinkState>,
    terminals: Mutex<Vec<RemoteTerminal>>,
    /// Every terminal the host last listed, and when: also those a chat-only
    /// client attaches no session to, for a project's roster.
    listed: Mutex<(Vec<TerminalInfo>, Instant)>,
    stop: AtomicBool,
    kick: AtomicBool,
    /// Set when this handle's own request changed the host (a restart, an add,
    /// a close): the next thing the supervisor does is list again.
    refresh: AtomicBool,
    wake: Arc<dyn Fn() + Send + Sync>,
    /// The connection while there is one, for management requests.
    client: Mutex<Option<Arc<RemoteClient>>>,
    /// Why the last management request failed, until the next one.
    last_error: Mutex<Option<String>>,
    /// The projects of the host's latest accepted list, in its order.
    projects: Mutex<Vec<ProjectInfo>>,
    /// The projects with a terminal working (a turn, or background work after
    /// one) in that list, whichever terminals this handle follows.
    working: Mutex<HashSet<String>>,
    /// The models the host offers, by (tool byte, project), as last fetched.
    models: Mutex<HashMap<(u8, String), CachedModels>>,
    /// The host projects' history, settings and token views, as last fetched.
    panels: crate::project_cache::Panels,
    /// The files the host offers to download, as last asked; `None` until the
    /// host has answered, and for a host from before offers.
    offers: Mutex<Option<Vec<crate::wire::OfferInfo>>>,
    /// The host answered that it knows no offers: not asked again on this
    /// connection.
    no_offers: AtomicBool,
    /// The requests for a secret the host's terminals wait on, as last
    /// asked, and whether the host is one from before they were shared.
    secret_asks: Mutex<Vec<connecto_core::SecretAsk>>,
    no_secret_asks: AtomicBool,
    /// The host's project groups, as last asked; `None` until it answered,
    /// and for a host from before groups were shared (asked no more).
    groups: Mutex<Option<connecto_core::RemoteGroups>>,
    no_groups: AtomicBool,
}

/// A host's answer to a models probe, kept so the window never waits for it.
#[derive(Default)]
struct CachedModels {
    value: ToolModels,
    /// When the last fetch ended; `None` before the first.
    at: Option<Instant>,
    fetching: bool,
}

/// How long a host's models are used before they are fetched again.
const MODELS_FRESH: Duration = Duration::from_secs(30);

/// Said when a host's Connecto is from before OpenCode.
pub const TOO_OLD_FOR_OPENCODE: &str = "That machine's Connecto does not know OpenCode yet: update it there to start OpenCode terminals.";

/// Whether the host on `client` can start a `tool` terminal as that tool:
/// OpenCode only on a host that granted [`crate::wire::FEATURE_OPENCODE`].
fn client_knows(client: &RemoteClient, tool: ToolKind) -> bool {
    tool != ToolKind::OpenCode || client.has_feature(crate::wire::FEATURE_OPENCODE)
}

impl HostHandle {
    #[must_use]
    pub fn new(name: &str, id: EndpointId, wake: Arc<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            chat_only: AtomicBool::new(false),
            listed: Mutex::new((Vec::new(), Instant::now())),
            name: Mutex::new(name.to_owned()),
            id,
            state: ArcSwap::from_pointee(LinkState::Idle),
            terminals: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            kick: AtomicBool::new(false),
            refresh: AtomicBool::new(false),
            wake,
            client: Mutex::new(None),
            last_error: Mutex::new(None),
            projects: Mutex::new(Vec::new()),
            working: Mutex::new(HashSet::new()),
            models: Mutex::new(HashMap::new()),
            panels: crate::project_cache::Panels::default(),
            offers: Mutex::new(None),
            no_offers: AtomicBool::new(false),
            secret_asks: Mutex::new(Vec::new()),
            no_secret_asks: AtomicBool::new(false),
            groups: Mutex::new(None),
            no_groups: AtomicBool::new(false),
        })
    }

    /// The files the host offers to download (`connecto send-file` there),
    /// as last asked. Empty while not connected.
    #[must_use]
    pub fn offers(&self) -> Vec<crate::wire::OfferInfo> {
        if !self.is_connected() {
            return Vec::new();
        }
        self.offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_default()
    }

    /// The file of the host's offer `id`: its name and bytes. It waits for
    /// the whole file, so never call it from the UI thread.
    ///
    /// # Errors
    /// Not connected, the host's refusal, or a transfer cut short.
    pub fn download_offer(&self, id: u64) -> Result<(String, Vec<u8>), String> {
        let _reading = Reading::start();
        let client = self.client().ok_or("Not connected to that machine.")?;
        client
            .read_offer(id)
            .map_err(|e| crate::project_cache::wording(&e))
    }

    /// Ask the host what it offers now; the window is woken when that
    /// changed.
    fn poll_offers(&self, client: &RemoteClient) {
        if self.no_offers.load(Ordering::SeqCst) || self.chat_only.load(Ordering::Acquire) {
            return;
        }
        let got = match client.offers() {
            Ok(Some(offers)) => Some(offers),
            Ok(None) => {
                self.no_offers.store(true, Ordering::SeqCst);
                None
            }
            // Kept as it was; asked again at the next heartbeat.
            Err(_) => return,
        };
        let mut shown = self.offers.lock().unwrap_or_else(PoisonError::into_inner);
        if *shown != got {
            *shown = got;
            drop(shown);
            (self.wake)();
        }
    }

    /// The requests for a secret the host's terminals wait on (in the
    /// projects of the latest list), by host project id, as last asked.
    /// Empty while not connected.
    #[must_use]
    pub fn secret_asks(&self) -> Vec<connecto_core::SecretAsk> {
        if !self.is_connected() {
            return Vec::new();
        }
        self.secret_asks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The host's project groups, as last asked. `None` while not
    /// connected, before the host answered, and for a host from before
    /// groups were shared.
    #[must_use]
    pub fn groups(&self) -> Option<connecto_core::RemoteGroups> {
        if !self.is_connected() {
            return None;
        }
        self.groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Ask the host for its groups now; the window is woken when they
    /// changed.
    pub(crate) fn poll_groups(&self, client: &RemoteClient) {
        if self.no_groups.load(Ordering::SeqCst) || self.chat_only.load(Ordering::Acquire) {
            return;
        }
        let got = match client.groups() {
            Ok(Some(groups)) => Some(groups),
            Ok(None) => {
                self.no_groups.store(true, Ordering::SeqCst);
                None
            }
            // Kept as they were; asked again at the next heartbeat.
            Err(_) => return,
        };
        let mut shown = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
        if *shown != got {
            *shown = got;
            drop(shown);
            (self.wake)();
        }
    }

    /// Ask the host which requests wait now; the window is woken when that
    /// changed.
    pub(crate) fn poll_secret_asks(&self, client: &RemoteClient) {
        if self.no_secret_asks.load(Ordering::SeqCst) || self.chat_only.load(Ordering::Acquire) {
            return;
        }
        let got = match client.secret_asks() {
            Ok(Some(asks)) => asks,
            Ok(None) => {
                self.no_secret_asks.store(true, Ordering::SeqCst);
                Vec::new()
            }
            // Paused there: none is shown, nor can be answered.
            Err(crate::client::ClientError::Denied(crate::wire::DenyReason::Paused)) => Vec::new(),
            // Kept as it was; asked again at the next heartbeat.
            Err(_) => return,
        };
        let mut shown = self
            .secret_asks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *shown != got {
            *shown = got;
            drop(shown);
            (self.wake)();
        }
    }

    pub(crate) const fn panels(&self) -> &crate::project_cache::Panels {
        &self.panels
    }

    /// The connection while there is one.
    pub(crate) fn client(&self) -> Option<Arc<RemoteClient>> {
        self.client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_last_error(&self, error: Option<String>) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = error;
    }

    pub(crate) fn wake_window(&self) {
        (self.wake)();
    }

    /// List the host's terminals again at the supervisor's next step.
    pub(crate) fn request_list(&self) {
        self.refresh.store(true, Ordering::SeqCst);
    }

    /// Every terminal of `project` as the host last listed it, attached or
    /// not: on a chat-only client too, which attaches only the app session.
    #[must_use]
    pub fn roster(&self, project: &str) -> Vec<crate::remote_terminals::RosterTerminal> {
        let listed = self.listed.lock().unwrap_or_else(PoisonError::into_inner);
        crate::remote_terminals::roster(&listed.0, listed.1, project)
    }

    pub(crate) fn set_chat_only(&self, on: bool) {
        self.chat_only.store(on, Ordering::Release);
    }

    /// Why the last add, close, restart or rename failed.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The projects of the host's latest accepted list, in its order, including
    /// those with no terminal yet.
    #[must_use]
    pub fn listed_projects(&self) -> Vec<ProjectInfo> {
        self.projects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The projects of the host's latest accepted list that have a terminal
    /// working: in a turn, or with background work after one. Counted over
    /// every terminal the host listed, also those a chat client does not
    /// follow.
    #[must_use]
    pub fn working_projects(&self) -> HashSet<String> {
        self.working
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The models the host offers for a new `tool` terminal in `project`.
    /// `None` when the host is from before models: only the tool's default.
    ///
    /// # Errors
    /// Not connected, or the host's refusal.
    pub fn models(&self, tool: ToolKind, project: &str) -> Result<Option<ToolModels>, String> {
        let client = self
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or("not connected to that host")?;
        if !client_knows(&client, tool) {
            return Err(TOO_OLD_FOR_OPENCODE.to_owned());
        }
        client
            .models(crate::hosting::tool_byte(tool), project)
            .map_err(|e| e.to_string())
    }

    /// A chat completion from the host's conversation model, waiting at most
    /// the request's `wait_ms`. It waits for the host, so never call it from the UI thread.
    ///
    /// # Errors
    /// [`CompletionError::Unavailable`] when the host has none to offer (none
    /// set up, or from before completions); [`CompletionError::Failed`]
    /// otherwise (not connected, too slow, the model's own error).
    pub fn complete(&self, request: &crate::wire::Completion) -> Result<String, CompletionError> {
        let client = self
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| CompletionError::Failed("not connected to that host".into()))?;
        client.complete(request).map_err(|e| match e {
            ClientError::Denied(DenyReason::Protocol) => CompletionError::Unavailable(
                "that machine's Connecto is too old to answer: update it".into(),
            ),
            ClientError::Remote(why) if why == crate::assistant::NOT_SET_UP => {
                CompletionError::Unavailable(why)
            }
            ClientError::Remote(why) => CompletionError::Failed(why),
            ClientError::Protocol => CompletionError::Failed("no answer in time".into()),
            other => CompletionError::Failed(other.to_string()),
        })
    }

    /// Whether `text` is a finished turn of a voice call, and fits its
    /// `context`, as the host's Jev judges it, waiting at most `wait`. It waits for the host, so never
    /// call it from the UI thread.
    ///
    /// A host from before turn checks answers
    /// [`crate::wire::TurnVerdict::Unavailable`]; a failure to ask,
    /// [`crate::wire::TurnVerdict::Failed`].
    #[must_use]
    pub fn turn_check(
        &self,
        text: &str,
        context: &str,
        wait: Duration,
    ) -> crate::wire::TurnVerdict {
        use crate::wire::TurnVerdict;
        let Some(client) = self
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return TurnVerdict::Failed("not connected to that host".into());
        };
        match client.turn_check(text, context, wait) {
            Ok(verdict) => verdict,
            Err(ClientError::Denied(DenyReason::Protocol)) => {
                TurnVerdict::Unavailable("that machine's Connecto is too old to answer".into())
            }
            Err(ClientError::Protocol) => TurnVerdict::Failed("no answer in time".into()),
            Err(other) => TurnVerdict::Failed(other.to_string()),
        }
    }

    /// The file at `path` on the host, as its terminal `terminal` printed
    /// it: the host's full path and the bytes. It waits for the whole file,
    /// so never call it from the UI thread.
    ///
    /// # Errors
    /// Not connected, the host's refusal, or a transfer cut short.
    pub fn read_file(&self, terminal: u32, path: &str) -> Result<(String, Vec<u8>), String> {
        let _reading = Reading::start();
        let client = self
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or("not connected to that host")?;
        client.read_file(terminal, path).map_err(|e| match e {
            ClientError::Denied(DenyReason::Protocol) => {
                "that machine's Connecto is too old to send files: update it".to_owned()
            }
            ClientError::Remote(why) => why,
            other => other.to_string(),
        })
    }

    /// Sends a file dropped or pasted on the host's terminal `terminal`,
    /// named after `name`: the host's path to it. It waits for the whole
    /// transfer, so never call it from the UI thread.
    ///
    /// # Errors
    /// Not connected, the host's refusal, or a transfer cut short.
    pub fn upload_file(&self, terminal: u32, name: &str, bytes: &[u8]) -> Result<String, String> {
        let _sending = Reading::start();
        let client = self
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or("Not connected to that host.")?;
        client
            .upload_file(terminal, name, bytes)
            .map_err(|e| match e {
                ClientError::Denied(DenyReason::Protocol) => {
                    "Update Connecto on the host to send files to its terminals.".to_owned()
                }
                ClientError::Denied(DenyReason::Paused) => {
                    "The host paused remote actions. Try again when it resumes.".to_owned()
                }
                ClientError::Denied(DenyReason::NoSuchTerminal) => {
                    "That terminal is no longer on the host.".to_owned()
                }
                ClientError::Remote(why) => why,
                ClientError::Net(_) => "The file transfer was interrupted. Try again.".to_owned(),
                other => other.to_string(),
            })
    }

    /// The models the host offers for a new `tool` terminal in `project`, as
    /// last fetched. Never blocks: a missing or stale answer is fetched on a
    /// thread of its own, which wakes the window when it ends. Empty until the
    /// first answer, and for a host from before models.
    pub fn offered_models(self: &Arc<Self>, tool: ToolKind, project: &str) -> ToolModels {
        let key = (crate::hosting::tool_byte(tool), project.to_owned());
        let mut cache = self.models.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = cache.entry(key.clone()).or_default();
        let stale = entry.at.is_none_or(|at| at.elapsed() >= MODELS_FRESH);
        if stale && !entry.fetching && self.is_connected() {
            entry.fetching = true;
            let this = Arc::clone(self);
            let project = project.to_owned();
            let spawned = thread::Builder::new()
                .name("connecto-remote-models".into())
                .spawn(move || {
                    let got = this.models(tool, &project);
                    {
                        let mut cache = this.models.lock().unwrap_or_else(PoisonError::into_inner);
                        let entry = cache.entry(key).or_default();
                        entry.fetching = false;
                        entry.at = Some(Instant::now());
                        match got {
                            Ok(Some(models)) => entry.value = models,
                            Ok(None) => entry.value = ToolModels::default(),
                            // Kept as it was; asked again once it is stale.
                            Err(_) => {}
                        }
                    }
                    (this.wake)();
                });
            if spawned.is_err() {
                entry.fetching = false;
            }
        }
        entry.value.clone()
    }

    /// Whether the connection's host lists background work and stops it.
    #[must_use]
    pub fn has_background(&self) -> bool {
        self.client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|client| client.has_feature(FEATURE_BACKGROUND))
    }

    /// Whether a new `tool` terminal can be asked of this host now: connected,
    /// and for OpenCode a host that knows it.
    #[must_use]
    pub fn knows(&self, tool: ToolKind) -> bool {
        self.client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_deref()
            .is_some_and(|client| client_knows(client, tool))
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
            // An older host would read OpenCode's byte as Claude and start
            // the wrong agent: it is never asked.
            Some(client)
                if matches!(&op, ManageOp::Add { tool, .. }
                    if !client_knows(&client, crate::hosting::tool_from_byte(*tool))) =>
            {
                Err(TOO_OLD_FOR_OPENCODE.to_owned())
            }
            Some(client) => client
                .manage(self.model_if_understood(&client, op))
                .map_err(|e| e.to_string()),
            None => Err("not connected to that host".to_owned()),
        };
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = result.as_ref().err().cloned();
        if result.is_ok() {
            // The change is listed again at once, so a restarted terminal can be
            // attached without waiting for the heartbeat.
            self.refresh.store(true, Ordering::SeqCst);
        }
        (self.wake)();
        result
    }

    /// `op` as this host reads it: an `Add` keeps its model only for a host
    /// that answers a models probe. One from before models would refuse the
    /// whole frame, so it gets the add without the model (the tool's
    /// default), as it always did.
    fn model_if_understood(&self, client: &RemoteClient, op: ManageOp) -> ManageOp {
        let ManageOp::Add {
            tool,
            name,
            project,
            model: Some(model),
        } = op
        else {
            return op;
        };
        let probe_project = project
            .clone()
            .or_else(|| self.listed_projects().first().map(|p| p.id.clone()));
        let understood = client.models_supported()
            || probe_project.is_some_and(|p| matches!(client.models(tool, &p), Ok(Some(_))));
        ManageOp::Add {
            tool,
            name,
            project,
            model: understood.then_some(model),
        }
    }

    /// Whether this handle's request changed the host since the last check.
    fn take_refresh(&self) -> bool {
        self.refresh.swap(false, Ordering::SeqCst)
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
            Ok(client) => {
                // Lists then carry each terminal's background work and its id
                // on the host. A host that cannot say grants less, and lists
                // as it always did.
                // It also says whether it knows OpenCode, and whether it
                // takes terminal uploads past 10 MiB.
                let _ = client.request_features(
                    FEATURE_BACKGROUND
                        | FEATURE_TERMINAL_IDS
                        | crate::wire::FEATURE_TERMINAL_OWNERS
                        | crate::wire::FEATURE_OPENCODE
                        | crate::wire::FEATURE_LARGE_UPLOADS,
                );
                return Ok(client);
            }
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

/// A terminal the host has not started since it launched, and that is not a
/// restored chat: it has no screen yet, so no stream is opened for it (each
/// costs threads on both machines) until a list says it runs.
fn dormant(info: &TerminalInfo) -> bool {
    crate::hosting::never_started(info.running, info.activity, &info.name)
}

/// Attaches every terminal whose stream ended, without listing first. A
/// terminal the host no longer grants is refused, and is dropped by the next
/// list.
fn reattach_closed(
    handle: &HostHandle,
    client: &RemoteClient,
    refused: &mut HashSet<u32>,
) -> Result<(), String> {
    for term in handle.terminals() {
        if term.session.is_closed() && !refused.contains(&term.info.id) && !dormant(&term.info) {
            match term.session.attach(client, term.info.id) {
                Ok(()) => {}
                Err(ClientError::Denied(DenyReason::NoSuchTerminal)) => {
                    refused.insert(term.info.id);
                }
                Err(err) => return Err(err.to_string()),
            }
        }
    }
    Ok(())
}

/// Make the sessions match the host's list, and attach any that lost their link.
fn sync(
    handle: &HostHandle,
    client: &RemoteClient,
    config: &SupervisorConfig,
) -> Result<(), String> {
    let listing = client.listing().map_err(|e| e.to_string())?;
    let listed_at = Instant::now();
    // Refused whole, before any session is made or kept.
    crate::remote_terminals::check_listing(&listing)?;
    let mut mine = handle
        .terminals
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let host = handle.name();
    let mut next = Vec::with_capacity(listing.terminals.len());
    let working: HashSet<String> = listing
        .terminals
        .iter()
        .filter(|info| crate::hosting::activity_from_byte(info.activity) == TermActivity::Working)
        .map(|info| info.project_id.clone())
        .collect();
    // The whole list, before a chat-only client leaves out what it does not
    // attach: a call is told of every terminal of its project.
    *handle.listed.lock().unwrap_or_else(PoisonError::into_inner) =
        (listing.terminals.clone(), listed_at);
    for info in listing.terminals {
        if handle.chat_only.load(Ordering::Acquire)
            && (info.parent.is_some()
                || info.name != connecto_core::session::APP_SESSION
                || ![
                    ToolKind::Claude,
                    ToolKind::Codex,
                    ToolKind::Pi,
                    ToolKind::Antigravity,
                    ToolKind::OpenCode,
                ]
                .map(crate::hosting::tool_byte)
                .contains(&info.tool))
        {
            continue;
        }
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
        if !session.is_linked() && !dormant(&info) {
            match session.attach(client, info.id) {
                Ok(()) => {}
                // Replaced or closed on the host since this list: the next list
                // says so, and the link stays up.
                Err(ClientError::Denied(DenyReason::NoSuchTerminal)) => {}
                Err(err) => return Err(err.to_string()),
            }
        }
        next.push(RemoteTerminal {
            info,
            session,
            listed_at,
        });
    }
    *mine = next;
    *handle
        .projects
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = listing.projects;
    *handle
        .working
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = working;
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

/// The least time between two lists that a lost terminal triggers.
const RESYNC_GAP: Duration = Duration::from_millis(200);

/// Follow one connection until it fails or the handle stops.
fn serve_link(
    handle: &HostHandle,
    client: &RemoteClient,
    config: &SupervisorConfig,
) -> Result<(), String> {
    // The terminals whose re-attachment the host refused, until the next list:
    // a refusal is not retried on its own, because nothing has changed for it.
    let mut refused: HashSet<u32> = HashSet::new();
    handle.no_offers.store(false, Ordering::SeqCst);
    handle.no_secret_asks.store(false, Ordering::SeqCst);
    handle.no_groups.store(false, Ordering::SeqCst);
    loop {
        sync(handle, client, config)?;
        refused.clear();
        handle.poll_offers(client);
        handle.poll_secret_asks(client);
        handle.poll_groups(client);
        let path = client.path();
        if matches!(handle.state(), LinkState::Connected(_)) {
            handle.state.store(Arc::new(LinkState::Connected(path)));
        }
        let until = Instant::now() + config.heartbeat;
        let mut last_list = Instant::now();
        while Instant::now() < until {
            if handle.stopped() {
                return Ok(());
            }
            // A change this handle made, or a terminal whose stream ended (it was
            // restarted or closed on the host, or the link is gone): list again
            // now. The list also attaches what the host still has, and a dead link
            // fails that list, which redials. Throttled, so a refused attach cannot
            // spin.
            if handle.take_refresh() {
                sync(handle, client, config)?;
                refused.clear();
                last_list = Instant::now();
            } else if last_list.elapsed() >= RESYNC_GAP
                && handle
                    .terminals()
                    .iter()
                    .any(|t| t.session.is_closed() && !dormant(&t.info))
            {
                // A terminal's stream ended: it was restarted or closed on the host.
                // A restart keeps the entry, so it is attached again now, without a
                // list; a link that is gone fails here and redials.
                reattach_closed(handle, client, &mut refused)?;
                last_list = Instant::now();
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
                        // What the last connection was told is not this one's:
                        // nothing of it shows until this host answers again.
                        *handle.groups.lock().unwrap_or_else(PoisonError::into_inner) = None;
                        handle
                            .secret_asks
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .clear();
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

/// Why a host gave no chat completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionError {
    /// It has none to offer: none set up, or too old. Asking again soon
    /// gets the same.
    Unavailable(String),
    /// This one failed: not connected, too slow, the model's own error.
    Failed(String),
}

impl std::fmt::Display for CompletionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}
