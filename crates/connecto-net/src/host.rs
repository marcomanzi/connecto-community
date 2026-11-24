//! The host: lets authenticated peers look at and type into terminals.
//!
//! Several peers are served at once, up to [`MAX_PEERS`], each on the thread
//! that called [`HostService::serve`]. Each has its own listing and rights;
//! what they share is the terminals, whose size goes to the latest peer that
//! resized it (see [`SizeBook`]). One connection is served on the calling
//! thread plus scoped threads: one for
//! the control stream and, per attached terminal, one reading the peer's input
//! while the serving thread streams output. Nothing is served before the
//! handshake ends: the first stream must say `Hello` and answer the challenge,
//! and only then are further streams accepted at all.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use connecto_core::{
    GridSize, OutputTap, ResizeOutcome, SessionStatus, TermSnapshot, TerminalSession,
};

use crate::auth::{AuthorizedKeySet, HostAuth};
use crate::guard::{Peer, RemoteGuard, deny_reason};
use crate::repaint::repaint_with_history;
use crate::seq::{At, Ring, RingRead};
use crate::transport::{Connection, NetError, Stream};
use crate::wire::Completion;
use crate::wire::cut;
use crate::wire::{
    BackgroundInfo, DenyReason, FEATURE_BACKGROUND, FEATURE_TERMINAL_IDS, FEATURE_TERMINAL_OWNERS,
    Listing, MAX_ATTACHMENT_BYTES, MAX_DATA, MAX_FILE_BYTES, MAX_LISTED, MAX_MODELS, MAX_OFFERS,
    MAX_PATH, MAX_PROJECTS, MAX_STR, MAX_UPLOAD_BYTES, ManageOp, Message, OfferInfo, ProjectInfo,
    RemoteStatus, TerminalInfo, ToolModels,
};

/// How many authenticated peers a host serves at once. One more is refused
/// with [`DenyReason::Busy`].
pub const MAX_PEERS: usize = 8;
/// How many chat attachments and terminal uploads the host receives at once,
/// across all peers. A chat attachment (at most 10 MiB) is held in memory
/// until stored; a terminal upload (up to 100 MiB) goes to disk as it
/// arrives. A peer receives one at a time.
pub const MAX_UPLOADS: usize = 2;
/// How many chat completions the host asks of its conversation model at once,
/// across all peers.
pub const MAX_COMPLETIONS: usize = 2;
/// How many voice-call turn checks the host asks Jev at once, across all
/// peers. Each is short (a fraction of a second).
pub const MAX_TURN_CHECKS: usize = 4;
/// The longest a turn check is given, whatever the peer asks: it runs at
/// every pause, and the call goes on as without it once it is late.
pub const MAX_TURN_WAIT: Duration = Duration::from_millis(2000);
/// How many files the host sends at once, across all peers. A peer is sent
/// one at a time.
pub const MAX_SENDS: usize = 4;

/// How long a peer gets to say something while the host waits for it.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);
/// Unacknowledged output the host sends before it waits. A peer that stays
/// behind falls off the ring and gets a repaint instead.
pub const SEND_WINDOW: u64 = 512 * 1024;

/// History cells read for a repaint, at most, so the copy made while the
/// terminal's output waits stays short; what fits [`MAX_REPAINT`] of them is
/// sent.
const HISTORY_CELLS: usize = 300_000;
/// How long a capture waits for a busy screen.
const CAPTURE_WAIT: Duration = Duration::from_secs(1);
/// How long a capture waits for a synchronized update to end before ending
/// it itself: longer than the update's own timeout (150 ms).
const SYNC_WAIT: Duration = Duration::from_millis(200);
/// The largest repaint a peer of any version reads in one `Resync`.
const MAX_REPAINT: usize = crate::wire::MAX_FRAME - 64;

/// Who decides a terminal's size while a peer is attached. The one place this
/// is decided: the user has not chosen yet, so the peer drives the host's PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizePolicy {
    /// An attach and a resize from the peer resize the host's PTY.
    PeerDrivesHost,
    /// The host's grid stands; the peer adopts it.
    HostOnly,
}

impl ResizePolicy {
    const fn peer_may_resize(self) -> bool {
        matches!(self, Self::PeerDrivesHost)
    }
}

/// The policy in force.
pub const RESIZE_POLICY: ResizePolicy = ResizePolicy::PeerDrivesHost;

/// Feeds the emulator and numbers the bytes in one step.
pub struct RingTap {
    ring: Arc<Ring>,
    /// Whether output is kept. Off, bytes are only numbered.
    record: Arc<AtomicBool>,
}

impl RingTap {
    #[must_use]
    pub const fn new(ring: Arc<Ring>, record: Arc<AtomicBool>) -> Self {
        Self { ring, record }
    }

    /// A tap that always keeps output.
    #[must_use]
    pub fn recording(ring: Arc<Ring>) -> Self {
        Self::new(ring, Arc::new(AtomicBool::new(true)))
    }
}

impl OutputTap for RingTap {
    fn feed(&self, bytes: &[u8], emulator_feed: &mut dyn FnMut(&[u8])) {
        self.ring
            .append_with(bytes, emulator_feed, self.record.load(Ordering::Relaxed));
    }

    fn resize(&self, resize: &mut dyn FnMut() -> Option<GridSize>) -> bool {
        self.ring.resize_with(resize)
    }
}

/// A terminal the host can show: its session, and the ring its tap fills.
#[derive(Clone)]
pub struct HostTerminal {
    pub id: u32,
    /// The terminal's own name, without its project.
    pub name: String,
    /// See [`TerminalInfo::tool`].
    pub tool: u8,
    /// See [`TerminalInfo::project_id`].
    pub project_id: String,
    /// See [`TerminalInfo::project_name`].
    pub project_name: String,
    /// See [`TerminalInfo::parent`]: the wire id of a terminal in the same list.
    pub parent: Option<u32>,
    /// The terminal's attempt number, for the audit record.
    pub attempt: u64,
    /// The status dot, as [`TerminalInfo::activity`].
    pub activity: u8,
    pub session: Arc<dyn TerminalSession>,
    pub ring: Arc<Ring>,
    /// The work its agent left running at the prompt, oldest first.
    pub background: Vec<BackgroundInfo>,
    /// See [`TerminalInfo::entry`]: its id in the host's own list.
    pub entry: Option<String>,
    /// It has run since the host started (it has output, or a ring for it),
    /// or it is the restored chat: what every peer is listed. One that has
    /// not goes only to a peer granted [`FEATURE_TERMINAL_IDS`].
    pub ran: bool,
    /// See [`TerminalInfo::owner`]: the wire id of the terminal it belongs to
    /// (the agent that asked for it), in the same list. `None`: as `parent`.
    pub owner: Option<u32>,
}

impl HostTerminal {
    fn grid(&self) -> (u16, u16) {
        // A screen busy with output gives no snapshot (`Contended`): asked
        // again, rather than reporting a made-up 80x24 the peer would adopt.
        let mut snap = TermSnapshot::default();
        for _ in 0..200 {
            match self.session.poll_snapshot(&mut snap) {
                connecto_core::SnapshotOutcome::Contended => {
                    thread::sleep(Duration::from_millis(1));
                }
                _ => break,
            }
        }
        shown_size(&snap)
    }

    fn info(&self) -> TerminalInfo {
        let (cols, rows) = self.grid();
        TerminalInfo {
            id: self.id,
            name: self.name.clone(),
            tool: self.tool,
            running: self.session.status() == SessionStatus::Running,
            activity: self.activity,
            cols,
            rows,
            project_id: self.project_id.clone(),
            project_name: self.project_name.clone(),
            parent: self.parent,
            background: self.background.clone(),
            entry: self.entry.clone(),
            owner: self.owner,
        }
    }

    /// Whether the terminal now has this size.
    fn resize(&self, cols: u16, rows: u16) -> bool {
        for _ in 0..500 {
            match self.session.resize_remote(GridSize { cols, rows }) {
                ResizeOutcome::Retry => thread::sleep(Duration::from_millis(2)),
                ResizeOutcome::Applied => return true,
                ResizeOutcome::Failed => return false,
            }
        }
        false
    }

    /// The screen as of a known position: every byte and size mark before
    /// it, none after.
    fn capture(&self) -> Option<(At, u16, u16, Vec<u8>)> {
        let start = Instant::now();
        while start.elapsed() < CAPTURE_WAIT {
            // A synchronized update is waited out for a while, then ended:
            // a program that starts the next one at once never lets it end.
            let flush = start.elapsed() >= SYNC_WAIT;
            let got = self.ring.consistent_at(|end| {
                let (mut snap, mut history) = (TermSnapshot::default(), TermSnapshot::default());
                match self
                    .session
                    .poll_capture(HISTORY_CELLS, flush, &mut snap, &mut history)
                {
                    connecto_core::SnapshotOutcome::Updated
                    | connecto_core::SnapshotOutcome::Unchanged => Some((end, snap, history)),
                    _ => None,
                }
            });
            if let Some((end, snap, history)) = got {
                let (cols, rows) = shown_size(&snap);
                let bytes = repaint_with_history(&snap, Some(&history), MAX_REPAINT);
                return Some((end, cols, rows, bytes));
            }
            thread::sleep(Duration::from_millis(1));
        }
        None
    }
}

/// The size a peer is told: a screen with no size (a stopped terminal has
/// none) is sent as 80x24, since a peer refuses a 0x0 frame as bad and drops
/// the link.
fn shown_size(snap: &TermSnapshot) -> (u16, u16) {
    if snap.cols == 0 || snap.rows == 0 {
        (80, 24)
    } else {
        (snap.cols, snap.rows)
    }
}

