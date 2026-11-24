//! Wire messages and their length-prefixed binary framing.
//!
//! A frame is `u32` big-endian length, then one tag byte, then the fields. The
//! length counts the tag and the fields. Nothing on the hot path is text: an
//! output chunk costs a 4-byte length, a tag, an 8-byte sequence number and a
//! 4-byte byte count on top of the bytes themselves.
//!
//! Terminals are addressed as [`TerminalAddr`], `(host, terminal)`. On the wire
//! a host only ever names its own terminals (`host` is empty); the client fills
//! in the display name it knows the machine by, so the same types serve a UI
//! that lists terminals of several hosts.

use std::fmt;

/// Bumped when a message changes shape.
pub const PROTOCOL_VERSION: u16 = 2;

/// The most bytes in one frame body (tag and fields). A longer length prefix is
/// refused before anything is buffered.
pub const MAX_FRAME: usize = 256 * 1024;

/// The most terminal bytes in one `Output` or `Input` message.
pub const MAX_DATA: usize = 64 * 1024;

/// Attachment limits apply to one chat message. Held in memory on both
/// ends, and sent from phones: they stay small.
pub const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_ATTACHMENTS: usize = connecto_core::terminals::MAX_UPLOAD_FILES;
/// The largest file dropped or pasted on a host's terminal
/// ([`Message::TerminalUpload`]), on a host that granted
/// [`FEATURE_LARGE_UPLOADS`].
pub const MAX_UPLOAD_BYTES: usize = connecto_core::terminals::MAX_UPLOAD_BYTES;
/// The largest such file a host that did not grant [`FEATURE_LARGE_UPLOADS`]
/// takes: what every host took before it.
pub const LEGACY_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
/// The largest file a host sends a peer to preview or download.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// The longest path a peer may ask a host's file by.
pub const MAX_PATH: usize = 4096;

/// The most terminals one list carries.
pub const MAX_LISTED: usize = 64;

/// The most projects one list carries. A host offers its first this-many open
/// projects, in its order; the rest are not listed, and cannot be named.
pub const MAX_PROJECTS: usize = 64;

/// The longest name or reason string.
pub const MAX_STR: usize = 256;

/// The most messages in one [`Completion`] request, the longest one, and the
/// most text in all of them together; and the longest answer.
pub const MAX_COMPLETION_MESSAGES: usize = 40;
pub const MAX_COMPLETION_TEXT: usize = 64 * 1024;
pub const MAX_COMPLETION_TOTAL: usize = 192 * 1024;

/// The longest transcript, and the longest context, a [`Message::TurnCheck`]
/// carries. A longer transcript is not checked, and a context is made of
/// whole pieces that fit: neither is ever cut, as both are redacted only on
/// the host and a cut could split a secret.
pub const MAX_TURN_TEXT: usize = 4 * 1024;
pub const MAX_TURN_CONTEXT: usize = 4 * 1024;

/// What Jev made of a turn, each a probability (0.0..=1.0).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnScores {
    /// That the person has finished what they wanted to say.
    pub complete: f64,
    /// That it is meant for the call and makes sense in it; asked only with
    /// a context.
    pub fits: Option<f64>,
}

/// Why a host without Jev answers no turn check.
pub const NO_TURN_JUDGE: &str = "no Jev key is set up on that machine";

/// A host's answer to [`Message::TurnCheck`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnVerdict {
    /// How likely the person has finished, and how likely what they said
    /// fits the call (asked only with a context), in percent (0..=100).
    Judged { complete: u8, fits: Option<u8> },
    /// The host cannot judge (no Jev key set up): asking again this call
    /// gets the same.
    Unavailable(String),
    /// This one failed (Jev too slow or unreachable).
    Failed(String),
}

/// Who said a [`Completion`] message, as an OpenAI-style chat names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "system" => Some(Self::System),
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::System => 0,
            Self::User => 1,
            Self::Assistant => 2,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::System),
            1 => Some(Self::User),
            2 => Some(Self::Assistant),
            _ => None,
        }
    }
}

/// A chat completion a peer asks of the host's conversation model (a local
/// OpenAI-compatible server the host talks to; peers never reach it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub messages: Vec<(Role, String)>,
    /// The most tokens to answer with.
    pub max_tokens: u16,
    /// Sampling temperature, in hundredths (70 is 0.7).
    pub temperature: u8,
    /// How long the peer waits for the answer, in ms: the host gives the
    /// model no longer (and never more than its own limit), so an answer
    /// nobody waits for does not hold the model.
    pub wait_ms: u32,
}

/// The most background tasks one terminal lists.
pub const MAX_BACKGROUND: usize = 6;

/// The longest text of a background task (bytes), so a full list of busy
/// terminals still fits in one frame.
pub const MAX_BACKGROUND_TEXT: usize = 160;

/// Features a peer asks for with [`Message::Features`], as bits.
///
/// [`FEATURE_BACKGROUND`]: lists come as [`Message::TerminalListEx`], with each
/// terminal's background work, and [`ManageOp::StopBackground`] is understood.
pub const FEATURE_BACKGROUND: u32 = 1;

/// [`FEATURE_TERMINAL_IDS`], granted with [`FEATURE_BACKGROUND`]: lists come
/// as [`Message::TerminalListIds`], an extended list with each terminal's own
/// id on the host (`claude-12`), which the peer shows as the host does.
pub const FEATURE_TERMINAL_IDS: u32 = 2;

/// [`FEATURE_OPENCODE`]: the host knows OpenCode's tool byte (6). A host
/// from before OpenCode reads that byte as Claude, so OpenCode's models are
/// never asked of it and no OpenCode terminal is added on it. Granted alone,
/// whatever else is.
pub const FEATURE_OPENCODE: u32 = 4;

/// [`FEATURE_TERMINAL_OWNERS`], granted with [`FEATURE_TERMINAL_IDS`]: lists
/// come as [`Message::TerminalListOwners`], the list with ids that also
/// names, for each terminal, the one it belongs to ([`TerminalInfo::owner`]).
pub const FEATURE_TERMINAL_OWNERS: u32 = 8;

/// [`FEATURE_LARGE_UPLOADS`]: the host takes a [`Message::TerminalUpload`]
/// of up to [`MAX_UPLOAD_BYTES`] on this connection. A connection it was not
/// granted to (not asked, or a host from before it) takes
/// [`LEGACY_UPLOAD_BYTES`]: the host refuses more, and the client refuses a
/// larger file before it is sent. Granted alone, whatever else is.
pub const FEATURE_LARGE_UPLOADS: u32 = 16;

/// The longest key or signature blob.
pub const MAX_BLOB: usize = 8 * 1024;

/// The largest grid either side may ask for.
pub const MAX_DIM: u16 = 1000;

/// The display name that always means the machine the app runs on.
pub use connecto_core::LOCALHOST;

/// Why a frame could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("frame length {0} is not allowed")]
    BadLength(usize),
    #[error("unknown message tag {0}")]
    UnknownTag(u8),
    #[error("message ended early")]
    Truncated,
    #[error("message has {0} bytes left over")]
    Trailing(usize),
    #[error("field is too long")]
    TooLong,
    #[error("text is not valid")]
    BadText,
    #[error("value is out of range")]
    BadValue,
}

/// A terminal on a host: the host's display name and the host's own id for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalAddr {
    /// [`LOCALHOST`] for this machine, else the saved display name.
    pub host: String,
    pub terminal: u32,
}

impl TerminalAddr {
    #[must_use]
    pub fn local(terminal: u32) -> Self {
        Self {
            host: LOCALHOST.to_owned(),
            terminal,
        }
    }

    #[must_use]
    pub fn is_local(&self) -> bool {
        self.host == LOCALHOST
    }
}

impl fmt::Display for TerminalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.terminal)
    }
}

/// Why a peer was refused. The text a peer sees is this code only, so a probe
/// learns nothing about which key lists exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The key is not allowed, or the signature does not verify.
    NotAuthorized,
    /// A message arrived out of order.
    Protocol,
    /// The peer speaks another protocol version.
    Version,
    /// Another remote peer is connected already.
    Busy,
    /// The host does not accept remote peers now.
    Disabled,
    /// The action is refused while Pause is on.
    Paused,
    /// The terminal does not exist.
    NoSuchTerminal,
}

impl DenyReason {
    const fn code(self) -> u8 {
        match self {
            Self::NotAuthorized => 1,
            Self::Protocol => 2,
            Self::Version => 3,
            Self::Busy => 4,
            Self::Disabled => 5,
            Self::Paused => 6,
            Self::NoSuchTerminal => 7,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::NotAuthorized,
            2 => Self::Protocol,
            3 => Self::Version,
            4 => Self::Busy,
            5 => Self::Disabled,
            6 => Self::Paused,
            7 => Self::NoSuchTerminal,
            _ => return None,
        })
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotAuthorized => "not authorized",
            Self::Protocol => "protocol error",
            Self::Version => "the two machines run different Connecto versions: update both to the same Connecto version",
            Self::Busy => "that machine is busy: try again in a moment",
            Self::Disabled => "remote access is off on that machine",
            Self::Paused => "paused on that machine",
            Self::NoSuchTerminal => "no such terminal",
        })
    }
}

/// What the host says about one terminal in a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalInfo {
    pub id: u32,
    /// The terminal's own name only, without its project.
    pub name: String,
    /// 0 unknown, 1 Claude, 2 Codex, 3 shell.
    pub tool: u8,
    pub running: bool,
    /// The host's status dot, as `TermActivity` in declaration order.
    pub activity: u8,
    pub cols: u16,
    pub rows: u16,
    /// The host's project id for the terminal's folder, stable on that host.
    pub project_id: String,
    /// The project's name on the host.
    pub project_name: String,
    /// The wire id of the terminal that started this one, when the host lists
    /// that terminal in the same list. Always a top-level terminal.
    pub parent: Option<u32>,
    /// The work its agent left running at the prompt, oldest first. Sent only
    /// in [`Message::TerminalListEx`]; empty otherwise.
    pub background: Vec<BackgroundInfo>,
    /// The terminal's id on the host (`claude-12`), the one its agents and
    /// sidebar use. Sent only in [`Message::TerminalListIds`]; `None` otherwise.
    pub entry: Option<String>,
    /// The wire id of the terminal it belongs to, when the host lists that
    /// terminal in the same list: the agent that asked for it, which can be
    /// a child filed under `parent`. Sent only in
    /// [`Message::TerminalListOwners`]; `None` otherwise (then `parent`).
    pub owner: Option<u32>,
}

