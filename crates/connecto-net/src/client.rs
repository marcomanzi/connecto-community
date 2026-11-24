//! The client: connects to a host, proves it holds an allowed key, and shows a
//! host's terminal as a [`TerminalSession`] the UI already knows how to draw.
//!
//! The terminal keeps its own emulator, fed with the host's output bytes, so
//! drawing, scrollback and selection are the local code. Its replies to
//! terminal queries are dropped: the host's emulator answers those, and a
//! second answer would reach the program twice.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use arc_swap::ArcSwap;
use connecto_core::{
    GridSize, InputRefused, ResizeOutcome, SelectOp, SelectionText, SessionStatus, SnapshotOutcome,
    TermSnapshot, TerminalSession,
};
use connecto_term::Emulator;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

use crate::auth::{SignError, Signer, client_response};
use crate::seq::{Accept, Tracker};
use crate::transport::{Connection, EndpointId, NetError, PathKind, Stream};
use crate::wire::{
    DenyReason, Listing, MAX_DATA, ManageOp, Message, PROTOCOL_VERSION, RemoteStatus, TerminalAddr,
    TerminalInfo, ToolModels,
};

const ATTACHMENT_UNKNOWN: u8 = 0;
const ATTACHMENT_PENDING: u8 = 1;
const ATTACHMENT_SUPPORTED: u8 = 2;
const ATTACHMENT_UNSUPPORTED: u8 = 3;

fn attachment_error(error: String) -> String {
    let text: String = error
        .chars()
        .filter(|ch| !ch.is_control())
        .take(240)
        .collect();
    if text.trim().is_empty() {
        "The host could not receive the attachment.".into()
    } else {
        text
    }
}

/// Sends an upload's `bytes` after the host's `AttachmentReady`, and reads
/// its `AttachmentResult`: the host's path, or its refusal. `None` when the
/// transfer was cut short or the reply was not one.
fn send_upload(stream: &dyn Stream, bytes: &[u8]) -> Option<Result<String, String>> {
    let deadline = Instant::now() + crate::attachments::transfer_timeout(bytes.len());
    send_upload_until(stream, bytes, deadline)
}

/// [`send_upload`] within `deadline`, the transfer's whole budget, which
/// is also the only limit on the host's answer: once the last chunk is
/// queued, up to [`crate::transport::STREAM_QUEUE`] chunks (16 MiB) may still
/// be on their way, and on a slow link they take longer than any short wait
/// for the answer would allow.
fn send_upload_until(
    stream: &dyn Stream,
    bytes: &[u8],
    deadline: Instant,
) -> Option<Result<String, String>> {
    let failed = AtomicBool::new(false);
    for chunk in bytes.chunks(MAX_DATA) {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(crate::attachments::IDLE_TIMEOUT);
        if remaining.is_zero()
            || !send_within(
                stream,
                &Message::AttachmentChunk(chunk.to_vec()),
                &failed,
                remaining,
            )
        {
            // The host may have ended the transfer with its reason (Pause):
            // read it, briefly, rather than call it an interruption.
            return match stream.recv(Duration::from_secs(2)) {
                Ok(Some(Message::AttachmentResult(Err(error)))) => {
                    Some(Err(attachment_error(error)))
                }
                _ => None,
            };
        }
    }
    match stream.recv(deadline.saturating_duration_since(Instant::now())) {
        Ok(Some(Message::AttachmentResult(Ok(path))))
            if !path.is_empty() && !path.chars().any(char::is_control) =>
        {
            Some(Ok(path))
        }
        Ok(Some(Message::AttachmentResult(Err(error)))) => Some(Err(attachment_error(error))),
        _ => None,
    }
}

const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// How much longer than its budget a completion's answer may take to arrive.
const COMPLETION_GRACE: Duration = Duration::from_millis(200);
/// Less time than this left, a completion is not asked at all.
const MIN_COMPLETION: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(50);
/// Applied bytes between acknowledgements.
const ACK_EVERY: u64 = 32 * 1024;
const INPUT_QUEUE: usize = 1024;