/// Whether a project chat on a terminal of this tool byte may be sent files:
/// the agents, which are given the files' paths and read them. Antigravity
/// reads outside its workspace only by a rule, which its launch adds for the
/// attachments folder. A shell (3) never.
/// The `bytes` of an upload, as the peer sends them in `AttachmentChunk`s.
/// `None` when the stream ends or sends anything else, the transfer runs
/// past its deadline or sits idle too long, `stop` is set, or `keep_going`
/// turns false.
/// Receives the `bytes` bytes of an upload as `AttachmentChunk`s, handing
/// each to `sink` (false: stop). `None` when the transfer was cut short, ran
/// past [`crate::attachments::transfer_timeout`] or stalled, a chunk went
/// past `bytes`, or `keep_going` said no.
fn receive_chunks(
    stream: &dyn Stream,
    stop: &AtomicBool,
    bytes: usize,
    keep_going: &dyn Fn() -> bool,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> Option<()> {
    let mut received = 0;
    let deadline = Instant::now() + crate::attachments::transfer_timeout(bytes);
    let mut last_chunk = Instant::now();
    while received < bytes {
        if stop.load(Ordering::SeqCst)
            || Instant::now() >= deadline
            || last_chunk.elapsed() >= crate::attachments::IDLE_TIMEOUT
            || !keep_going()
        {
            return None;
        }
        match stream.recv(POLL) {
            Ok(Some(Message::AttachmentChunk(chunk)))
                if !chunk.is_empty() && chunk.len() <= bytes - received =>
            {
                if !sink(&chunk) {
                    return None;
                }
                received += chunk.len();
                last_chunk = Instant::now();
            }
            Ok(None) => {}
            _ => return None,
        }
    }
    Some(())
}

/// A probability as a whole percent (0..=100); anything else as 0.
fn percent(p: f64) -> u8 {
    let p = if p.is_finite() {
        p.clamp(0.0, 1.0)
    } else {
        0.0
    };
    // In 0..=100 after the clamp.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole = (p * 100.0).round() as u8;
    whole
}

fn takes_attachments(tool: u8) -> bool {
    use connecto_core::ToolKind;
    [
        ToolKind::Claude,
        ToolKind::Codex,
        ToolKind::Pi,
        ToolKind::Antigravity,
        ToolKind::OpenCode,
    ]
    .map(crate::hosting::tool_byte)
    .contains(&tool)
}

/// The terminals a host offers, now.
pub trait HostTerminals: Send + Sync {
    fn list(&self) -> Vec<HostTerminal>;

    /// The host's private data directory for chat files, if enabled.
    fn attachment_root(&self) -> Option<&Path> {
        None
    }

    /// The host's open projects that are set up, in its order, whether or not
    /// they have a listed terminal. By default, the projects of [`list`].
    ///
    /// [`list`]: HostTerminals::list
    fn projects(&self) -> Vec<ProjectInfo> {
        let mut out: Vec<ProjectInfo> = Vec::new();
        for term in self.list() {
            if !out.iter().any(|p| p.id == term.project_id) {
                out.push(ProjectInfo {
                    id: term.project_id,
                    name: term.project_name,
                });
            }
        }
        out
    }

    /// Closes the terminal `right` names and every terminal under it, but only
    /// if each of them is in `listed` (the wire ids the connection was sent in
    /// that project). That is checked where the close runs, so a child added after
    /// the list is refused. By default, nothing can be closed for a peer.
    ///
    /// # Errors
    /// With the reason, as the peer will read it; nothing is closed then.
    fn close_within(&self, right: &Right, listed: &[u32]) -> Result<Option<u32>, String> {
        let _ = (right, listed);
        Err("this machine does not let peers manage terminals".to_owned())
    }

    /// The project a new terminal goes to when a peer names none.
    fn active_project(&self) -> Option<String> {
        None
    }

    /// Do what a peer asked to the host's terminals, through the same seam the
    /// window uses. The new terminal's id is returned when it is known at once.
    /// An `Add` with a project the host does not have is refused.
    ///
    /// # Errors
    /// With the reason, as the peer will read it.
    fn manage(&self, op: &ManageOp, right: &Right) -> Result<Option<u32>, String> {
        let _ = (op, right);
        Err("this machine does not let peers manage terminals".to_owned())
    }

    /// Whether terminal output is being kept for peers. Off, the ring only
    /// numbers the bytes, so nothing is retained while nobody can look.
    fn set_recording(&self, on: bool) {
        let _ = on;
    }

    /// The models a peer may choose for a new terminal of `tool` (a tool
    /// byte) in `project`, a project the connection was sent. By default only
    /// the tool's own default.
    fn models(&self, tool: u8, project: &str) -> ToolModels {
        let _ = (tool, project);
        ToolModels::default()
    }

    /// The file a peer asked for by `path`, as the terminal `right` names
    /// printed it (absolute, `~/…`, or relative to its project's folder),
    /// opened for reading: the path to show for it, and the open regular
    /// file. What is opened is what was checked. By default no file is sent.
    ///
    /// # Errors
    /// Why not, as the peer will read it.
    fn open_file(&self, right: &Right, path: &str) -> Result<(String, std::fs::File), String> {
        let _ = (right, path);
        Err("this machine does not share files".to_owned())
    }

    /// A chat completion from this machine's conversation model, for a peer
    /// with none of its own. By default there is none.
    ///
    /// # Errors
    /// Why not, as the peer will read it; [`crate::assistant::NOT_SET_UP`]
    /// when none is set up.
    fn complete(&self, request: &Completion) -> Result<String, String> {
        let _ = request;
        Err(crate::assistant::NOT_SET_UP.to_owned())
    }

    /// How likely `text`, what a peer's person has said so far in a voice
    /// call, is a finished turn, and (with a `context`) how likely it fits
    /// the call, answered within `wait`. By default this machine cannot
    /// judge.
    ///
    /// # Errors
    /// [`crate::wire::TurnVerdict::Unavailable`] when it cannot judge at all
    /// (no Jev key), [`crate::wire::TurnVerdict::Failed`] when this one failed.
    fn judge_turn(
        &self,
        text: &str,
        context: &str,
        wait: Duration,
    ) -> Result<crate::wire::TurnScores, crate::wire::TurnVerdict> {
        let _ = (text, context, wait);
        Err(crate::wire::TurnVerdict::Unavailable(
            crate::wire::NO_TURN_JUDGE.to_owned(),
        ))
    }

    /// Answer what a peer asked of the project `right` names (its history,
    /// a transcript, its settings, its token optimization, or a change to
    /// them): the answer's bytes, as [`crate::project_panels`] writes them.
    /// The guard has let it through. It may wait (a transcript is read off
    /// disk), so it runs on the peer's stream, never the window's thread. By
    /// default a peer is told this machine does not share them.
    ///
    /// # Errors
    /// Why not, as the peer will read it.
    fn project_ask(
        &self,
        right: &Right,
        ask: &crate::project_panels::ProjectAsk,
    ) -> Result<Vec<u8>, String> {
        let _ = (right, ask);
        Err(NO_PROJECT_PANELS.to_owned())
    }

    /// [`Self::project_ask`], knowing who asks: the authenticated peer, and
    /// the guarded way to type for it (a Siri ask). By default the same as
    /// [`Self::project_ask`].
    ///
    /// # Errors
    /// Why not, as the peer will read it.
    fn project_ask_by(
        &self,
        right: &Right,
        ask: &crate::project_panels::ProjectAsk,
        caller: &dyn crate::siri::SiriCaller,
    ) -> Result<Vec<u8>, String> {
        let _ = caller;
        self.project_ask(right, ask)
    }

    /// The files offered to download (`connecto send-file`), in every
    /// project; the host keeps those of the connection's list. By default
    /// none.
    fn offers(&self) -> Vec<OfferInfo> {
        Vec::new()
    }

    /// The requests for a secret terminals are waiting on, in every
    /// project; the host tells a peer those of the connection's list. By
    /// default none.
    fn secret_asks(&self) -> Vec<connecto_core::SecretAsk> {
        Vec::new()
    }

    /// The host's project groups, as its sidebar has them; the host tells
    /// a peer the memberships of the connection's list only. By default
    /// none.
    fn groups(&self) -> connecto_core::RemoteGroups {
        connecto_core::RemoteGroups::default()
    }

    /// The file of offer `id`, opened for reading: its name and the open
    /// regular file. By default there is none.
    ///
    /// # Errors
    /// Why not, as the peer will read it.
    fn open_offer(&self, id: u64) -> Result<(String, std::fs::File), String> {
        let _ = id;
        Err(NO_SUCH_OFFER.to_owned())
    }
}

/// Read into `buf` from `offset`, leaving the handle's own position alone.
pub(crate) fn read_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buf, offset)
    }
}

/// What a host says of an offer that is gone.
pub const NO_SUCH_OFFER: &str = "that file is no longer offered: ask for it again";

/// What a host that does not share its projects' history and settings says.
pub const NO_PROJECT_PANELS: &str =
    "that machine does not share its projects' history and settings";

/// Where the host reads its authorized keys, each time a peer connects.
pub type KeySource = Arc<dyn Fn() -> Arc<AuthorizedKeySet> + Send + Sync>;

/// What one connection was last sent in a list: the only projects and terminals
/// it may name.
///
/// The rule for every request a connection makes (attach, input, resize, each
/// management request, add): an operation from a connection is allowed only on a
/// terminal or project that the connection's latest list contained, identified by
/// its stable identity, and checked at dispatch time. A terminal's identity is its
/// wire id: the registry numbers each terminal entry (its project and entry id)
/// once, for the life of the app, and never gives a number to a second entry. So
/// a terminal that was closed and a new one created is a different identity, and
/// an old grant never matches it. A restarted entry is the same entry, so it stays
/// granted: a restart changes nothing the rule names, and the peer could restart
/// it itself. A project's identity is its folder id, so a project closed and
/// opened again is the same project, and stays granted.
#[derive(Debug, Clone, Default)]
pub struct Shown {
    /// The project ids the list sent.
    projects: HashSet<String>,
    /// Each terminal the list sent, by wire id, with the project it was sent in.
    terminals: HashMap<u32, String>,
}

impl Shown {
    fn has_project(&self, id: &str) -> bool {
        self.projects.contains(id)
    }

    fn terminal_project(&self, id: u32) -> Option<&str> {
        self.terminals.get(&id).map(String::as_str)
    }

    /// The wire ids the list sent in `project`.
    fn listed_in(&self, project: &str) -> Vec<u32> {
        self.terminals
            .iter()
            .filter(|(_, p)| p.as_str() == project)
            .map(|(id, _)| *id)
            .collect()
    }
}

/// The right to act on one terminal or project for one connection, as of one
/// check. Made only by [`HostService::authorize`], which is the one place that
/// checks a connection's latest list against the host. Every send to the peer
/// about the target and every action on it presents this right. Its fields are
/// private to this module, so no other code can make one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Right {
    target: Target,
}

impl Target {
    /// The project a target is in: a terminal's, or a project's own.
    #[must_use]
    pub fn project(&self) -> Option<&str> {
        match self {
            Self::Terminal { project, .. } => Some(project),
            Self::Project(_) => None,
        }
    }
}

/// What a [`Right`] is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A terminal, by wire id, in the project it was sent under.
    Terminal { id: u32, project: String },
    /// A project, by id.
    Project(String),
}

impl Right {
    #[must_use]
    pub const fn target(&self) -> &Target {
        &self.target
    }

    /// A right for a test of a registry, which cannot be made otherwise.
    #[cfg(test)]
    pub(crate) fn for_test(target: Target) -> Self {
        Self { target }
    }
}

/// What a stream sends through: the connection's listing, the grant it attached
/// under, and whether it is still running (a wait for the peer ends when it is not).
#[derive(Clone, Copy)]
struct Serve<'a> {
    shown: &'a Mutex<Shown>,
    right: &'a Right,
    keep_going: &'a dyn Fn() -> bool,
}

/// A connection's view of one stream: what it attached to, and what ends it.
struct PeerLink<'a> {
    ended: &'a AtomicBool,
    acked: &'a AtomicU64,
    shown: &'a Mutex<Shown>,
    right: &'a Right,
}

/// What a peer is told when closing a terminal would close one it was not sent.
pub const HIDDEN_CHILD_REFUSAL: &str = connecto_core::CLOSE_NOT_ALL_LISTED;

fn no_such_terminal() -> String {
    DenyReason::NoSuchTerminal.to_string()
}

/// The refusal for any project id the peer was not offered, and for one that
/// is offered but gone by the time it is used. The same text for all of them,
/// so it tells nothing about which projects exist.
pub const NO_SUCH_PROJECT: &str = "no such project on that machine";

/// Serves remote peers, several at once (at most [`MAX_PEERS`]).
/// The peer of a project ask, for the project to answer it as that peer:
/// its identity, and its typing, which passes the guard as the peer's
/// terminal input does (so the input is the peer's, recorded and refused in
/// Pause) and is checked again at each write.
struct PeerCaller<'a> {
    host: &'a HostService,
    peer: &'a Peer,
    shown: &'a Mutex<Shown>,
    right: &'a Right,
    stop: &'a AtomicBool,
    /// The terminals typed into, whose held-back audit records are written
    /// when the ask is over.
    typed: std::cell::RefCell<HashSet<String>>,
}

