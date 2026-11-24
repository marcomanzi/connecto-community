//! Questions asked by voice (Siri on the phone) for a project's app session,
//! taken in charge by the host (research_notes/siri-intents/plan.md §5).
//!
//! A [`ProjectAsk::SiriAsk`] is registered in a table kept on disk before
//! its text is typed, so a retry with the same id is answered from the table
//! and never typed again. The host then follows the app session's chat: the
//! prompt that is the request's text, the agent's messages after it up to the
//! next prompt, and the agent's questions. The outcome is kept for a day, and
//! a request that asked for it is notified to the phone through a queue kept
//! on disk too: a notification counts as sent only once the push gateway
//! took it, and is retried until then.
//!
//! A request whose typing a restart interrupted is never typed again: it is
//! `uncertain` until its prompt shows in the chat, or fails as not sent.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use connecto_core::session::{APP_SESSION, ChatHistory};
use connecto_core::siri_reply;
use connecto_core::{InputModes, TermActivity, TermSnapshot, TerminalEntry};
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::project_panels::ProjectAsk;

/// The table's file in the host's data folder.
pub const STORE_FILE: &str = "siri-requests.json";
/// What a host says of a Siri ask it could not read.
pub const MALFORMED: &str = "that Siri request is malformed";

/// A push registration the table could not keep: the phone keeps its
/// rotation and tells it again.
pub const UNSAVED_REGISTRATION: &str =
    "this Mac could not save the new notification destination; try again later";

/// What every Siri ask but a status gets while the request table cannot be
/// read: the phone keeps what it has and asks again later.
pub const UNREADABLE_TABLE: &str =
    "this Mac cannot read its voice request table right now; try again later";

const SECOND: u64 = 1000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
/// Siri's budget and a margin: a reply read out by Siri in this time is
/// not notified as well.
pub const GRACE_MS: u64 = 12 * SECOND;
/// How long an uncertain request waits for its prompt to show.
pub const UNCERTAIN_MS: u64 = 2 * MINUTE;
/// One "still working" notice after this.
pub const STILL_WORKING_MS: u64 = 30 * MINUTE;
/// A request with no outcome by then fails.
pub const DEADLINE_MS: u64 = 6 * HOUR;
/// Outcomes are kept this long.
pub const KEEP_MS: u64 = 24 * HOUR;
/// A notification is retried this long.
pub const RETRY_MS: u64 = 6 * HOUR;
const FIRST_RETRY_MS: u64 = 5 * SECOND;
const LAST_RETRY_MS: u64 = 10 * MINUTE;
/// The most text of a reply carried in a notification, before encryption.
pub const MAX_PREVIEW: usize = 1500;
/// APNs' limit for a whole notification.
pub const MAX_PAYLOAD: usize = 4096;
/// The longest text a Siri ask types, as the app's own send.
pub const MAX_TEXT: usize = 60_000;
const MAX_ID: usize = 64;
const MAX_TOKEN: usize = 512;
const MAX_TOPIC: usize = 255;
const MAX_LOCALE: usize = 35;
/// The most requests kept. Every one stays for its advertised retention (its
/// outcome a day, its notices until they went or gave up): at this many,
/// new requests are refused instead.
pub const MAX_ROWS: usize = 512;

/// Which APNs environment a device token is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushEnv {
    Sandbox,
    Production,
}

impl PushEnv {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Production => "production",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "sandbox" => Some(Self::Sandbox),
            "production" => Some(Self::Production),
            _ => None,
        }
    }
}

/// Where a request's notifications go, and the key their preview is
/// encrypted with (made on the phone, sent only over the authenticated link).
#[derive(Clone, PartialEq, Eq)]
pub struct SiriNotify {
    /// Hex.
    pub device_token: String,
    /// Hex: the push token of the request's Live Activity.
    pub live_activity_token: Option<String>,
    /// Base64 of 32 bytes (AES-256-GCM).
    pub key: String,
    pub env: PushEnv,
    /// The app's bundle id.
    pub topic: String,
    pub locale: String,
}

impl std::fmt::Debug for SiriNotify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiriNotify")
            .field("env", &self.env)
            .field("topic", &self.topic)
            .field("locale", &self.locale)
            .finish_non_exhaustive()
    }
}

fn hex_token(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|t| {
            !t.is_empty() && t.len() <= MAX_TOKEN && t.chars().all(|c| c.is_ascii_hexdigit())
        })
        .map(str::to_ascii_lowercase)
}

fn word(v: Option<&Value>, max: usize, extra: &[char]) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|t| {
            !t.is_empty()
                && t.len() <= max
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
        })
        .map(str::to_owned)
}

impl SiriNotify {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "device_token": self.device_token,
            "live_activity_token": self.live_activity_token,
            "key": self.key,
            "env": self.env.name(),
            "topic": self.topic,
            "locale": self.locale,
        })
    }

    /// # Errors
    /// None when a field is missing or not as the phone sends it.
    #[must_use]
    pub fn from_json(v: &Value) -> Option<Self> {
        let key = v.get("key").and_then(Value::as_str)?;
        if base64::engine::general_purpose::STANDARD
            .decode(key)
            .ok()?
            .len()
            != 32
        {
            return None;
        }
        Some(Self {
            device_token: hex_token(v.get("device_token"))?,
            live_activity_token: match v.get("live_activity_token") {
                None | Some(Value::Null) => None,
                token => Some(hex_token(token)?),
            },
            key: key.to_owned(),
            env: PushEnv::from_name(v.get("env").and_then(Value::as_str)?)?,
            topic: word(v.get("topic"), MAX_TOPIC, &['.', '-'])?,
            locale: word(v.get("locale"), MAX_LOCALE, &['-', '_'])?,
        })
    }

    fn italian(&self) -> bool {
        self.locale.to_ascii_lowercase().starts_with("it")
    }
}

fn request_id(v: &Value) -> Option<String> {
    word(v.get("request_id"), MAX_ID, &['-'])
}