/// One task a terminal's agent left running, as a list carries it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BackgroundInfo {
    /// The tool's own task id, which a stop names.
    pub id: String,
    /// `shell`, `subagent`, `monitor`, `workflow`, ...
    pub kind: String,
    /// What it runs.
    pub label: String,
    /// What the agent said it is for, when it said so.
    pub description: Option<String>,
    /// How long it has run, in seconds, when the list was made.
    pub seconds: u64,
}

/// A project the host has open and set up, as it names itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInfo {
    /// The host's id for the folder: canonical, see the receiver's checks.
    pub id: String,
    /// The name the host shows, as sent.
    pub name: String,
}

/// A file the host offers to download (`connecto send-file` on the host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferInfo {
    pub id: u64,
    /// The project of the terminal that offered it, from the connection's
    /// latest list.
    pub project: String,
    /// The terminal that offered it, as the host names it (`claude-3`).
    pub terminal: String,
    /// The file's name, to save it by.
    pub name: String,
    pub bytes: u64,
    /// How long ago it was offered.
    pub age_secs: u32,
}

/// The most offers one [`Message::Offers`] carries.
pub const MAX_OFFERS: usize = 32;

/// What a host sends for a list: its projects, in its order, and the terminals
/// of those projects.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
    pub projects: Vec<ProjectInfo>,
    pub terminals: Vec<TerminalInfo>,
}

/// A change to the host's terminals that a peer asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManageOp {
    /// A new terminal of `tool` (see `hosting::tool_byte`). `project` is a
    /// project id from the peer's latest list; `None` means the host's active
    /// project. It appears in the list once it has started. `model` is the
    /// model to launch it on (`None`: the tool's default). It goes on the wire
    /// only when set, after the other fields, so an `Add` without one is the
    /// same frame a host from before models reads; a peer sends one only to a
    /// host that answered [`Message::ModelsProbe`].
    Add {
        tool: u8,
        name: Option<String>,
        project: Option<String>,
        model: Option<String>,
    },
    Close {
        terminal: u32,
    },
    Restart {
        terminal: u32,
    },
    /// `None` clears the name.
    Rename {
        terminal: u32,
        name: Option<String>,
    },
    /// Stop the background shells of a terminal's agent, or only the task
    /// `task`. Sent only to a host that granted [`FEATURE_BACKGROUND`].
    StopBackground {
        terminal: u32,
        task: Option<String>,
    },
}

/// The most models one [`Message::Models`] carries.
pub const MAX_MODELS: usize = 512;

/// The models a host offers for one tool in one project, for a peer's chooser.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolModels {
    /// The model a new terminal of the tool starts on when none is chosen, when
    /// the host knows it. `None` leaves it to the tool.
    pub default: Option<String>,
    /// The models to offer, in the host's order. Empty: only the default.
    pub models: Vec<String>,
}

/// How a terminal's process stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteStatus {
    Running,
    Exited { code: u32, signal: Option<String> },
    Unknown,
}

/// An encoded [`crate::project_panels::ProjectAsk`]. It may hold a
/// secret's value, so it never prints: its debug form is its length.
#[derive(Clone, PartialEq, Eq)]
pub struct AskBytes(pub Vec<u8>);

impl std::fmt::Debug for AskBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AskBytes({} bytes)", self.0.len())
    }
}

/// Every message either side sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    // Control stream, before and during auth.
    Hello {
        version: u16,
    },
    Challenge {
        nonce: [u8; 32],
    },
    AuthResponse {
        /// The key in OpenSSH wire form.
        public_key: Vec<u8>,
        /// An SSHSIG, armored.
        signature: Vec<u8>,
    },
    AuthOk,
    Denied(DenyReason),
    /// One step of the pairing protocol (see `pairing`). Opaque here.
    Pair(Vec<u8>),
    // Control stream, after auth.
    ListTerminals,
    TerminalList(Listing),
    /// A list with each terminal's background work: sent instead of
    /// `TerminalList` once the connection was granted [`FEATURE_BACKGROUND`].
    TerminalListEx(Listing),
    /// An extended list with each terminal's id on the host: sent instead of
    /// `TerminalListEx` once the connection was granted [`FEATURE_TERMINAL_IDS`].
    TerminalListIds(Listing),
    /// The list with ids that also names each terminal's owner: sent instead
    /// of `TerminalListIds` once the connection was granted
    /// [`FEATURE_TERMINAL_OWNERS`].
    TerminalListOwners(Listing),
    Bye,
    /// A request to change the host's terminals; answered by `ManageResult`
    /// with the same `req`.
    Manage {
        req: u32,
        op: ManageOp,
    },
    ManageResult {
        req: u32,
        /// The new terminal's id when it is known, or why it was refused.
        outcome: Result<Option<u32>, String>,
    },
    // Terminal stream, client to host.
    Attach {
        terminal: u32,
        /// The next byte the client wants; 0 for a first attach.
        resume_seq: u64,
        cols: u16,
        rows: u16,
    },
    /// Optional extension requested only by a chat client on an attached stream.
    ReadChat {
        revision: Option<u64>,
    },
    Chat(connecto_core::session::ChatHistory),
    /// These messages are exchanged on an independent authenticated stream.
    AttachmentProbe {
        terminal: u32,
    },
    AttachmentReady,
    AttachmentStart {
        terminal: u32,
        project: String,
        token: String,
        name: String,
        bytes: u32,
    },
    AttachmentChunk(Vec<u8>),
    AttachmentResult(Result<String, String>),
    /// A file dropped or pasted on terminal `terminal`, asked on a stream of
    /// its own: the host saves it in that terminal's project's attachments
    /// folder. Answered as `AttachmentStart` is (`AttachmentReady`, the
    /// `AttachmentChunk`s, then `AttachmentResult` with the host's path), or
    /// `Denied`. A host from before it answers `Denied(Protocol)`. At most
    /// [`MAX_UPLOAD_BYTES`] on a connection granted [`FEATURE_LARGE_UPLOADS`],
    /// [`LEGACY_UPLOAD_BYTES`] on any other.
    TerminalUpload {
        terminal: u32,
        name: String,
        bytes: u32,
    },
    /// Asked on its own stream, after auth: the models of `tool` (a tool byte)
    /// in `project`, a project of the connection's latest list. Answered by
    /// `Models`, or `Denied`. A host from before models answers `Denied
    /// (Protocol)` on that stream only.
    ModelsProbe {
        tool: u8,
        project: String,
    },
    Models(ToolModels),
    /// Asked on its own stream, after auth: a chat completion from the host's
    /// conversation model. Answered by `Completed` with the text or why not
    /// (none set up, not reachable), or `Denied`. A host from before it
    /// answers `Denied(Protocol)`.
    Complete(Completion),
    Completed(Result<String, String>),
    /// Asked on its own stream, after auth: whether `text`, what the person
    /// has said so far in a voice call, is a finished turn, and (with a
    /// `context`: the call so far, the task, the project) whether it fits
    /// the call, answered within `wait_ms`. Answered by `TurnChecked`, or
    /// `Denied`. A host from before it answers `Denied(Protocol)`.
    TurnCheck {
        text: String,
        context: String,
        wait_ms: u32,
    },
    TurnChecked(TurnVerdict),
    /// Asked on its own stream, after auth: the features the peer reads (bits,
    /// such as [`FEATURE_BACKGROUND`]). Answered by `Features` with the ones
    /// the host grants for this connection. A host from before features
    /// answers `Denied(Protocol)` on that stream only.
    Features(u32),
    /// Asked on its own stream, after auth: the file at `path` on the host,
    /// for a preview or a download. `path` is as terminal `terminal` printed
    /// it: absolute, `~/…`, or relative to the terminal's project folder.
    /// Answered by `FileStart` and then `FileChunk`s holding exactly its
    /// bytes, by `FileRefused`, or by `Denied`. A host from before files
    /// answers `Denied(Protocol)`.
    FileRead {
        terminal: u32,
        path: String,
    },
    /// The file: the host's full path to it and its length, at most
    /// [`MAX_FILE_BYTES`].
    FileStart {
        path: String,
        bytes: u64,
    },
    FileChunk(Vec<u8>),
    /// Why the host does not send the file, as the peer shows it.
    FileRefused(String),
    /// Asked on its own stream, after auth: something of `project`, a
    /// project of the connection's latest list (its history, a transcript,
    /// its settings, its token optimization, or a change to them). `ask` is a
    /// [`crate::project_panels::ProjectAsk`], at most
    /// [`crate::project_panels::MAX_ASK`] bytes. Answered as a `FileRead` is,
    /// with the answer's bytes in place of a file's (`FileStart` with an empty
    /// path, then `FileChunk`s), by `FileRefused` with the host's reason, or by
    /// `Denied`. A host from before it answers `Denied(Protocol)`.
    ProjectAsk {
        project: String,
        ask: AskBytes,
    },
    /// Asked on its own stream, after auth: the files the host offers to
    /// download in the projects of the connection's latest list. Answered by
    /// `Offers`, or `Denied`. A host from before offers answers
    /// `Denied(Protocol)`.
    OffersProbe,
    Offers(Vec<OfferInfo>),
    /// Asked on its own stream, after auth: the file of offer `id`. Answered
    /// as a `FileRead` is (`FileStart` with the file's name, then
    /// `FileChunk`s), by `FileRefused`, or by `Denied`.
    OfferRead {
        id: u64,
    },
    /// Asked on its own stream, after auth: the requests for a secret that
    /// terminals of the connection's latest list are waiting on. Answered by
    /// `SecretAsks` (JSON, see [`crate::remote_secrets::asks_json`]), or
    /// `Denied`. A host from before answers `Denied(Protocol)`.
    SecretAsksProbe,
    SecretAsks(Vec<u8>),
    /// Asked on its own stream, after auth: the host's project groups and
    /// the group of each project of the connection's latest list. Answered
    /// by `Groups` (JSON, see [`crate::groups::groups_json`]). A host from
    /// before answers `Denied(Protocol)`, and its projects show flat.
    GroupsProbe,
    Groups(Vec<u8>),
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Everything below `seq` is applied, so the host may drop it.
    Ack {
        seq: u64,
    },
    // Terminal stream, host to client.
    Attached {
        /// The sequence number of the first byte that follows.
        start_seq: u64,
        /// True when the host could not resume and a `Resync` follows.
        resync: bool,
        /// The terminal's grid on the host.
        cols: u16,
        rows: u16,
    },
    Output {
        seq: u64,
        bytes: Vec<u8>,
    },
    /// Replace the screen with `repaint`, after which output continues at `seq`.
    Resync {
        seq: u64,
        cols: u16,
        rows: u16,
        repaint: Vec<u8>,
    },
    /// The host's grid changed.
    Size {
        cols: u16,
        rows: u16,
    },
    Status(RemoteStatus),
}