/// Why connecting or attaching failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the machine that answered is not the one saved for this host")]
    HostMismatch,
    #[error("the host refused: {0}")]
    Denied(DenyReason),
    #[error("{0}")]
    Net(#[from] NetError),
    #[error("{0}")]
    Sign(#[from] SignError),
    #[error("the host sent something unexpected")]
    Protocol,
    #[error("{0}")]
    Remote(String),
}

/// A connection to one host, after the handshake.
pub struct RemoteClient {
    next_req: AtomicU32,
    host: String,
    conn: Arc<dyn Connection>,
    control: Mutex<Box<dyn Stream>>,
    chat_confirmed: Arc<AtomicBool>,
    /// The host answered a models probe on this connection, so it reads an
    /// `Add` that names a model.
    models_confirmed: AtomicBool,
    /// The features the host granted this connection (see [`Self::features`]).
    features: AtomicU32,
}

impl RemoteClient {
    /// Connect and authenticate. `expected_host` is the pinned identity: a
    /// connection to any other machine is refused before anything is signed.
    ///
    /// # Errors
    /// See [`ClientError`].
    pub fn connect(
        conn: Arc<dyn Connection>,
        host_name: &str,
        expected_host: &EndpointId,
        signer: &dyn Signer,
    ) -> Result<Self, ClientError> {
        if &conn.remote_id() != expected_host {
            conn.close();
            return Err(ClientError::HostMismatch);
        }
        let control = conn.open_stream()?;
        control.send(&Message::Hello {
            version: PROTOCOL_VERSION,
        })?;
        let nonce = match control.recv(REPLY_TIMEOUT)? {
            Some(Message::Challenge { nonce }) => nonce,
            Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
            _ => return Err(ClientError::Protocol),
        };
        let binding = conn.channel_binding()?;
        let response = client_response(
            signer,
            &nonce,
            &conn.local_id(),
            &conn.remote_id(),
            &binding,
        )?;
        control.send(&response)?;
        match control.recv(REPLY_TIMEOUT)? {
            Some(Message::AuthOk) => {}
            Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
            _ => return Err(ClientError::Protocol),
        }
        Ok(Self {
            next_req: AtomicU32::new(1),
            host: host_name.to_owned(),
            conn,
            control: Mutex::new(control),
            chat_confirmed: Arc::default(),
            models_confirmed: AtomicBool::new(false),
            features: AtomicU32::new(0),
        })
    }

    /// The display name this client knows the host by.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Whether traffic goes straight to the host or through the relay.
    #[must_use]
    pub fn path(&self) -> PathKind {
        self.conn.path()
    }

    /// The host's terminals, addressed as `(host, terminal)`.
    ///
    /// # Errors
    /// On a closed connection or an unexpected reply.
    pub fn list(&self) -> Result<Vec<(TerminalAddr, TerminalInfo)>, ClientError> {
        Ok(self
            .listing()?
            .terminals
            .into_iter()
            .map(|info| {
                (
                    TerminalAddr {
                        host: self.host.clone(),
                        terminal: info.id,
                    },
                    info,
                )
            })
            .collect())
    }

    /// The host's projects and terminals, as its latest list. The host keeps
    /// this list as the only things this connection may name.
    ///
    /// # Errors
    /// On a closed connection or an unexpected reply.
    pub fn listing(&self) -> Result<Listing, ClientError> {
        let control = self.control.lock().unwrap_or_else(PoisonError::into_inner);
        control.send(&Message::ListTerminals)?;
        // A host that keeps talking about other things still gets an end.
        let deadline = std::time::Instant::now() + REPLY_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match control.recv(REPLY_TIMEOUT)? {
                Some(
                    Message::TerminalList(listing)
                    | Message::TerminalListEx(listing)
                    | Message::TerminalListIds(listing)
                    | Message::TerminalListOwners(listing),
                ) => {
                    return Ok(listing);
                }
                Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
                Some(_) | None => {}
            }
        }
        Err(ClientError::Protocol)
    }

    /// Ask the host to add, close, restart or rename a terminal. Returns the new
    /// terminal's id when the host knows it at once.
    ///
    /// # Errors
    /// `Remote` with the host's reason (Pause, no such terminal...), or a
    /// transport error.
    pub fn manage(&self, op: ManageOp) -> Result<Option<u32>, ClientError> {
        let req = self.next_req.fetch_add(1, Ordering::SeqCst);
        let control = self.control.lock().unwrap_or_else(PoisonError::into_inner);
        control.send(&Message::Manage { req, op })?;
        let deadline = std::time::Instant::now() + REPLY_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match control.recv(Duration::from_millis(200))? {
                Some(Message::ManageResult { req: got, outcome }) if got == req => {
                    return outcome.map_err(ClientError::Remote);
                }
                Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
                _ => {}
            }
        }
        Err(ClientError::Protocol)
    }

    /// The models the host offers for a new terminal of `tool` (a tool byte)
    /// in `project`, asked on a stream of its own. `None` when the host is
    /// from before models: it then refuses an `Add` that names one, so none
    /// is sent to it.
    ///
    /// # Errors
    /// The host's refusal (a project it did not list), or a transport error.
    pub fn models(&self, tool: u8, project: &str) -> Result<Option<ToolModels>, ClientError> {
        let stream = self.conn.open_stream()?;
        stream.send(&Message::ModelsProbe {
            tool,
            project: project.to_owned(),
        })?;
        match stream.recv(REPLY_TIMEOUT)? {
            Some(Message::Models(models)) => {
                self.models_confirmed.store(true, Ordering::SeqCst);
                Ok(Some(models))
            }
            Some(Message::Denied(DenyReason::Protocol)) => Ok(None),
            Some(Message::Denied(reason)) => Err(ClientError::Denied(reason)),
            _ => Err(ClientError::Protocol),
        }
    }

    /// A chat completion from the host's conversation model, asked on a
    /// stream of its own, waiting at most the request's `wait_ms` in all
    /// (opening the stream included).
    ///
    /// # Errors
    /// [`ClientError::Remote`] with the host's reason (none set up, not
    /// running, too slow); [`ClientError::Denied`] with `Protocol` from a host
    /// from before completions; [`ClientError::Protocol`] when no answer came
    /// in time.
    pub fn complete(&self, request: &crate::wire::Completion) -> Result<String, ClientError> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(request.wait_ms));
        let left = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|d| *d >= MIN_COMPLETION)
                .ok_or(ClientError::Protocol)
        };
        let stream = self.conn.open_stream_within(left()?)?;
        // Only what is left goes to the host: it never works for an answer
        // nobody waits for, and nothing goes once the time is up.
        let budget = left()?;
        let asked = crate::wire::Completion {
            wait_ms: u32::try_from(budget.as_millis()).unwrap_or(u32::MAX),
            ..request.clone()
        };
        stream.send(&Message::Complete(asked))?;
        // A little over the host's budget, for its answer to come back.
        match stream.recv(budget + COMPLETION_GRACE)? {
            Some(Message::Completed(answer)) => answer.map_err(ClientError::Remote),
            Some(Message::Denied(reason)) => Err(ClientError::Denied(reason)),
            _ => Err(ClientError::Protocol),
        }
    }

    /// Whether `text`, what the person has said so far in a voice call, is a
    /// finished turn, asked on a stream of its own, waiting at most `wait`
    /// in all (opening the stream included).
    ///
    /// A transcript over [`crate::wire::MAX_TURN_TEXT`], or a context over
    /// [`crate::wire::MAX_TURN_CONTEXT`], is not asked about at all ([`crate::wire::TurnVerdict::Failed`]): cutting it here, before
    /// the host redacts it, could split a secret and send its end as plain
    /// text.
    ///
    /// # Errors
    /// [`ClientError::Denied`] with `Protocol` from a host from before turn
    /// checks; [`ClientError::Protocol`] when no answer came in time.
    pub fn turn_check(
        &self,
        text: &str,
        context: &str,
        wait: Duration,
    ) -> Result<crate::wire::TurnVerdict, ClientError> {
        if text.len() > crate::wire::MAX_TURN_TEXT || context.len() > crate::wire::MAX_TURN_CONTEXT
        {
            return Ok(crate::wire::TurnVerdict::Failed(
                "too long to judge".to_owned(),
            ));
        }
        let deadline = Instant::now() + wait;
        let left = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|d| *d >= MIN_COMPLETION)
                .ok_or(ClientError::Protocol)
        };
        let stream = self.conn.open_stream_within(left()?)?;
        let budget = left()?;
        stream.send(&Message::TurnCheck {
            text: text.to_owned(),
            context: context.to_owned(),
            wait_ms: u32::try_from(budget.as_millis()).unwrap_or(u32::MAX),
        })?;
        match stream.recv(budget + COMPLETION_GRACE)? {
            Some(Message::TurnChecked(verdict)) => Ok(verdict),
            Some(Message::Denied(reason)) => Err(ClientError::Denied(reason)),
            _ => Err(ClientError::Protocol),
        }
    }

    /// The file at `path` on the host, as terminal `terminal` printed it,
    /// asked on a stream of its own: the host's full path to it, and its
    /// bytes. A file is at most [`crate::wire::MAX_FILE_BYTES`].
    ///
    /// # Errors
    /// The host's refusal, as it worded it; [`ClientError::Denied`] with
    /// `Protocol` from a host from before files; a transfer cut short.
    pub fn read_file(&self, terminal: u32, path: &str) -> Result<(String, Vec<u8>), ClientError> {
        let stream = self.conn.open_stream()?;
        stream.send(&Message::FileRead {
            terminal,
            path: path.to_owned(),
        })?;
        receive_file(&*stream)
    }

    /// The files the host offers to download in the projects of this
    /// connection's list. `None` from a host from before offers.
    ///
    /// # Errors
    /// The host's refusal, or no answer.
    pub fn offers(&self) -> Result<Option<Vec<crate::wire::OfferInfo>>, ClientError> {
        let stream = self.conn.open_stream()?;
        stream.send(&Message::OffersProbe)?;
        match stream.recv(REPLY_TIMEOUT)? {
            Some(Message::Offers(offers)) => Ok(Some(offers)),
            Some(Message::Denied(DenyReason::Protocol)) => Ok(None),
            Some(Message::Denied(reason)) => Err(ClientError::Denied(reason)),
            _ => Err(ClientError::Protocol),
        }
    }

    /// The host's project groups. `None` from a host from before they were
    /// shared.
    ///
    /// # Errors
    /// The host's refusal, or no answer.
    pub fn groups(&self) -> Result<Option<connecto_core::RemoteGroups>, ClientError> {
        let stream = self.conn.open_stream()?;
        stream.send(&Message::GroupsProbe)?;
        match stream.recv(REPLY_TIMEOUT)? {
            Some(Message::Groups(json)) => Ok(Some(crate::groups::groups_from(&json))),
            Some(Message::Denied(DenyReason::Protocol)) => Ok(None),
            Some(Message::Denied(reason)) => Err(ClientError::Denied(reason)),
            _ => Err(ClientError::Protocol),
        }
    }

    /// The requests for a secret that terminals of this connection's list
    /// wait on. `None` from a host from before they were shared.
    ///
    /// # Errors
    /// The host's refusal, or no answer.
    pub fn secret_asks(&self) -> Result<Option<Vec<connecto_core::SecretAsk>>, ClientError> {
        let stream = self.conn.open_stream()?;
        stream.send(&Message::SecretAsksProbe)?;
        match stream.recv(REPLY_TIMEOUT)? {
            Some(Message::SecretAsks(json)) => Ok(Some(crate::remote_secrets::asks_from(&json))),
            Some(Message::Denied(DenyReason::Protocol)) => Ok(None),
            Some(Message::Denied(reason)) => Err(ClientError::Denied(reason)),
            _ => Err(ClientError::Protocol),
        }
    }

    /// The file of the host's offer `id`: its name and bytes.
    ///
    /// # Errors
    /// As [`RemoteClient::read_file`].
    pub fn read_offer(&self, id: u64) -> Result<(String, Vec<u8>), ClientError> {
        let stream = self.conn.open_stream()?;
        stream.send(&Message::OfferRead { id })?;
        receive_file(&*stream)
    }

    /// Asks the host something of its project `project` (see
    /// [`crate::project_panels`]) on a stream of its own: the answer's bytes.
    ///
    /// # Errors
    /// [`ClientError::Remote`] with the host's reason; [`ClientError::Denied`]
    /// with `Protocol` from a host from before project asks; a transfer cut
    /// short or larger than [`crate::project_panels::MAX_ANSWER`].
    pub fn project_ask(
        &self,
        project: &str,
        ask: &crate::project_panels::ProjectAsk,
    ) -> Result<Vec<u8>, ClientError> {
        let stream = self.conn.open_stream()?;
        stream.send(&Message::ProjectAsk {
            project: project.to_owned(),
            ask: crate::wire::AskBytes(ask.encode()),
        })?;
        let (path, data) = receive_file(&*stream)?;
        if !path.is_empty() || data.len() as u64 > crate::project_panels::MAX_ANSWER {
            return Err(ClientError::Protocol);
        }
        Ok(data)
    }

    /// [`Self::project_ask`] within `wait` in all: opening the stream, the
    /// host's answer and every chunk of it share the one deadline, so a
    /// stalled host never holds the caller (Siri, with its few seconds)
    /// past it. Whether the ask may have reached the host is told apart.
    ///
    /// # Errors
    /// See [`crate::siri::SiriCallError`].
    pub fn project_ask_within(
        &self,
        project: &str,
        ask: &crate::project_panels::ProjectAsk,
        wait: Duration,
    ) -> Result<Vec<u8>, crate::siri::SiriCallError> {
        use crate::siri::SiriCallError;
        let deadline = Instant::now() + wait;
        let left = || deadline.saturating_duration_since(Instant::now());
        let stream = self
            .conn
            .open_stream_within(left())
            .map_err(|e| SiriCallError::NotSent(e.to_string()))?;
        if left().is_zero() {
            return Err(SiriCallError::NotSent("no time left to send".to_owned()));
        }
        // From here on the host may have it: a write cut short could still
        // have reached it.
        stream
            .send(&Message::ProjectAsk {
                project: project.to_owned(),
                ask: crate::wire::AskBytes(ask.encode()),
            })
            .map_err(|_| SiriCallError::Unconfirmed)?;
        let refused = |why: String| {
            if crate::siri::is_unknown_ask(&why) {
                SiriCallError::UnknownAsk
            } else {
                SiriCallError::Refused(why)
            }
        };
        let denied = |reason: DenyReason| match reason {
            DenyReason::Protocol => SiriCallError::UnknownAsk,
            other => SiriCallError::Refused(other.to_string()),
        };
        let len = match stream.recv(left()) {
            Ok(Some(Message::FileStart { path, bytes })) if path.is_empty() => {
                usize::try_from(bytes)
                    .ok()
                    .filter(|n| *n as u64 <= crate::project_panels::MAX_ANSWER)
                    .ok_or(SiriCallError::Unconfirmed)?
            }
            Ok(Some(Message::FileRefused(why))) => return Err(refused(why)),
            Ok(Some(Message::Denied(reason))) => return Err(denied(reason)),
            _ => return Err(SiriCallError::Unconfirmed),
        };
        let mut data = Vec::with_capacity(len);
        while data.len() < len {
            match stream.recv(left()) {
                Ok(Some(Message::FileChunk(chunk)))
                    if !chunk.is_empty() && chunk.len() <= len - data.len() =>
                {
                    data.extend_from_slice(&chunk);
                }
                Ok(Some(Message::Denied(reason))) => return Err(denied(reason)),
                _ => return Err(SiriCallError::Unconfirmed),
            }
        }
        Ok(data)
    }

    /// Sends `bytes`, a file dropped or pasted on the host's terminal
    /// `terminal`, on a stream of its own, named after `name`: the host's
    /// path to the saved file. At most [`crate::wire::MAX_UPLOAD_BYTES`] to a
    /// host that granted [`crate::wire::FEATURE_LARGE_UPLOADS`], and
    /// [`crate::wire::LEGACY_UPLOAD_BYTES`] to any other: a larger file is
    /// refused here, before anything is sent.
    ///
    /// # Errors
    /// The host's refusal, as it worded it; [`ClientError::Denied`] with
    /// `Protocol` from a host from before terminal uploads; a transfer cut
    /// short.
    pub fn upload_file(
        &self,
        terminal: u32,
        name: &str,
        bytes: &[u8],
    ) -> Result<String, ClientError> {
        use crate::wire::{FEATURE_LARGE_UPLOADS, LEGACY_UPLOAD_BYTES, MAX_UPLOAD_BYTES};
        let mib = |limit: usize| limit / (1024 * 1024);
        if bytes.len() > MAX_UPLOAD_BYTES {
            return Err(ClientError::Remote(format!(
                "The file is larger than the {} MiB a host's terminal takes.",
                mib(MAX_UPLOAD_BYTES)
            )));
        }
        // An older host takes 10 MiB: told so here rather than sent a file
        // it would refuse.
        if bytes.len() > LEGACY_UPLOAD_BYTES && !self.has_feature(FEATURE_LARGE_UPLOADS) {
            return Err(ClientError::Remote(format!(
                "This host takes files of up to {} MiB. Update Connecto on the host to send \
                 files of up to {} MiB.",
                mib(LEGACY_UPLOAD_BYTES),
                mib(MAX_UPLOAD_BYTES)
            )));
        }
        let size = u32::try_from(bytes.len()).map_err(|_| ClientError::Protocol)?;
        let stream = self.conn.open_stream()?;
        stream.send(&Message::TerminalUpload {
            terminal,
            name: name.to_owned(),
            bytes: size,
        })?;
        match stream.recv(REPLY_TIMEOUT)? {
            Some(Message::AttachmentReady) => {}
            Some(Message::AttachmentResult(Err(why))) => {
                return Err(ClientError::Remote(attachment_error(why)));
            }
            Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
            _ => return Err(ClientError::Protocol),
        }
        match send_upload(&*stream, bytes) {
            Some(Ok(path)) => Ok(path),
            Some(Err(why)) => Err(ClientError::Remote(why)),
            None => Err(ClientError::Net(NetError::Closed)),
        }
    }

    /// Ask the host for `wanted` features (bits such as [`crate::wire::FEATURE_BACKGROUND`])
    /// on a stream of its own, and keep what it grants. A host from before
    /// features grants none. Asked before the first list, so that list already
    /// comes with them.
    ///
    /// # Errors
    /// A transport error, or a reply that is not one.
    pub fn request_features(&self, wanted: u32) -> Result<u32, ClientError> {
        let stream = self.conn.open_stream()?;
        stream.send(&Message::Features(wanted))?;
        let granted = match stream.recv(REPLY_TIMEOUT)? {
            Some(Message::Features(granted)) => granted & wanted,
            Some(Message::Denied(DenyReason::Protocol)) => 0,
            Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
            _ => return Err(ClientError::Protocol),
        };
        self.features.store(granted, Ordering::SeqCst);
        Ok(granted)
    }

    /// Whether the host granted `feature` on this connection.
    #[must_use]
    pub fn has_feature(&self, feature: u32) -> bool {
        self.features.load(Ordering::SeqCst) & feature == feature
    }

    /// Whether the host answered a models probe on this connection.
    #[must_use]
    pub fn models_supported(&self) -> bool {
        self.models_confirmed.load(Ordering::SeqCst)
    }

    /// Say goodbye and close.
    pub fn close(&self) {
        let control = self.control.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = control.send(&Message::Bye);
        self.conn.close();
    }
}