fn terminal(v: &Value) -> Option<u32> {
    v.get("terminal")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// A Siri ask, from its JSON; `None` when it is not one this host reads.
#[must_use]
pub fn decode_ask(v: &Value) -> Option<ProjectAsk> {
    Some(match v.get("ask").and_then(Value::as_str)? {
        "siri-ask" => ProjectAsk::SiriAsk {
            request_id: request_id(v)?,
            terminal: terminal(v)?,
            text: v
                .get("text")
                .and_then(Value::as_str)
                .filter(|t| t.len() <= MAX_TEXT)?
                .to_owned(),
            question: match v.get("question") {
                None | Some(Value::Null) => None,
                Some(q) => Some(q.as_str()?.to_owned()),
            },
            notify: match v.get("notify") {
                None | Some(Value::Null) => None,
                Some(n) => Some(SiriNotify::from_json(n)?),
            },
        },
        "siri-result" => ProjectAsk::SiriResult {
            request_id: request_id(v)?,
        },
        "siri-delivered" => ProjectAsk::SiriDelivered {
            request_id: request_id(v)?,
        },
        "push-register" => ProjectAsk::PushRegister {
            old_device_token: match v.get("old_device_token") {
                None | Some(Value::Null) => None,
                token => Some(hex_token(token)?),
            },
            to: SiriNotify::from_json(v)?,
        },
        "activity-token" => ProjectAsk::ActivityToken {
            request_id: request_id(v)?,
            token: hex_token(v.get("token"))?,
        },
        "siri-status" => ProjectAsk::SiriStatus {
            terminal: terminal(v)?,
        },
        _ => return None,
    })
}

/// Whether `err`, a host's refusal of a project ask, says it is from before
/// that ask: the phone then falls back to the plain send, and promises no
/// notification.
#[must_use]
pub fn is_unknown_ask(err: &str) -> bool {
    [
        crate::project_panels::UNKNOWN_ASK,
        crate::host::NO_PROJECT_PANELS,
        crate::project_cache::UPDATE_TO_SEE,
    ]
    .iter()
    .any(|known| err.contains(known))
}

/// Why a Siri ask bounded by a deadline got no answer
/// ([`crate::manager::RemoteManager::siri_within`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiriCallError {
    /// Nothing reached the host: not connected, or no stream in time.
    NotSent(String),
    /// It went, but no answer came in time (or the link broke): the host may
    /// have acted on it. Ask again only with the same request id.
    Unconfirmed,
    /// The host refused it, in its words (Pause, a project gone…).
    Refused(String),
    /// A host from before this ask.
    UnknownAsk,
}

impl std::fmt::Display for SiriCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSent(why) => write!(f, "not sent: {why}"),
            Self::Unconfirmed => f.write_str("sent, but no answer came in time"),
            Self::Refused(why) => f.write_str(why),
            Self::UnknownAsk => f.write_str(crate::project_panels::UNKNOWN_ASK),
        }
    }
}

/// Why a request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// It may never have reached the agent.
    NotSent,
    /// The app session stopped, failed, or was replaced.
    Session,
    /// No reply by the deadline.
    Timeout,
    /// Its prompt never showed in the chat.
    Unmatched,
}

impl Why {
    const fn name(self) -> &'static str {
        match self {
            Self::NotSent => "not_sent",
            Self::Session => "session",
            Self::Timeout => "timeout",
            Self::Unmatched => "unmatched",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "not_sent" => Self::NotSent,
            "session" => Self::Session,
            "timeout" => Self::Timeout,
            "unmatched" => Self::Unmatched,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Answered(String),
    NeedsAnswer(String),
    NeedsChoice(String),
    Failed(Why),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// In the table, nothing typed yet.
    Registered,
    /// Being typed.
    Typing,
    /// Typed; its prompt is looked for in the chat, then its reply.
    Submitted,
    /// Typing was cut short (a restart, a refused Enter): it may or may not
    /// have reached the agent.
    Uncertain {
        since_ms: u64,
    },
    Done(Outcome),
}

#[derive(Debug, Clone)]
struct Row {
    id: String,
    /// The authenticated peer that asked it (its key's fingerprint): the
    /// only one its id answers to.
    owner: String,
    project: String,
    project_name: String,
    /// The app session's id on this host.
    terminal: String,
    text: String,
    question: Option<String>,
    notify: Option<SiriNotify>,
    /// The phone was told it will be notified.
    notifies: bool,
    /// Siri said the reply: nothing more is notified.
    delivered: bool,
    /// The chat's revision when it was registered: its prompt is at or after.
    base: u64,
    /// The prompt's position, once found.
    prompt: Option<u64>,
    phase: Phase,
    accepted_ms: u64,
    done_ms: Option<u64>,
    working_sent: bool,
    /// An answer to the agent's question whose turn has moved on: the
    /// question it answered is gone or changed, or the chat has grown. Not
    /// kept on disk (a restart ends a typed request anyway).
    moved_on: bool,
}

impl Row {
    fn open(&self) -> bool {
        !matches!(self.phase, Phase::Done(_))
    }

    /// Holding its session: a second request now could be matched to this
    /// one's prompt, or answer the same question again. A plain prompt
    /// holds it until the prompt shows; an answer to the agent's question
    /// (which may never show as a prompt) until its turn moves on.
    fn holds_session(&self) -> bool {
        match self.phase {
            Phase::Registered | Phase::Typing | Phase::Uncertain { .. } => true,
            // An answer holds it for as long as its question shows, even
            // once its echo appears as a prompt.
            Phase::Submitted if self.question.is_some() => !self.moved_on,
            Phase::Submitted => self.prompt.is_none(),
            Phase::Done(_) => false,
        }
    }

    fn state_json(&self) -> Value {
        let (state, reply, question, why) = match &self.phase {
            Phase::Registered | Phase::Typing | Phase::Submitted => ("pending", None, None, None),
            Phase::Uncertain { .. } => ("uncertain", None, None, None),
            Phase::Done(Outcome::Answered(r)) => ("answered", Some(r.as_str()), None, None),
            Phase::Done(Outcome::NeedsAnswer(q)) => ("needs-answer", None, Some(q.as_str()), None),
            Phase::Done(Outcome::NeedsChoice(q)) => ("needs-choice", None, Some(q.as_str()), None),
            Phase::Done(Outcome::Failed(w)) => ("failed", None, None, Some(w.name())),
        };
        json!({
            "state": state,
            "reply": reply,
            "question": question,
            "why": why,
            "project": self.project,
            "acceptedMs": self.accepted_ms,
            // The chat's revision it was typed at: its prompt is after it.
            "revision": self.base,
            // Whether the phone was promised a notification for it.
            "notifies": self.notifies,
        })
    }