impl crate::siri::SiriCaller for PeerCaller<'_> {
    fn owner(&self) -> &str {
        &self.peer.fingerprint
    }

    fn type_into(
        &self,
        entry: &connecto_core::TerminalEntry,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        let guard = &self.host.guard;
        if self.stop.load(Ordering::SeqCst) || !guard.enabled() {
            return Err("hosting is off".to_owned());
        }
        if guard.paused() {
            return Err(DenyReason::Paused.to_string());
        }
        if !self.host.holds(self.shown, self.right) {
            return Err(no_such_terminal());
        }
        self.typed.borrow_mut().insert(entry.id.clone());
        guard
            .input(self.peer, &entry.id, 0, &bytes)
            .map_err(|why| deny_reason(why).to_string())?;
        entry.session.send_input(bytes).map_err(|e| e.to_string())
    }

    fn granted(&self, wire: u32) -> bool {
        let Target::Project(project) = self.right.target() else {
            return false;
        };
        self.host
            .authorize(
                &lock(self.shown),
                Target::Terminal {
                    id: wire,
                    project: project.clone(),
                },
            )
            .is_some()
    }

    fn type_now(
        &self,
        entry: &connecto_core::TerminalEntry,
        wire: u32,
        condition: &connecto_core::InputCondition,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        if self.stop.load(Ordering::SeqCst) || !self.host.guard.enabled() {
            return Err("hosting is off".to_owned());
        }
        if self.host.guard.paused() {
            return Err(DenyReason::Paused.to_string());
        }
        let Target::Project(project) = self.right.target() else {
            return Err(no_such_terminal());
        };
        // The peer's listing held from the check to the write: a grant
        // revoked meanwhile cannot slip between them.
        let sent = lock(self.shown);
        if !self.host.holds_in(&sent, self.right)
            || self
                .host
                .authorize(
                    &sent,
                    Target::Terminal {
                        id: wire,
                        project: project.clone(),
                    },
                )
                .is_none()
        {
            return Err(no_such_terminal());
        }
        self.typed.borrow_mut().insert(entry.id.clone());
        self.host
            .guard
            .input(self.peer, &entry.id, 0, &bytes)
            .map_err(|why| deny_reason(why).to_string())?;
        entry
            .session
            .send_input_if(condition, bytes)
            .map_err(|e| e.to_string())
    }
}

impl Drop for PeerCaller<'_> {
    fn drop(&mut self) {
        for terminal in self.typed.borrow().iter() {
            self.host.guard.detach(self.peer, terminal, 0);
        }
    }
}

pub struct HostService {
    terminals: Arc<dyn HostTerminals>,
    keys: KeySource,
    guard: Arc<dyn RemoteGuard>,
    /// Authenticated peers being served.
    peers: Slots,
    /// Chat attachments being received, across all peers.
    uploads: Slots,
    /// Files being sent, across all peers.
    sends: Slots,
    /// Chat completions being answered, across all peers.
    completions: Slots,
    /// Voice-call turn checks being answered, across all peers.
    turn_checks: Slots,
    /// Which peer has each terminal's size.
    sizes: SizeBook,
}

/// A counted, bounded resource; each taken slot is given back when its lease drops.
struct Slots {
    used: AtomicUsize,
    max: usize,
}

impl Slots {
    const fn new(max: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            max,
        }
    }

    fn take(&self) -> Option<SlotLease<'_>> {
        self.used
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < self.max).then_some(used + 1)
            })
            .ok()
            .map(|_| SlotLease(self))
    }
}

struct SlotLease<'a>(&'a Slots);

impl Drop for SlotLease<'_> {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What one connection is doing that is limited to one at a time per peer.
#[derive(Default)]
struct PeerTransfers {
    uploading: AtomicBool,
    sending: AtomicBool,
}

/// A transfer slot: the peer's own flag and one of the host's slots, both
/// given back on drop.
struct TransferLease<'a> {
    _slot: SlotLease<'a>,
    own: &'a AtomicBool,
}

impl Drop for TransferLease<'_> {
    fn drop(&mut self) {
        self.own.store(false, Ordering::SeqCst);
    }
}

fn take_transfer<'a>(own: &'a AtomicBool, slots: &'a Slots) -> Option<TransferLease<'a>> {
    own.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .ok()?;
    let Some(slot) = slots.take() else {
        own.store(false, Ordering::SeqCst);
        return None;
    };
    Some(TransferLease { _slot: slot, own })
}

/// Who has each terminal's size, across every connected peer.
///
/// The rule: the latest peer to resize a terminal has its size. When it lets
/// go (its stream ends), the size goes to the peer that held it before, at the
/// size that peer last asked for, if that peer is still attached; only when no
/// peer holds it does the host's window get it back. A click in the host
/// window takes it from every peer at once; the next peer to resize takes it
/// again. Every peer whose view of the size changed, the one that resized
/// included, is told the size the terminal has, once, by its stream's writer:
/// one sender per stream, so an older size never overtakes a newer one.
#[derive(Default)]
struct SizeBook {
    /// Each terminal's holders behind a lock of its own: a change of hands and
    /// the resize it makes happen together, and a slow resize of one terminal
    /// holds up no other.
    terminals: Mutex<HashMap<u32, Arc<Mutex<Holders>>>>,
    /// Numbers leases and changes; never repeats, so a stale `seen` never matches.
    counter: AtomicU64,
}

#[derive(Default)]
struct Holders {
    /// Each lease that asked for the size, oldest first, with what it asked
    /// for; the last one has it.
    stack: Vec<(u64, GridSize)>,
    /// The size the terminal was given, when the holders know it; `None`
    /// when it is the host window's.
    effective: Option<GridSize>,
    /// The session's local claims when the stack was last checked.
    claims: u64,
    /// Bumped (from [`SizeBook::counter`]) whenever the size changes hands.
    epoch: u64,
    /// Taken out of the book: a lease that still had it looks again.
    dead: bool,
}

/// What a peer is told about a terminal's size.
enum SizeChange {
    /// The terminal was given this size.
    Known(GridSize),
    /// The host's window has it: the size its view asked for, or the grid.
    Host,
}

impl SizeBook {
    fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Arc<Mutex<Holders>>>> {
        self.terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, id: u32) -> Option<Arc<Mutex<Holders>>> {
        self.map().get(&id).cloned()
    }

    /// Runs `f` on the terminal's live entry, made if there is none, and
    /// returns the entry with what `f` returned.
    fn with_entry<T>(
        &self,
        id: u32,
        f: impl FnOnce(&mut Holders) -> T,
    ) -> (Arc<Mutex<Holders>>, T) {
        loop {
            let entry = Arc::clone(self.map().entry(id).or_default());
            let mut holders = lock_holders(&entry);
            if !holders.dead {
                let out = f(&mut holders);
                drop(holders);
                return (entry, out);
            }
            drop(holders);
            // Marked for removal and not yet taken out: taken out now.
            self.remove_dead(id, &entry);
        }
    }

    /// Takes a terminal's entry out once no lease is in it. The entry is
    /// marked under its own lock and taken out under the map's, never both
    /// waited for at once, so a slow resize of one terminal holds up no other.
    fn forget_if_empty(&self, id: u32, entry: &Arc<Mutex<Holders>>) {
        {
            let mut holders = lock_holders(entry);
            if !holders.stack.is_empty() || holders.dead {
                return;
            }
            holders.dead = true;
        }
        self.remove_dead(id, entry);
    }

    fn remove_dead(&self, id: u32, entry: &Arc<Mutex<Holders>>) {
        let mut map = self.map();
        if map.get(&id).is_some_and(|e| Arc::ptr_eq(e, entry)) {
            map.remove(&id);
        }
    }

    /// The host's window took the size since the stack was last checked:
    /// every peer lost it.
    fn reconcile(&self, holders: &mut Holders, term: &HostTerminal) {
        let claims = term.session.local_size_claims();
        if claims != holders.claims {
            holders.claims = claims;
            if !holders.stack.is_empty() || holders.effective.is_some() {
                holders.stack.clear();
                holders.effective = None;
                holders.epoch = self.next();
            }
        }
    }

    /// After a change made for peers, a click in the host window that landed
    /// while it was made wins: the session may have been given the size back
    /// and then taken again, so it is given back now, and every peer loses it.
    fn settle(&self, holders: &mut Holders, term: &HostTerminal) {
        if term.session.local_size_claims() != holders.claims {
            term.session.set_remote_size_control(false);
            self.reconcile(holders, term);
        }
    }
}

fn lock_holders(entry: &Mutex<Holders>) -> std::sync::MutexGuard<'_, Holders> {
    entry.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A terminal's size for one peer's stream, in the [`SizeBook`]. Taken at the
/// peer's first resize, not at its attach: a peer attaches every terminal in
/// the background to keep them live, and only a terminal its window shows is
/// resized. Given back when dropped, whatever way the stream ended.
struct SizeLease<'a> {
    term: &'a HostTerminal,
    book: &'a SizeBook,
    token: u64,
    /// The terminal's epoch this stream last told its peer about.
    seen: AtomicU64,
    /// The size this stream last sent its peer (in `Attached`, `Resync` or
    /// `Size`), as `cols << 16 | rows`; 0 before any.
    told: AtomicU32,
}

const fn pack(cols: u16, rows: u16) -> u32 {
    ((cols as u32) << 16) | rows as u32
}

impl<'a> SizeLease<'a> {
    fn new(term: &'a HostTerminal, book: &'a SizeBook) -> Self {
        let seen = book.get(term.id).map_or(0, |entry| {
            let mut holders = lock_holders(&entry);
            if holders.dead {
                return 0;
            }
            book.reconcile(&mut holders, term);
            holders.epoch
        });
        Self {
            term,
            book,
            token: book.next(),
            seen: AtomicU64::new(seen),
            told: AtomicU32::new(0),
        }
    }

    /// The peer resizes: it has the size from now on, and the terminal is
    /// resized. The peer is told the outcome by its stream's writer, whatever
    /// it was. A resize that fails changes nothing: who held the size, and at
    /// what, stays as it was.
    fn take(&self, size: GridSize) {
        let (book, term) = (self.book, self.term);
        let (entry, empty) = book.with_entry(term.id, |holders| {
            book.reconcile(holders, term);
            term.session.set_remote_size_control(true);
            if term.resize(size.cols, size.rows) {
                holders.stack.retain(|(token, _)| *token != self.token);
                holders.stack.push((self.token, size));
                holders.effective = Some(size);
                holders.epoch = book.next();
            } else if holders.stack.is_empty() {
                term.session.set_remote_size_control(false);
            }
            book.settle(holders, term);
            holders.stack.is_empty()
        });
        self.tell_anyway();
        if empty {
            book.forget_if_empty(term.id, &entry);
        }
    }

    /// The peer was sent this size.
    fn told(&self, cols: u16, rows: u16) {
        self.told.store(pack(cols, rows), Ordering::SeqCst);
    }

    /// The size to tell the peer now, if it differs from what it was told.
    /// A session that reports the size it applied is compared with what the
    /// peer was sent, whatever changed it: another peer, a hand-back that
    /// took a while, or an attach that read a screen from before a resize.
    /// Otherwise the book's changes are followed.
    fn to_tell(&self, wait: bool) -> Option<(u16, u16)> {
        if let Some(applied) = self.term.session.applied_size() {
            let packed = pack(applied.cols, applied.rows);
            return (self.told.load(Ordering::SeqCst) != packed)
                .then_some((applied.cols, applied.rows));
        }
        match self.changed(wait)? {
            SizeChange::Known(size) => Some((size.cols, size.rows)),
            SizeChange::Host => Some(self.term.grid()),
        }
    }

    /// The peer is told the terminal's size on the writer's next pass, though
    /// nothing changed hands (a resize the policy does not let it make).
    fn tell_anyway(&self) {
        self.seen.store(u64::MAX, Ordering::SeqCst);
    }

    /// The change since this stream last told its peer, once. Unless `wait`,
    /// a change being made is not waited for: it is seen on a later pass.
    /// Before output is sent it is waited for, as the output may be the
    /// redraw the resize caused, which must follow the size.
    fn changed(&self, wait: bool) -> Option<SizeChange> {
        let entry = self.book.get(self.term.id);
        let (epoch, change) = match &entry {
            None => (0, SizeChange::Host),
            Some(entry) => {
                let mut holders = if wait {
                    lock_holders(entry)
                } else {
                    match entry.try_lock() {
                        Ok(holders) => holders,
                        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                        Err(std::sync::TryLockError::WouldBlock) => return None,
                    }
                };
                if holders.dead {
                    (0, SizeChange::Host)
                } else {
                    self.book.reconcile(&mut holders, self.term);
                    let change = holders
                        .effective
                        .map_or(SizeChange::Host, SizeChange::Known);
                    (holders.epoch, change)
                }
            }
        };
        // An entry taken out (no lease held it) left the host's window with
        // the size: a change for whoever saw a holder.
        (self.seen.swap(epoch, Ordering::SeqCst) != epoch).then_some(change)
    }
}