const TAG_HELLO: u8 = 1;
const TAG_CHALLENGE: u8 = 2;
const TAG_AUTH_RESPONSE: u8 = 3;
const TAG_AUTH_OK: u8 = 4;
const TAG_DENIED: u8 = 5;
const TAG_LIST: u8 = 6;
const TAG_TERMINAL_LIST: u8 = 7;
const TAG_BYE: u8 = 8;
const TAG_MANAGE: u8 = 9;
const TAG_MANAGE_RESULT: u8 = 10;
const TAG_ATTACH: u8 = 16;
const TAG_INPUT: u8 = 17;
const TAG_RESIZE: u8 = 18;
const TAG_ACK: u8 = 19;
const TAG_ATTACHED: u8 = 20;
const TAG_OUTPUT: u8 = 21;
const TAG_RESYNC: u8 = 22;
const TAG_STATUS: u8 = 23;
const TAG_SIZE: u8 = 24;
const TAG_READ_CHAT: u8 = 25;
const TAG_CHAT: u8 = 26;
const TAG_ATTACHMENT_PROBE: u8 = 27;
const TAG_ATTACHMENT_READY: u8 = 28;
const TAG_ATTACHMENT_START: u8 = 29;
const TAG_ATTACHMENT_CHUNK: u8 = 30;
const TAG_ATTACHMENT_RESULT: u8 = 31;
const TAG_MODELS_PROBE: u8 = 32;
const TAG_MODELS: u8 = 33;
const TAG_FEATURES: u8 = 34;
const TAG_TERMINAL_LIST_EX: u8 = 35;
const TAG_FILE_READ: u8 = 36;
const TAG_FILE_START: u8 = 37;
const TAG_FILE_CHUNK: u8 = 38;
const TAG_FILE_REFUSED: u8 = 39;
const TAG_TERMINAL_UPLOAD: u8 = 40;
const TAG_COMPLETE: u8 = 41;
const TAG_COMPLETED: u8 = 42;
const TAG_PROJECT_ASK: u8 = 43;
const TAG_OFFERS_PROBE: u8 = 44;
const TAG_OFFERS: u8 = 45;
const TAG_OFFER_READ: u8 = 46;
const TAG_TURN_CHECK: u8 = 47;
const TAG_TURN_CHECKED: u8 = 48;
const TAG_SECRET_ASKS_PROBE: u8 = 49;
const TAG_SECRET_ASKS: u8 = 50;
const TAG_GROUPS_PROBE: u8 = 51;
const TAG_GROUPS: u8 = 52;
const TAG_TERMINAL_LIST_IDS: u8 = 53;
const TAG_TERMINAL_LIST_OWNERS: u8 = 54;
/// The most bytes of a [`Message::Groups`].
pub const MAX_GROUPS_BYTES: usize = 192 * 1024;
/// The most bytes of a [`Message::SecretAsks`]: within one frame.
pub const MAX_SECRET_ASKS: usize = 192 * 1024;
const TAG_PAIR: u8 = 11;

struct Put(Vec<u8>);

impl Put {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn blob(&mut self, v: &[u8]) {
        // Lengths are checked against MAX_FRAME on decode; a local value cannot
        // exceed u32 because frames this large are refused when sent.
        self.u32(u32::try_from(v.len()).unwrap_or(u32::MAX));
        self.0.extend_from_slice(v);
    }
    fn str(&mut self, v: &str) {
        self.blob(v.as_bytes());
    }
}

fn opt_str(p: &mut Put, v: Option<&str>) {
    match v {
        Some(v) => {
            p.u8(1);
            p.str(v);
        }
        None => p.u8(0),
    }
}

struct Take<'a>(&'a [u8]);

impl<'a> Take<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.0.len() < n {
            return Err(WireError::Truncated);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.bytes(1)?.first().copied().unwrap_or(0))
    }
    fn u16(&mut self) -> Result<u16, WireError> {
        let mut a = [0; 2];
        a.copy_from_slice(self.bytes(2)?);
        Ok(u16::from_be_bytes(a))
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        let mut a = [0; 4];
        a.copy_from_slice(self.bytes(4)?);
        Ok(u32::from_be_bytes(a))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        let mut a = [0; 8];
        a.copy_from_slice(self.bytes(8)?);
        Ok(u64::from_be_bytes(a))
    }
    fn blob(&mut self, max: usize) -> Result<Vec<u8>, WireError> {
        let len = usize::try_from(self.u32()?).map_err(|_| WireError::TooLong)?;
        if len > max {
            return Err(WireError::TooLong);
        }
        Ok(self.bytes(len)?.to_vec())
    }
    fn str(&mut self) -> Result<String, WireError> {
        String::from_utf8(self.blob(MAX_STR)?).map_err(|_| WireError::BadText)
    }
    fn opt_str(&mut self) -> Result<Option<String>, WireError> {
        if self.u8()? == 0 {
            Ok(None)
        } else {
            Ok(Some(self.str()?))
        }
    }
    fn opt_u32(&mut self) -> Result<Option<u32>, WireError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u32()?)),
            _ => Err(WireError::BadValue),
        }
    }
    fn dims(&mut self) -> Result<(u16, u16), WireError> {
        let (cols, rows) = (self.u16()?, self.u16()?);
        if cols == 0 || rows == 0 || cols > MAX_DIM || rows > MAX_DIM {
            return Err(WireError::BadValue);
        }
        Ok((cols, rows))
    }
    fn done(&self) -> Result<(), WireError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(WireError::Trailing(self.0.len()))
        }
    }
}

/// A list's projects and terminals; with `background`, each terminal's
/// background work after it, and with `ids` its id on the host after that.
fn put_listing(
    p: &mut Put,
    listing: &Listing,
    background: bool,
    ids: bool,
    owners: bool,
) -> Result<(), WireError> {
    if listing.projects.len() > MAX_PROJECTS {
        return Err(WireError::TooLong);
    }
    p.u16(u16::try_from(listing.projects.len()).map_err(|_| WireError::TooLong)?);
    for project in &listing.projects {
        p.str(&project.id);
        p.str(&project.name);
    }
    p.u16(u16::try_from(listing.terminals.len()).map_err(|_| WireError::TooLong)?);
    for t in &listing.terminals {
        p.u32(t.id);
        p.str(&t.name);
        p.u8(t.tool);
        p.u8(u8::from(t.running));
        p.u8(t.activity);
        p.u16(t.cols);
        p.u16(t.rows);
        p.str(&t.project_id);
        p.str(&t.project_name);
        match t.parent {
            Some(parent) => {
                p.u8(1);
                p.u32(parent);
            }
            None => p.u8(0),
        }
        if background {
            let tasks = &t.background[..t.background.len().min(MAX_BACKGROUND)];
            p.u8(u8::try_from(tasks.len()).map_err(|_| WireError::TooLong)?);
            for task in tasks {
                p.str(cut(&task.id, MAX_STR));
                p.str(cut(&task.kind, MAX_STR));
                p.str(cut(&task.label, MAX_BACKGROUND_TEXT));
                opt_str(
                    p,
                    task.description
                        .as_deref()
                        .map(|d| cut(d, MAX_BACKGROUND_TEXT)),
                );
                p.u64(task.seconds);
            }
        }
        if ids {
            opt_str(p, t.entry.as_deref().map(|e| cut(e, MAX_STR)));
        }
        if owners {
            match t.owner {
                Some(owner) => {
                    p.u8(1);
                    p.u32(owner);
                }
                None => p.u8(0),
            }
        }
    }
    Ok(())
}

fn take_listing(
    t: &mut Take<'_>,
    background: bool,
    ids: bool,
    owners: bool,
) -> Result<Listing, WireError> {
    let projects_count = usize::from(t.u16()?);
    if projects_count > MAX_PROJECTS {
        return Err(WireError::TooLong);
    }
    let mut projects = Vec::with_capacity(projects_count);
    for _ in 0..projects_count {
        projects.push(ProjectInfo {
            id: t.str()?,
            name: t.str()?,
        });
    }
    let count = usize::from(t.u16()?);
    if count > MAX_LISTED {
        return Err(WireError::TooLong);
    }
    let mut list = Vec::with_capacity(count);
    for _ in 0..count {
        let mut info = TerminalInfo {
            id: t.u32()?,
            name: t.str()?,
            tool: t.u8()?,
            running: t.u8()? != 0,
            activity: t.u8()?,
            cols: t.u16()?,
            rows: t.u16()?,
            project_id: t.str()?,
            project_name: t.str()?,
            parent: t.opt_u32()?,
            background: Vec::new(),
            entry: None,
            owner: None,
        };
        if background {
            let tasks = usize::from(t.u8()?);
            if tasks > MAX_BACKGROUND {
                return Err(WireError::TooLong);
            }
            for _ in 0..tasks {
                info.background.push(BackgroundInfo {
                    id: t.str()?,
                    kind: t.str()?,
                    label: t.str()?,
                    description: t.opt_str()?,
                    seconds: t.u64()?,
                });
            }
        }
        if ids {
            info.entry = t.opt_str()?;
        }
        if owners {
            info.owner = t.opt_u32()?;
        }
        list.push(info);
    }
    Ok(Listing {
        projects,
        terminals: list,
    })
}