    fn to_json(&self) -> Value {
        let (phase, value) = match &self.phase {
            Phase::Registered => ("registered", Value::Null),
            Phase::Typing => ("typing", Value::Null),
            Phase::Submitted => ("submitted", Value::Null),
            Phase::Uncertain { since_ms } => ("uncertain", json!(since_ms)),
            Phase::Done(Outcome::Answered(r)) => ("answered", json!(r)),
            Phase::Done(Outcome::NeedsAnswer(q)) => ("needs-answer", json!(q)),
            Phase::Done(Outcome::NeedsChoice(q)) => ("needs-choice", json!(q)),
            Phase::Done(Outcome::Failed(w)) => ("failed", json!(w.name())),
        };
        json!({
            "id": self.id,
            "owner": self.owner,
            "project": self.project,
            "projectName": self.project_name,
            "terminal": self.terminal,
            "text": self.text,
            "question": self.question,
            "notify": self.notify.as_ref().map(SiriNotify::to_json),
            "notifies": self.notifies,
            "delivered": self.delivered,
            "base": self.base,
            "prompt": self.prompt,
            "phase": phase,
            "value": value,
            "acceptedMs": self.accepted_ms,
            "doneMs": self.done_ms,
            "workingSent": self.working_sent,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let value = v.get("value");
        let text = || value.and_then(Value::as_str).map(str::to_owned);
        let phase = match v.get("phase").and_then(Value::as_str)? {
            "registered" => Phase::Registered,
            "typing" => Phase::Typing,
            "submitted" => Phase::Submitted,
            "uncertain" => Phase::Uncertain {
                since_ms: value.and_then(Value::as_u64)?,
            },
            "answered" => Phase::Done(Outcome::Answered(text()?)),
            "needs-answer" => Phase::Done(Outcome::NeedsAnswer(text()?)),
            "needs-choice" => Phase::Done(Outcome::NeedsChoice(text()?)),
            "failed" => Phase::Done(Outcome::Failed(Why::from_name(&text()?)?)),
            _ => return None,
        };
        Some(Self {
            id: s("id")?,
            // A row from before owners has none: it answers to no peer.
            owner: s("owner").unwrap_or_default(),
            project: s("project")?,
            project_name: s("projectName").unwrap_or_default(),
            terminal: s("terminal")?,
            text: s("text")?,
            question: s("question"),
            notify: v.get("notify").and_then(SiriNotify::from_json),
            notifies: v.get("notifies").and_then(Value::as_bool).unwrap_or(false),
            delivered: v.get("delivered").and_then(Value::as_bool).unwrap_or(false),
            base: v.get("base").and_then(Value::as_u64)?,
            prompt: v.get("prompt").and_then(Value::as_u64),
            phase,
            accepted_ms: v.get("acceptedMs").and_then(Value::as_u64)?,
            done_ms: v.get("doneMs").and_then(Value::as_u64),
            working_sent: v
                .get("workingSent")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            moved_on: false,
        })
    }
}

/// What a notification says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Reply,
    NeedsAnswer,
    NeedsChoice,
    Working,
    Failed(Why),
}

impl Kind {
    fn name(self) -> String {
        match self {
            Self::Reply => "reply".into(),
            Self::NeedsAnswer => "needs-answer".into(),
            Self::NeedsChoice => "needs-choice".into(),
            Self::Working => "working".into(),
            Self::Failed(w) => format!("failed-{}", w.name()),
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "reply" => Self::Reply,
            "needs-answer" => Self::NeedsAnswer,
            "needs-choice" => Self::NeedsChoice,
            "working" => Self::Working,
            other => Self::Failed(Why::from_name(other.strip_prefix("failed-")?)?),
        })
    }

    fn body(self, italian: bool) -> &'static str {
        match (self, italian) {
            (Self::Reply, true) => "La risposta è pronta",
            (Self::Reply, false) => "The reply is ready",
            (Self::NeedsAnswer, true) => "L'agente aspetta una risposta",
            (Self::NeedsAnswer, false) => "The agent is waiting for an answer",
            (Self::NeedsChoice, true) => "C'è una scelta da fare",
            (Self::NeedsChoice, false) => "There is a choice to make",
            (Self::Working, true) => "Sta ancora lavorando",
            (Self::Working, false) => "Still working",
            (Self::Failed(Why::NotSent), true) => {
                "Non sono sicuro che la domanda sia partita: controlla in Connecto"
            }
            (Self::Failed(Why::NotSent), false) => {
                "I'm not sure the question was sent: check in Connecto"
            }
            (Self::Failed(Why::Session), true) => "La sessione dell'agente si è fermata",
            (Self::Failed(Why::Session), false) => "The agent's session stopped",
            (Self::Failed(Why::Timeout), true) => "Nessuna risposta dopo 6 ore",
            (Self::Failed(Why::Timeout), false) => "No reply after 6 hours",
            (Self::Failed(Why::Unmatched), true) => {
                "Non trovo la domanda nella chat: apri Connecto"
            }
            (Self::Failed(Why::Unmatched), false) => {
                "I can't find the question in the chat: open Connecto"
            }
        }
    }

    /// The Live Activity's state for it, and whether it ends there.
    const fn activity(self) -> (&'static str, bool) {
        match self {
            Self::Reply => ("done", true),
            Self::NeedsAnswer | Self::NeedsChoice => ("needsChoice", true),
            Self::Working => ("working", false),
            Self::Failed(_) => ("failed", true),
        }
    }

    const fn of(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Answered(_) => Self::Reply,
            Outcome::NeedsAnswer(_) => Self::NeedsAnswer,
            Outcome::NeedsChoice(_) => Self::NeedsChoice,
            Outcome::Failed(w) => Self::Failed(*w),
        }
    }
}

#[derive(Debug, Clone)]
struct Notice {
    request_id: String,
    kind: Kind,
    created_ms: u64,
    next_ms: u64,
    attempts: u32,
    sent: bool,
    /// The device token APNs refused for it: kept, not sent again, until the
    /// phone tells another token (or it expires as any notice does).
    parked: Option<String>,
}

impl Notice {
    fn key(&self) -> String {
        format!("{}/{}", self.request_id, self.kind.name())
    }

    fn to_json(&self) -> Value {
        json!({
            "request": self.request_id,
            "kind": self.kind.name(),
            "createdMs": self.created_ms,
            "nextMs": self.next_ms,
            "attempts": self.attempts,
            "sent": self.sent,
            "parkedToken": self.parked,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            request_id: v.get("request").and_then(Value::as_str)?.to_owned(),
            kind: Kind::from_name(v.get("kind").and_then(Value::as_str)?)?,
            created_ms: v.get("createdMs").and_then(Value::as_u64)?,
            next_ms: v.get("nextMs").and_then(Value::as_u64)?,
            attempts: v
                .get("attempts")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
            sent: v.get("sent").and_then(Value::as_bool).unwrap_or(false),
            parked: v
                .get("parkedToken")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

/// Which APNs push a [`Push`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushType {
    Alert,
    LiveActivity,
}

impl PushType {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Alert => "alert",
            Self::LiveActivity => "liveactivity",
        }
    }
}

/// One push for the gateway to hand to APNs.
#[derive(Debug, Clone)]
pub struct Push {
    pub device_token: String,
    pub env: PushEnv,
    /// The APNs topic: the bundle id, with `.push-type.liveactivity` for a
    /// Live Activity.
    pub topic: String,
    pub push_type: PushType,
    /// `apns-collapse-id`: the same for a retry, so a push sent twice shows
    /// once.
    pub collapse_id: String,
    pub priority: u8,
    /// The APNs JSON.
    pub payload: Value,
}

impl Push {
    /// The gateway's request body (see research_notes/siri-intents/push-gateway.md).
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "device_token": self.device_token,
            "env": self.env.name(),
            "topic": self.topic,
            "push_type": self.push_type.name(),
            "collapse_id": self.collapse_id,
            "priority": self.priority,
            "payload": self.payload,
        })
    }
}

/// Why a push was not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushError {
    /// Try again later (gateway unreachable, overloaded).
    Temporary(String),
    /// Never to this destination (APNs refused the device token): the
    /// notification is kept, parked, until the phone tells a new token (it
    /// then goes there) or it expires.
    BadDestination(String),
    /// Never (the push itself is not one APNs or the gateway takes): the
    /// notification is dropped.
    Permanent(String),
}

/// Hands pushes to the push gateway.
pub trait PushSender: Send + Sync {
    /// # Errors
    /// Why it was not taken.
    fn send(&self, push: &Push) -> Result<(), PushError>;
}