impl Drop for SizeLease<'_> {
    fn drop(&mut self) {
        let Some(entry) = self.book.get(self.term.id) else {
            return;
        };
        let (book, term) = (self.book, self.term);
        let empty = {
            let mut holders = lock_holders(&entry);
            if holders.dead {
                return;
            }
            book.reconcile(&mut holders, term);
            if let Some(at) = holders.stack.iter().position(|(t, _)| *t == self.token) {
                let was_holder = at + 1 == holders.stack.len();
                holders.stack.remove(at);
                if was_holder {
                    holders.epoch = book.next();
                    match holders.stack.last().map(|(_, size)| *size) {
                        // The peer before it gets the size back, at its own size.
                        // Not applied, the terminal keeps the size it had.
                        Some(size) => {
                            if term.resize(size.cols, size.rows) {
                                holders.effective = Some(size);
                            }
                        }
                        // No peer holds it: the host's window has it again.
                        None => {
                            holders.effective = None;
                            term.session.set_remote_size_control(false);
                        }
                    }
                    book.settle(&mut holders, term);
                }
            }
            holders.stack.is_empty()
        };
        if empty {
            book.forget_if_empty(term.id, &entry);
        }
    }
}

impl HostService {
    /// Files being sent to peers and chat attachments being received from
    /// them, now.
    #[must_use]
    pub fn transfers_in_progress(&self) -> usize {
        self.sends.used.load(Ordering::SeqCst) + self.uploads.used.load(Ordering::SeqCst)
    }

    /// The listing for a peer and what it lets that peer name: the first
    /// [`MAX_PROJECTS`] open projects in the host's order, then the first
    /// [`MAX_LISTED`] terminals of those projects, in order. A parent cut by
    /// the cap is dropped, so no listed terminal names one that is not listed.
    ///
    /// Terminals not started since the host launched go only to a peer that
    /// asked for the host's ids ([`FEATURE_TERMINAL_IDS`], `all`); to any
    /// other they are left out before the cap, as before them, so an older
    /// peer still sees, and may name, every terminal it saw before.
    pub(crate) fn listing(&self, all_terminals: bool) -> (Listing, Shown) {
        let projects: Vec<ProjectInfo> = self
            .terminals
            .projects()
            .into_iter()
            .take(MAX_PROJECTS)
            .collect();
        let open: HashSet<String> = projects.iter().map(|p| p.id.clone()).collect();
        let mut all = self.terminals.list();
        all.retain(|t| open.contains(&t.project_id) && (all_terminals || t.ran));
        all.truncate(MAX_LISTED);
        let kept: HashSet<u32> = all.iter().map(|t| t.id).collect();
        for term in &mut all {
            if term.parent.is_some_and(|parent| !kept.contains(&parent)) {
                term.parent = None;
            }
            if term.owner.is_some_and(|owner| !kept.contains(&owner)) {
                term.owner = None;
            }
        }
        let terminals = all.iter().map(HostTerminal::info).collect();
        let grants = all.iter().map(|t| (t.id, t.project_id.clone())).collect();
        (
            Listing {
                projects,
                terminals,
            },
            Shown {
                projects: open,
                terminals: grants,
            },
        )
    }

    #[must_use]
    pub fn new(
        terminals: Arc<dyn HostTerminals>,
        keys: KeySource,
        guard: Arc<dyn RemoteGuard>,
    ) -> Self {
        Self {
            terminals,
            keys,
            guard,
            peers: Slots::new(MAX_PEERS),
            uploads: Slots::new(MAX_UPLOADS),
            sends: Slots::new(MAX_SENDS),
            completions: Slots::new(MAX_COMPLETIONS),
            turn_checks: Slots::new(MAX_TURN_CHECKS),
            sizes: SizeBook::default(),
        }
    }

    /// Serve `conn` until it ends. Blocks; run it on a thread of its own.
    pub fn serve(&self, conn: &dyn Connection) {
        self.serve_with(conn, &|_| true);
    }

    /// As [`HostService::serve`], asking `admit` about this very connection's
    /// peer once it has authenticated, before it is told so or served: the
    /// caller learns which key this connection holds, whatever other
    /// connections from the same endpoint do, and refuses it (`false`) when
    /// that key was revoked while the handshake ran.
    pub fn serve_with(&self, conn: &dyn Connection, admit: &dyn Fn(&Peer) -> bool) {
        if !self.guard.enabled() {
            conn.close();
            return;
        }
        self.serve_inner(conn, admit);
        conn.close();
    }

    fn deny(&self, control: &dyn Stream, peer: &Peer, reason: DenyReason) {
        // Recorded first, so a peer that has seen the refusal has been logged.
        self.guard.connected(peer, Err(&reason.to_string()));
        let _ = control.send(&Message::Denied(reason));
    }