/// `text` cut to at most `max` bytes, on a character boundary.
pub(crate) fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl Message {
    /// The frame for this message: length prefix, tag, fields.
    ///
    /// # Errors
    /// `BadLength` when the body would exceed [`MAX_FRAME`].
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut p = Put(vec![0, 0, 0, 0]);
        match self {
            Self::AttachmentProbe { terminal } => {
                p.u8(TAG_ATTACHMENT_PROBE);
                p.u32(*terminal);
            }
            Self::AttachmentReady => p.u8(TAG_ATTACHMENT_READY),
            Self::AttachmentStart {
                terminal,
                project,
                token,
                name,
                bytes,
            } => {
                p.u8(TAG_ATTACHMENT_START);
                p.u32(*terminal);
                p.str(project);
                p.str(token);
                p.str(name);
                p.u32(*bytes);
            }
            Self::AttachmentChunk(bytes) => {
                p.u8(TAG_ATTACHMENT_CHUNK);
                p.blob(bytes);
            }
            Self::TerminalUpload {
                terminal,
                name,
                bytes,
            } => {
                p.u8(TAG_TERMINAL_UPLOAD);
                p.u32(*terminal);
                p.str(name);
                p.u32(*bytes);
            }
            Self::AttachmentResult(result) => {
                p.u8(TAG_ATTACHMENT_RESULT);
                p.u8(u8::from(result.is_ok()));
                match result {
                    Ok(path) | Err(path) => p.str(path),
                }
            }
            Self::ModelsProbe { tool, project } => {
                p.u8(TAG_MODELS_PROBE);
                p.u8(*tool);
                p.str(project);
            }
            Self::Models(models) => {
                if models.models.len() > MAX_MODELS {
                    return Err(WireError::TooLong);
                }
                p.u8(TAG_MODELS);
                opt_str(&mut p, models.default.as_deref());
                p.u16(u16::try_from(models.models.len()).map_err(|_| WireError::TooLong)?);
                for model in &models.models {
                    p.str(model);
                }
            }
            Self::ReadChat { revision } => {
                p.u8(TAG_READ_CHAT);
                p.u64(revision.unwrap_or(u64::MAX));
            }
            Self::Chat(chat) => {
                p.u8(TAG_CHAT);
                p.u64(chat.revision);
                p.u16(u16::try_from(chat.messages.len()).map_err(|_| WireError::TooLong)?);
                for (user, text) in &chat.messages {
                    p.u8(u8::from(*user));
                    p.blob(text.as_bytes());
                }
            }
            Self::Hello { version } => {
                p.u8(TAG_HELLO);
                p.u16(*version);
            }
            Self::Challenge { nonce } => {
                p.u8(TAG_CHALLENGE);
                p.0.extend_from_slice(nonce);
            }
            Self::AuthResponse {
                public_key,
                signature,
            } => {
                p.u8(TAG_AUTH_RESPONSE);
                p.blob(public_key);
                p.blob(signature);
            }
            Self::Pair(bytes) => {
                p.u8(TAG_PAIR);
                p.blob(bytes);
            }
            Self::AuthOk => p.u8(TAG_AUTH_OK),
            Self::Denied(reason) => {
                p.u8(TAG_DENIED);
                p.u8(reason.code());
            }
            Self::ListTerminals => p.u8(TAG_LIST),
            Self::TerminalList(listing) => {
                p.u8(TAG_TERMINAL_LIST);
                put_listing(&mut p, listing, false, false, false)?;
            }
            Self::TerminalListEx(listing) => {
                p.u8(TAG_TERMINAL_LIST_EX);
                put_listing(&mut p, listing, true, false, false)?;
            }
            Self::TerminalListIds(listing) => {
                p.u8(TAG_TERMINAL_LIST_IDS);
                put_listing(&mut p, listing, true, true, false)?;
            }
            Self::TerminalListOwners(listing) => {
                p.u8(TAG_TERMINAL_LIST_OWNERS);
                put_listing(&mut p, listing, true, true, true)?;
            }
            Self::Features(bits) => {
                p.u8(TAG_FEATURES);
                p.u32(*bits);
            }
            Self::FileRead { terminal, path } => {
                if path.len() > MAX_PATH {
                    return Err(WireError::TooLong);
                }
                p.u8(TAG_FILE_READ);
                p.u32(*terminal);
                p.blob(path.as_bytes());
            }
            Self::OffersProbe => p.u8(TAG_OFFERS_PROBE),
            Self::Offers(offers) => {
                if offers.len() > MAX_OFFERS {
                    return Err(WireError::TooLong);
                }
                p.u8(TAG_OFFERS);
                p.u8(u8::try_from(offers.len()).map_err(|_| WireError::TooLong)?);
                for offer in offers {
                    p.u64(offer.id);
                    p.str(cut(&offer.project, MAX_STR));
                    p.str(cut(&offer.terminal, MAX_STR));
                    p.str(cut(&offer.name, MAX_STR));
                    p.u64(offer.bytes);
                    p.u32(offer.age_secs);
                }
            }
            Self::GroupsProbe => p.u8(TAG_GROUPS_PROBE),
            Self::Groups(json) => {
                if json.len() > MAX_GROUPS_BYTES {
                    return Err(WireError::TooLong);
                }
                p.u8(TAG_GROUPS);
                p.blob(json);
            }
            Self::SecretAsksProbe => p.u8(TAG_SECRET_ASKS_PROBE),
            Self::SecretAsks(json) => {
                if json.len() > MAX_SECRET_ASKS {
                    return Err(WireError::TooLong);
                }
                p.u8(TAG_SECRET_ASKS);
                p.blob(json);
            }
            Self::OfferRead { id } => {
                p.u8(TAG_OFFER_READ);
                p.u64(*id);
            }
            Self::ProjectAsk { project, ask } => {
                if project.len() > MAX_STR || ask.0.len() > crate::project_panels::MAX_ASK {
                    return Err(WireError::TooLong);
                }
                p.u8(TAG_PROJECT_ASK);
                p.str(project);
                p.blob(&ask.0);
            }
            Self::FileStart { path, bytes } => {
                if path.len() > MAX_PATH || *bytes > MAX_FILE_BYTES {
                    return Err(WireError::TooLong);
                }
                p.u8(TAG_FILE_START);
                p.blob(path.as_bytes());
                p.u64(*bytes);
            }
            Self::FileChunk(bytes) => {
                if bytes.len() > MAX_DATA {
                    return Err(WireError::TooLong);
                }
                p.u8(TAG_FILE_CHUNK);
                p.blob(bytes);
            }
            Self::FileRefused(why) => {
                p.u8(TAG_FILE_REFUSED);
                p.str(cut(why, MAX_STR));
            }
            Self::Complete(request) => {
                let total: usize = request.messages.iter().map(|(_, t)| t.len()).sum();
                if request.messages.is_empty()
                    || request.messages.len() > MAX_COMPLETION_MESSAGES
                    || total > MAX_COMPLETION_TOTAL
                    || request
                        .messages
                        .iter()
                        .any(|(_, t)| t.len() > MAX_COMPLETION_TEXT)
                {
                    return Err(WireError::TooLong);
                }
                p.u8(TAG_COMPLETE);
                p.u16(request.max_tokens);
                p.u8(request.temperature);
                p.u32(request.wait_ms);
                p.u8(u8::try_from(request.messages.len()).map_err(|_| WireError::TooLong)?);
                for (role, text) in &request.messages {
                    p.u8(role.code());
                    p.str(text);
                }
            }
            Self::Completed(result) => {
                p.u8(TAG_COMPLETED);
                p.u8(u8::from(result.is_ok()));
                match result {
                    Ok(text) => p.str(cut(text, MAX_COMPLETION_TEXT)),
                    Err(why) => p.str(cut(why, MAX_STR)),
                }
            }
            Self::TurnCheck {
                text,
                context,
                wait_ms,
            } => {
                if text.len() > MAX_TURN_TEXT || context.len() > MAX_TURN_CONTEXT {
                    return Err(WireError::TooLong);
                }
                p.u8(TAG_TURN_CHECK);
                p.u32(*wait_ms);
                p.str(text);
                p.str(context);
            }
            Self::TurnChecked(verdict) => {
                p.u8(TAG_TURN_CHECKED);
                match verdict {
                    TurnVerdict::Judged { complete, fits } => {
                        p.u8(1);
                        p.u8((*complete).min(100));
                        // 255: not asked.
                        p.u8(fits.map_or(u8::MAX, |f| f.min(100)));
                    }
                    TurnVerdict::Unavailable(why) => {
                        p.u8(2);
                        p.str(cut(why, MAX_STR));
                    }
                    TurnVerdict::Failed(why) => {
                        p.u8(0);
                        p.str(cut(why, MAX_STR));
                    }
                }
            }
            Self::Bye => p.u8(TAG_BYE),
            Self::Manage { req, op } => {
                p.u8(TAG_MANAGE);
                p.u32(*req);
                match op {
                    ManageOp::Add {
                        tool,
                        name,
                        project,
                        model,
                    } => {
                        p.u8(1);
                        p.u8(*tool);
                        opt_str(&mut p, name.as_deref());
                        opt_str(&mut p, project.as_deref());
                        // Absent, not a 0 byte: the frame stays the one a host
                        // from before models reads.
                        if let Some(model) = model {
                            p.str(model);
                        }
                    }
                    ManageOp::Close { terminal } => {
                        p.u8(2);
                        p.u32(*terminal);
                    }
                    ManageOp::Restart { terminal } => {
                        p.u8(3);
                        p.u32(*terminal);
                    }
                    ManageOp::Rename { terminal, name } => {
                        p.u8(4);
                        p.u32(*terminal);
                        opt_str(&mut p, name.as_deref());
                    }
                    ManageOp::StopBackground { terminal, task } => {
                        p.u8(5);
                        p.u32(*terminal);
                        opt_str(&mut p, task.as_deref());
                    }
                }
            }
            Self::ManageResult { req, outcome } => {
                p.u8(TAG_MANAGE_RESULT);
                p.u32(*req);
                match outcome {
                    Ok(id) => {
                        p.u8(1);
                        match id {
                            Some(id) => {
                                p.u8(1);
                                p.u32(*id);
                            }
                            None => p.u8(0),
                        }
                    }
                    Err(why) => {
                        p.u8(0);
                        p.str(why.get(..why.len().min(MAX_STR)).unwrap_or(""));
                    }
                }
            }
            Self::Attach {
                terminal,
                resume_seq,
                cols,
                rows,
            } => {
                p.u8(TAG_ATTACH);
                p.u32(*terminal);
                p.u64(*resume_seq);
                p.u16(*cols);
                p.u16(*rows);
            }
            Self::Input(bytes) => {
                p.u8(TAG_INPUT);
                p.blob(bytes);
            }
            Self::Resize { cols, rows } => {
                p.u8(TAG_RESIZE);
                p.u16(*cols);
                p.u16(*rows);
            }
            Self::Ack { seq } => {
                p.u8(TAG_ACK);
                p.u64(*seq);
            }
            Self::Attached {
                start_seq,
                resync,
                cols,
                rows,
            } => {
                p.u8(TAG_ATTACHED);
                p.u64(*start_seq);
                p.u8(u8::from(*resync));
                p.u16(*cols);
                p.u16(*rows);
            }
            Self::Size { cols, rows } => {
                p.u8(TAG_SIZE);
                p.u16(*cols);
                p.u16(*rows);
            }
            Self::Output { seq, bytes } => {
                p.u8(TAG_OUTPUT);
                p.u64(*seq);
                p.blob(bytes);
            }
            Self::Resync {
                seq,
                cols,
                rows,
                repaint,
            } => {
                p.u8(TAG_RESYNC);
                p.u64(*seq);
                p.u16(*cols);
                p.u16(*rows);
                p.blob(repaint);
            }
            Self::Status(status) => {
                p.u8(TAG_STATUS);
                match status {
                    RemoteStatus::Running => p.u8(0),
                    RemoteStatus::Exited { code, signal } => {
                        p.u8(1);
                        p.u32(*code);
                        p.u8(u8::from(signal.is_some()));
                        if let Some(signal) = signal {
                            p.str(signal);
                        }
                    }
                    RemoteStatus::Unknown => p.u8(2),
                }
            }
        }
        let body = p.0.len() - 4;
        if body > MAX_FRAME {
            return Err(WireError::BadLength(body));
        }
        let len = u32::try_from(body).map_err(|_| WireError::BadLength(body))?;
        p.0.splice(0..4, len.to_be_bytes());
        Ok(p.0)
    }

    /// Read one message body (tag and fields, without the length prefix).
    ///
    /// # Errors
    /// Any [`WireError`] for a malformed body.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        let mut t = Take(body);
        let tag = t.u8()?;
        let message = match tag {
            TAG_ATTACHMENT_PROBE => Self::AttachmentProbe { terminal: t.u32()? },
            TAG_ATTACHMENT_READY => Self::AttachmentReady,
            TAG_ATTACHMENT_START => Self::AttachmentStart {
                terminal: t.u32()?,
                project: t.str()?,
                token: t.str()?,
                name: t.str()?,
                bytes: t.u32()?,
            },
            TAG_ATTACHMENT_CHUNK => Self::AttachmentChunk(t.blob(MAX_DATA)?),
            TAG_TERMINAL_UPLOAD => Self::TerminalUpload {
                terminal: t.u32()?,
                name: t.str()?,
                bytes: t.u32()?,
            },
            TAG_ATTACHMENT_RESULT => {
                let ok = t.u8()?;
                let text = String::from_utf8(t.blob(4096)?).map_err(|_| WireError::BadText)?;
                Self::AttachmentResult(match ok {
                    0 => Err(text),
                    1 => Ok(text),
                    _ => return Err(WireError::BadValue),
                })
            }
            TAG_MODELS_PROBE => Self::ModelsProbe {
                tool: t.u8()?,
                project: t.str()?,
            },
            TAG_MODELS => {
                let default = t.opt_str()?;
                let count = usize::from(t.u16()?);
                if count > MAX_MODELS {
                    return Err(WireError::TooLong);
                }
                let mut models = Vec::with_capacity(count);
                for _ in 0..count {
                    models.push(t.str()?);
                }
                Self::Models(ToolModels { default, models })
            }
            TAG_READ_CHAT => {
                let rev = t.u64()?;
                Self::ReadChat {
                    revision: (rev != u64::MAX).then_some(rev),
                }
            }
            TAG_CHAT => {
                let revision = t.u64()?;
                let count = t.u16()?;
                if count > 256 {
                    return Err(WireError::TooLong);
                }
                let mut messages = Vec::new();
                for _ in 0..count {
                    let user = match t.u8()? {
                        0 => false,
                        1 => true,
                        _ => return Err(WireError::BadValue),
                    };
                    let text =
                        String::from_utf8(t.blob(96 * 1024)?).map_err(|_| WireError::BadText)?;
                    messages.push((user, text));
                }
                Self::Chat(connecto_core::session::ChatHistory { revision, messages })
            }
            TAG_HELLO => Self::Hello { version: t.u16()? },
            TAG_CHALLENGE => {
                let mut nonce = [0; 32];
                nonce.copy_from_slice(t.bytes(32)?);
                Self::Challenge { nonce }
            }
            TAG_AUTH_RESPONSE => Self::AuthResponse {
                public_key: t.blob(MAX_BLOB)?,
                signature: t.blob(MAX_BLOB)?,
            },
            TAG_PAIR => Self::Pair(t.blob(MAX_BLOB)?),
            TAG_AUTH_OK => Self::AuthOk,
            TAG_DENIED => Self::Denied(DenyReason::from_code(t.u8()?).ok_or(WireError::BadValue)?),
            TAG_LIST => Self::ListTerminals,
            TAG_TERMINAL_LIST => Self::TerminalList(take_listing(&mut t, false, false, false)?),
            TAG_TERMINAL_LIST_EX => Self::TerminalListEx(take_listing(&mut t, true, false, false)?),
            TAG_TERMINAL_LIST_IDS => {
                Self::TerminalListIds(take_listing(&mut t, true, true, false)?)
            }
            TAG_TERMINAL_LIST_OWNERS => {
                Self::TerminalListOwners(take_listing(&mut t, true, true, true)?)
            }
            TAG_FEATURES => Self::Features(t.u32()?),
            TAG_FILE_READ => Self::FileRead {
                terminal: t.u32()?,
                path: String::from_utf8(t.blob(MAX_PATH)?).map_err(|_| WireError::BadText)?,
            },
            TAG_OFFERS_PROBE => Self::OffersProbe,
            TAG_OFFERS => {
                let count = usize::from(t.u8()?);
                if count > MAX_OFFERS {
                    return Err(WireError::TooLong);
                }
                let mut offers = Vec::with_capacity(count);
                for _ in 0..count {
                    offers.push(OfferInfo {
                        id: t.u64()?,
                        project: t.str()?,
                        terminal: t.str()?,
                        name: t.str()?,
                        bytes: t.u64()?,
                        age_secs: t.u32()?,
                    });
                }
                Self::Offers(offers)
            }
            TAG_OFFER_READ => Self::OfferRead { id: t.u64()? },
            TAG_GROUPS_PROBE => Self::GroupsProbe,
            TAG_GROUPS => Self::Groups(t.blob(MAX_GROUPS_BYTES)?),
            TAG_SECRET_ASKS_PROBE => Self::SecretAsksProbe,
            TAG_SECRET_ASKS => Self::SecretAsks(t.blob(MAX_SECRET_ASKS)?),
            TAG_PROJECT_ASK => Self::ProjectAsk {
                project: t.str()?,
                ask: AskBytes(t.blob(crate::project_panels::MAX_ASK)?),
            },
            TAG_FILE_START => {
                let path = String::from_utf8(t.blob(MAX_PATH)?).map_err(|_| WireError::BadText)?;
                let bytes = t.u64()?;
                if bytes > MAX_FILE_BYTES {
                    return Err(WireError::TooLong);
                }
                Self::FileStart { path, bytes }
            }
            TAG_FILE_CHUNK => Self::FileChunk(t.blob(MAX_DATA)?),
            TAG_FILE_REFUSED => Self::FileRefused(t.str()?),
            TAG_COMPLETE => {
                let max_tokens = t.u16()?;
                let temperature = t.u8()?;
                let wait_ms = t.u32()?;
                let count = usize::from(t.u8()?);
                if count == 0 || count > MAX_COMPLETION_MESSAGES {
                    return Err(WireError::BadValue);
                }
                let mut messages = Vec::with_capacity(count);
                let mut total = 0;
                for _ in 0..count {
                    let role = Role::from_code(t.u8()?).ok_or(WireError::BadValue)?;
                    let text = String::from_utf8(t.blob(MAX_COMPLETION_TEXT)?)
                        .map_err(|_| WireError::BadText)?;
                    total += text.len();
                    if total > MAX_COMPLETION_TOTAL {
                        return Err(WireError::TooLong);
                    }
                    messages.push((role, text));
                }
                Self::Complete(Completion {
                    messages,
                    max_tokens,
                    temperature,
                    wait_ms,
                })
            }
            TAG_COMPLETED => {
                let ok = t.u8()?;
                let limit = if ok == 1 {
                    MAX_COMPLETION_TEXT
                } else {
                    MAX_STR
                };
                let text = String::from_utf8(t.blob(limit)?).map_err(|_| WireError::BadText)?;
                match ok {
                    1 => Self::Completed(Ok(text)),
                    0 => Self::Completed(Err(text)),
                    _ => return Err(WireError::BadValue),
                }
            }
            TAG_TURN_CHECK => {
                let wait_ms = t.u32()?;
                let text =
                    String::from_utf8(t.blob(MAX_TURN_TEXT)?).map_err(|_| WireError::BadText)?;
                let context =
                    String::from_utf8(t.blob(MAX_TURN_CONTEXT)?).map_err(|_| WireError::BadText)?;
                Self::TurnCheck {
                    text,
                    context,
                    wait_ms,
                }
            }
            TAG_TURN_CHECKED => match t.u8()? {
                1 => {
                    let complete = t.u8()?;
                    let fits = match t.u8()? {
                        u8::MAX => None,
                        f if f <= 100 => Some(f),
                        _ => return Err(WireError::BadValue),
                    };
                    if complete > 100 {
                        return Err(WireError::BadValue);
                    }
                    Self::TurnChecked(TurnVerdict::Judged { complete, fits })
                }
                kind @ (0 | 2) => {
                    let why =
                        String::from_utf8(t.blob(MAX_STR)?).map_err(|_| WireError::BadText)?;
                    Self::TurnChecked(if kind == 2 {
                        TurnVerdict::Unavailable(why)
                    } else {
                        TurnVerdict::Failed(why)
                    })
                }
                _ => return Err(WireError::BadValue),
            },
            TAG_BYE => Self::Bye,
            TAG_MANAGE => {
                let req = t.u32()?;
                let op = match t.u8()? {
                    1 => ManageOp::Add {
                        tool: t.u8()?,
                        name: t.opt_str()?,
                        project: t.opt_str()?,
                        model: if t.0.is_empty() { None } else { Some(t.str()?) },
                    },
                    2 => ManageOp::Close { terminal: t.u32()? },
                    3 => ManageOp::Restart { terminal: t.u32()? },
                    4 => ManageOp::Rename {
                        terminal: t.u32()?,
                        name: t.opt_str()?,
                    },
                    5 => ManageOp::StopBackground {
                        terminal: t.u32()?,
                        task: t.opt_str()?,
                    },
                    _ => return Err(WireError::BadValue),
                };
                Self::Manage { req, op }
            }
            TAG_MANAGE_RESULT => {
                let req = t.u32()?;
                let outcome = if t.u8()? == 1 {
                    Ok(if t.u8()? == 1 { Some(t.u32()?) } else { None })
                } else {
                    Err(t.str()?)
                };
                Self::ManageResult { req, outcome }
            }
            TAG_ATTACH => {
                let terminal = t.u32()?;
                let resume_seq = t.u64()?;
                let (cols, rows) = t.dims()?;
                Self::Attach {
                    terminal,
                    resume_seq,
                    cols,
                    rows,
                }
            }
            TAG_INPUT => Self::Input(t.blob(MAX_DATA)?),
            TAG_RESIZE => {
                let (cols, rows) = t.dims()?;
                Self::Resize { cols, rows }
            }
            TAG_ACK => Self::Ack { seq: t.u64()? },
            TAG_ATTACHED => {
                let start_seq = t.u64()?;
                let resync = t.u8()? != 0;
                let (cols, rows) = t.dims()?;
                Self::Attached {
                    start_seq,
                    resync,
                    cols,
                    rows,
                }
            }
            TAG_SIZE => {
                let (cols, rows) = t.dims()?;
                Self::Size { cols, rows }
            }
            TAG_OUTPUT => Self::Output {
                seq: t.u64()?,
                bytes: t.blob(MAX_DATA)?,
            },
            TAG_RESYNC => {
                let seq = t.u64()?;
                let (cols, rows) = t.dims()?;
                Self::Resync {
                    seq,
                    cols,
                    rows,
                    repaint: t.blob(MAX_FRAME - 64)?,
                }
            }
            TAG_STATUS => Self::Status(match t.u8()? {
                0 => RemoteStatus::Running,
                1 => {
                    let code = t.u32()?;
                    let signal = if t.u8()? == 0 { None } else { Some(t.str()?) };
                    RemoteStatus::Exited { code, signal }
                }
                2 => RemoteStatus::Unknown,
                _ => return Err(WireError::BadValue),
            }),
            other => return Err(WireError::UnknownTag(other)),
        };
        t.done()?;
        Ok(message)
    }
}