/// A project's app session, as the host has it now.
#[derive(Clone)]
pub struct AppSession {
    pub project: String,
    pub project_name: String,
    pub entry: TerminalEntry,
}

/// The app sessions of the host's open projects.
pub trait AppSessions: Send + Sync {
    fn app_sessions(&self) -> Vec<AppSession>;
}

/// Who asks: the authenticated peer the host answers, and the way to type
/// for it. The host's own ([`crate::host`]) types through the guard as the
/// peer's terminal input is, checking again each time that hosting is on,
/// Pause is off and the peer still holds the project.
pub trait SiriCaller {
    /// The peer's identity: its key's fingerprint. Never empty.
    fn owner(&self) -> &str;

    /// Types `bytes` into `entry` as this peer.
    ///
    /// # Errors
    /// Why it was refused: nothing of `bytes` reached the terminal then.
    fn type_into(&self, entry: &TerminalEntry, bytes: Vec<u8>) -> Result<(), String>;

    /// Whether the peer's latest list still grants it the terminal with wire
    /// id `wire`, in the project asked about. None by default.
    fn granted(&self, wire: u32) -> bool {
        let _ = wire;
        false
    }

    /// Types `bytes` into `entry` (wire id `wire`) as this peer, only if
    /// that terminal is still [`Self::granted`] and `condition` holds, both
    /// checked with the write itself
    /// ([`connecto_core::TerminalSession::send_input_if`]): at once or not at
    /// all. Refused by default.
    ///
    /// # Errors
    /// Why it was refused: nothing of `bytes` reached the terminal then.
    fn type_now(
        &self,
        entry: &TerminalEntry,
        wire: u32,
        condition: &connecto_core::InputCondition,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        let _ = (entry, wire, condition, bytes);
        Err("no peer to type for".to_owned())
    }
}

/// The app session of `terminals`: the top-level agent terminal named
/// [`APP_SESSION`], as the phone finds it.
#[must_use]
pub fn app_session_of(terminals: &[TerminalEntry]) -> Option<&TerminalEntry> {
    terminals
        .iter()
        .find(|t| t.name.as_deref() == Some(APP_SESSION) && t.parent.is_none() && t.tool.is_agent())
}

/// What an app session shows now, as the phone's chat reads it.
struct Look {
    state: TermActivity,
    question: Option<connecto_core::question::Question>,
    history: Option<Arc<ChatHistory>>,
    modes: InputModes,
}