    fn serve_inner(&self, conn: &dyn Connection, admit: &dyn Fn(&Peer) -> bool) {
        let mut peer = Peer {
            fingerprint: String::new(),
            endpoint: conn.remote_id(),
        };
        let Ok(Some(control)) = conn.accept_stream(PEER_TIMEOUT) else {
            return;
        };
        let control: Arc<dyn Stream> = Arc::from(control);
        let Ok(binding) = conn.channel_binding() else {
            return self.deny(&*control, &peer, DenyReason::Protocol);
        };
        let mut auth = HostAuth::new(conn.local_id(), conn.remote_id(), binding, (self.keys)());
        match control.recv(PEER_TIMEOUT) {
            Ok(Some(Message::Hello { version })) => match auth.on_hello(version) {
                Ok(challenge) => {
                    if control.send(&challenge).is_err() {
                        return;
                    }
                }
                Err(reason) => return self.deny(&*control, &peer, reason),
            },
            Ok(_) => return self.deny(&*control, &peer, DenyReason::Protocol),
            Err(_) => return,
        }
        match control.recv(PEER_TIMEOUT) {
            Ok(Some(Message::AuthResponse {
                public_key,
                signature,
            })) => match auth.on_response(&public_key, &signature) {
                Ok(authed) => peer.fingerprint = authed.fingerprint,
                Err(reason) => return self.deny(&*control, &peer, reason),
            },
            Ok(_) => return self.deny(&*control, &peer, DenyReason::Protocol),
            Err(_) => return,
        }
        // Each peer is served on its own; only how many at once is limited.
        let Some(_lease) = self.peers.take() else {
            return self.deny(&*control, &peer, DenyReason::Busy);
        };
        if !admit(&peer) {
            return self.deny(&*control, &peer, DenyReason::NotAuthorized);
        }
        if control.send(&Message::AuthOk).is_err() {
            return;
        }
        self.guard.connected(&peer, Ok(()));

        let stop = AtomicBool::new(false);
        let shown = Mutex::new(Shown::default());
        // The features the peer asked for and this host granted.
        let features = AtomicU32::new(0);
        let transfers = PeerTransfers::default();
        let (peer, stop, this, shown, features, transfers) =
            (&peer, &stop, self, &shown, &features, &transfers);
        thread::scope(|scope| {
            let control = Arc::clone(&control);
            scope.spawn(move || this.control_loop(peer, &*control, stop, shown, features));
            while !stop.load(Ordering::SeqCst) {
                match conn.accept_stream(POLL) {
                    Ok(Some(stream)) => {
                        scope.spawn(move || {
                            this.terminal_stream(peer, &*stream, stop, shown, features, transfers);
                        });
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
            stop.store(true, Ordering::SeqCst);
        });
    }

    fn control_loop(
        &self,
        peer: &Peer,
        control: &dyn Stream,
        stop: &AtomicBool,
        shown: &Mutex<Shown>,
        features: &AtomicU32,
    ) {
        while !stop.load(Ordering::SeqCst) {
            match control.recv(POLL) {
                Ok(Some(Message::ListTerminals)) => {
                    let granted = features.load(Ordering::SeqCst);
                    let (mut listing, sent) = self.listing(granted & FEATURE_TERMINAL_IDS != 0);
                    // Kept as it is handed to the wire, so the peer can act on
                    // nothing it has not been sent.
                    *lock(shown) = sent;
                    let reply = if granted & FEATURE_BACKGROUND == 0 {
                        for terminal in &mut listing.terminals {
                            terminal.background.clear();
                        }
                        Message::TerminalList(listing)
                    } else if granted & FEATURE_TERMINAL_IDS == 0 {
                        Message::TerminalListEx(listing)
                    } else if granted & FEATURE_TERMINAL_OWNERS == 0 {
                        Message::TerminalListIds(listing)
                    } else {
                        Message::TerminalListOwners(listing)
                    };
                    if control.send(&reply).is_err() {
                        break;
                    }
                }
                Ok(Some(Message::Manage { req, op })) => {
                    let outcome = if matches!(op, ManageOp::StopBackground { .. })
                        && features.load(Ordering::SeqCst) & FEATURE_BACKGROUND == 0
                    {
                        Err(DenyReason::Protocol.to_string())
                    } else {
                        self.manage(peer, &op, shown)
                    };
                    if control
                        .send(&Message::ManageResult { req, outcome })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Some(Message::Bye)) | Err(_) => break,
                Ok(Some(_)) => {
                    let _ = control.send(&Message::Denied(DenyReason::Protocol));
                }
                Ok(None) => {}
            }
        }
        stop.store(true, Ordering::SeqCst);
    }

    /// One management request, under the rule on [`Shown`]: its target must be in
    /// the connection's latest list. The guard (the Gate and the audit log, as the
    /// `remote` actor, and Pause) runs next, and the right is checked again after
    /// it. A close also needs every terminal under the target to be one the list
    /// sent, or nothing is closed. An `Add` is dispatched with the project it was
    /// granted, whatever the peer named.
    fn manage(
        &self,
        peer: &Peer,
        op: &ManageOp,
        shown: &Mutex<Shown>,
    ) -> Result<Option<u32>, String> {
        let sent = lock(shown).clone();
        let (action, right, refusal) = match op {
            ManageOp::Add { project, .. } => {
                let project = project.clone().or_else(|| self.terminals.active_project());
                let right = project.and_then(|p| self.authorize(&sent, Target::Project(p)));
                ("add", right, NO_SUCH_PROJECT.to_owned())
            }
            ManageOp::Close { terminal } => (
                "close",
                self.terminal_right(&sent, *terminal),
                no_such_terminal(),
            ),
            ManageOp::Restart { terminal } => (
                "restart",
                self.terminal_right(&sent, *terminal),
                no_such_terminal(),
            ),
            ManageOp::Rename { terminal, .. } => (
                "rename",
                self.terminal_right(&sent, *terminal),
                no_such_terminal(),
            ),
            ManageOp::StopBackground { terminal, .. } => (
                "stop background",
                self.terminal_right(&sent, *terminal),
                no_such_terminal(),
            ),
        };
        let right = right.ok_or_else(|| refusal.clone())?;
        let (name, attempt) = match right.target() {
            Target::Terminal { .. } => self
                .current_terminal(&right)
                .map_or((String::new(), 0), |t| (t.name, t.attempt)),
            Target::Project(_) => (String::new(), 0),
        };
        self.guard
            .manage(peer, action, &name, attempt)
            .map_err(|denied| deny_reason(denied).to_string())?;
        if !self.holds(shown, &right) {
            return Err(refusal);
        }
        // A close is decided where it runs: the registry closes the target and
        // everything under it only if all of it is in the connection's list.
        if let ManageOp::Close { .. } = op {
            let listed = right
                .target()
                .project()
                .map(|project| sent.listed_in(project))
                .unwrap_or_default();
            return self.terminals.close_within(&right, &listed);
        }
        // An add names the project it was granted, whatever the peer named.
        let resolved = match (op, right.target()) {
            (
                ManageOp::Add {
                    tool, name, model, ..
                },
                Target::Project(project),
            ) => ManageOp::Add {
                tool: *tool,
                name: name.clone(),
                project: Some(project.clone()),
                model: model.clone(),
            },
            _ => op.clone(),
        };
        self.terminals.manage(&resolved, &right)
    }

    /// The right to act on terminal `id` as the connection's latest list sent it.
    fn terminal_right(&self, sent: &Shown, id: u32) -> Option<Right> {
        let project = sent.terminal_project(id)?.to_owned();
        self.authorize(sent, Target::Terminal { id, project })
    }

    /// The host's terminal that `right` names, as it is now.
    fn current_terminal(&self, right: &Right) -> Option<HostTerminal> {
        let Target::Terminal { id, project } = right.target() else {
            return None;
        };
        self.terminals
            .list()
            .into_iter()
            .find(|t| t.id == *id && t.project_id == *project)
    }

    /// Sends a control message to the peer about a target, if the right still
    /// holds, with the listing held. The send never waits: a peer whose queue is
    /// full is not reading, and a control reply that cannot be queued ends the
    /// stream.
    fn deliver(
        &self,
        shown: &Mutex<Shown>,
        right: &Right,
        stream: &dyn Stream,
        message: &Message,
    ) -> Result<(), Held> {
        match self.try_deliver(shown, right, stream, message) {
            Ok(()) => Ok(()),
            Err(Attempt::Held(held)) => Err(held),
            Err(Attempt::Full) => Err(Held::Failed),
        }
    }

    /// Sends output or a repaint, waiting while the peer's queue is full: a slow
    /// peer is still a live one. The right is checked on every attempt, and the
    /// wait is outside the listing lock. `keep_going` ends the wait when the
    /// stream is being torn down.
    fn deliver_retrying(
        &self,
        shown: &Mutex<Shown>,
        right: &Right,
        stream: &dyn Stream,
        message: &Message,
        keep_going: &dyn Fn() -> bool,
    ) -> Result<(), Held> {
        loop {
            if !keep_going() {
                return Err(Held::Failed);
            }
            match self.try_deliver(shown, right, stream, message) {
                Ok(()) => return Ok(()),
                Err(Attempt::Held(held)) => return Err(held),
                Err(Attempt::Full) => thread::sleep(RETRY_WAIT),
            }
        }
    }

    fn try_deliver(
        &self,
        shown: &Mutex<Shown>,
        right: &Right,
        stream: &dyn Stream,
        message: &Message,
    ) -> Result<(), Attempt> {
        self.try_deliver_carrying(shown, right, &[], stream, message)
    }

    /// [`Self::try_deliver`] of a project's answer that carries text of the
    /// terminals `carried` (wire ids): each must still be the peer's, checked
    /// with the listing held through the send.
    fn try_deliver_carrying(
        &self,
        shown: &Mutex<Shown>,
        right: &Right,
        carried: &[u32],
        stream: &dyn Stream,
        message: &Message,
    ) -> Result<(), Attempt> {
        let sent = lock(shown);
        if !self.holds_in(&sent, right) {
            return Err(Attempt::Held(Held::Revoked));
        }
        if let Target::Project(project) = right.target()
            && !carried.iter().all(|wire| {
                self.authorize(
                    &sent,
                    Target::Terminal {
                        id: *wire,
                        project: project.clone(),
                    },
                )
                .is_some()
            })
        {
            return Err(Attempt::Held(Held::Revoked));
        }
        stream.send(message).map_err(|err| match err {
            NetError::Full => Attempt::Full,
            _ => Attempt::Held(Held::Failed),
        })
    }

    /// Runs `act` on the terminal, if `right` still holds, with the listing held.
    fn act_held<T>(
        &self,
        shown: &Mutex<Shown>,
        right: &Right,
        act: impl FnOnce() -> T,
    ) -> Option<T> {
        let sent = lock(shown);
        self.holds_in(&sent, right).then(act)
    }

    fn holds(&self, shown: &Mutex<Shown>, right: &Right) -> bool {
        self.holds_in(&lock(shown), right)
    }

    fn holds_in(&self, sent: &Shown, right: &Right) -> bool {
        self.authorize(sent, right.target.clone()).is_some()
    }

    /// The only way to obtain a [`Right`]: the target must be in the connection's
    /// latest list under the identity it was sent with, and it must still be the
    /// host's.
    pub(crate) fn authorize(&self, sent: &Shown, target: Target) -> Option<Right> {
        let live = match &target {
            Target::Terminal { id, project } => {
                sent.terminal_project(*id) == Some(project.as_str())
                    && self
                        .terminals
                        .list()
                        .iter()
                        .any(|t| t.id == *id && t.project_id == *project)
            }
            Target::Project(id) => {
                sent.has_project(id) && self.terminals.projects().iter().any(|p| p.id == *id)
            }
        };
        live.then_some(Right { target })
    }

    /// Ends a stream whose right was revoked: the peer is told, and nothing
    /// more is sent. Its size lease is released when the stream returns.
    fn end_revoked(stream: &dyn Stream) {
        let _ = stream.send(&Message::Denied(DenyReason::NoSuchTerminal));
    }

    /// Ends a stream whose terminal was replaced while the peer was not reading.
    /// The peer is told, for a short while, as far as its queue takes it: the
    /// refusal is what makes it attach the new terminal from the start. Then the
    /// stream ends either way, so the replacement attachment can proceed.
    fn retire(stream: &dyn Stream) {
        let deadline = Instant::now() + RETIRE_TELL;
        while Instant::now() < deadline {
            match stream.send(&Message::Denied(DenyReason::NoSuchTerminal)) {
                Err(NetError::Full) => thread::sleep(RETRY_WAIT),
                _ => return,
            }
        }
    }

    /// A peer's chat completion, from this machine's conversation model:
    /// while remote access is on and the guard allows it, a few at a time.
    fn completion(&self, peer: &Peer, request: &Completion) -> Message {
        if !self.guard.enabled() {
            return Message::Denied(DenyReason::NoSuchTerminal);
        }
        if let Err(why) = self.guard.manage(peer, "assistant", "", 0) {
            return Message::Denied(deny_reason(why));
        }
        let Some(_slot) = self.completions.take() else {
            return Message::Completed(Err("the conversation model is busy".to_owned()));
        };
        Message::Completed(self.terminals.complete(request))
    }

    /// A peer's turn check, from this machine's Jev: allowed as completions
    /// are, a few at a time.
    fn turn_check(&self, peer: &Peer, text: &str, context: &str, wait_ms: u32) -> Message {
        use crate::wire::TurnVerdict;
        if !self.guard.enabled() {
            return Message::Denied(DenyReason::NoSuchTerminal);
        }
        if let Err(why) = self.guard.manage(peer, "assistant", "", 0) {
            return Message::Denied(deny_reason(why));
        }
        let Some(_slot) = self.turn_checks.take() else {
            return Message::TurnChecked(TurnVerdict::Failed("busy".to_owned()));
        };
        let wait = Duration::from_millis(u64::from(wait_ms)).min(MAX_TURN_WAIT);
        Message::TurnChecked(match self.terminals.judge_turn(text, context, wait) {
            Ok(scores) => TurnVerdict::Judged {
                complete: percent(scores.complete),
                fits: scores.fits.map(percent),
            },
            Err(verdict) => verdict,
        })
    }

    /// The models of `tool` in `project`, for a project the connection was
    /// sent. Only what fits a frame is sent: names a launch could take, and
    /// at most [`MAX_MODELS`] of them.
    fn models_reply(&self, shown: &Mutex<Shown>, tool: u8, project: String) -> Message {
        if !self.guard.enabled()
            || self
                .authorize(&lock(shown), Target::Project(project.clone()))
                .is_none()
        {
            return Message::Denied(DenyReason::NoSuchTerminal);
        }
        let mut models = self.terminals.models(tool, &project);
        let fits = |model: &String| !model.is_empty() && model.len() <= MAX_STR;
        models.models.retain(fits);
        models.models.truncate(MAX_MODELS);
        models.default = models.default.filter(fits);
        Message::Models(models)
    }

    fn attachment_terminal(
        &self,
        shown: &Mutex<Shown>,
        terminal: u32,
    ) -> Option<(Right, HostTerminal)> {
        self.terminals.attachment_root()?;
        let right = self.terminal_right(&lock(shown), terminal)?;
        let term = self.current_terminal(&right)?;
        (self.guard.enabled()
            && term.name == connecto_core::session::APP_SESSION
            && term.parent.is_none()
            && takes_attachments(term.tool))
        .then_some((right, term))
    }

    /// Sends the file a peer asked for, if the terminal it names is still
    /// the connection's and the guard lets it (a pause refuses it, and each
    /// read is recorded). One file is sent to a peer at a time, and at most
    /// [`MAX_SENDS`] across peers. The send stops when the
    /// stream ends, the terminal leaves the peer's list, hosting is turned
    /// off, Pause is turned on, or the transfer deadline passes; the peer
    /// reads a stream ended short as a failure.
    #[allow(clippy::too_many_arguments)]
    fn send_file(
        &self,
        peer: &Peer,
        stream: &dyn Stream,
        stop: &AtomicBool,
        shown: &Mutex<Shown>,
        transfers: &PeerTransfers,
        terminal: u32,
        path: &str,
    ) {
        let right = self
            .guard
            .enabled()
            .then(|| self.terminal_right(&lock(shown), terminal))
            .flatten();
        let Some((right, term)) =
            right.and_then(|right| Some((right.clone(), self.current_terminal(&right)?)))
        else {
            let _ = stream.send(&Message::Denied(DenyReason::NoSuchTerminal));
            return;
        };
        if let Err(why) = self
            .guard
            .manage(peer, "read-file", &term.name, term.attempt)
        {
            let _ = stream.send(&Message::Denied(deny_reason(why)));
            return;
        }
        self.send_opened(stream, stop, shown, transfers, &right, || {
            self.terminals.open_file(&right, path)
        });
    }

    /// Sends a file opened for a peer, one file at a time per peer and at
    /// most [`MAX_SENDS`] across peers: `FileStart` with the path to show,
    /// then its bytes in chunks while the right holds, or `FileRefused`.
    fn send_opened(
        &self,
        stream: &dyn Stream,
        stop: &AtomicBool,
        shown: &Mutex<Shown>,
        transfers: &PeerTransfers,
        right: &Right,
        open: impl FnOnce() -> Result<(String, std::fs::File), String>,
    ) {
        let refuse = |why: &str| {
            let _ = stream.send(&Message::FileRefused(cut(why, MAX_STR).to_owned()));
        };
        // The last transfer may be waiting for its peer to close: a moment's
        // wait, then a refusal.
        let wait_until = Instant::now() + Duration::from_secs(2);
        let _lease = loop {
            if let Some(lease) = take_transfer(&transfers.sending, &self.sends) {
                break lease;
            }
            if stop.load(Ordering::SeqCst) || Instant::now() >= wait_until {
                return refuse("that machine is already sending a file: try again in a moment");
            }
            thread::sleep(Duration::from_millis(20));
        };
        let (shown_path, reader) = match open() {
            Ok(opened) => opened,
            Err(why) => return refuse(&why),
        };
        let bytes = match reader.metadata() {
            Ok(meta) if meta.is_file() => meta.len(),
            _ => return refuse("that file cannot be read on that machine"),
        };
        if bytes > MAX_FILE_BYTES {
            return refuse(&format!(
                "that file is {} MB, and at most {} MB are sent",
                bytes.div_ceil(1024 * 1024),
                MAX_FILE_BYTES / (1024 * 1024)
            ));
        }
        if shown_path.len() > MAX_PATH {
            return refuse("that file's path is too long");
        }
        let deadline = Instant::now() + crate::attachments::TRANSFER_TIMEOUT;
        let keep_going = || {
            !stop.load(Ordering::SeqCst)
                && Instant::now() < deadline
                && self.guard.enabled()
                && !self.guard.paused()
        };
        let start = Message::FileStart {
            path: shown_path,
            bytes,
        };
        if self
            .deliver_retrying(shown, right, stream, &start, &keep_going)
            .is_err()
        {
            return;
        }
        // Read by offset: an offer's handle may be sending to another peer
        // at the same time, and the two never share a position.
        let mut chunk = vec![0; MAX_DATA];
        let mut sent = 0;
        while sent < bytes {
            let want = usize::try_from(bytes - sent).map_or(MAX_DATA, |left| left.min(MAX_DATA));
            let read = match read_at(&reader, &mut chunk[..want], sent) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            sent += read as u64;
            let message = Message::FileChunk(chunk[..read].to_vec());
            if self
                .deliver_retrying(shown, right, stream, &message, &keep_going)
                .is_err()
            {
                return;
            }
        }
        // The transfer holds its place until the peer has read it all and
        // closed the stream, so another cannot queue more behind it.
        while keep_going() {
            match stream.recv(POLL) {
                Ok(None) => {}
                _ => return,
            }
        }
    }

    /// The offers in projects of the connection's latest list that the host
    /// still has open, newest last, at most [`MAX_OFFERS`], from `sent`.
    fn offers_in(&self, sent: &Shown) -> Vec<OfferInfo> {
        let live: HashSet<String> = self
            .terminals
            .projects()
            .into_iter()
            .map(|p| p.id)
            .collect();
        let mut offers: Vec<OfferInfo> = self
            .terminals
            .offers()
            .into_iter()
            .filter(|offer| sent.has_project(&offer.project) && live.contains(&offer.project))
            .collect();
        let excess = offers.len().saturating_sub(MAX_OFFERS);
        offers.drain(..excess);
        offers
    }

    fn listed_offers(&self, shown: &Mutex<Shown>) -> Vec<OfferInfo> {
        self.offers_in(&lock(shown))
    }

    /// Answers a probe of the secret requests with those of the projects
    /// the connection's list sent and the host still has. Pause refuses it:
    /// a request names a secret, a terminal and its reason. A request never
    /// holds a value, and is answered through a `ProjectAsk`, which the
    /// guard records (and Pause refuses).
    fn send_secret_asks(&self, shown: &Mutex<Shown>, stream: &dyn Stream) {
        let sent = lock(shown);
        let reply = if self.guard.paused() {
            Message::Denied(DenyReason::Paused)
        } else if self.guard.enabled() {
            let live: HashSet<String> = self
                .terminals
                .projects()
                .into_iter()
                .map(|p| p.id)
                .filter(|id| sent.has_project(id))
                .collect();
            let live: Vec<String> = live.into_iter().collect();
            Message::SecretAsks(crate::remote_secrets::asks_json(
                &self.terminals.secret_asks(),
                &live,
            ))
        } else {
            Message::Denied(DenyReason::Disabled)
        };
        let _ = stream.send(&reply);
    }

    /// Answers an offers probe with the listing held, as every other send
    /// about the connection's projects is: a project the list drops or the
    /// host closes meanwhile is never named.
    fn send_offers(&self, shown: &Mutex<Shown>, stream: &dyn Stream) {
        let sent = lock(shown);
        let reply = if self.guard.enabled() {
            Message::Offers(self.offers_in(&sent))
        } else {
            Message::Denied(DenyReason::Disabled)
        };
        let _ = stream.send(&reply);
    }

    /// Sends the file of an offer in a project of the connection's list,
    /// once the guard has let it (Pause refuses it; each is recorded).
    fn send_offer(
        &self,
        peer: &Peer,
        stream: &dyn Stream,
        stop: &AtomicBool,
        shown: &Mutex<Shown>,
        transfers: &PeerTransfers,
        id: u64,
    ) {
        let offer = self.listed_offers(shown).into_iter().find(|o| o.id == id);
        let right = offer
            .as_ref()
            .filter(|_| self.guard.enabled())
            .and_then(|offer| self.authorize(&lock(shown), Target::Project(offer.project.clone())));
        let (Some(offer), Some(right)) = (offer, right) else {
            let _ = stream.send(&Message::FileRefused(NO_SUCH_OFFER.to_owned()));
            return;
        };
        if let Err(why) = self.guard.manage(peer, "read-offer", &offer.name, 0) {
            let _ = stream.send(&Message::Denied(deny_reason(why)));
            return;
        }
        self.send_opened(stream, stop, shown, transfers, &right, || {
            self.terminals.open_offer(id)
        });
    }

    /// Answers a peer's [`Message::ProjectAsk`]: only for a project of the
    /// connection's latest list, only while hosting is on, and only once the
    /// guard has let the ask through (Pause refuses it, and each is
    /// recorded). The answer stops when the stream ends, the project leaves
    /// the peer's list, hosting is turned off or Pause is turned on.
    fn answer_project(
        &self,
        peer: &Peer,
        stream: &dyn Stream,
        stop: &AtomicBool,
        shown: &Mutex<Shown>,
        project: String,
        ask: &[u8],
    ) {
        use crate::project_panels::{MAX_ANSWER, ProjectAsk};
        let refuse = |why: &str| {
            let _ = stream.send(&Message::FileRefused(cut(why, MAX_STR).to_owned()));
        };
        let right = self
            .guard
            .enabled()
            .then(|| self.authorize(&lock(shown), Target::Project(project.clone())))
            .flatten();
        let Some(right) = right else {
            let _ = stream.send(&Message::Denied(DenyReason::NoSuchTerminal));
            return;
        };
        let ask = match ProjectAsk::decode(ask) {
            Ok(ask) => ask,
            Err(why) => return refuse(&why),
        };
        // An answer to a request is recorded by the secret it names, as
        // the host has the request; never by its value.
        let audited = match &ask {
            ProjectAsk::SecretsAct(crate::remote_secrets::SecretsAction::Answer {
                ask: id,
                ..
            }) => self
                .terminals
                .secret_asks()
                .into_iter()
                .find(|a| a.id == *id && a.project.as_deref() == Some(project.as_str()))
                .map_or_else(
                    || ask.audited(&project),
                    |a| format!("{project}/secret:{} (request {id})", a.name),
                ),
            _ => ask.audited(&project),
        };
        if let Err(why) = self.guard.manage(peer, ask.action(), &audited, 0) {
            let _ = stream.send(&Message::Denied(deny_reason(why)));
            return;
        }
        // The guard may have taken time: everything it decided on is checked
        // again just before the ask reaches the project, so a pause, hosting
        // turned off, a closed stream or a project gone meanwhile changes
        // nothing there.
        if stop.load(Ordering::SeqCst) || !self.guard.enabled() {
            let _ = stream.send(&Message::Denied(DenyReason::Disabled));
            return;
        }
        if self.guard.paused() {
            let _ = stream.send(&Message::Denied(DenyReason::Paused));
            return;
        }
        if !self.holds(shown, &right) {
            return Self::end_revoked(stream);
        }
        let caller = PeerCaller {
            host: self,
            peer,
            shown,
            right: &right,
            stop,
            typed: std::cell::RefCell::default(),
        };
        let answer = match self.terminals.project_ask_by(&right, &ask, &caller) {
            Ok(answer) => answer,
            Err(why) => return refuse(&why),
        };
        let bytes = answer.len() as u64;
        if bytes > MAX_ANSWER {
            return refuse("that answer is too large to send");
        }
        // A list of the terminals' questions carries those terminals' text:
        // each must still be the peer's for as long as it is delivered.
        let carried = if matches!(ask, crate::project_panels::ProjectAsk::Questions) {
            crate::voice_answers::carried_terminals(&answer)
        } else {
            Vec::new()
        };
        let deadline = Instant::now() + crate::attachments::TRANSFER_TIMEOUT;
        let keep_going = || {
            !stop.load(Ordering::SeqCst)
                && Instant::now() < deadline
                && self.guard.enabled()
                && !self.guard.paused()
        };
        // Each piece is sent with the listing held from the grants' check
        // to the send: a terminal revoked between them is not sent its text.
        let deliver = |message: &Message| -> Result<(), Held> {
            loop {
                if !keep_going() {
                    return Err(Held::Failed);
                }
                match self.try_deliver_carrying(shown, &right, &carried, stream, message) {
                    Ok(()) => return Ok(()),
                    Err(Attempt::Held(held)) => return Err(held),
                    Err(Attempt::Full) => thread::sleep(RETRY_WAIT),
                }
            }
        };
        let start = Message::FileStart {
            path: String::new(),
            bytes,
        };
        if deliver(&start).is_err() {
            return;
        }
        for chunk in answer.chunks(MAX_DATA) {
            if deliver(&Message::FileChunk(chunk.to_vec())).is_err() {
                return;
            }
        }
        // Held open until the peer has read it all and closed the stream.
        while keep_going() {
            match stream.recv(POLL) {
                Ok(None) => {}
                _ => return,
            }
        }
    }

    fn receive_attachment(
        &self,
        peer: &Peer,
        stream: &dyn Stream,
        stop: &AtomicBool,
        shown: &Mutex<Shown>,
        transfers: &PeerTransfers,
        message: Message,
    ) -> Result<String, String> {
        const TRANSFER_FAILED: &str = "The attachment transfer was interrupted. Try again.";
        const REFUSED: &str = "The host no longer allows attachments for this session.";
        let Message::AttachmentStart {
            terminal,
            project,
            token,
            name,
            bytes,
        } = message
        else {
            return Err(TRANSFER_FAILED.to_owned());
        };
        let bytes = bytes as usize;
        if bytes > MAX_ATTACHMENT_BYTES
            || !crate::attachments::valid_token(&token)
            || !crate::remote_terminals::is_canonical_project_id(&project)
            || name.is_empty()
            || name.chars().any(char::is_control)
            || name.contains(['/', '\\'])
        {
            return Err("The attachment is invalid.".to_owned());
        }
        let (right, term) = self.attachment_terminal(shown, terminal).ok_or(REFUSED)?;
        if term.project_id != project {
            return Err(REFUSED.to_owned());
        }
        let Some(_lease) = take_transfer(&transfers.uploading, &self.uploads) else {
            return Err("The host is already receiving an attachment. Try again.".to_owned());
        };
        stream
            .send(&Message::AttachmentReady)
            .map_err(|_| TRANSFER_FAILED)?;
        let mut received = Vec::with_capacity(bytes);
        receive_chunks(stream, stop, bytes, &|| true, &mut |chunk| {
            received.extend_from_slice(chunk);
            true
        })
        .ok_or(TRANSFER_FAILED)?;
        if stop.load(Ordering::SeqCst) || !self.guard.enabled() {
            return Err(REFUSED.to_owned());
        }
        self.act_held(shown, &right, || {
            let live = self.current_terminal(&right).ok_or(REFUSED)?;
            if live.attempt != term.attempt
                || live.name != connecto_core::session::APP_SESSION
                || live.parent.is_some()
            {
                return Err(REFUSED.to_owned());
            }
            self.guard
                .manage(peer, "attach-file", &term.name, term.attempt)
                .map_err(|why| match deny_reason(why) {
                    DenyReason::Paused => {
                        "The host paused remote actions. Try again when it resumes."
                    }
                    _ => REFUSED,
                })?;
            let root = self.terminals.attachment_root().ok_or(REFUSED)?;
            crate::attachments::store(root, &project, &peer.endpoint, &token, &name, &received)
        })
        .ok_or_else(|| REFUSED.to_owned())?
    }

    /// Receives a file a peer dropped or pasted on terminal `terminal` and
    /// saves it in that terminal's project's attachments folder: the host's
    /// path to it, which the peer then types into the terminal as input. It
    /// takes what that peer's input to the terminal takes: the terminal is
    /// in the connection's latest list, hosting is on, and the guard allows
    /// it (Pause refuses it, and it is recorded), checked again before the
    /// file is saved. One upload per peer, as for chat attachments.
    #[allow(clippy::too_many_arguments)]
    fn receive_terminal_upload(
        &self,
        peer: &Peer,
        stream: &dyn Stream,
        stop: &AtomicBool,
        shown: &Mutex<Shown>,
        transfers: &PeerTransfers,
        terminal: u32,
        name: &str,
        bytes: u32,
        large: bool,
    ) -> Result<String, String> {
        const TRANSFER_FAILED: &str = "The file transfer was interrupted. Try again.";
        const REFUSED: &str = "The host no longer lets this computer type into that terminal.";
        const PAUSED: &str = "The host paused remote actions. Try again when it resumes.";
        let bytes = bytes as usize;
        // A connection that did not ask for large uploads is held to what
        // every host took before them, as its peer expects.
        let limit = if large {
            MAX_UPLOAD_BYTES
        } else {
            crate::wire::LEGACY_UPLOAD_BYTES
        };
        if bytes > limit {
            return Err(format!(
                "The file is larger than the {} MiB a host's terminal takes.",
                limit / (1024 * 1024)
            ));
        }
        if name.is_empty()
            || name.len() > MAX_STR
            || name.chars().any(char::is_control)
            || name.contains(['/', '\\'])
        {
            return Err("The file's name is not one the host takes.".to_owned());
        }
        if !self.guard.enabled() {
            return Err(REFUSED.to_owned());
        }
        let right = self.terminal_right(&lock(shown), terminal).ok_or(REFUSED)?;
        let term = self.current_terminal(&right).ok_or(REFUSED)?;
        let Some(root) = self.terminals.attachment_root() else {
            return Err("The host cannot keep files for its terminals.".to_owned());
        };
        self.guard
            .manage(peer, "upload-file", &term.name, term.attempt)
            .map_err(|why| match deny_reason(why) {
                DenyReason::Paused => PAUSED,
                _ => REFUSED,
            })?;
        let Some(_lease) = take_transfer(&transfers.uploading, &self.uploads) else {
            return Err("The host is already receiving a file. Try again.".to_owned());
        };
        // Written to disk as it arrives, so the host holds one chunk of it,
        // not up to 100 MiB per upload; refused before it is sent when the
        // project's storage has no room for it.
        let mut incoming =
            crate::attachments::Incoming::begin(root, &term.project_id, bytes, MAX_UPLOAD_BYTES)?;
        stream
            .send(&Message::AttachmentReady)
            .map_err(|_| TRANSFER_FAILED)?;
        let keep_going = || self.guard.enabled() && !self.guard.paused();
        let mut failed = None;
        let received = receive_chunks(stream, stop, bytes, &keep_going, &mut |chunk| {
            incoming
                .write(chunk)
                .map_err(|why| failed = Some(why))
                .is_ok()
        });
        if self.guard.paused() {
            return Err(PAUSED.to_owned());
        }
        if let Some(why) = failed {
            return Err(why);
        }
        received.ok_or(TRANSFER_FAILED)?;
        if stop.load(Ordering::SeqCst) || !self.guard.enabled() {
            return Err(REFUSED.to_owned());
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| TRANSFER_FAILED)?;
        let token = format!("{:032x}", u128::from_be_bytes(random));
        // Saved only into the terminal the upload began on (a restart makes
        // a new session and ring, whatever its attempt), and only while
        // hosting is on and not paused once the save's turn has come.
        self.act_held(shown, &right, || {
            let live = self.current_terminal(&right).ok_or(REFUSED)?;
            if live.attempt != term.attempt || !Arc::ptr_eq(&live.ring, &term.ring) {
                return Err(REFUSED.to_owned());
            }
            // Asked once the save's turn has come and just before it is
            // published: hosting still on, not paused, the stream not
            // ended, and the same terminal session still there.
            let allowed = || {
                self.guard.enabled()
                    && !self.guard.paused()
                    && !stop.load(Ordering::SeqCst)
                    && self.current_terminal(&right).is_some_and(|now| {
                        now.attempt == term.attempt && Arc::ptr_eq(&now.ring, &term.ring)
                    })
            };
            incoming
                .publish(&peer.endpoint, &token, name, &allowed)
                .map_err(|why| {
                    if why != crate::attachments::NOT_ALLOWED {
                        why
                    } else if self.guard.paused() {
                        PAUSED.to_owned()
                    } else {
                        REFUSED.to_owned()
                    }
                })
        })
        .ok_or_else(|| REFUSED.to_owned())?
    }

    fn terminal_stream(
        &self,
        peer: &Peer,
        stream: &dyn Stream,
        stop: &AtomicBool,
        shown: &Mutex<Shown>,
        features: &AtomicU32,
        transfers: &PeerTransfers,
    ) {
        let (terminal, resume_seq) = match stream.recv(PEER_TIMEOUT) {
            Ok(Some(Message::Attach {
                terminal,
                resume_seq,
                ..
            })) => (terminal, resume_seq),
            Ok(Some(Message::AttachmentProbe { terminal })) => {
                let allowed = self.attachment_terminal(shown, terminal).is_some();
                let reply = if allowed {
                    Message::AttachmentReady
                } else {
                    Message::Denied(DenyReason::NoSuchTerminal)
                };
                let _ = stream.send(&reply);
                return;
            }
            Ok(Some(message @ Message::AttachmentStart { .. })) => {
                let reply = self.receive_attachment(peer, stream, stop, shown, transfers, message);
                let _ = stream.send(&Message::AttachmentResult(reply));
                return;
            }
            Ok(Some(Message::TerminalUpload {
                terminal,
                name,
                bytes,
            })) => {
                let large =
                    features.load(Ordering::SeqCst) & crate::wire::FEATURE_LARGE_UPLOADS != 0;
                let reply = self.receive_terminal_upload(
                    peer, stream, stop, shown, transfers, terminal, &name, bytes, large,
                );
                let _ = stream.send(&Message::AttachmentResult(reply));
                return;
            }
            Ok(Some(Message::ModelsProbe { tool, project })) => {
                let _ = stream.send(&self.models_reply(shown, tool, project));
                return;
            }
            Ok(Some(Message::FileRead { terminal, path })) => {
                self.send_file(peer, stream, stop, shown, transfers, terminal, &path);
                return;
            }
            Ok(Some(Message::Complete(request))) => {
                let _ = stream.send(&self.completion(peer, &request));
                return;
            }
            Ok(Some(Message::TurnCheck {
                text,
                context,
                wait_ms,
            })) => {
                let _ = stream.send(&self.turn_check(peer, &text, &context, wait_ms));
                return;
            }
            Ok(Some(Message::ProjectAsk { project, ask })) => {
                self.answer_project(peer, stream, stop, shown, project, &ask.0);
                return;
            }
            Ok(Some(Message::OffersProbe)) => {
                self.send_offers(shown, stream);
                return;
            }
            Ok(Some(Message::GroupsProbe)) => {
                let sent = lock(shown);
                let reply = if self.guard.enabled() {
                    let listed: Vec<String> = self
                        .terminals
                        .projects()
                        .into_iter()
                        .map(|p| p.id)
                        .filter(|id| sent.has_project(id))
                        .collect();
                    Message::Groups(crate::groups::groups_json(
                        &self.terminals.groups(),
                        &listed,
                    ))
                } else {
                    Message::Denied(DenyReason::Disabled)
                };
                drop(sent);
                let _ = stream.send(&reply);
                return;
            }
            Ok(Some(Message::SecretAsksProbe)) => {
                self.send_secret_asks(shown, stream);
                return;
            }
            Ok(Some(Message::OfferRead { id })) => {
                self.send_offer(peer, stream, stop, shown, transfers, id);
                return;
            }
            Ok(Some(Message::Features(asked))) => {
                // The ids come in the extended list, so only with it.
                // The owners come in the list with ids, so only with it.
                let mut granted =
                    asked & (FEATURE_BACKGROUND | FEATURE_TERMINAL_IDS | FEATURE_TERMINAL_OWNERS);
                if granted & FEATURE_BACKGROUND == 0 {
                    granted = 0;
                }
                if granted & FEATURE_TERMINAL_IDS == 0 {
                    granted &= !FEATURE_TERMINAL_OWNERS;
                }
                granted |= asked & crate::wire::FEATURE_OPENCODE;
                granted |= asked & crate::wire::FEATURE_LARGE_UPLOADS;
                features.store(granted, Ordering::SeqCst);
                let _ = stream.send(&Message::Features(granted));
                return;
            }
            _ => {
                let _ = stream.send(&Message::Denied(DenyReason::Protocol));
                return;
            }
        };
        let sent = lock(shown).clone();
        let Some(right) = self.terminal_right(&sent, terminal) else {
            let _ = stream.send(&Message::Denied(DenyReason::NoSuchTerminal));
            return;
        };
        let Some(term) = self.current_terminal(&right) else {
            let _ = stream.send(&Message::Denied(DenyReason::NoSuchTerminal));
            return;
        };
        if let Err(reason) = self.guard.attach(peer, &term.name, term.attempt) {
            let _ = stream.send(&Message::Denied(deny_reason(reason)));
            return;
        }
        // The guard may have taken time: the right is checked again before
        // anything is sent for it.
        if !self.holds(shown, &right) {
            Self::end_revoked(stream);
            return;
        }
        let lease = SizeLease::new(&term, &self.sizes);

        // A first attach, or a position the ring no longer holds, gets a
        // repaint. A position it holds is resumed exactly, at the size the
        // screen had there when the ring follows sizes: what the peer is
        // sent next was drawn at that size, and later sizes come in place.
        let resumed = (resume_seq > 0)
            .then(|| term.ring.resume_at(resume_seq))
            .flatten();
        let mut pos = At {
            seq: resume_seq,
            mark: 0,
        };
        let keep_going = || !stop.load(Ordering::SeqCst);
        let started = if let Some((at, size)) = resumed {
            pos = at;
            let (cols, rows) = size.map_or_else(|| term.grid(), |s| (s.cols, s.rows));
            self.deliver_retrying(
                shown,
                &right,
                stream,
                &Message::Attached {
                    start_seq: resume_seq,
                    resync: false,
                    cols,
                    rows,
                },
                &keep_going,
            )
            .map(|()| (cols, rows))
        } else {
            self.send_repaint(
                stream,
                &term,
                true,
                &mut pos,
                &Serve {
                    shown,
                    right: &right,
                    keep_going: &keep_going,
                },
            )
        };
        match started {
            Ok((cols, rows)) => lease.told(cols, rows),
            Err(Held::Revoked) => return Self::end_revoked(stream),
            Err(Held::Failed) => return,
        }
        match self.deliver(
            shown,
            &right,
            stream,
            &Message::Status(RemoteStatus::Running),
        ) {
            Ok(()) => {}
            Err(Held::Revoked) => return Self::end_revoked(stream),
            Err(Held::Failed) => return,
        }

        let acked = AtomicU64::new(pos.seq);
        let ended = AtomicBool::new(false);
        let link = PeerLink {
            ended: &ended,
            acked: &acked,
            shown,
            right: &right,
        };
        let lease_ref = &lease;
        let link_ref = &link;
        thread::scope(|scope| {
            scope.spawn(move || {
                self.read_peer(peer, stream, lease_ref, stop, link_ref);
            });
            self.write_peer(stream, lease_ref, stop, link_ref, &mut pos);
            ended.store(true, Ordering::SeqCst);
        });
        self.guard.detach(peer, &term.name, term.attempt);
    }

    /// Send a screen: the `Attached` reply when `first`, then the bytes. Each
    /// message is checked against the right when it is sent.
    fn send_repaint(
        &self,
        stream: &dyn Stream,
        term: &HostTerminal,
        first: bool,
        pos: &mut At,
        serve: &Serve<'_>,
    ) -> Result<(u16, u16), Held> {
        let Serve {
            shown,
            right,
            keep_going,
        } = *serve;
        let Some(captured) = self.act_held(shown, right, || term.capture()) else {
            return Err(Held::Revoked);
        };
        let Some((at, cols, rows, bytes)) = captured else {
            let _ = stream.send(&Message::Denied(DenyReason::Protocol));
            return Err(Held::Failed);
        };
        if first {
            self.deliver_retrying(
                shown,
                right,
                stream,
                &Message::Attached {
                    start_seq: at.seq,
                    resync: true,
                    cols,
                    rows,
                },
                keep_going,
            )?;
        }
        *pos = at;
        self.deliver_retrying(
            shown,
            right,
            stream,
            &Message::Resync {
                seq: at.seq,
                cols,
                rows,
                repaint: bytes,
            },
            keep_going,
        )?;
        Ok((cols, rows))
    }

    fn read_peer(
        &self,
        peer: &Peer,
        stream: &dyn Stream,
        lease: &SizeLease<'_>,
        stop: &AtomicBool,
        link: &PeerLink<'_>,
    ) {
        let (ended, acked, shown, right) = (link.ended, link.acked, link.shown, link.right);
        let term = lease.term;
        while !stop.load(Ordering::SeqCst) && !ended.load(Ordering::SeqCst) {
            match stream.recv(POLL) {
                Ok(Some(Message::ReadChat { revision }))
                    if term.name == connecto_core::session::APP_SESSION =>
                {
                    let Some(history) = self.act_held(shown, right, || term.session.chat_history())
                    else {
                        break;
                    };
                    if let Some(history) = history
                        && Some(history.revision) != revision
                        && self
                            .deliver(shown, right, stream, &Message::Chat((*history).clone()))
                            .is_err()
                    {
                        break;
                    }
                }
                Ok(Some(Message::Input(bytes))) => {
                    // Checked on arrival and again at the session: a revoked
                    // stream takes no input, and the write side ends it.
                    if !self.holds(shown, right) {
                        if stream
                            .send(&Message::Denied(DenyReason::NoSuchTerminal))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    match self.guard.input(peer, &term.name, term.attempt, &bytes) {
                        Ok(()) => {
                            match self.act_held(shown, right, || term.session.send_input(bytes)) {
                                None => {
                                    if stream
                                        .send(&Message::Denied(DenyReason::NoSuchTerminal))
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                Some(Err(_)) => {
                                    if stream.send(&Message::Denied(DenyReason::Busy)).is_err() {
                                        break;
                                    }
                                }
                                Some(Ok(())) => {}
                            }
                        }
                        Err(reason) => {
                            if stream.send(&Message::Denied(deny_reason(reason))).is_err() {
                                break;
                            }
                        }
                    }
                }
                Ok(Some(Message::Resize { cols, rows })) => {
                    if !self.holds(shown, right) {
                        if stream
                            .send(&Message::Denied(DenyReason::NoSuchTerminal))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    match self.guard.resize(peer, &term.name, term.attempt) {
                        Ok(()) => {
                            // From now until it detaches the peer has the size;
                            // the host's view follows it. Applied under the right;
                            // the size it has is sent by the stream's writer, the
                            // one sender of sizes, so no reply overtakes a newer one.
                            let applied = self.act_held(shown, right, || {
                                if RESIZE_POLICY.peer_may_resize() {
                                    lease.take(GridSize { cols, rows });
                                } else {
                                    lease.tell_anyway();
                                }
                            });
                            if applied.is_none()
                                && stream
                                    .send(&Message::Denied(DenyReason::NoSuchTerminal))
                                    .is_err()
                            {
                                break;
                            }
                        }
                        Err(reason) => {
                            if stream.send(&Message::Denied(deny_reason(reason))).is_err() {
                                break;
                            }
                        }
                    }
                }
                Ok(Some(Message::Ack { seq })) => {
                    acked.fetch_max(seq, Ordering::SeqCst);
                }
                Ok(Some(Message::Bye)) | Err(_) => break,
                Ok(Some(_)) => {
                    if stream.send(&Message::Denied(DenyReason::Protocol)).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
            }
        }
        ended.store(true, Ordering::SeqCst);
    }

    /// When the terminal's size is not the one the peer was last sent
    /// (another peer resized, the holder let go, or the host's view claimed it
    /// back), the peer is told the size now, so it draws what follows at that
    /// width. A full queue is waited out as output is, so a busy peer is not
    /// dropped for another's resize.
    fn tell_size_change(
        &self,
        stream: &dyn Stream,
        lease: &SizeLease<'_>,
        serve: &Serve<'_>,
        wait: bool,
    ) -> Result<(), Held> {
        let Some((cols, rows)) = lease.to_tell(wait) else {
            return Ok(());
        };
        self.deliver_retrying(
            serve.shown,
            serve.right,
            stream,
            &Message::Size { cols, rows },
            serve.keep_going,
        )?;
        lease.told(cols, rows);
        Ok(())
    }

    fn write_peer(
        &self,
        stream: &dyn Stream,
        lease: &SizeLease<'_>,
        stop: &AtomicBool,
        link: &PeerLink<'_>,
        pos: &mut At,
    ) {
        let term = lease.term;
        let (ended, acked, shown, right) = (link.ended, link.acked, link.shown, link.right);
        let mut told_exit = false;
        let alive = || {
            !stop.load(Ordering::SeqCst) && !ended.load(Ordering::SeqCst) && !term.ring.is_closed()
        };
        let sizes = Serve {
            shown,
            right,
            keep_going: &alive,
        };
        while !stop.load(Ordering::SeqCst) && !ended.load(Ordering::SeqCst) {
            // Every pass, idle or not: a revoked stream ends here, so its size
            // lease is released and nothing more is sent.
            if !self.holds(shown, right) {
                return Self::end_revoked(stream);
            }
            // A ring that follows the screen's size gives each size at its
            // byte (below); one that does not leaves the size to be asked of
            // the session, as before.
            let follows = term.ring.follows_size();
            if !follows {
                match self.tell_size_change(stream, lease, &sizes, false) {
                    Ok(()) => {}
                    Err(Held::Revoked) => return Self::end_revoked(stream),
                    Err(Held::Failed) if term.ring.is_closed() => return Self::retire(stream),
                    Err(Held::Failed) => return,
                }
            }
            if pos.seq.saturating_sub(acked.load(Ordering::SeqCst)) > SEND_WINDOW {
                // The peer is behind. Hold off; the ring keeps moving, and if
                // it moves past the peer, the peer is repainted below.
                thread::sleep(Duration::from_millis(2));
                if matches!(
                    term.ring.read(pos.seq, 0),
                    RingRead::Data { .. } | RingRead::Empty
                ) {
                    continue;
                }
            }
            match term.ring.read_wait_at(*pos, MAX_DATA, POLL) {
                RingRead::Size { mark, size } => {
                    // The screen took this size after the bytes the peer has
                    // and before the ones that follow: the peer takes it here.
                    match self.deliver_retrying(
                        shown,
                        right,
                        stream,
                        &Message::Size {
                            cols: size.cols,
                            rows: size.rows,
                        },
                        &alive,
                    ) {
                        Ok(()) => {
                            lease.told(size.cols, size.rows);
                            pos.mark = mark + 1;
                        }
                        Err(Held::Revoked) => return Self::end_revoked(stream),
                        Err(Held::Failed) if term.ring.is_closed() => {
                            return Self::retire(stream);
                        }
                        Err(Held::Failed) => return,
                    }
                }
                RingRead::Data { seq, bytes } => {
                    // The redraw a claim causes can arrive within this wait;
                    // the claim was counted before the PTY moved, so the size
                    // goes out ahead of it.
                    if !follows {
                        match self.tell_size_change(stream, lease, &sizes, true) {
                            Ok(()) => {}
                            Err(Held::Revoked) => return Self::end_revoked(stream),
                            Err(Held::Failed) if term.ring.is_closed() => {
                                return Self::retire(stream);
                            }
                            Err(Held::Failed) => return,
                        }
                    }
                    let len = bytes.len() as u64;
                    // A wait ends when the connection stops, the peer's reader ends, or
                    // the attached ring is closed (the terminal was restarted or
                    // closed): the stream is then retired, so its replacement can
                    // attach.
                    let keep_going = || {
                        !stop.load(Ordering::SeqCst)
                            && !ended.load(Ordering::SeqCst)
                            && !term.ring.is_closed()
                    };
                    match self.deliver_retrying(
                        shown,
                        right,
                        stream,
                        &Message::Output { seq, bytes },
                        &keep_going,
                    ) {
                        // The cursor moves only once the bytes are queued.
                        Ok(()) => pos.seq = seq + len,
                        Err(Held::Revoked) => return Self::end_revoked(stream),
                        Err(Held::Failed) if term.ring.is_closed() => {
                            return Self::retire(stream);
                        }
                        Err(Held::Failed) => return,
                    }
                }
                RingRead::Empty => {
                    // The terminal restarted: its ring was closed and a new
                    // one made. This peer is let go and attaches again.
                    if term.ring.is_closed() {
                        return Self::end_revoked(stream);
                    }
                    if !told_exit {
                        let status = match term.session.status() {
                            SessionStatus::Running => None,
                            SessionStatus::Exited { code, signal } => {
                                Some(RemoteStatus::Exited { code, signal })
                            }
                            SessionStatus::Unknown { .. } => Some(RemoteStatus::Unknown),
                        };
                        if let Some(status) = status {
                            told_exit = true;
                            match self.deliver(shown, right, stream, &Message::Status(status)) {
                                Ok(()) => {}
                                Err(Held::Revoked) => return Self::end_revoked(stream),
                                Err(Held::Failed) => return,
                            }
                        }
                    }
                }
                RingRead::Evicted { .. } | RingRead::Future { .. } => {
                    let keep_going = || {
                        !stop.load(Ordering::SeqCst)
                            && !ended.load(Ordering::SeqCst)
                            && !term.ring.is_closed()
                    };
                    let serve = Serve {
                        shown,
                        right,
                        keep_going: &keep_going,
                    };
                    match self.send_repaint(stream, term, false, pos, &serve) {
                        Ok((cols, rows)) => {
                            lease.told(cols, rows);
                            acked.store(pos.seq, Ordering::SeqCst);
                        }
                        Err(Held::Revoked) => return Self::end_revoked(stream),
                        Err(Held::Failed) if term.ring.is_closed() => return Self::retire(stream),
                        Err(Held::Failed) => return,
                    }
                }
            }
        }
    }
}

/// One attempt to send: the outcome, or that the peer's queue is full.
enum Attempt {
    Held(Held),
    Full,
}

/// How long a send waits before it tries a full queue again.
const RETRY_WAIT: Duration = Duration::from_millis(2);

/// How long a retired stream keeps trying to tell its peer why it ended.
const RETIRE_TELL: Duration = Duration::from_millis(200);

/// Why a send to a peer did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// The right no longer holds: the peer is told `NoSuchTerminal` and the
    /// stream ends.
    Revoked,
    /// The stream is gone.
    Failed,
}

fn lock(shown: &Mutex<Shown>) -> std::sync::MutexGuard<'_, Shown> {
    shown.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A [`HostTerminals`] over a fixed list, for tests and simple hosts.
pub struct FixedTerminals(pub Mutex<Vec<HostTerminal>>);

impl HostTerminals for FixedTerminals {
    fn list(&self) -> Vec<HostTerminal> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::takes_attachments;

    #[test]
    fn every_agent_takes_attachments_and_a_shell_does_not() {
        for (tool, takes) in [
            (1, true),
            (2, true),
            (3, false),
            (4, true),
            (5, true),
            (0, false),
        ] {
            assert_eq!(takes_attachments(tool), takes, "tool {tool}");
        }
    }
}