/// Reassembles frames from a byte stream that delivers them in any chunking.
/// Its debug form is the buffer's length: the buffer may hold a secret on
/// its way.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    failed: bool,
}

impl std::fmt::Debug for FrameDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameDecoder")
            .field("buffered", &self.buf.len())
            .field("failed", &self.failed)
            .finish()
    }
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        if !self.failed {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// The next whole message, `Ok(None)` when more bytes are needed. After an
    /// error the decoder stays failed: a stream with a bad frame is closed.
    ///
    /// # Errors
    /// Any [`WireError`]; the length prefix is checked before the body arrives.
    pub fn next_message(&mut self) -> Result<Option<Message>, WireError> {
        if self.failed {
            return Err(WireError::BadValue);
        }
        let Some(prefix) = self.buf.get(..4) else {
            return Ok(None);
        };
        let mut a = [0; 4];
        a.copy_from_slice(prefix);
        let len = usize::try_from(u32::from_be_bytes(a)).unwrap_or(usize::MAX);
        if len == 0 || len > MAX_FRAME {
            self.failed = true;
            return Err(WireError::BadLength(len));
        }
        let Some(body) = self.buf.get(4..4 + len) else {
            return Ok(None);
        };
        let result = Message::decode(body);
        match result {
            Ok(message) => {
                self.buf.drain(..4 + len);
                Ok(Some(message))
            }
            Err(err) => {
                self.failed = true;
                Err(err)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    #[test]
    fn a_completion_is_held_to_its_limits() {
        let one = |text: String| Completion {
            messages: vec![(Role::User, text)],
            max_tokens: 10,
            temperature: 0,
            wait_ms: 5000,
        };
        assert!(
            Message::Complete(one("x".repeat(MAX_COMPLETION_TEXT + 1)))
                .encode()
                .is_err()
        );
        let many = Completion {
            messages: vec![(Role::User, "x".into()); MAX_COMPLETION_MESSAGES + 1],
            max_tokens: 10,
            temperature: 0,
            wait_ms: 5000,
        };
        assert!(Message::Complete(many).encode().is_err());
        let none = Completion {
            messages: vec![],
            max_tokens: 10,
            temperature: 0,
            wait_ms: 5000,
        };
        assert!(Message::Complete(none).encode().is_err());
        // Too much in all, though each fits.
        let big = Completion {
            messages: vec![(Role::User, "x".repeat(MAX_COMPLETION_TEXT)); 4],
            max_tokens: 10,
            temperature: 0,
            wait_ms: 5000,
        };
        assert!(Message::Complete(big).encode().is_err());
        // A turn check carries at most MAX_TURN_TEXT, and a percent at most 100.
        let long = Message::TurnCheck {
            text: "x".repeat(MAX_TURN_TEXT + 1),
            context: String::new(),
            wait_ms: 1,
        };
        assert_eq!(long.encode(), Err(WireError::TooLong));
        let long = Message::TurnCheck {
            text: "x".into(),
            context: "x".repeat(MAX_TURN_CONTEXT + 1),
            wait_ms: 1,
        };
        assert_eq!(long.encode(), Err(WireError::TooLong));
        let judged = |fits| {
            Message::TurnChecked(TurnVerdict::Judged {
                complete: 100,
                fits,
            })
            .encode()
            .unwrap()
        };
        // Complete over 100; fits over 100 (and not 255, "not asked").
        let mut over = judged(None);
        let at = over.len() - 2;
        over[at] = 101;
        assert!(Message::decode(&over[4..]).is_err());
        let mut over = judged(Some(100));
        let last = over.len() - 1;
        over[last] = 101;
        assert!(Message::decode(&over[4..]).is_err());
        // A role byte that is none of them.
        let mut body = Message::Complete(one("hi".into())).encode().unwrap();
        let at = body.len() - 7;
        body[at] = 9;
        assert!(Message::decode(&body[4..]).is_err());
    }

    use super::*;

    fn samples() -> Vec<Message> {
        vec![
            Message::AttachmentProbe { terminal: 7 },
            Message::AttachmentReady,
            Message::AttachmentStart {
                terminal: 7,
                project: "p".into(),
                token: "7".repeat(32),
                name: "notes.txt".into(),
                bytes: 3,
            },
            Message::AttachmentChunk(vec![0, 1, 255]),
            Message::TerminalUpload {
                terminal: 7,
                name: "shot.png".into(),
                bytes: 42,
            },
            Message::FileRead {
                terminal: 7,
                path: "docs/plan.md".into(),
            },
            Message::SecretAsksProbe,
            Message::SecretAsks(br#"[{"id":1}]"#.to_vec()),
            Message::GroupsProbe,
            Message::Groups(br#"{"groups":[],"of":[]}"#.to_vec()),
            Message::OffersProbe,
            Message::Offers(vec![OfferInfo {
                id: 3,
                project: "0123456789abcdef".into(),
                terminal: "claude-2".into(),
                name: "report.html".into(),
                bytes: 2048,
                age_secs: 12,
            }]),
            Message::OfferRead { id: 3 },
            Message::ProjectAsk {
                project: "0123456789abcdef".into(),
                ask: AskBytes(br#"{"ask":"history"}"#.to_vec()),
            },
            Message::FileStart {
                path: "/host/docs/plan.md".into(),
                bytes: 3,
            },
            Message::FileChunk(vec![0, 1, 255]),
            Message::FileRefused("no such file on that machine".into()),
            Message::Complete(Completion {
                messages: vec![
                    (Role::System, "Be brief.".into()),
                    (Role::User, "Ciao".into()),
                    (Role::Assistant, "Ciao! Dimmi.".into()),
                ],
                max_tokens: 80,
                temperature: 30,
                wait_ms: 5000,
            }),
            Message::Completed(Ok("Certo. Quale pagina?".into())),
            Message::Completed(Err("no conversation model is set up".into())),
            Message::TurnCheck {
                text: "vorrei che tu aprissi il file e poi".into(),
                context: "Progetto: connecto".into(),
                wait_ms: 1200,
            },
            Message::TurnCheck {
                text: "sì".into(),
                context: String::new(),
                wait_ms: 0,
            },
            Message::TurnChecked(TurnVerdict::Judged {
                complete: 0,
                fits: None,
            }),
            Message::TurnChecked(TurnVerdict::Judged {
                complete: 100,
                fits: Some(0),
            }),
            Message::TurnChecked(TurnVerdict::Judged {
                complete: 7,
                fits: Some(100),
            }),
            Message::TurnChecked(TurnVerdict::Unavailable("no Jev key".into())),
            Message::TurnChecked(TurnVerdict::Failed("too slow".into())),
            Message::AttachmentResult(Ok("/host/notes.txt".into())),
            Message::AttachmentResult(Err("The attachment transfer was interrupted.".into())),
            Message::ReadChat { revision: None },
            Message::ReadChat { revision: Some(3) },
            Message::Chat(connecto_core::session::ChatHistory {
                revision: 4,
                messages: vec![
                    (true, "a question".into()),
                    (false, "è una risposta".into()),
                ],
            }),
            Message::Hello { version: 1 },
            Message::Challenge { nonce: [7; 32] },
            Message::AuthResponse {
                public_key: vec![1, 2, 3],
                signature: vec![4; 600],
            },
            Message::AuthOk,
            Message::Pair(vec![1, 2, 3]),
            Message::Denied(DenyReason::Busy),
            Message::ListTerminals,
            Message::TerminalList(Listing {
                projects: vec![ProjectInfo {
                    id: "9f2c".into(),
                    name: "magneto".into(),
                }],
                terminals: vec![
                    TerminalInfo {
                        background: Vec::new(),
                        id: 3,
                        name: "claude-1".into(),
                        tool: 1,
                        running: true,
                        activity: 5,
                        cols: 120,
                        rows: 40,
                        project_id: "9f2c".into(),
                        project_name: "magneto".into(),
                        parent: None,
                        entry: None,
                        owner: None,
                    },
                    TerminalInfo {
                        background: Vec::new(),
                        id: 4,
                        name: "child".into(),
                        tool: 2,
                        running: false,
                        activity: 0,
                        cols: 80,
                        rows: 24,
                        project_id: "9f2c".into(),
                        project_name: "magneto".into(),
                        parent: Some(3),
                        entry: None,
                        owner: None,
                    },
                ],
            }),
            Message::Bye,
            Message::Manage {
                req: 7,
                op: ManageOp::Add {
                    tool: 2,
                    name: Some("review".into()),
                    project: Some("9f2c".into()),
                    model: None,
                },
            },
            Message::Manage {
                req: 11,
                op: ManageOp::Add {
                    tool: 1,
                    name: None,
                    project: None,
                    model: None,
                },
            },
            Message::Manage {
                req: 12,
                op: ManageOp::Add {
                    tool: 4,
                    name: Some("app-session".into()),
                    project: Some("9f2c".into()),
                    model: Some("openrouter/openai/gpt-5:high".into()),
                },
            },
            Message::ModelsProbe {
                tool: 1,
                project: "9f2c".into(),
            },
            Message::Models(ToolModels::default()),
            Message::Models(ToolModels {
                default: Some("lmstudio/google/gemma-4-e2b".into()),
                models: vec!["opus".into(), "sonnet".into()],
            }),
            Message::Manage {
                req: 8,
                op: ManageOp::Close { terminal: 3 },
            },
            Message::Manage {
                req: 9,
                op: ManageOp::Restart { terminal: 3 },
            },
            Message::Manage {
                req: 10,
                op: ManageOp::Rename {
                    terminal: 3,
                    name: None,
                },
            },
            Message::ManageResult {
                req: 7,
                outcome: Ok(Some(12)),
            },
            Message::ManageResult {
                req: 8,
                outcome: Ok(None),
            },
            Message::ManageResult {
                req: 9,
                outcome: Err("paused on that machine".into()),
            },
            Message::Attach {
                terminal: 2,
                resume_seq: 99,
                cols: 80,
                rows: 24,
            },
            Message::Input(b"ls\r".to_vec()),
            Message::Resize { cols: 90, rows: 30 },
            Message::Ack { seq: u64::MAX },
            Message::Attached {
                start_seq: 5,
                resync: true,
                cols: 80,
                rows: 24,
            },
            Message::Size {
                cols: 100,
                rows: 50,
            },
            Message::Output {
                seq: 1 << 40,
                bytes: vec![0xff; 1000],
            },
            Message::Resync {
                seq: 12,
                cols: 80,
                rows: 24,
                repaint: vec![b'x'; 5000],
            },
            Message::Status(RemoteStatus::Exited {
                code: 1,
                signal: Some("SIGTERM".into()),
            }),
            Message::Status(RemoteStatus::Running),
            Message::Status(RemoteStatus::Unknown),
            Message::Features(FEATURE_BACKGROUND),
            Message::TerminalListEx(Listing {
                projects: vec![],
                terminals: vec![TerminalInfo {
                    background: vec![
                        BackgroundInfo {
                            id: "b1".into(),
                            kind: "shell".into(),
                            label: "npm run dev".into(),
                            description: Some("Start the dev server".into()),
                            seconds: 43_500,
                        },
                        BackgroundInfo {
                            id: "a2".into(),
                            kind: "subagent".into(),
                            label: "Review the diff".into(),
                            description: None,
                            seconds: 3,
                        },
                    ],
                    ..info(5, None, "claude")
                }],
            }),
            Message::Features(FEATURE_BACKGROUND | FEATURE_TERMINAL_IDS),
            Message::TerminalListIds(Listing {
                projects: vec![],
                terminals: vec![
                    TerminalInfo {
                        entry: Some("claude-12".into()),
                        ..info(5, None, "app-session")
                    },
                    info(6, Some(5), "video-agent"),
                ],
            }),
            Message::Manage {
                req: 13,
                op: ManageOp::StopBackground {
                    terminal: 5,
                    task: Some("b1".into()),
                },
            },
            Message::Manage {
                req: 14,
                op: ManageOp::StopBackground {
                    terminal: 5,
                    task: None,
                },
            },
        ]
    }

    /// A terminal's id on the host goes only in the list that carries it, so
    /// a peer from before it reads the extended list it always did.
    #[test]
    fn the_host_terminal_id_goes_only_in_the_ids_list() {
        let listing = Listing {
            projects: vec![],
            terminals: vec![TerminalInfo {
                entry: Some("antigravity-4".into()),
                ..info(1, None, "agentic_works_2")
            }],
        };
        let decode = |message: Message| {
            let mut decoder = FrameDecoder::default();
            decoder.push(&message.encode().unwrap());
            decoder.next_message().unwrap().unwrap()
        };
        let Message::TerminalListEx(ex) = decode(Message::TerminalListEx(listing.clone())) else {
            panic!("not an extended list");
        };
        assert_eq!(ex.terminals[0].entry, None);
        let Message::TerminalListIds(ids) = decode(Message::TerminalListIds(listing)) else {
            panic!("not a list with ids");
        };
        assert_eq!(ids.terminals[0].entry.as_deref(), Some("antigravity-4"));
    }

    /// A terminal's owner goes only in the list that carries it, with the
    /// ids, so a peer from before it reads the list with ids it always did.
    #[test]
    fn the_owner_goes_only_in_the_owners_list() {
        let listing = Listing {
            projects: vec![],
            terminals: vec![
                info(1, None, "lead"),
                info(2, Some(1), "coder"),
                TerminalInfo {
                    entry: Some("claude-80".into()),
                    owner: Some(2),
                    ..info(3, Some(1), "check")
                },
            ],
        };
        let decode = |message: Message| {
            let mut decoder = FrameDecoder::default();
            decoder.push(&message.encode().unwrap());
            decoder.next_message().unwrap().unwrap()
        };
        let Message::TerminalListIds(ids) = decode(Message::TerminalListIds(listing.clone()))
        else {
            panic!("not a list with ids");
        };
        assert_eq!(ids.terminals[2].owner, None);
        assert_eq!(ids.terminals[2].entry.as_deref(), Some("claude-80"));
        let Message::TerminalListOwners(owners) = decode(Message::TerminalListOwners(listing))
        else {
            panic!("not a list with owners");
        };
        assert_eq!(owners.terminals[2].owner, Some(2));
        assert_eq!(owners.terminals[2].parent, Some(1));
        assert_eq!(owners.terminals[0].owner, None);
        assert_eq!(owners.terminals[2].entry.as_deref(), Some("claude-80"));
    }

    /// A plain list never carries background work, so a peer from before it
    /// reads the frame it always did; the extended list cuts what is too long.
    #[test]
    fn background_goes_only_in_the_extended_list_and_is_cut_to_fit() {
        let task = BackgroundInfo {
            id: "b1".into(),
            kind: "shell".into(),
            label: "é".repeat(400),
            description: Some("x".repeat(400)),
            seconds: 9,
        };
        let listing = Listing {
            projects: vec![],
            terminals: vec![TerminalInfo {
                background: vec![task; MAX_BACKGROUND + 3],
                ..info(1, None, "t")
            }],
        };
        let decode = |message: Message| {
            let mut decoder = FrameDecoder::default();
            decoder.push(&message.encode().unwrap());
            decoder.next_message().unwrap().unwrap()
        };
        let Message::TerminalList(plain) = decode(Message::TerminalList(listing.clone())) else {
            panic!("not a list");
        };
        assert!(plain.terminals[0].background.is_empty());
        let Message::TerminalListEx(ex) = decode(Message::TerminalListEx(listing)) else {
            panic!("not an extended list");
        };
        let sent = &ex.terminals[0].background;
        assert_eq!(sent.len(), MAX_BACKGROUND);
        assert!(sent[0].label.len() <= MAX_BACKGROUND_TEXT);
        assert!(sent[0].label.chars().all(|c| c == 'é'));
        assert_eq!(
            sent[0].description.as_deref().map(str::len),
            Some(MAX_BACKGROUND_TEXT)
        );
        // A host from before the extended list cannot read the stop: it is a
        // value it refuses.
        let mut stop = Message::Manage {
            req: 1,
            op: ManageOp::StopBackground {
                terminal: 1,
                task: None,
            },
        }
        .encode()
        .unwrap();
        // The op byte is after the length, the tag and the request id.
        stop[9] = 6;
        let mut decoder = FrameDecoder::default();
        decoder.push(&stop);
        assert!(decoder.next_message().is_err());
    }

    #[test]
    fn every_message_round_trips() {
        for message in samples() {
            let frame = message.encode().unwrap();
            let mut decoder = FrameDecoder::default();
            decoder.push(&frame);
            assert_eq!(decoder.next_message().unwrap(), Some(message));
            assert_eq!(decoder.next_message().unwrap(), None);
        }
    }

    #[test]
    fn frames_survive_one_byte_chunks_and_back_to_back_delivery() {
        let all = samples();
        let mut stream = Vec::new();
        for m in &all {
            stream.extend(m.encode().unwrap());
        }
        let mut decoder = FrameDecoder::default();
        let mut got = Vec::new();
        for byte in &stream {
            decoder.push(std::slice::from_ref(byte));
            while let Some(m) = decoder.next_message().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, all);
    }

    #[test]
    fn an_oversized_length_is_refused_before_the_body_arrives() {
        let mut decoder = FrameDecoder::default();
        decoder.push(&u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes());
        assert_eq!(
            decoder.next_message(),
            Err(WireError::BadLength(MAX_FRAME + 1))
        );
        // It stays failed.
        decoder.push(&Message::AuthOk.encode().unwrap());
        assert!(decoder.next_message().is_err());
    }

    #[test]
    fn a_zero_length_frame_is_refused() {
        let mut decoder = FrameDecoder::default();
        decoder.push(&[0, 0, 0, 0]);
        assert_eq!(decoder.next_message(), Err(WireError::BadLength(0)));
    }

    #[test]
    fn a_decoder_never_prints_what_it_holds() {
        let mut decoder = FrameDecoder::default();
        decoder.push(br#"{"value":"sk-live-0123456789"}"#);
        let printed = format!("{decoder:?}");
        assert!(!printed.contains("sk-live"), "{printed}");
        assert!(!printed.contains("115, 107"), "nor its bytes: {printed}");
    }

    #[test]
    fn a_project_ask_never_prints_what_it_carries() {
        let ask = Message::ProjectAsk {
            project: "p".into(),
            ask: AskBytes(br#"{"value":"sk-live-0123456789"}"#.to_vec()),
        };
        let printed = format!("{ask:?}");
        assert!(!printed.contains("sk-live"), "{printed}");
        assert!(!printed.contains("115, 107"), "nor its bytes: {printed}");
    }

    #[test]
    fn an_unknown_tag_is_refused() {
        let mut decoder = FrameDecoder::default();
        decoder.push(&[0, 0, 0, 1, 250]);
        assert_eq!(decoder.next_message(), Err(WireError::UnknownTag(250)));
    }

    #[test]
    fn truncated_and_trailing_bodies_are_refused() {
        assert_eq!(Message::decode(&[TAG_ACK, 0, 0]), Err(WireError::Truncated));
        assert_eq!(
            Message::decode(&[TAG_AUTH_OK, 9]),
            Err(WireError::Trailing(1))
        );
        assert_eq!(Message::decode(&[]), Err(WireError::Truncated));
    }

    #[test]
    fn a_blob_longer_than_its_limit_is_refused_without_allocating_it() {
        let mut body = vec![TAG_INPUT];
        body.extend((MAX_DATA as u32 + 1).to_be_bytes());
        assert_eq!(Message::decode(&body), Err(WireError::TooLong));
    }

    #[test]
    fn a_list_over_the_cap_and_bad_dimensions_are_refused() {
        let mut body = vec![TAG_TERMINAL_LIST];
        body.extend(0_u16.to_be_bytes());
        body.extend(((MAX_LISTED + 1) as u16).to_be_bytes());
        assert_eq!(Message::decode(&body), Err(WireError::TooLong));
        let zero = Message::Resize { cols: 0, rows: 5 }.encode().unwrap();
        assert_eq!(Message::decode(&zero[4..]), Err(WireError::BadValue));
        let huge = Message::Resize {
            cols: MAX_DIM + 1,
            rows: 5,
        }
        .encode()
        .unwrap();
        assert_eq!(Message::decode(&huge[4..]), Err(WireError::BadValue));
    }

    #[test]
    fn an_unknown_deny_code_and_invalid_utf8_are_refused() {
        assert_eq!(Message::decode(&[TAG_DENIED, 99]), Err(WireError::BadValue));
        let mut body = vec![TAG_TERMINAL_LIST, 0, 0, 0, 1];
        body.extend(1_u32.to_be_bytes());
        body.extend(2_u32.to_be_bytes());
        body.extend([0xff, 0xfe]);
        body.extend([0, 1, 0, 0, 80, 0, 24]);
        assert_eq!(Message::decode(&body), Err(WireError::BadText));
    }

    #[test]
    fn a_message_too_big_to_send_is_refused() {
        let big = Message::Resync {
            seq: 0,
            cols: 80,
            rows: 24,
            repaint: vec![0; MAX_FRAME],
        };
        assert!(big.encode().is_err());
    }

    fn listing(terminals: Vec<TerminalInfo>) -> Listing {
        Listing {
            projects: vec![ProjectInfo {
                id: "p".into(),
                name: "Project".into(),
            }],
            terminals,
        }
    }

    fn info(id: u32, parent: Option<u32>, name: &str) -> TerminalInfo {
        TerminalInfo {
            background: Vec::new(),
            id,
            name: name.into(),
            tool: 1,
            running: true,
            activity: 0,
            cols: 80,
            rows: 24,
            project_id: "p".into(),
            project_name: "Project".into(),
            parent,
            entry: None,
            owner: None,
        }
    }

    #[test]
    fn a_list_keeps_project_and_parent_with_and_without_a_parent() {
        let list = vec![info(1, None, "top"), info(2, Some(1), "child")];
        let frame = Message::TerminalList(listing(list.clone()))
            .encode()
            .unwrap();
        assert_eq!(
            Message::decode(&frame[4..]),
            Ok(Message::TerminalList(listing(list)))
        );
    }

    #[test]
    fn hostile_names_and_projects_travel_as_text_and_are_bounded() {
        let mut hostile = info(1, None, "a\u{2028}b\u{2029}c\r\nd\u{7}e\u{202e}f");
        hostile.project_name = "x".repeat(MAX_STR);
        let frame = Message::TerminalList(listing(vec![hostile.clone()]))
            .encode()
            .unwrap();
        assert_eq!(
            Message::decode(&frame[4..]),
            Ok(Message::TerminalList(listing(vec![hostile])))
        );
        // A project id one byte over the limit is refused, not cut.
        let mut body = vec![TAG_TERMINAL_LIST, 0, 0, 0, 1];
        body.extend(1_u32.to_be_bytes());
        body.extend(0_u32.to_be_bytes());
        body.extend([1, 0, 0, 0, 0, 0, 0]);
        body.extend(((MAX_STR + 1) as u32).to_be_bytes());
        assert_eq!(Message::decode(&body), Err(WireError::TooLong));
    }

    #[test]
    fn a_parent_flag_other_than_zero_or_one_is_refused() {
        let frame = Message::TerminalList(listing(vec![info(1, None, "t")]))
            .encode()
            .unwrap();
        let mut body = frame[4..].to_vec();
        let last = body.len() - 1;
        assert_eq!(body[last], 0, "no parent is flag 0");
        body[last] = 2;
        assert_eq!(Message::decode(&body), Err(WireError::BadValue));
    }

    #[test]
    fn a_list_over_the_project_cap_is_refused_and_projects_round_trip() {
        let mut body = vec![TAG_TERMINAL_LIST];
        body.extend(((MAX_PROJECTS + 1) as u16).to_be_bytes());
        assert_eq!(Message::decode(&body), Err(WireError::TooLong));
        let full = Listing {
            projects: (0..MAX_PROJECTS)
                .map(|n| ProjectInfo {
                    id: format!("{n:016x}"),
                    name: format!("project {n}"),
                })
                .collect(),
            terminals: vec![info(1, None, "t")],
        };
        let frame = Message::TerminalList(full.clone()).encode().unwrap();
        assert_eq!(
            Message::decode(&frame[4..]),
            Ok(Message::TerminalList(full))
        );
        let too_many = Listing {
            projects: vec![
                ProjectInfo {
                    id: "a".into(),
                    name: "a".into()
                };
                MAX_PROJECTS + 1
            ],
            terminals: Vec::new(),
        };
        assert_eq!(
            Message::TerminalList(too_many).encode(),
            Err(WireError::TooLong)
        );
    }

    #[test]
    fn an_add_names_its_project_and_round_trips_with_none() {
        for project in [None, Some("0a1b".to_owned())] {
            let op = Message::Manage {
                req: 3,
                op: ManageOp::Add {
                    tool: 1,
                    name: None,
                    project,
                    model: None,
                },
            };
            let frame = op.encode().unwrap();
            assert_eq!(Message::decode(&frame[4..]), Ok(op));
        }
    }

    #[test]
    fn an_add_without_a_model_is_the_frame_an_older_host_reads() {
        let add = |model: Option<&str>| {
            Message::Manage {
                req: 3,
                op: ManageOp::Add {
                    tool: 1,
                    name: Some("app-session".into()),
                    project: Some("0a1b".into()),
                    model: model.map(str::to_owned),
                },
            }
            .encode()
            .unwrap()
        };
        // The frame before models: tag, req, op, tool, name, project.
        let mut legacy = vec![TAG_MANAGE];
        legacy.extend_from_slice(&3_u32.to_be_bytes());
        legacy.extend_from_slice(&[1, 1, 1]);
        legacy.extend_from_slice(&11_u32.to_be_bytes());
        legacy.extend_from_slice(b"app-session");
        legacy.push(1);
        legacy.extend_from_slice(&4_u32.to_be_bytes());
        legacy.extend_from_slice(b"0a1b");
        assert_eq!(&add(None)[4..], legacy.as_slice());
        // A new host reads the old frame as no model, and the model after it.
        let with = add(Some("opus"));
        assert_eq!(&with[4..4 + legacy.len()], legacy.as_slice());
        assert!(matches!(
            Message::decode(&with[4..]),
            Ok(Message::Manage {
                op: ManageOp::Add { model: Some(ref m), .. },
                ..
            }) if m == "opus"
        ));
    }

    #[test]
    fn a_models_list_is_bounded() {
        let models = ToolModels {
            default: None,
            models: vec!["m".into(); MAX_MODELS + 1],
        };
        assert_eq!(Message::Models(models).encode(), Err(WireError::TooLong));
        let mut body = vec![TAG_MODELS, 0];
        body.extend_from_slice(&u16::try_from(MAX_MODELS + 1).unwrap().to_be_bytes());
        assert_eq!(Message::decode(&body), Err(WireError::TooLong));
    }

    #[test]
    fn a_version_refusal_tells_the_user_what_to_do() {
        assert_eq!(PROTOCOL_VERSION, 2);
        let text = DenyReason::Version.to_string();
        assert!(text.contains("same Connecto version"), "{text}");
    }

    #[test]
    fn terminal_addresses_name_a_host_and_localhost_is_local() {
        let local = TerminalAddr::local(4);
        assert!(local.is_local());
        assert_eq!(local.to_string(), "localhost:4");
        let remote = TerminalAddr {
            host: "build-box".into(),
            terminal: 4,
        };
        assert!(!remote.is_local());
        assert_ne!(local, remote);
    }
}