fn look(entry: &TerminalEntry) -> Look {
    let mut snap = TermSnapshot::default();
    entry.session.poll_snapshot(&mut snap);
    let screen = (0..snap.rows)
        .map(|r| {
            snap.row(r)
                .iter()
                .map(|c| c.ch)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let question = if matches!(
        entry.state,
        TermActivity::NotStarted | TermActivity::Failed | TermActivity::Exited
    ) {
        None
    } else {
        connecto_core::question::active(entry.tool, &screen)
    };
    Look {
        state: entry.state,
        question,
        history: entry.session.chat_history(),
        modes: snap.modes,
    }
}

const fn stopped(state: TermActivity) -> bool {
    matches!(
        state,
        TermActivity::NotStarted | TermActivity::Exited | TermActivity::Failed
    )
}

/// Whether the session takes a prompt now, as the phone's `send` checks.
const fn ready(state: TermActivity) -> bool {
    !matches!(
        state,
        TermActivity::Working
            | TermActivity::Starting
            | TermActivity::NeedsLogin
            | TermActivity::NotStarted
            | TermActivity::Exited
            | TermActivity::Failed
    )
}

fn refused(reason: &str, message: &str) -> Value {
    json!({ "outcome": "refused", "reason": reason, "message": message })
}

/// The text a phone may send: not empty, at most [`MAX_TEXT`] bytes, no
/// control characters but new lines and tabs.
fn valid_text(text: &str) -> bool {
    !text.trim().is_empty()
        && text.len() <= MAX_TEXT
        && !text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[derive(Default)]
struct Inner {
    rows: Vec<Row>,
    notices: Vec<Notice>,
    /// When each app session (`<project>/<terminal>`) was first seen working
    /// in its current turn.
    working: HashMap<String, u64>,
    /// Notices being sent now, by key.
    sending: HashSet<String>,
    /// The table on disk is behind: the last write failed.
    dirty: bool,
    /// Why the table on disk could not be read (anything but its absence).
    /// While set the file is left exactly as it is, never written over, and
    /// the desk takes no request and sends nothing: what it holds may be
    /// requests already typed, which a fresh table would type again.
    broken: Option<String>,
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The host's Siri requests.
pub struct SiriDesk {
    path: Option<PathBuf>,
    sessions: Arc<dyn AppSessions>,
    sender: Option<Arc<dyn PushSender>>,
    clock: Clock,
    /// Between the paste and the Enter, as the phone's send waits.
    type_pause: Duration,
    /// The most requests kept ([`MAX_ROWS`]).
    max_rows: usize,
    inner: Mutex<Inner>,
}

impl SiriDesk {
    /// The desk over `sessions`, its table in `path` (none: kept in memory
    /// only), notifying through `sender` (none: never promised).
    #[must_use]
    pub fn new(
        path: Option<PathBuf>,
        sessions: Arc<dyn AppSessions>,
        sender: Option<Arc<dyn PushSender>>,
    ) -> Self {
        Self::with_clock(
            path,
            sessions,
            sender,
            Arc::new(now_ms),
            Duration::from_millis(150),
        )
    }

    /// As [`Self::new`], with the time (ms since the epoch) from `clock` and
    /// `type_pause` between the paste and the Enter.
    #[must_use]
    pub fn with_clock(
        path: Option<PathBuf>,
        sessions: Arc<dyn AppSessions>,
        sender: Option<Arc<dyn PushSender>>,
        clock: Clock,
        type_pause: Duration,
    ) -> Self {
        let desk = Self {
            path,
            sessions,
            sender,
            clock,
            type_pause,
            max_rows: MAX_ROWS,
            inner: Mutex::default(),
        };
        desk.load();
        desk
    }

    /// At most `rows` requests kept, for a test of a full table.
    #[must_use]
    pub fn with_max_rows(mut self, rows: usize) -> Self {
        self.max_rows = rows;
        self
    }

    /// Follows the requests every second on a thread of its own, for as long
    /// as the desk is kept.
    pub fn start(self: &Arc<Self>) {
        let weak: Weak<Self> = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("siri-desk".into())
            .spawn(move || {
                while let Some(desk) = weak.upgrade() {
                    desk.tick();
                    drop(desk);
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    /// Reads the table. A request whose typing was under way is uncertain
    /// now, and one already typed failed: the chat it was followed in is
    /// gone with the restart.
    ///
    /// Only a missing table is a new one. A table that cannot be read, is not
    /// the table's JSON, or holds a row that does not decode puts the desk
    /// in recovery (see [`Inner::broken`]) and leaves the file untouched.
    fn load(&self) {
        let Some(path) = &self.path else { return };
        let mut inner = self.lock();
        let (rows, notices) = match read_table(path) {
            Ok(Some(table)) => table,
            Ok(None) => (Vec::new(), Vec::new()),
            Err(why) => {
                if inner.broken.as_deref() != Some(why.as_str()) {
                    eprintln!(
                        "siri: the request table {} cannot be read ({why}); voice requests are \
                         refused until it is moved away or repaired. It is kept as it is.",
                        path.display()
                    );
                }
                inner.broken = Some(why);
                return;
            }
        };
        inner.broken = None;
        let now = self.now();
        inner.rows = rows;
        inner.notices = notices;
        let mut failed = Vec::new();
        for row in &mut inner.rows {
            match row.phase {
                Phase::Registered | Phase::Typing => {
                    // The chat restarted with the host: look from its start.
                    row.phase = Phase::Uncertain { since_ms: now };
                    row.base = 0;
                    row.prompt = None;
                }
                Phase::Uncertain { .. } => {
                    row.base = 0;
                    row.prompt = None;
                }
                Phase::Submitted => failed.push(row.id.clone()),
                Phase::Done(_) => {}
            }
        }
        for id in failed {
            finish(&mut inner, &id, Outcome::Failed(Why::Session), now);
        }
        self.save(&mut inner);
    }

    /// Writes the table, durably; false (and marked for the next tick to
    /// write again) when it could not be.
    fn save(&self, inner: &mut Inner) -> bool {
        let Some(path) = &self.path else { return true };
        // A table that could not be read is never written over.
        if inner.broken.is_some() {
            return false;
        }
        let saved = save(path, inner);
        if let Err(e) = &saved {
            eprintln!("siri: the request table could not be written: {e}");
        }
        inner.dirty = saved.is_err();
        !inner.dirty
    }

    /// Answers a Siri ask of `project` from `caller`. `terminal` is the host
    /// id of the terminal the peer named (none: not one of the project's).
    /// A request answers only to the peer that asked it: to any other its
    /// id is unknown.
    pub fn answer(
        &self,
        project: &str,
        terminal: Option<&str>,
        ask: &ProjectAsk,
        caller: &dyn SiriCaller,
    ) -> Result<Value, String> {
        let owner = caller.owner();
        if owner.is_empty() {
            return Err(MALFORMED.to_owned());
        }
        // A table that cannot be read could hold this very request, already
        // typed: nothing is taken, typed or answered for until it is
        // recovered. A result is an error, never "unknown", so the phone
        // keeps the request rather than offering to send it again.
        if self.lock().broken.is_some() && !matches!(ask, ProjectAsk::SiriStatus { .. }) {
            return match ask {
                ProjectAsk::SiriAsk { .. } => Ok(refused(
                    "storage",
                    "This Mac cannot read its voice request table, so nothing was sent.",
                )),
                _ => Err(UNREADABLE_TABLE.to_owned()),
            };
        }
        match ask {
            ProjectAsk::SiriAsk {
                request_id,
                text,
                question,
                notify,
                ..
            } => Ok(self.ask(
                project,
                terminal,
                request_id,
                text,
                question.as_deref(),
                notify.clone(),
                caller,
            )),
            ProjectAsk::SiriResult { request_id } => Ok(self.result(project, request_id, owner)),
            ProjectAsk::SiriDelivered { request_id } => {
                self.delivered(project, request_id, owner);
                Ok(json!({ "done": true }))
            }
            ProjectAsk::PushRegister {
                old_device_token,
                to,
            } => {
                self.register(old_device_token.as_deref(), to, owner)?;
                Ok(json!({ "done": true }))
            }
            ProjectAsk::ActivityToken { request_id, token } => {
                self.activity_token(project, request_id, token, owner);
                Ok(json!({ "done": true }))
            }
            ProjectAsk::SiriStatus { .. } => {
                Ok(json!({ "workingSecs": self.working_secs(project, terminal) }))
            }
            _ => Err(crate::project_panels::UNKNOWN_ASK.to_owned()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn ask(
        &self,
        project: &str,
        terminal: Option<&str>,
        request_id: &str,
        text: &str,
        question: Option<&str>,
        notify: Option<SiriNotify>,
        caller: &dyn SiriCaller,
    ) -> Value {
        let mut inner = self.lock();
        // A retry is answered from the table, and never typed again. Another
        // peer's id is never answered for (nor typed under).
        if let Some(row) = inner.rows.iter().find(|r| r.id == request_id) {
            if row.project != project || row.owner != caller.owner() {
                return refused("invalid", "That request id is already in use.");
            }
            return json!({ "outcome": "duplicate", "state": row.state_json() });
        }
        let sessions = self.sessions.app_sessions();
        let Some(session) = sessions.iter().find(|s| s.project == project) else {
            return refused("session", "This project has no app session.");
        };
        let entry = &session.entry;
        if terminal != Some(entry.id.as_str()) {
            return refused("session", "The session changed. Please try again.");
        }
        let now = look(entry);
        let Some(history) = now.history.clone() else {
            return refused("not-ready", "The project chat is not ready yet.");
        };
        let prompt = now.question.as_ref().map(|q| q.text.as_str());
        if prompt != question {
            return refused("session", "The session changed. Check its current state.");
        }
        if now.question.as_ref().is_some_and(|q| !q.accepts_text) {
            return refused("choice", "There is a choice to make on the screen.");
        }
        if prompt.is_none() && !ready(now.state) {
            return refused("not-ready", "The agent is busy or not ready.");
        }
        if !valid_text(text) {
            return refused("invalid", "Enter a message of up to 60,000 bytes.");
        }
        if !now.modes.bracketed_paste && text.contains('\n') {
            return refused("invalid", "This prompt accepts one line at a time.");
        }
        if inner
            .rows
            .iter()
            .any(|r| r.project == project && r.terminal == entry.id && r.holds_session())
        {
            return refused("busy", "Another voice request is still being sent.");
        }
        // Everything kept stays for as long as it was promised: a full table
        // takes no more, before anything is typed.
        if inner.rows.len() >= self.max_rows {
            return refused(
                "full",
                "This Mac is keeping too many voice requests. Try again later.",
            );
        }
        let at = self.now();
        let notifies = notify.is_some() && self.sender.is_some();
        inner.rows.push(Row {
            id: request_id.to_owned(),
            owner: caller.owner().to_owned(),
            project: project.to_owned(),
            project_name: session.project_name.clone(),
            terminal: entry.id.clone(),
            text: text.to_owned(),
            question: question.map(str::to_owned),
            notify,
            notifies,
            delivered: false,
            base: history.revision,
            prompt: None,
            // On disk as being typed before anything is: after a restart it
            // is uncertain, never typed again.
            phase: Phase::Typing,
            accepted_ms: at,
            done_ms: None,
            working_sent: false,
            moved_on: false,
        });
        if !self.save(&mut inner) {
            // Not kept: a retry could not be told from a new request. Nothing
            // is typed.
            inner.rows.retain(|r| r.id != request_id);
            return refused(
                "storage",
                "This Mac could not save the request, so it was not sent.",
            );
        }
        if let Err(why) = caller.type_into(entry, connecto_core::encode_paste(text, now.modes)) {
            // Nothing reached the terminal.
            finish(&mut inner, request_id, Outcome::Failed(Why::NotSent), at);
            self.save(&mut inner);
            return refused("session", &format!("The message was not sent: {why}"));
        }
        std::thread::sleep(self.type_pause);
        // Checked again by the caller: Pause or a revoked project between the
        // paste and the Enter leaves it uncertain, never typed again.
        if caller.type_into(entry, vec![13]).is_err() {
            set_phase(&mut inner, request_id, Phase::Uncertain { since_ms: at });
            // Failing here too, the disk says typing: uncertain after a
            // restart as well.
            self.save(&mut inner);
            let state = inner
                .rows
                .iter()
                .find(|r| r.id == request_id)
                .map(Row::state_json);
            return json!({ "outcome": "duplicate", "state": state });
        }
        set_phase(&mut inner, request_id, Phase::Submitted);
        self.save(&mut inner);
        json!({ "outcome": "accepted", "revision": history.revision, "notifies": notifies })
    }

    /// The row of `request_id` in `project`, when `owner` asked it.
    fn owned<'a>(
        inner: &'a mut Inner,
        project: &str,
        request_id: &str,
        owner: &str,
    ) -> Option<&'a mut Row> {
        inner
            .rows
            .iter_mut()
            .find(|r| r.id == request_id && r.project == project && r.owner == owner)
    }

    fn result(&self, project: &str, request_id: &str, owner: &str) -> Value {
        let mut inner = self.lock();
        Self::owned(&mut inner, project, request_id, owner)
            .map_or_else(|| json!({ "state": "unknown" }), |r| r.state_json())
    }

    fn delivered(&self, project: &str, request_id: &str, owner: &str) {
        let mut inner = self.lock();
        let Some(row) = Self::owned(&mut inner, project, request_id, owner) else {
            return;
        };
        row.delivered = true;
        inner
            .notices
            .retain(|n| n.request_id != request_id || n.sent);
        self.save(&mut inner);
    }

    /// The phone's new destination, for its own requests only: those that
    /// go to `old` (a rotation it may tell a host late, once it is
    /// reachable again) or to the same token already. Applying it again
    /// changes nothing.
    ///
    /// A notice parked because APNs refused the old token goes to the new
    /// one at once. The phone is told it is done only once the change is on
    /// disk: otherwise it keeps the rotation and tells it again.
    fn register(&self, old: Option<&str>, to: &SiriNotify, owner: &str) -> Result<(), String> {
        let mut inner = self.lock();
        let now = self.now();
        let mut moved = HashSet::new();
        for row in inner.rows.iter_mut().filter(|r| r.owner == owner) {
            if let Some(n) = &mut row.notify
                && (Some(n.device_token.as_str()) == old || n.device_token == to.device_token)
            {
                let live = n.live_activity_token.take();
                let next = SiriNotify {
                    live_activity_token: live,
                    ..to.clone()
                };
                if *n != next {
                    moved.insert(row.id.clone());
                }
                *n = next;
            }
        }
        if moved.is_empty() && !inner.dirty {
            return Ok(());
        }
        for n in inner
            .notices
            .iter_mut()
            .filter(|n| moved.contains(&n.request_id) && n.parked.is_some())
        {
            n.parked = None;
            n.attempts = 0;
            n.next_ms = now;
        }
        if self.save(&mut inner) {
            Ok(())
        } else {
            Err(UNSAVED_REGISTRATION.to_owned())
        }
    }

    fn activity_token(&self, project: &str, request_id: &str, token: &str, owner: &str) {
        let mut inner = self.lock();
        if let Some(n) =
            Self::owned(&mut inner, project, request_id, owner).and_then(|r| r.notify.as_mut())
        {
            n.live_activity_token = Some(token.to_owned());
        }
        self.save(&mut inner);
    }

    fn working_secs(&self, project: &str, terminal: Option<&str>) -> Option<u64> {
        let sessions = self.sessions.app_sessions();
        let session = sessions.iter().find(|s| s.project == project)?;
        if terminal != Some(session.entry.id.as_str()) {
            return None;
        }
        let now = self.now();
        let mut inner = self.lock();
        observe(&mut inner, &sessions, now);
        let since = inner
            .working
            .get(&format!("{project}/{}", session.entry.id))
            .copied()?;
        Some(now.saturating_sub(since) / SECOND)
    }

    /// One look at every open request and due notification.
    pub fn tick(&self) {
        // In recovery: only a table moved away or repaired brings the desk
        // back. Nothing is followed or sent meanwhile.
        if self.lock().broken.is_some() {
            self.load();
            if self.lock().broken.is_some() {
                return;
            }
        }
        let sessions = self.sessions.app_sessions();
        let now = self.now();
        let due = {
            let mut inner = self.lock();
            observe(&mut inner, &sessions, now);
            let before = inner.rows.len() + inner.notices.len();
            let mut changed = follow(&mut inner, &sessions, now);
            inner
                .rows
                .retain(|r| r.open() || r.done_ms.is_none_or(|d| now.saturating_sub(d) < KEEP_MS));
            let ids: HashSet<String> = inner.rows.iter().map(|r| r.id.clone()).collect();
            inner.notices.retain(|n| {
                ids.contains(&n.request_id)
                    && (n.sent || now.saturating_sub(n.created_ms) < RETRY_MS)
            });
            changed |= before != inner.rows.len() + inner.notices.len();
            if changed || inner.dirty {
                self.save(&mut inner);
            }
            self.due(&mut inner, now)
        };
        for (key, push, live) in due {
            if let Some(live) = live {
                // Best effort: the notification is what delivers.
                if let Some(sender) = &self.sender {
                    let _ = sender.send(&live);
                }
            }
            let sent = self.sender.as_ref().map(|s| s.send(&push));
            let mut inner = self.lock();
            inner.sending.remove(&key);
            if let Some(n) = inner.notices.iter_mut().find(|n| n.key() == key) {
                match sent {
                    Some(Ok(())) => n.sent = true,
                    // The token, not the notice: it waits for the phone's
                    // next one (`register`), or its expiry.
                    Some(Err(PushError::BadDestination(_))) => {
                        n.parked = Some(push.device_token.clone());
                    }
                    Some(Err(PushError::Permanent(_))) | None => {
                        n.created_ms = 0;
                    }
                    Some(Err(PushError::Temporary(_))) => {
                        n.attempts = n.attempts.saturating_add(1);
                        let wait = FIRST_RETRY_MS
                            .saturating_mul(1 << n.attempts.min(10))
                            .min(LAST_RETRY_MS);
                        n.next_ms = now.saturating_add(wait);
                    }
                }
            }
            // Dropped ones go with the next tick.
            inner.notices.retain(|n| n.created_ms != 0 || n.sent);
            self.save(&mut inner);
        }
    }

    /// The notifications to send now, marked as being sent.
    fn due(&self, inner: &mut Inner, now: u64) -> Vec<(String, Push, Option<Push>)> {
        if self.sender.is_none() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for i in 0..inner.notices.len() {
            let n = &inner.notices[i];
            let key = n.key();
            if n.sent || n.next_ms > now || inner.sending.contains(&key) {
                continue;
            }
            let Some(row) = inner.rows.iter().find(|r| r.id == n.request_id) else {
                continue;
            };
            if now < row.accepted_ms.saturating_add(GRACE_MS) {
                continue;
            }
            // Progress only while the request is still in progress.
            if n.kind == Kind::Working && !row.open() {
                continue;
            }
            let Some(notify) = &row.notify else { continue };
            // Refused for this very token: not before the phone tells another.
            if n.parked.as_deref() == Some(notify.device_token.as_str()) {
                continue;
            }
            let Some(push) = alert(row, notify, n.kind) else {
                continue;
            };
            let live = notify
                .live_activity_token
                .as_ref()
                .map(|token| activity(row, notify, token, n.kind, now));
            out.push((key.clone(), push, live));
            inner.sending.insert(key);
        }
        out
    }
}

/// Notes which app sessions work now, and since when.
fn observe(inner: &mut Inner, sessions: &[AppSession], now: u64) {
    let mut live = HashSet::new();
    for s in sessions {
        if s.entry.state == TermActivity::Working {
            let key = format!("{}/{}", s.project, s.entry.id);
            inner.working.entry(key.clone()).or_insert(now);
            live.insert(key);
        }
    }
    inner.working.retain(|k, _| live.contains(k));
}

fn set_phase(inner: &mut Inner, id: &str, phase: Phase) {
    if let Some(row) = inner.rows.iter_mut().find(|r| r.id == id) {
        row.phase = phase;
    }
}

/// Ends request `id` with `outcome`, and queues its notification when the
/// phone was promised one and has not had the reply from Siri.
fn finish(inner: &mut Inner, id: &str, outcome: Outcome, now: u64) {
    let Some(row) = inner.rows.iter_mut().find(|r| r.id == id) else {
        return;
    };
    let kind = Kind::of(&outcome);
    row.phase = Phase::Done(outcome);
    row.done_ms = Some(now);
    let notify = row.notifies && !row.delivered;
    // A "still working" not sent yet (parked on a refused token, or waiting
    // for the gateway) would now arrive after the end: it goes.
    inner
        .notices
        .retain(|n| !(n.request_id == id && n.kind == Kind::Working && !n.sent));
    if notify {
        queue(inner, id, kind, now);
    }
}

fn queue(inner: &mut Inner, id: &str, kind: Kind, now: u64) {
    let notice = Notice {
        request_id: id.to_owned(),
        kind,
        created_ms: now,
        next_ms: now,
        attempts: 0,
        sent: false,
        parked: None,
    };
    if !inner.notices.iter().any(|n| n.key() == notice.key()) {
        inner.notices.push(notice);
    }
}

/// Follows every open request in the chats; whether anything changed.
fn follow(inner: &mut Inner, sessions: &[AppSession], now: u64) -> bool {
    let mut changed = false;
    let open: Vec<String> = inner
        .rows
        .iter()
        .filter(|r| r.open())
        .map(|r| r.id.clone())
        .collect();
    for id in open {
        // A prompt just found is followed at once, to its reply if it is
        // there already.
        for _pass in 0..2 {
            let Some(row) = inner.rows.iter().find(|r| r.id == id).cloned() else {
                break;
            };
            let session = sessions.iter().find(|s| s.project == row.project);
            let found = session.map(|s| (s, look(&s.entry)));
            // The newest prompt another request of this session claimed.
            let claimed = inner
                .rows
                .iter()
                .filter(|r| {
                    r.id != row.id && r.project == row.project && r.terminal == row.terminal
                })
                .filter_map(|r| r.prompt)
                .max();
            let from = claimed.map_or(row.base, |c| row.base.max(c + 1));
            let step = match &row.phase {
                Phase::Uncertain { since_ms } => {
                    let at = found
                        .as_ref()
                        .and_then(|(_, l)| l.history.as_deref())
                        .and_then(|h| siri_reply::find_prompt(h, from, &row.text));
                    match at {
                        Some(at) => Step::Prompt(at, Phase::Submitted),
                        None if now.saturating_sub(*since_ms) >= UNCERTAIN_MS => {
                            Step::Done(Outcome::Failed(Why::NotSent))
                        }
                        None => Step::Nothing,
                    }
                }
                Phase::Submitted => match &found {
                    Some((s, l)) if s.entry.id == row.terminal && !stopped(l.state) => {
                        // An answer holds its session until its turn moves on.
                        if row.question.is_some()
                            && !row.moved_on
                            && moved_on(&row, l)
                            && let Some(r) = inner.rows.iter_mut().find(|r| r.id == id)
                        {
                            r.moved_on = true;
                        }
                        submitted(&row, l, from, now)
                    }
                    _ => Step::Done(Outcome::Failed(Why::Session)),
                },
                // Typing is under the lock of the ask itself.
                Phase::Registered | Phase::Typing | Phase::Done(_) => Step::Nothing,
            };
            match step {
                Step::Nothing => {}
                Step::Prompt(at, phase) => {
                    if let Some(r) = inner.rows.iter_mut().find(|r| r.id == id) {
                        r.prompt = Some(at);
                        r.phase = phase;
                    }
                    changed = true;
                    continue;
                }
                Step::Working => {
                    if let Some(r) = inner.rows.iter_mut().find(|r| r.id == id) {
                        r.working_sent = true;
                    }
                    if row.notifies && !row.delivered {
                        queue(inner, &id, Kind::Working, now);
                    }
                    changed = true;
                }
                Step::Done(outcome) => {
                    finish(inner, &id, outcome, now);
                    changed = true;
                }
            }
            break;
        }
    }
    changed
}

enum Step {
    Nothing,
    Prompt(u64, Phase),
    Working,
    Done(Outcome),
}

/// Whether the question an answer was typed for has left the screen (gone,
/// or another one in its place). Only the question's own lifecycle counts:
/// a message in the chat meanwhile (a board report, the echo of this very
/// answer) leaves it on screen, unanswered as far as the agent shows, and
/// a second answer to it is still refused. Once it has left, it stays so
/// for this request (the same words asked later are a new question).
fn moved_on(row: &Row, now_look: &Look) -> bool {
    now_look.question.as_ref().map(|q| &q.text) != row.question.as_ref()
}

/// The question `row` answers is still on screen: the agent has not taken
/// the answer yet.
fn still_asking(row: &Row, now_look: &Look) -> bool {
    row.question.is_some() && !row.moved_on && !moved_on(row, now_look)
}

/// A typed request: its prompt, then its reply or the agent's question.
fn submitted(row: &Row, now_look: &Look, from: u64, now: u64) -> Step {
    let Some(history) = now_look.history.as_deref() else {
        return Step::Done(Outcome::Failed(Why::Session));
    };
    // The chat started again (a new session): the request cannot be followed.
    if history.revision < row.base {
        return Step::Done(Outcome::Failed(Why::Session));
    }
    let late = now.saturating_sub(row.accepted_ms) >= DEADLINE_MS;
    let prompt = row
        .prompt
        .or_else(|| siri_reply::find_prompt(history, from, &row.text));
    if prompt.is_some() && row.prompt.is_none() {
        return Step::Prompt(prompt.unwrap_or_default(), Phase::Submitted);
    }
    // An answer to the agent's question may never show as a prompt: its
    // reply is what the agent says from then on.
    let anchor = match (prompt, &row.question) {
        (Some(at), _) => at + 1,
        (None, Some(_)) => row.base,
        (None, None) => {
            return if late {
                Step::Done(Outcome::Failed(Why::Unmatched))
            } else {
                Step::Nothing
            };
        }
    };
    let replies = siri_reply::replies_from(history, anchor);
    // The question this request answered may still show for a moment.
    if let Some(q) = now_look
        .question
        .as_ref()
        .filter(|q| Some(&q.text) != row.question.as_ref())
    {
        return Step::Done(if q.accepts_text {
            Outcome::NeedsAnswer(q.text.clone())
        } else {
            Outcome::NeedsChoice(q.text.clone())
        });
    }
    // Its question still on screen: whatever the chat says meanwhile is not
    // the agent's answer to it, and the request keeps its session.
    if let Some(reply) = replies.joined().filter(|_| !still_asking(row, now_look))
        && (now_look.state != TermActivity::Working || replies.next_prompt)
    {
        return Step::Done(Outcome::Answered(reply));
    }
    if late {
        return Step::Done(Outcome::Failed(Why::Timeout));
    }
    if !row.working_sent && now.saturating_sub(row.accepted_ms) >= STILL_WORKING_MS {
        return Step::Working;
    }
    Step::Nothing
}

/// What the table on disk holds: its requests and notifications.
type Table = (Vec<Row>, Vec<Notice>);

/// The table at `path`: `None` when there is none yet (only `NotFound`), or
/// why it cannot be trusted: unreadable, not JSON, not the table's shape, or
/// a request or notice that does not decode. A row is never skipped: one
/// dropped would be a request already typed that a retry types again.
fn read_table(path: &Path) -> Result<Option<Table>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("unreadable: {e}")),
    };
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("not JSON: {e}"))?;
    let list = |key: &str| {
        v.get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("no \"{key}\" list"))
    };
    let rows = list("requests")?
        .iter()
        .enumerate()
        .map(|(i, r)| Row::from_json(r).ok_or_else(|| format!("request {i} does not decode")))
        .collect::<Result<Vec<_>, _>>()?;
    let notices = list("notices")?
        .iter()
        .enumerate()
        .map(|(i, n)| Notice::from_json(n).ok_or_else(|| format!("notice {i} does not decode")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some((rows, notices)))
}

/// Writes the table to `path` durably: a temporary file readable by this
/// user only (it holds prompts, replies, device tokens and preview keys),
/// flushed to disk, renamed over the table, and the folder flushed too.
fn save(path: &Path, inner: &Inner) -> std::io::Result<()> {
    use std::io::Write as _;
    let v = json!({
        "requests": inner.rows.iter().map(Row::to_json).collect::<Vec<_>>(),
        "notices": inner.notices.iter().map(Notice::to_json).collect::<Vec<_>>(),
    });
    let tmp = path.with_extension("json.tmp");
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    // A temporary file left by an older build may have kept wider rights.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(v.to_string().as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    if let Some(dir) = dir {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn cut(text: &str, max: usize) -> &str {
    &text[..text.floor_char_boundary(max)]
}

/// The preview carried by a notification: the reply, or the agent's question.
fn preview(row: &Row) -> Option<&str> {
    match &row.phase {
        Phase::Done(Outcome::Answered(t) | Outcome::NeedsAnswer(t) | Outcome::NeedsChoice(t)) => {
            Some(cut(t, MAX_PREVIEW))
        }
        _ => None,
    }
}

/// `text` sealed with `key` (base64 of 32 bytes): base64 of the 12-byte
/// nonce, then the ciphertext and its tag. `aad` (the request id) is bound
/// to it, so a preview cannot be shown under another request.
///
/// # Errors
/// None for a bad key.
#[must_use]
pub fn seal(key: &str, aad: &str, text: &str) -> Option<String> {
    let key = base64::engine::general_purpose::STANDARD.decode(key).ok()?;
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).ok()?;
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: text.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .ok()?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    Some(base64::engine::general_purpose::STANDARD.encode(out))
}

/// [`seal`] undone, as the phone's notification extension does.
#[must_use]
pub fn open_sealed(key: &str, aad: &str, sealed: &str) -> Option<String> {
    let key = base64::engine::general_purpose::STANDARD.decode(key).ok()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(sealed)
        .ok()?;
    if bytes.len() < 12 {
        return None;
    }
    let (nonce, body) = bytes.split_at(12);
    let plain = Aes256Gcm::new_from_slice(&key)
        .ok()?
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: body,
                aad: aad.as_bytes(),
            },
        )
        .ok()?;
    String::from_utf8(plain).ok()
}

/// The same for every send of one notification, at most 64 bytes.
fn collapse_id(request_id: &str, kind: Kind) -> String {
    let digest = sha2::Sha256::digest(format!("{request_id}/{}", kind.name()).as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

fn title(row: &Row) -> String {
    let name = if row.project_name.is_empty() {
        &row.project
    } else {
        &row.project_name
    };
    format!("Connecto · {}", cut(name, 80))
}

/// The notification for `kind` of `row`: a generic alert, with the preview
/// encrypted for the phone's extension to show.
fn alert(row: &Row, notify: &SiriNotify, kind: Kind) -> Option<Push> {
    let mut payload = json!({
        "aps": {
            "alert": { "title": title(row), "body": kind.body(notify.italian()) },
            "mutable-content": 1,
            "thread-id": row.project,
            "category": "REPLY",
            "sound": "default",
        },
        "r": row.id,
        "p": row.project,
        "k": kind.name(),
    });
    if let Some(text) = preview(row) {
        payload["c"] = json!(seal(&notify.key, &row.id, text)?);
    }
    Some(Push {
        device_token: notify.device_token.clone(),
        env: notify.env,
        topic: notify.topic.clone(),
        push_type: PushType::Alert,
        collapse_id: collapse_id(&row.id, kind),
        priority: 10,
        payload,
    })
}

/// The Live Activity update for `kind` of `row`: no text of the reply.
fn activity(row: &Row, notify: &SiriNotify, token: &str, kind: Kind, now: u64) -> Push {
    let (state, ends) = kind.activity();
    let mut aps = json!({
        "timestamp": now / SECOND,
        "event": if ends { "end" } else { "update" },
        "content-state": {
            "project": cut(&row.project_name, 80),
            "state": state,
            "requestId": row.id,
            "startedAt": row.accepted_ms / SECOND,
        },
    });
    if ends {
        aps["dismissal-date"] = json!(now / SECOND + 4 * 3600);
    }
    Push {
        device_token: token.to_owned(),
        env: notify.env,
        topic: format!("{}.push-type.liveactivity", notify.topic),
        push_type: PushType::LiveActivity,
        collapse_id: collapse_id(&row.id, kind),
        priority: 10,
        payload: json!({ "aps": aps }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests;