struct Shared {
    tracker: Mutex<Tracker>,
    status: ArcSwap<SessionStatus>,
    denied: ArcSwap<Option<DenyReason>>,
    /// The grid the host last reported.
    size: Mutex<GridSize>,
    /// The link that may change this session. A reader of an older link
    /// finds it is not current and leaves everything alone.
    generation: AtomicU64,
    /// Repaints received, which a resume does not need.
    resyncs: AtomicU64,
    /// An attach succeeded once, so a later one resumes.
    attached_once: AtomicBool,
}

/// What a live link to the host holds.
struct Link {
    connection: Arc<dyn Connection>,
    stream: Arc<dyn Stream>,
    /// Input and resizes, in one queue so they reach the host in the order
    /// they were made.
    input: Sender<Outgoing>,
    closed: Arc<AtomicBool>,
    chat: Arc<LinkChat>,
    previous_unanswered: u8,
    previous_history: Option<Arc<connecto_core::session::ChatHistory>>,
    /// The attach that made this link (see `Shared::generation`).
    generation: u64,
}

#[derive(Default)]
struct LinkChat {
    confirmed: Arc<AtomicBool>,
    history: arc_swap::ArcSwapOption<connecto_core::session::ChatHistory>,
    attachments: Arc<AtomicU8>,
    requested: AtomicBool,
    peer_closed: AtomicBool,
}

impl Link {
    fn cached_chat_history(&self) -> Option<Arc<connecto_core::session::ChatHistory>> {
        self.chat
            .history
            .load_full()
            .or_else(|| self.previous_history.clone())
    }

    fn unanswered_chat_attempts(&self) -> u8 {
        if self.chat.confirmed.load(Ordering::SeqCst) {
            return 0;
        }
        let unanswered = self.chat.requested.load(Ordering::SeqCst)
            && self.chat.peer_closed.load(Ordering::SeqCst);
        self.previous_unanswered
            .saturating_add(u8::from(unanswered))
    }
}

/// How long a send waits before a full queue is tried again.
const FULL_WAIT: Duration = Duration::from_millis(2);

/// How long a send keeps waiting on a full queue before the host is taken to be
/// gone. A host that paused is back well within this; one that never reads is
/// not waited for forever.
const FULL_PATIENCE: Duration = Duration::from_secs(30);

/// Sends `message`, waiting while the host's queue is full: a host that paused
/// for a moment is still there, so nothing is dropped or reordered. A genuine
/// failure sets `closed`, which makes the link read as gone, so the supervisor
/// redials. Returns false when the send did not happen (the link is closed, or
/// someone else closed it).
fn send_or_close(stream: &dyn Stream, message: &Message, closed: &AtomicBool) -> bool {
    send_within(stream, message, closed, FULL_PATIENCE)
}

fn send_within(
    stream: &dyn Stream,
    message: &Message,
    closed: &AtomicBool,
    patience: Duration,
) -> bool {
    let started = std::time::Instant::now();
    loop {
        match stream.send(message) {
            Ok(()) => return true,
            Err(NetError::Full) => {
                if closed.load(Ordering::SeqCst) {
                    return false;
                }
                if started.elapsed() >= patience {
                    closed.store(true, Ordering::SeqCst);
                    return false;
                }
                thread::sleep(FULL_WAIT);
            }
            Err(_) => {
                closed.store(true, Ordering::SeqCst);
                return false;
            }
        }
    }
}

fn apply_size(emulator: &Emulator, shared: &Shared, cols: u16, rows: u16) {
    let size = GridSize { cols, rows };
    // A screen busy drawing a burst of output can hold its lock longer than a
    // few tries: up to a second, so the size is not recorded while the screen
    // stays at the old one.
    for _ in 0..500 {
        if emulator.try_resize(size) != ResizeOutcome::Retry {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    *shared.size.lock().unwrap_or_else(PoisonError::into_inner) = size;
}

fn read_loop(
    stream: &dyn Stream,
    emulator: &Emulator,
    shared: &Shared,
    generation: u64,
    closed: &AtomicBool,
    chat: &LinkChat,
) {
    let current = || shared.generation.load(Ordering::SeqCst) == generation;
    let mut unacked = 0_u64;
    let mut denied = false;
    loop {
        // Checked on every pass, idle or not: a link closed by its writer, or a
        // newer attachment of this session, ends this reader, and with it the
        // stream it reads (the writer ends the same way).
        if closed.load(Ordering::SeqCst) || !current() {
            break;
        }
        let message = match stream.recv(POLL) {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(NetError::Closed) => {
                chat.peer_closed.store(!denied, Ordering::SeqCst);
                break;
            }
            Err(_) => break,
        };
        if !current() {
            break;
        }
        match message {
            Message::Output { seq, bytes } => {
                let verdict = shared
                    .tracker
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .accept(seq, bytes.len());
                match verdict {
                    Accept::Apply { skip } => {
                        emulator.feed(bytes.get(skip..).unwrap_or_default());
                        unacked += (bytes.len() - skip) as u64;
                        if unacked >= ACK_EVERY {
                            unacked = 0;
                            let next = shared
                                .tracker
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .next();
                            if !send_or_close(stream, &Message::Ack { seq: next }, closed) {
                                break;
                            }
                        }
                    }
                    Accept::Duplicate => {}
                    Accept::Gap { .. } => {
                        // Bytes are missing: drop the link. The next attach
                        // resumes at `position()` and is repainted if need be.
                        break;
                    }
                }
            }
            Message::Resync {
                seq,
                cols,
                rows,
                repaint,
            } => {
                apply_size(emulator, shared, cols, rows);
                emulator.feed(&repaint);
                shared
                    .tracker
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .reset(seq);
                shared.resyncs.fetch_add(1, Ordering::SeqCst);
                unacked = 0;
                if !send_or_close(stream, &Message::Ack { seq }, closed) {
                    break;
                }
            }
            Message::Size { cols, rows } => apply_size(emulator, shared, cols, rows),
            Message::Chat(history) => {
                chat.confirmed.store(true, Ordering::SeqCst);
                chat.history.store(Some(Arc::new(history)));
            }
            Message::Status(status) => {
                shared.status.store(Arc::new(match status {
                    RemoteStatus::Running => SessionStatus::Running,
                    RemoteStatus::Exited { code, signal } => SessionStatus::Exited { code, signal },
                    RemoteStatus::Unknown => SessionStatus::Unknown {
                        reason: "the host lost track of the process".to_owned(),
                    },
                }));
            }
            Message::Denied(reason) => {
                denied = true;
                // A refusal may mean the terminal was replaced, so the next attach
                // starts from the start, not from the old position.
                shared.attached_once.store(false, Ordering::SeqCst);
                shared.denied.store(Arc::new(Some(reason)));
            }
            _ => {}
        }
    }
    closed.store(true, Ordering::SeqCst);
    if current() && matches!(**shared.status.load(), SessionStatus::Running) {
        shared.status.store(Arc::new(SessionStatus::Unknown {
            reason: "connection lost".to_owned(),
        }));
    }
}

/// What the writer sends, in the order it was queued.
#[derive(Debug)]
enum Outgoing {
    Input(Vec<u8>),
    /// The view's size: asked by the view, by a new link, or held back by a
    /// new link and released by the input queued after it (see
    /// `RemoteSession::wish_held`). On the input's queue, so no keystroke
    /// overtakes it and no older size overtakes a newer one.
    Resize(GridSize),
}

fn send_resize(stream: &dyn Stream, size: GridSize, closed: &AtomicBool) -> bool {
    let resize = Message::Resize {
        cols: size.cols,
        rows: size.rows,
    };
    send_or_close(stream, &resize, closed)
}

fn send_typed(stream: &dyn Stream, bytes: &[u8], closed: &AtomicBool) -> bool {
    bytes
        .chunks(MAX_DATA)
        .all(|chunk| send_or_close(stream, &Message::Input(chunk.to_vec()), closed))
}

fn write_loop(stream: &dyn Stream, input: &Receiver<Outgoing>, closed: &AtomicBool) {
    loop {
        if closed.load(Ordering::SeqCst) {
            return;
        }
        match input.recv_timeout(Duration::from_millis(20)) {
            Ok(Outgoing::Resize(size)) => {
                if !send_resize(stream, size, closed) {
                    return;
                }
            }
            Ok(Outgoing::Input(mut bytes)) => {
                // Whatever else is queued goes in the same message, but the
                // first bytes never wait for more. A resize queued among them
                // goes out between them, where it was queued.
                let mut then = None;
                while bytes.len() < MAX_DATA {
                    match input.try_recv() {
                        Ok(Outgoing::Input(more)) => bytes.extend_from_slice(&more),
                        Ok(Outgoing::Resize(size)) => {
                            then = Some(size);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if !send_typed(stream, &bytes, closed) {
                    return;
                }
                if let Some(size) = then
                    && !send_resize(stream, size, closed)
                {
                    return;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                let _ = stream.send(&Message::Bye);
                return;
            }
        }
    }
}

/// A host's terminal, as a local terminal session. It outlives connections:
/// the screen stays, and [`RemoteSession::attach`] joins it to a new link,
/// resuming at the last byte it applied.
pub struct RemoteSession {
    addr: TerminalAddr,
    emulator: Arc<Emulator>,
    shared: Arc<Shared>,
    link: Mutex<Option<Link>>,
    /// The last size the local view asked for, asked again on a new link. None
    /// until a view showed the terminal, so a background attach leaves the
    /// host's size alone.
    wish: Mutex<Option<GridSize>>,
    /// When the person last used this terminal here: a resize from its view,
    /// input or a scroll. Wall-clock time, since a sleeping Mac stops
    /// `Instant`, and sleep is exactly the gap to measure.
    used_at: Mutex<Option<SystemTime>>,
    /// A new link left the wish unsent, as nobody had used the terminal
    /// lately. The next use sends it.
    wish_held: AtomicBool,
    clock: Mutex<Clock>,
}

type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// How recently the terminal must have been used here for a new link to ask
/// for its view's size again. A paired Mac that sleeps reconnects on every dark
/// wake; asking each time resized the host's terminal back and forth all night,
/// and every resize made its program repaint, piling copies into the
/// scrollback. A network blip while someone works stays well inside this.
pub const VIEW_IN_USE: Duration = Duration::from_secs(10 * 60);

/// The size a new link asks for: the view's, when it was used within
/// [`VIEW_IN_USE`] of `now`. A clock that went back counts as recent.
fn wish_for_new_link(
    wish: Option<GridSize>,
    used_at: Option<SystemTime>,
    now: SystemTime,
) -> Option<GridSize> {
    let used_at = used_at?;
    let recent = now
        .duration_since(used_at)
        .map_or(true, |gap| gap <= VIEW_IN_USE);
    wish.filter(|_| recent)
}

impl RemoteSession {
    /// A session with no link yet. Its status is unknown until it attaches.
    #[must_use]
    pub fn new(addr: TerminalAddr, size: GridSize, wake: Arc<dyn Fn() + Send + Sync>) -> Arc<Self> {
        let emulator = Arc::new(Emulator::new(size, Arc::new(|_| {}), wake));
        let shared = Arc::new(Shared {
            tracker: Mutex::new(Tracker::default()),
            status: ArcSwap::from_pointee(SessionStatus::Unknown {
                reason: "connecting".to_owned(),
            }),
            denied: ArcSwap::from_pointee(None),
            size: Mutex::new(size),
            generation: AtomicU64::new(0),
            resyncs: AtomicU64::new(0),
            attached_once: AtomicBool::new(false),
        });
        Arc::new(Self {
            addr,
            emulator,
            shared,
            link: Mutex::new(None),
            wish: Mutex::new(None),
            used_at: Mutex::new(None),
            wish_held: AtomicBool::new(false),
            clock: Mutex::new(Arc::new(SystemTime::now)),
        })
    }

    /// Join this session to `client`'s connection and start following the
    /// host's terminal `terminal`. After the first time it resumes at the last
    /// applied byte; the host repaints only if it no longer holds that point.
    ///
    /// # Errors
    /// When the host refuses or the stream fails.
    pub fn attach(&self, client: &RemoteClient, terminal: u32) -> Result<(), ClientError> {
        let resume = self
            .shared
            .attached_once
            .load(Ordering::SeqCst)
            .then(|| self.position())
            .filter(|p| *p > 0);
        let size = self.host_size();
        let stream: Arc<dyn Stream> = Arc::from(client.conn.open_stream()?);
        stream.send(&Message::Attach {
            terminal,
            resume_seq: resume.unwrap_or(0),
            cols: size.cols,
            rows: size.rows,
        })?;
        let (cols, rows) = match stream.recv(REPLY_TIMEOUT)? {
            Some(Message::Attached { cols, rows, .. }) => (cols, rows),
            Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
            _ => return Err(ClientError::Protocol),
        };
        apply_size(&self.emulator, &self.shared, cols, rows);
        let generation = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared.attached_once.store(true, Ordering::SeqCst);
        self.shared.denied.store(Arc::new(None));
        self.shared.status.store(Arc::new(SessionStatus::Running));
        let closed = Arc::new(AtomicBool::new(false));
        let chat = Arc::new(LinkChat {
            confirmed: Arc::clone(&client.chat_confirmed),
            ..LinkChat::default()
        });
        let (input_tx, input_rx) = bounded(INPUT_QUEUE);
        {
            let (stream, emulator, shared, closed, chat) = (
                Arc::clone(&stream),
                Arc::clone(&self.emulator),
                Arc::clone(&self.shared),
                Arc::clone(&closed),
                Arc::clone(&chat),
            );
            thread::Builder::new()
                .name("remote-reader".into())
                .spawn(move || read_loop(&*stream, &emulator, &shared, generation, &closed, &chat))
                .map_err(|e| NetError::Transport(e.to_string()))?;
        }
        {
            let closed = Arc::clone(&closed);
            let stream = Arc::clone(&stream);
            thread::Builder::new()
                .name("remote-writer".into())
                .spawn(move || write_loop(&*stream, &input_rx, &closed))
                .map_err(|e| NetError::Transport(e.to_string()))?;
        }
        let mut link = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        // Decided under the link's lock, as `used` and `resize` record a use:
        // a use made while this attach ran is seen here, or finds this link
        // published and the wish held. Nothing else can send on the new
        // queue yet, so the size, when asked, goes first and fits.
        let wish = *self.wish.lock().unwrap_or_else(PoisonError::into_inner);
        let used_at = *self.used_at.lock().unwrap_or_else(PoisonError::into_inner);
        match wish_for_new_link(wish, used_at, self.now()) {
            // The view's size, which the host takes from this new link only
            // when it is asked.
            Some(size) => {
                let _ = input_tx.try_send(Outgoing::Resize(size));
                self.wish_held.store(false, Ordering::SeqCst);
            }
            // Nobody is using it here: the host keeps its size until someone
            // is. Someone only watching for longer than VIEW_IN_USE loses
            // their size on a brief disconnect, until they click, type or
            // scroll: watching leaves no trace to tell it from a Mac asleep.
            None => self.wish_held.store(wish.is_some(), Ordering::SeqCst),
        }
        // The supervisor may have reattached before the UI next polls. Only
        // accumulate unanswered requests on the same host connection.
        let same_connection = link
            .as_ref()
            .filter(|link| Arc::ptr_eq(&link.chat.confirmed, &chat.confirmed));
        let previous_unanswered = same_connection.map_or(0, Link::unanswered_chat_attempts);
        let previous_history = same_connection.and_then(Link::cached_chat_history);
        let replaced = link.replace(Link {
            connection: Arc::clone(&client.conn),
            stream,
            input: input_tx,
            closed,
            chat,
            previous_unanswered,
            previous_history,
            generation,
        });
        // The previous link's reader and writer end: a re-attachment replaces
        // the stream they serve, so nothing of it is kept.
        if let Some(previous) = replaced {
            previous.closed.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Which terminal on which host.
    #[must_use]
    pub const fn addr(&self) -> &TerminalAddr {
        &self.addr
    }

    /// The next output byte this session wants.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.shared
            .tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next()
    }

    /// How many repaints this session has taken. A resume adds none.
    #[must_use]
    pub fn resyncs(&self) -> u64 {
        self.shared.resyncs.load(Ordering::SeqCst)
    }

    /// The host's grid, which the local view must match to draw it right.
    #[must_use]
    pub fn host_size(&self) -> GridSize {
        *self
            .shared
            .size
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace the wall clock that decides whether the terminal is in use
    /// here (see [`VIEW_IN_USE`]).
    pub fn set_clock(&self, clock: Arc<dyn Fn() -> SystemTime + Send + Sync>) {
        *self.clock.lock().unwrap_or_else(PoisonError::into_inner) = clock;
    }

    fn now(&self) -> SystemTime {
        let clock = Arc::clone(&self.clock.lock().unwrap_or_else(PoisonError::into_inner));
        clock()
    }

    /// The person used the terminal here (a scroll).
    fn used(&self) {
        let guard = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        let link = guard.as_ref().filter(|l| !l.closed.load(Ordering::SeqCst));
        self.used_on(link);
    }

    /// The person used the terminal here, on `link` (the live one, if any),
    /// whose lock the caller holds: the use, the held wish and the link's
    /// publication in `attach` are one order. A wish a new link held back is
    /// queued on the link's input queue now, so the terminal takes this
    /// view's size again before any input queued after it.
    fn used_on(&self, link: Option<&Link>) {
        *self.used_at.lock().unwrap_or_else(PoisonError::into_inner) = Some(self.now());
        if !self.wish_held.load(Ordering::SeqCst) {
            return;
        }
        let Some(size) = *self.wish.lock().unwrap_or_else(PoisonError::into_inner) else {
            return;
        };
        if link.is_some_and(|link| link.input.try_send(Outgoing::Resize(size)).is_ok()) {
            self.wish_held.store(false, Ordering::SeqCst);
        }
    }

    /// The last refusal the host sent, such as Pause.
    #[must_use]
    pub fn last_denied(&self) -> Option<DenyReason> {
        **self.shared.denied.load()
    }

    /// Whether a link is carrying this session now.
    #[must_use]
    pub fn is_linked(&self) -> bool {
        self.link
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|link| !link.closed.load(Ordering::SeqCst))
    }

    /// Whether the link has ended (or there never was one).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        !self.is_linked()
    }

    fn queue_input(&self, bytes: Vec<u8>, require_chat: bool) -> Result<(), InputRefused> {
        self.queue_input_on(bytes, require_chat, None)
    }

    /// Queues input on the current link, if it is the one made by attach
    /// `generation` when one is named: checked under the link's lock, so a
    /// reattachment cannot slip in between.
    fn queue_input_on(
        &self,
        bytes: Vec<u8>,
        require_chat: bool,
        generation: Option<u64>,
    ) -> Result<(), InputRefused> {
        let guard = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(link) = guard.as_ref().filter(|l| !l.closed.load(Ordering::SeqCst)) else {
            return Err(InputRefused::Closed);
        };
        if generation.is_some_and(|generation| generation != link.generation) {
            return Err(InputRefused::Closed);
        }
        if require_chat && link.chat.history.load().is_none() {
            return Err(InputRefused::Closed);
        }
        // Only input this link takes is a use. Queued before the input, so a
        // size held back reaches the host first.
        self.used_on(Some(link));
        self.shared.denied.store(Arc::new(None));
        link.input
            .try_send(Outgoing::Input(bytes))
            .map_err(|err| match err {
                TrySendError::Full(_) => InputRefused::QueueFull,
                TrySendError::Disconnected(_) => InputRefused::Closed,
            })
    }
}

impl TerminalSession for RemoteSession {
    fn chat_error(&self) -> Option<String> {
        let guard = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        let link = guard.as_ref()?;
        if let Some(reason) = self.last_denied() {
            return Some(reason.to_string());
        }
        // Older hosts close the attachment when they decode an unknown
        // ReadChat. Stream loss alone cannot identify the host's version.
        if link.unanswered_chat_attempts() >= 2 {
            return Some(
                "The chat connection keeps closing before history arrives. If the host is running an older build, update Connecto on the host to a build that supports project chat."
                    .to_owned(),
            );
        }
        None
    }

    fn chat_history(&self) -> Option<Arc<connecto_core::session::ChatHistory>> {
        let guard = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        let link = guard
            .as_ref()
            .filter(|link| !link.closed.load(Ordering::SeqCst))?;
        let current = link.chat.history.load_full();
        match link.stream.send(&Message::ReadChat {
            revision: current.as_ref().map(|c| c.revision),
        }) {
            Ok(()) => link.chat.requested.store(true, Ordering::SeqCst),
            Err(NetError::Closed) => return None,
            Err(_) => {}
        }
        current
    }

    fn cached_chat_history(&self) -> Option<Arc<connecto_core::session::ChatHistory>> {
        self.link
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(Link::cached_chat_history)
    }
    fn attachment_generation(&self) -> u64 {
        self.link
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map_or(0, |link| link.generation)
    }

    fn send_input_on(&self, bytes: Vec<u8>, attachment: u64) -> Result<(), InputRefused> {
        self.queue_input_on(bytes, false, Some(attachment))
    }

    fn chat_attachments_supported(&self) -> bool {
        let (connection, capability) = {
            let guard = self.link.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(link) = guard.as_ref().filter(|link| {
                !link.closed.load(Ordering::SeqCst) && link.chat.history.load().is_some()
            }) else {
                return false;
            };
            (
                Arc::clone(&link.connection),
                Arc::clone(&link.chat.attachments),
            )
        };
        match capability.compare_exchange(
            ATTACHMENT_UNKNOWN,
            ATTACHMENT_PENDING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {
                let terminal = self.addr.terminal;
                let probe_state = Arc::clone(&capability);
                if thread::Builder::new()
                    .name("attachment-probe".into())
                    .spawn(move || {
                        let response = connection.open_stream().and_then(|stream| {
                            stream.send(&Message::AttachmentProbe { terminal })?;
                            stream.recv(REPLY_TIMEOUT)
                        });
                        probe_state.store(
                            match response {
                                Ok(Some(Message::AttachmentReady)) => ATTACHMENT_SUPPORTED,
                                Ok(Some(Message::Denied(
                                    DenyReason::Protocol | DenyReason::NoSuchTerminal,
                                ))) => ATTACHMENT_UNSUPPORTED,
                                _ => ATTACHMENT_UNKNOWN,
                            },
                            Ordering::SeqCst,
                        );
                    })
                    .is_err()
                {
                    capability.store(ATTACHMENT_UNKNOWN, Ordering::SeqCst);
                }
                false
            }
            Err(ATTACHMENT_SUPPORTED) => true,
            Err(_) => false,
        }
    }

    fn upload_chat_attachment(
        &self,
        project: &str,
        token: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<String, String> {
        let connection = {
            let guard = self.link.lock().unwrap_or_else(PoisonError::into_inner);
            let link = guard
                .as_ref()
                .filter(|link| {
                    !link.closed.load(Ordering::SeqCst) && link.chat.history.load().is_some()
                })
                .ok_or("Wait for project chat to reconnect before sending attachments.")?;
            if link.chat.attachments.load(Ordering::SeqCst) != ATTACHMENT_SUPPORTED {
                return Err("This host can't receive files. Update Connecto on the host.".into());
            }
            Arc::clone(&link.connection)
        };
        if bytes.len() > crate::wire::MAX_ATTACHMENT_BYTES {
            return Err("The attachment is larger than the 10 MiB limit.".into());
        }
        const INTERRUPTED: &str = "The attachment transfer was interrupted. Try again.";
        let stream = connection.open_stream().map_err(|_| INTERRUPTED)?;
        stream
            .send(&Message::AttachmentStart {
                terminal: self.addr.terminal,
                project: project.to_owned(),
                token: token.to_owned(),
                name: name.to_owned(),
                bytes: u32::try_from(bytes.len()).map_err(|_| INTERRUPTED)?,
            })
            .map_err(|_| INTERRUPTED)?;
        match stream.recv(REPLY_TIMEOUT) {
            Ok(Some(Message::AttachmentReady)) => {}
            Ok(Some(Message::AttachmentResult(Err(error)))) => return Err(attachment_error(error)),
            _ => return Err(INTERRUPTED.into()),
        }
        send_upload(&*stream, bytes).ok_or(INTERRUPTED)?
    }

    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        self.emulator.poll_snapshot(out)
    }

    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        self.queue_input(bytes, false)
    }

    fn send_chat_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        self.queue_input(bytes, true)
    }

    fn resize(&self, size: GridSize) -> ResizeOutcome {
        // The host's grid decides the local one; this only asks.
        let guard = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(link) = guard.as_ref().filter(|l| !l.closed.load(Ordering::SeqCst)) else {
            return ResizeOutcome::Failed;
        };
        match link.input.try_send(Outgoing::Resize(size)) {
            Ok(()) => {
                *self.wish.lock().unwrap_or_else(PoisonError::into_inner) = Some(size);
                *self.used_at.lock().unwrap_or_else(PoisonError::into_inner) = Some(self.now());
                self.wish_held.store(false, Ordering::SeqCst);
                ResizeOutcome::Applied
            }
            Err(TrySendError::Full(_)) => ResizeOutcome::Retry,
            Err(TrySendError::Disconnected(_)) => ResizeOutcome::Failed,
        }
    }

    fn scroll(&self, lines: i32) -> bool {
        self.used();
        self.emulator.try_scroll(lines)
    }

    fn select(&self, op: SelectOp) -> bool {
        self.emulator.try_select(op)
    }

    fn selection_text(&self) -> SelectionText {
        self.emulator.selection_text()
    }

    fn clean_selection_text(&self) -> SelectionText {
        self.emulator.clean_selection_text()
    }

    fn forwards_mouse(&self) -> bool {
        true
    }

    fn status(&self) -> SessionStatus {
        (**self.shared.status.load()).clone()
    }
}

/// A file the host sends on `stream`: `FileStart`, then exactly its bytes.
fn receive_file(stream: &dyn Stream) -> Result<(String, Vec<u8>), ClientError> {
    let (shown, bytes) = match stream.recv(REPLY_TIMEOUT)? {
        Some(Message::FileStart { path, bytes }) => (path, bytes),
        Some(Message::FileRefused(why)) => return Err(ClientError::Remote(why)),
        Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
        _ => return Err(ClientError::Protocol),
    };
    let len = usize::try_from(bytes).map_err(|_| ClientError::Protocol)?;
    let mut data = Vec::with_capacity(len);
    let deadline = Instant::now() + crate::attachments::TRANSFER_TIMEOUT;
    while data.len() < len {
        if Instant::now() >= deadline {
            return Err(ClientError::Net(NetError::Closed));
        }
        match stream.recv(crate::attachments::IDLE_TIMEOUT)? {
            Some(Message::FileChunk(chunk))
                if !chunk.is_empty() && chunk.len() <= len - data.len() =>
            {
                data.extend_from_slice(&chunk);
            }
            Some(Message::Denied(reason)) => return Err(ClientError::Denied(reason)),
            _ => return Err(ClientError::Protocol),
        }
    }
    Ok((shown, data))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod send_tests {
    use super::*;

    /// A host that reads `chunks` chunks, one every `pace`, then answers
    /// with a path if `answer`, or stays silent (open) for `silent`.
    fn host_reading(
        host: crate::transport::ChannelStream,
        chunks: usize,
        pace: Duration,
        answer: bool,
        silent: Duration,
    ) -> std::thread::JoinHandle<usize> {
        std::thread::spawn(move || {
            let mut got = 0;
            while got < chunks {
                std::thread::sleep(pace);
                match host.recv(Duration::from_secs(5)) {
                    Ok(Some(Message::AttachmentChunk(_))) => got += 1,
                    _ => break,
                }
            }
            if answer {
                let saved = Message::AttachmentResult(Ok("/host/saved.bin".into()));
                host.send(&saved).unwrap();
            }
            std::thread::sleep(silent);
            got
        })
    }

    /// Every chunk is queued at once, and the host drains them slowly: the
    /// client still waits for its answer, within the transfer's budget,
    /// however long after the last chunk was queued it comes.
    #[test]
    fn a_slowly_drained_upload_still_waits_for_the_hosts_answer() {
        let (client, host) = crate::transport::ChannelStream::pair();
        let chunks = 40;
        let host = host_reading(
            host,
            chunks,
            Duration::from_millis(25),
            true,
            Duration::ZERO,
        );
        let bytes = vec![7; chunks * MAX_DATA];
        let started = Instant::now();
        let deadline = started + Duration::from_secs(10);
        let outcome = send_upload_until(&client, &bytes, deadline);
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "the host drained the queue for about a second"
        );
        assert_eq!(outcome, Some(Ok("/host/saved.bin".into())));
        assert_eq!(host.join().unwrap(), chunks);
    }

    /// A host that takes every chunk and then never answers: the upload
    /// gives up at the transfer's deadline.
    #[test]
    fn an_upload_whose_answer_never_comes_gives_up_at_its_deadline() {
        let (client, host) = crate::transport::ChannelStream::pair();
        let host = host_reading(host, 3, Duration::ZERO, false, Duration::from_secs(3));
        let started = Instant::now();
        let deadline = started + Duration::from_millis(500);
        let outcome = send_upload_until(&client, &vec![7; 3 * MAX_DATA], deadline);
        assert_eq!(outcome, None);
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(450) && waited < Duration::from_secs(2),
            "{waited:?}"
        );
        assert_eq!(host.join().unwrap(), 3);
    }

    #[test]
    fn a_new_link_asks_for_the_view_size_only_while_the_terminal_is_in_use() {
        let size = Some(GridSize { cols: 80, rows: 24 });
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ago = |secs| Some(now - Duration::from_secs(secs));
        assert_eq!(wish_for_new_link(size, ago(30), now), size, "a blip");
        assert_eq!(wish_for_new_link(size, ago(600), now), size);
        assert_eq!(wish_for_new_link(size, ago(601), now), None, "slept");
        assert_eq!(wish_for_new_link(size, None, now), None, "never used");
        assert_eq!(wish_for_new_link(None, ago(1), now), None, "never shown");
        let ahead = Some(now + Duration::from_secs(60));
        assert_eq!(wish_for_new_link(size, ahead, now), size, "clock went back");
    }

    /// A stream whose sends fail as `full` says: `Full` for the first sends, or
    /// `Closed` for all of them. What is sent is recorded, in order.
    struct Scripted {
        full: Mutex<usize>,
        closed: bool,
        sent: Mutex<Vec<Message>>,
    }

    impl Stream for Scripted {
        fn send(&self, message: &Message) -> Result<(), NetError> {
            if self.closed {
                return Err(NetError::Closed);
            }
            let mut full = self.full.lock().unwrap();
            if *full > 0 {
                *full -= 1;
                return Err(NetError::Full);
            }
            self.sent.lock().unwrap().push(message.clone());
            Ok(())
        }
        fn recv(&self, _timeout: Duration) -> Result<Option<Message>, NetError> {
            Ok(None)
        }
    }

    #[test]
    fn a_full_queue_is_retried_and_nothing_is_dropped_or_reordered() {
        let stream = Scripted {
            full: Mutex::new(9),
            closed: false,
            sent: Mutex::new(Vec::new()),
        };
        let (input_tx, input_rx) = bounded(16);
        let closed = AtomicBool::new(false);
        input_tx
            .send(Outgoing::Resize(GridSize { cols: 70, rows: 20 }))
            .unwrap();
        for piece in [&b"one "[..], b"two ", b"three"] {
            input_tx.send(Outgoing::Input(piece.to_vec())).unwrap();
        }
        input_tx
            .send(Outgoing::Resize(GridSize { cols: 80, rows: 24 }))
            .unwrap();
        input_tx.send(Outgoing::Input(b"!".to_vec())).unwrap();
        drop(input_tx);
        write_loop(&stream, &input_rx, &closed);
        let sent = stream.sent.lock().unwrap().clone();
        let mut typed: Vec<u8> = Vec::new();
        for message in &sent {
            if let Message::Input(bytes) = message {
                typed.extend(bytes);
            }
        }
        assert_eq!(typed, b"one two three!".to_vec());
        assert_eq!(sent.first(), Some(&Message::Resize { cols: 70, rows: 20 }));
        // A resize queued among the input goes out where it was queued.
        let at = |wanted: &Message| sent.iter().position(|m| m == wanted).unwrap();
        assert!(
            at(&Message::Resize { cols: 80, rows: 24 })
                > at(&Message::Input(b"one two three".to_vec()))
        );
        assert!(at(&Message::Resize { cols: 80, rows: 24 }) < at(&Message::Input(b"!".to_vec())));
        assert_eq!(sent.last(), Some(&Message::Bye));
        assert!(
            !closed.load(Ordering::SeqCst),
            "a retried send closed the link"
        );
    }

    #[test]
    fn a_genuine_send_failure_marks_the_link_closed_so_it_is_redialed() {
        let stream = Scripted {
            full: Mutex::new(0),
            closed: true,
            sent: Mutex::new(Vec::new()),
        };
        let (input_tx, input_rx) = bounded(4);
        let closed = AtomicBool::new(false);
        input_tx.send(Outgoing::Input(b"lost".to_vec())).unwrap();
        write_loop(&stream, &input_rx, &closed);
        assert!(closed.load(Ordering::SeqCst), "the link still reads as up");
    }

    #[test]
    fn a_host_that_never_reads_is_given_up_on_and_the_link_reads_closed() {
        let stream = Scripted {
            full: Mutex::new(usize::MAX),
            closed: false,
            sent: Mutex::new(Vec::new()),
        };
        let closed = AtomicBool::new(false);
        let patience = Duration::from_millis(30);
        assert!(!send_within(
            &stream,
            &Message::Ack { seq: 1 },
            &closed,
            patience
        ));
        assert!(
            closed.load(Ordering::SeqCst),
            "a host that never reads kept the link up"
        );
    }

    #[test]
    fn a_retry_ends_when_the_link_is_closed_by_someone_else() {
        let stream = Scripted {
            full: Mutex::new(usize::MAX),
            closed: false,
            sent: Mutex::new(Vec::new()),
        };
        let closed = AtomicBool::new(true);
        assert!(!send_or_close(&stream, &Message::Ack { seq: 1 }, &closed));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod idle_reader_tests {
    use super::*;
    use std::sync::mpsc;

    /// A stream that never has a message: an idle peer.
    struct Silent;

    impl Stream for Silent {
        fn send(&self, _message: &Message) -> Result<(), NetError> {
            Ok(())
        }
        fn recv(&self, timeout: Duration) -> Result<Option<Message>, NetError> {
            thread::sleep(timeout);
            Ok(None)
        }
    }

    /// Runs a reader over an idle stream and reports when it returns.
    fn reader_ends(closed_now: bool, generation_moves: bool) -> bool {
        let session = RemoteSession::new(
            TerminalAddr {
                host: "h".into(),
                terminal: 1,
            },
            GridSize { cols: 60, rows: 12 },
            Arc::new(|| {}),
        );
        let generation = session.shared.generation.load(Ordering::SeqCst);
        if generation_moves {
            session.shared.generation.fetch_add(1, Ordering::SeqCst);
        }
        let closed = Arc::new(AtomicBool::new(closed_now));
        let (done_tx, done_rx) = mpsc::channel();
        let (emulator, shared) = (Arc::clone(&session.emulator), Arc::clone(&session.shared));
        let closed_for_reader = Arc::clone(&closed);
        thread::spawn(move || {
            read_loop(
                &Silent,
                &emulator,
                &shared,
                generation,
                &closed_for_reader,
                &LinkChat::default(),
            );
            let _ = done_tx.send(());
        });
        // Closed by the writer (or replaced): the reader must end though the
        // stream never speaks.
        done_rx.recv_timeout(Duration::from_secs(2)).is_ok()
    }

    #[test]
    fn an_idle_reader_ends_when_its_link_is_closed() {
        assert!(
            reader_ends(true, false),
            "an idle reader outlived its closed link"
        );
    }

    #[test]
    fn an_idle_reader_ends_when_its_session_has_a_newer_attachment() {
        assert!(
            reader_ends(false, true),
            "an idle stale reader outlived its attachment"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod chat_tests {
    use super::*;
    use crate::transport::loopback;

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for reader"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// A connection whose streams take `delay` to open.
    struct SlowOpen {
        inner: loopback::LoopbackConnection,
        delay: Duration,
    }

    impl Connection for SlowOpen {
        fn local_id(&self) -> crate::transport::EndpointId {
            self.inner.local_id()
        }
        fn remote_id(&self) -> crate::transport::EndpointId {
            self.inner.remote_id()
        }
        fn channel_binding(&self) -> Result<[u8; 32], NetError> {
            self.inner.channel_binding()
        }
        fn open_stream(&self) -> Result<Box<dyn Stream>, NetError> {
            thread::sleep(self.delay);
            self.inner.open_stream()
        }
        fn open_stream_within(&self, _: Duration) -> Result<Box<dyn Stream>, NetError> {
            self.open_stream()
        }
        fn accept_stream(&self, timeout: Duration) -> Result<Option<Box<dyn Stream>>, NetError> {
            self.inner.accept_stream(timeout)
        }
        fn close(&self) {
            self.inner.close();
        }
    }

    fn client_pair() -> (RemoteClient, loopback::LoopbackConnection, Box<dyn Stream>) {
        let (local, host) = loopback::pair([1; 32], [2; 32], [3; 32]);
        let client = RemoteClient {
            features: AtomicU32::new(0),
            next_req: AtomicU32::new(1),
            host: "host".into(),
            control: Mutex::new(local.open_stream().unwrap()),
            conn: Arc::new(local),
            chat_confirmed: Arc::default(),
            models_confirmed: AtomicBool::new(false),
        };
        let control = host.accept_stream(REPLY_TIMEOUT).unwrap().unwrap();
        (client, host, control)
    }

    fn attach(
        session: &RemoteSession,
        client: &RemoteClient,
        host: &loopback::LoopbackConnection,
    ) -> Box<dyn Stream> {
        thread::scope(|scope| {
            let serving = scope.spawn(|| {
                let stream = host.accept_stream(REPLY_TIMEOUT).unwrap().unwrap();
                assert!(matches!(
                    stream.recv(REPLY_TIMEOUT).unwrap(),
                    Some(Message::Attach { terminal: 1, .. })
                ));
                stream
                    .send(&Message::Attached {
                        start_seq: 0,
                        resync: false,
                        cols: 60,
                        rows: 12,
                    })
                    .unwrap();
                stream
            });
            session.attach(client, 1).unwrap();
            serving.join().unwrap()
        })
    }

    fn request_chat(session: &RemoteSession, stream: &dyn Stream) {
        assert!(session.chat_history().is_none());
        assert_eq!(
            stream.recv(REPLY_TIMEOUT).unwrap(),
            Some(Message::ReadChat { revision: None })
        );
    }

    #[test]
    fn a_completion_is_asked_on_its_own_stream_and_an_older_host_refuses() {
        use crate::wire::{Completion, Role};
        let (client, host, _control) = client_pair();
        let request = Completion {
            messages: vec![(Role::User, "Ciao".into())],
            max_tokens: 10,
            temperature: 0,
            wait_ms: 5000,
        };
        for (reply, expect) in [
            (Message::Denied(DenyReason::Protocol), Err("protocol")),
            (Message::Completed(Err("no model".into())), Err("no model")),
            (Message::Completed(Ok("Ciao!".into())), Ok("Ciao!")),
        ] {
            thread::scope(|scope| {
                let serving = scope.spawn(|| {
                    let asked = host.accept_stream(REPLY_TIMEOUT).unwrap().unwrap();
                    let Some(Message::Complete(got)) = asked.recv(REPLY_TIMEOUT).unwrap() else {
                        unreachable!("a completion");
                    };
                    // The time left, not more.
                    assert!(got.wait_ms <= request.wait_ms && got.wait_ms > 4000);
                    assert_eq!(got.messages, request.messages);
                    asked.send(&reply).unwrap();
                });
                let got = client.complete(&request);
                let right = match (&got, expect) {
                    (Ok(text), Ok(want)) => text == want,
                    (Err(ClientError::Denied(DenyReason::Protocol)), Err("protocol")) => true,
                    (Err(ClientError::Remote(why)), Err(want)) => why == want,
                    _ => false,
                };
                assert!(right, "{got:?}");
                serving.join().unwrap();
            });
        }
        // A stream that opens after the time is up carries nothing.
        let (local, far) = loopback::pair([1; 32], [2; 32], [3; 32]);
        let late = RemoteClient {
            features: AtomicU32::new(0),
            next_req: AtomicU32::new(1),
            host: "host".into(),
            control: Mutex::new(local.open_stream().unwrap()),
            conn: Arc::new(SlowOpen {
                inner: local,
                delay: Duration::from_millis(400),
            }),
            chat_confirmed: Arc::default(),
            models_confirmed: AtomicBool::new(false),
        };
        let _control = far.accept_stream(REPLY_TIMEOUT).unwrap().unwrap();
        let quick = Completion {
            wait_ms: 300,
            ..request.clone()
        };
        assert!(matches!(late.complete(&quick), Err(ClientError::Protocol)));
        let opened = far.accept_stream(REPLY_TIMEOUT).unwrap().unwrap();
        assert!(
            !matches!(opened.recv(Duration::from_millis(200)), Ok(Some(_))),
            "nothing is sent on a stream opened too late"
        );

        // No answer in time.
        thread::scope(|scope| {
            let serving = scope.spawn(|| {
                let asked = host.accept_stream(REPLY_TIMEOUT).unwrap().unwrap();
                assert!(asked.recv(REPLY_TIMEOUT).unwrap().is_some());
                thread::sleep(Duration::from_millis(900));
            });
            let quick = crate::wire::Completion {
                wait_ms: 300,
                ..request.clone()
            };
            assert!(matches!(
                client.complete(&quick),
                Err(ClientError::Protocol)
            ));
            serving.join().unwrap();
        });
    }

    #[test]
    fn models_are_asked_on_their_own_stream_and_an_older_host_has_none() {
        let (client, host, _control) = client_pair();
        thread::scope(|scope| {
            let serving = scope.spawn(|| {
                let probe = host.accept_stream(REPLY_TIMEOUT).unwrap().unwrap();
                assert_eq!(
                    probe.recv(REPLY_TIMEOUT).unwrap(),
                    Some(Message::ModelsProbe {
                        tool: 1,
                        project: "p".into()
                    })
                );
                // What a host from before models says to an unknown tag.
                probe.send(&Message::Denied(DenyReason::Protocol)).unwrap();
            });
            assert!(client.models(1, "p").unwrap().is_none());
            serving.join().unwrap();
        });
        assert!(!client.models_supported());

        let offered = ToolModels {
            default: None,
            models: vec!["opus".into()],
        };
        thread::scope(|scope| {
            let serving = scope.spawn(|| {
                let probe = host.accept_stream(REPLY_TIMEOUT).unwrap().unwrap();
                assert!(probe.recv(REPLY_TIMEOUT).unwrap().is_some());
                probe.send(&Message::Models(offered.clone())).unwrap();
            });
            assert_eq!(client.models(1, "p").unwrap(), Some(offered.clone()));
            serving.join().unwrap();
        });
        assert!(client.models_supported());

        thread::scope(|scope| {
            let serving = scope.spawn(|| {
                let probe = host.accept_stream(REPLY_TIMEOUT).unwrap().unwrap();
                assert!(probe.recv(REPLY_TIMEOUT).unwrap().is_some());
                probe
                    .send(&Message::Denied(DenyReason::NoSuchTerminal))
                    .unwrap();
            });
            assert!(matches!(
                client.models(1, "gone"),
                Err(ClientError::Denied(DenyReason::NoSuchTerminal))
            ));
            serving.join().unwrap();
        });
    }

    #[test]
    fn a_legacy_attachment_probe_leaves_text_chat_linked() {
        let (client, host, _control) = client_pair();
        let session = RemoteSession::new(
            TerminalAddr {
                host: client.host.clone(),
                terminal: 1,
            },
            GridSize { cols: 60, rows: 12 },
            Arc::new(|| {}),
        );
        let stream = attach(&session, &client, &host);
        request_chat(&session, &*stream);
        stream.send(&Message::Chat(Default::default())).unwrap();
        wait_until(|| session.cached_chat_history().is_some());

        assert!(!session.chat_attachments_supported());
        let probe = host.accept_stream(REPLY_TIMEOUT).unwrap().unwrap();
        assert_eq!(
            probe.recv(REPLY_TIMEOUT).unwrap(),
            Some(Message::AttachmentProbe { terminal: 1 })
        );
        probe.send(&Message::Denied(DenyReason::Protocol)).unwrap();
        drop(probe);
        wait_until(|| {
            session
                .link
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .chat
                .attachments
                .load(Ordering::SeqCst)
                == ATTACHMENT_UNSUPPORTED
        });
        assert!(!session.chat_attachments_supported());
        assert!(session.is_linked());
        session.send_chat_input(b"Text works".to_vec()).unwrap();
        assert_eq!(
            stream.recv(REPLY_TIMEOUT).unwrap(),
            Some(Message::Input(b"Text works".to_vec()))
        );
    }

    #[test]
    fn unanswered_chat_requests_survive_reattach_until_history_arrives() {
        let (client, host, _control) = client_pair();
        let session = RemoteSession::new(
            TerminalAddr {
                host: client.host.clone(),
                terminal: 1,
            },
            GridSize { cols: 60, rows: 12 },
            Arc::new(|| {}),
        );
        // A stream closed before any request gives no compatibility evidence.
        let stream = attach(&session, &client, &host);
        stream.send(&Message::Denied(DenyReason::Protocol)).unwrap();
        wait_until(|| session.last_denied().is_some());
        assert_eq!(session.chat_error(), Some(DenyReason::Protocol.to_string()));
        drop(stream);
        wait_until(|| session.is_closed());
        assert!(session.chat_history().is_none());

        for attempt in 1..=2 {
            let stream = attach(&session, &client, &host);
            request_chat(&session, &*stream);
            // Legacy hosts end the attach stream on the unknown tag, without
            // a Denied reply. Their control stream remains connected.
            drop(stream);
            wait_until(|| session.is_closed());
            let problem = session.chat_error();
            if attempt == 1 {
                assert_eq!(problem, None);
            } else {
                assert!(problem.unwrap().contains("update Connecto on the host"));
            }
        }

        let stream = attach(&session, &client, &host);
        assert!(session.chat_error().is_some());
        request_chat(&session, &*stream);
        assert!(session.send_chat_input(b"too soon".to_vec()).is_err());
        stream.send(&Message::Chat(Default::default())).unwrap();
        wait_until(|| session.cached_chat_history().is_some());
        assert_eq!(session.chat_error(), None);
        session.send_chat_input(b"hello".to_vec()).unwrap();
        assert_eq!(
            stream.recv(REPLY_TIMEOUT).unwrap(),
            Some(Message::Input(b"hello".to_vec()))
        );

        drop(stream);
        wait_until(|| session.is_closed());
        assert!(session.cached_chat_history().is_some());
        assert!(session.chat_history().is_none());
        for _ in 0..2 {
            let stream = attach(&session, &client, &host);
            assert!(session.cached_chat_history().is_some());
            assert!(session.send_chat_input(b"too soon".to_vec()).is_err());
            request_chat(&session, &*stream);
            drop(stream);
            wait_until(|| session.is_closed());
            assert_eq!(session.chat_error(), None);
        }
    }

    #[test]
    fn a_new_host_connection_starts_without_prior_unanswered_requests() {
        let (client, host, _control) = client_pair();
        let session = RemoteSession::new(
            TerminalAddr {
                host: client.host.clone(),
                terminal: 1,
            },
            GridSize { cols: 60, rows: 12 },
            Arc::new(|| {}),
        );
        let stream = attach(&session, &client, &host);
        request_chat(&session, &*stream);
        drop(stream);
        wait_until(|| session.is_closed());

        let (client, host, _control) = client_pair();
        let stream = attach(&session, &client, &host);
        request_chat(&session, &*stream);
        drop(stream);
        wait_until(|| session.is_closed());
        assert_eq!(session.chat_error(), None);
    }
}
