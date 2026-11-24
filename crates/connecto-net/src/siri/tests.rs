use super::*;
use connecto_core::{
    Cell, GridSize, InputRefused, ResizeOutcome, SessionStatus, SnapshotOutcome, TerminalSession,
    ToolKind,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

#[derive(Default)]
struct Pane {
    chat: Mutex<ChatHistory>,
    screen: Mutex<String>,
    inputs: Mutex<Vec<Vec<u8>>>,
}

impl Pane {
    fn push(&self, user: bool, text: &str) {
        let mut chat = self.chat.lock().unwrap();
        chat.messages.push((user, text.to_owned()));
        chat.revision += 1;
    }

    fn typed(&self) -> usize {
        self.inputs.lock().unwrap().len()
    }
}

impl TerminalSession for Pane {
    fn chat_history(&self) -> Option<Arc<ChatHistory>> {
        Some(Arc::new(self.chat.lock().unwrap().clone()))
    }
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        let screen = self.screen.lock().unwrap().clone();
        let lines: Vec<&str> = screen.lines().collect();
        let cols = 100;
        out.cols = cols;
        out.rows = u16::try_from(lines.len()).unwrap();
        out.modes.bracketed_paste = true;
        out.cells = lines
            .iter()
            .flat_map(|l| {
                let mut row: Vec<Cell> = l
                    .chars()
                    .map(|ch| Cell {
                        ch,
                        ..Cell::default()
                    })
                    .collect();
                row.resize(usize::from(cols), Cell::default());
                row
            })
            .collect();
        SnapshotOutcome::Updated
    }
    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        self.inputs.lock().unwrap().push(bytes);
        Ok(())
    }
    fn resize(&self, _: GridSize) -> ResizeOutcome {
        unreachable!()
    }
    fn scroll(&self, _: i32) -> bool {
        unreachable!()
    }
    fn status(&self) -> SessionStatus {
        unreachable!()
    }
}

struct Host {
    pane: Arc<Pane>,
    tool: ToolKind,
    state: Mutex<TermActivity>,
}

impl Host {
    fn new(tool: ToolKind) -> Arc<Self> {
        Arc::new(Self {
            pane: Arc::new(Pane::default()),
            tool,
            state: Mutex::new(TermActivity::Idle),
        })
    }
    fn set(&self, state: TermActivity) {
        *self.state.lock().unwrap() = state;
    }
}

impl AppSessions for Host {
    fn app_sessions(&self) -> Vec<AppSession> {
        vec![AppSession {
            project: "p".into(),
            project_name: "Connecto".into(),
            entry: TerminalEntry {
                id: "claude-1".into(),
                name: Some(APP_SESSION.into()),
                parent: None,
                asked_by: None,
                tool: self.tool,
                state: *self.state.lock().unwrap(),
                in_turn: false,
                background: Vec::new(),
                finished_turns: 0,
                session: self.pane.clone(),
                control: None,
                desired_running: true,
                shown_id: None,
                activity: 0,
            },
        }]
    }
}

#[derive(Default)]
struct Gateway {
    down: AtomicBool,
    sent: Mutex<Vec<Push>>,
    tries: AtomicU64,
    /// Device tokens APNs refuses (`410`).
    gone: Mutex<HashSet<String>>,
}

impl PushSender for Gateway {
    fn send(&self, push: &Push) -> Result<(), PushError> {
        self.tries.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return Err(PushError::Temporary("down".into()));
        }
        if self.gone.lock().unwrap().contains(&push.device_token) {
            return Err(PushError::BadDestination("Unregistered".into()));
        }
        self.sent.lock().unwrap().push(push.clone());
        Ok(())
    }
}

impl Gateway {
    fn alerts(&self) -> Vec<Push> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.push_type == PushType::Alert)
            .cloned()
            .collect()
    }
}

/// A paired phone, as the host's caller is: its key's fingerprint, and its
/// typing, which Pause stops after `allowed` writes.
struct Phone {
    owner: String,
    allowed: AtomicU64,
}

impl Phone {
    fn new(owner: &str) -> Self {
        Self {
            owner: owner.into(),
            allowed: AtomicU64::new(u64::MAX),
        }
    }
}

impl SiriCaller for Phone {
    fn owner(&self) -> &str {
        &self.owner
    }
    fn type_into(&self, entry: &TerminalEntry, bytes: Vec<u8>) -> Result<(), String> {
        if self
            .allowed
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_err()
        {
            return Err("paused".into());
        }
        entry.session.send_input(bytes).map_err(|e| e.to_string())
    }
}

struct Rig {
    phone: Phone,
    host: Arc<Host>,
    clock: Arc<AtomicU64>,
    gateway: Arc<Gateway>,
    desk: SiriDesk,
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new(tool: ToolKind) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE_FILE);
        let host = Host::new(tool);
        let clock = Arc::new(AtomicU64::new(1_000_000));
        let gateway = Arc::new(Gateway::default());
        let desk = Self::desk(&path, &host, &clock, &gateway);
        Self {
            phone: Phone::new("SHA256:phone-a"),
            host,
            clock,
            gateway,
            desk,
            path,
            _dir: dir,
        }
    }

    fn desk(
        path: &Path,
        host: &Arc<Host>,
        clock: &Arc<AtomicU64>,
        gateway: &Arc<Gateway>,
    ) -> SiriDesk {
        let c = Arc::clone(clock);
        SiriDesk::with_clock(
            Some(path.to_owned()),
            Arc::clone(host) as Arc<dyn AppSessions>,
            Some(Arc::clone(gateway) as Arc<dyn PushSender>),
            Arc::new(move || c.load(Ordering::SeqCst)),
            Duration::ZERO,
        )
    }

    /// A rig around `desk` with no table of its own.
    fn with_desk(desk: SiriDesk, host: Arc<Host>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            phone: Phone::new("SHA256:phone-a"),
            host,
            clock: Arc::new(AtomicU64::new(1_000_000)),
            gateway: Arc::new(Gateway::default()),
            desk,
            path: dir.path().join(STORE_FILE),
            _dir: dir,
        }
    }

    /// The desk as a restarted host has it, from the same table.
    fn restart(&mut self) {
        self.desk = Self::desk(&self.path, &self.host, &self.clock, &self.gateway);
    }

    fn later(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    fn ask(&self, id: &str, text: &str, notify: bool) -> Value {
        self.ask_answering(id, text, None, notify)
    }

    fn ask_answering(&self, id: &str, text: &str, question: Option<&str>, notify: bool) -> Value {
        self.desk
            .answer(
                "p",
                Some("claude-1"),
                &ProjectAsk::SiriAsk {
                    request_id: id.into(),
                    terminal: 1,
                    text: text.into(),
                    question: question.map(Into::into),
                    notify: notify.then(destination),
                },
                &self.phone,
            )
            .unwrap()
    }

    fn result(&self, id: &str) -> Value {
        self.desk
            .answer(
                "p",
                None,
                &ProjectAsk::SiriResult {
                    request_id: id.into(),
                },
                &self.phone,
            )
            .unwrap()
    }
}

fn destination() -> SiriNotify {
    SiriNotify {
        device_token: "ab12".into(),
        live_activity_token: None,
        key: KEY.into(),
        env: PushEnv::Sandbox,
        topic: "studio.connecto.connectoFlutterApp".into(),
        locale: "it-IT".into(),
    }
}

#[test]
fn a_retry_with_the_same_id_is_never_typed_again() {
    let rig = Rig::new(ToolKind::Claude);
    assert_eq!(rig.ask("r1", "run the tests", false)["outcome"], "accepted");
    assert_eq!(rig.host.pane.typed(), 2, "the paste, then Enter");
    let again = rig.ask("r1", "run the tests", false);
    assert_eq!(again["outcome"], "duplicate");
    assert_eq!(again["state"]["state"], "pending");
    assert_eq!(rig.host.pane.typed(), 2, "a retry typed the question again");
}

#[test]
fn a_second_request_waits_until_the_first_ones_prompt_shows() {
    let rig = Rig::new(ToolKind::Claude);
    assert_eq!(rig.ask("r1", "first", false)["outcome"], "accepted");
    let second = rig.ask("r2", "second", false);
    assert_eq!(second["outcome"], "refused");
    assert_eq!(second["reason"], "busy");
    assert_eq!(rig.host.pane.typed(), 2);
    rig.host.pane.push(true, "first");
    rig.desk.tick();
    assert_eq!(rig.ask("r2", "second", false)["outcome"], "accepted");
}

#[test]
fn the_reply_after_its_own_prompt_is_kept_and_identical_texts_never_share_one() {
    let rig = Rig::new(ToolKind::Claude);
    rig.host.pane.push(true, "run the tests");
    rig.host.pane.push(false, "Old result.");
    rig.ask("r1", "run the tests", false);
    rig.host.set(TermActivity::Working);
    rig.host.pane.push(true, "Run the tests!");
    rig.host.pane.push(false, "Running.");
    rig.desk.tick();
    assert_eq!(rig.result("r1")["state"], "pending", "still working");
    rig.host.pane.push(false, "All green.");
    rig.host.set(TermActivity::Idle);
    rig.desk.tick();
    let done = rig.result("r1");
    assert_eq!(done["state"], "answered");
    assert_eq!(done["reply"], "Running.\n\nAll green.");
    // The same words again: their own prompt, their own reply.
    rig.ask("r2", "run the tests", false);
    rig.host.pane.push(true, "run the tests");
    rig.host.pane.push(false, "Second run.");
    rig.host.pane.push(true, "from the desktop");
    rig.host.pane.push(false, "desktop answer");
    rig.desk.tick();
    assert_eq!(rig.result("r2")["reply"], "Second run.");
    assert_eq!(rig.result("r1")["reply"], "Running.\n\nAll green.");
}

#[test]
fn a_question_with_no_new_reply_is_reported_at_once() {
    let rig = Rig::new(ToolKind::Codex);
    rig.ask("r1", "plan it", true);
    rig.host.pane.push(true, "plan it");
    rig.host.set(TermActivity::NeedsUser);
    *rig.host.pane.screen.lock().unwrap() = "tool output\nQuestion 1/2 (2 unanswered)\nWhich store?\n› Type your answer\nenter to submit answer | esc to interrupt".into();
    rig.later(SECOND);
    rig.desk.tick();
    let state = rig.result("r1");
    assert_eq!(state["state"], "needs-answer");
    assert!(state["question"].as_str().unwrap().contains("Which store?"));
}

#[test]
fn a_restart_while_typing_leaves_it_uncertain_and_never_types_it_again() {
    for (found, want) in [(false, "failed"), (true, "answered")] {
        let mut rig = Rig::new(ToolKind::Claude);
        rig.ask("r1", "deploy it", true);
        // As if the host stopped between the paste and the Enter.
        let mut table: Value = serde_json::from_slice(&std::fs::read(&rig.path).unwrap()).unwrap();
        table["requests"][0]["phase"] = json!("typing");
        std::fs::write(&rig.path, table.to_string()).unwrap();
        // The restarted host's chat starts empty.
        *rig.host.pane.chat.lock().unwrap() = ChatHistory::default();
        rig.restart();
        let typed = rig.host.pane.typed();
        assert_eq!(rig.result("r1")["state"], "uncertain");
        assert_eq!(
            rig.ask("r1", "deploy it", true)["state"]["state"],
            "uncertain"
        );
        if found {
            rig.host.pane.push(true, "deploy it");
            rig.host.pane.push(false, "Deployed.");
        }
        rig.later(UNCERTAIN_MS - SECOND);
        rig.desk.tick();
        rig.later(2 * SECOND);
        rig.desk.tick();
        let state = rig.result("r1");
        assert_eq!(state["state"], want, "{state}");
        if !found {
            assert_eq!(state["why"], "not_sent");
            rig.later(GRACE_MS);
            rig.desk.tick();
            let alerts = rig.gateway.alerts();
            assert_eq!(alerts.len(), 1);
            assert!(
                alerts[0].payload["aps"]["alert"]["body"]
                    .as_str()
                    .unwrap()
                    .contains("Non sono sicuro")
            );
        }
        assert_eq!(
            rig.host.pane.typed(),
            typed,
            "an uncertain request was typed again"
        );
    }
}

#[test]
fn a_reply_siri_read_out_is_not_notified_and_others_wait_for_the_grace() {
    for delivered in [true, false] {
        let rig = Rig::new(ToolKind::Claude);
        assert_eq!(rig.ask("r1", "status?", true)["notifies"], true);
        rig.host.pane.push(true, "status?");
        rig.host.pane.push(false, "All good.");
        rig.later(2 * SECOND);
        rig.desk.tick();
        assert_eq!(rig.result("r1")["state"], "answered");
        assert!(
            rig.gateway.alerts().is_empty(),
            "notified inside Siri's grace"
        );
        if delivered {
            rig.desk
                .answer(
                    "p",
                    None,
                    &ProjectAsk::SiriDelivered {
                        request_id: "r1".into(),
                    },
                    &rig.phone,
                )
                .unwrap();
        }
        rig.later(GRACE_MS);
        rig.desk.tick();
        rig.desk.tick();
        assert_eq!(rig.gateway.alerts().len(), usize::from(!delivered));
    }
}

#[test]
fn a_notification_waits_out_a_gateway_outage_and_a_restart_and_goes_once() {
    let mut rig = Rig::new(ToolKind::Claude);
    rig.ask("r1", "status?", true);
    rig.host.pane.push(true, "status?");
    rig.host.pane.push(false, "All good.");
    rig.gateway.down.store(true, Ordering::SeqCst);
    rig.later(GRACE_MS);
    rig.desk.tick();
    rig.later(5 * MINUTE);
    rig.desk.tick();
    assert!(rig.gateway.tries.load(Ordering::SeqCst) >= 2);
    assert!(rig.gateway.alerts().is_empty());
    rig.restart();
    rig.gateway.down.store(false, Ordering::SeqCst);
    rig.later(LAST_RETRY_MS);
    rig.desk.tick();
    rig.later(LAST_RETRY_MS);
    rig.desk.tick();
    let alerts = rig.gateway.alerts();
    assert_eq!(alerts.len(), 1, "sent other than once");
    assert_eq!(alerts[0].payload["r"], "r1");
    assert_eq!(alerts[0].collapse_id.len(), 32);
    let preview = alerts[0].payload["c"].as_str().unwrap();
    assert_eq!(
        open_sealed(KEY, "r1", preview).as_deref(),
        Some("All good.")
    );
    assert_eq!(
        open_sealed(KEY, "r2", preview),
        None,
        "bound to its request"
    );
}

#[test]
fn a_notification_with_the_longest_preview_fits_apns() {
    let reply = "è".repeat(3000);
    let row = Row {
        id: "0".repeat(MAX_ID),
        owner: "SHA256:phone-a".into(),
        project: "p".repeat(64),
        project_name: "n".repeat(300),
        terminal: "claude-1".into(),
        text: String::new(),
        question: None,
        notify: Some(destination()),
        notifies: true,
        delivered: false,
        base: 0,
        prompt: None,
        phase: Phase::Done(Outcome::Answered(reply)),
        accepted_ms: 0,
        done_ms: None,
        working_sent: false,
        moved_on: false,
    };
    let push = alert(&row, &destination(), Kind::Reply).unwrap();
    let size = push.payload.to_string().len();
    assert!(size < MAX_PAYLOAD, "{size} bytes");
    let preview = open_sealed(KEY, &row.id, push.payload["c"].as_str().unwrap()).unwrap();
    assert!(preview.len() <= MAX_PREVIEW && preview.len() >= MAX_PREVIEW - 1);
}

#[test]
fn asks_round_trip_and_bad_ones_are_refused_as_malformed() {
    let asks = [
        ProjectAsk::SiriAsk {
            request_id: "A-1".into(),
            terminal: 3,
            text: "hi".into(),
            question: Some("Which?".into()),
            notify: Some(destination()),
        },
        ProjectAsk::SiriResult {
            request_id: "a".into(),
        },
        ProjectAsk::SiriDelivered {
            request_id: "a".into(),
        },
        ProjectAsk::PushRegister {
            old_device_token: Some("ff".into()),
            to: destination(),
        },
        ProjectAsk::ActivityToken {
            request_id: "a".into(),
            token: "00ff".into(),
        },
        ProjectAsk::SiriStatus { terminal: 9 },
    ];
    for ask in asks {
        assert_eq!(ProjectAsk::decode(&ask.encode()).unwrap(), ask);
    }
    let bad = br#"{"ask":"siri-result","request_id":"has space"}"#;
    assert_eq!(ProjectAsk::decode(bad).unwrap_err(), MALFORMED);
    assert!(is_unknown_ask(crate::project_panels::UNKNOWN_ASK));
    assert!(!is_unknown_ask("That machine paused remote actions."));
}

#[test]
fn the_status_says_how_long_the_turn_has_run() {
    let rig = Rig::new(ToolKind::Claude);
    let status = |terminal: Option<&str>| {
        rig.desk
            .answer(
                "p",
                terminal,
                &ProjectAsk::SiriStatus { terminal: 1 },
                &rig.phone,
            )
            .unwrap()["workingSecs"]
            .clone()
    };
    assert_eq!(status(Some("claude-1")), Value::Null);
    rig.host.set(TermActivity::Working);
    rig.desk.tick();
    rig.later(4 * MINUTE);
    assert_eq!(status(Some("claude-1")), json!(240));
    assert_eq!(status(None), Value::Null, "not the app session");
    rig.host.set(TermActivity::Idle);
    assert_eq!(status(Some("claude-1")), Value::Null);
}

const CODEX_QUESTION: &str = "tool output\nQuestion 1/2 (2 unanswered)\nWhich store?\n› Type your answer\nenter to submit answer | esc to interrupt";

fn ask_as(rig: &Rig, phone: &Phone, id: &str, text: &str) -> Value {
    rig.desk
        .answer(
            "p",
            Some("claude-1"),
            &ProjectAsk::SiriAsk {
                request_id: id.into(),
                terminal: 1,
                text: text.into(),
                question: None,
                notify: Some(destination()),
            },
            phone,
        )
        .unwrap()
}

#[test]
fn a_request_the_table_cannot_keep_is_refused_before_anything_is_typed() {
    let dir = tempfile::tempdir().unwrap();
    // The table's folder is a file: nothing can be written there.
    let blocked = dir.path().join("remote");
    std::fs::write(&blocked, b"not a folder").unwrap();
    let path = blocked.join(STORE_FILE);
    let host = Host::new(ToolKind::Claude);
    let desk = Rig::desk(
        &path,
        &host,
        &Arc::new(AtomicU64::new(1_000_000)),
        &Arc::new(Gateway::default()),
    );
    let phone = Phone::new("SHA256:phone-a");
    let answer = ask_as(&Rig::with_desk(desk, host.clone()), &phone, "r1", "deploy");
    assert_eq!(answer["outcome"], "refused", "{answer}");
    assert_eq!(answer["reason"], "storage");
    assert_eq!(host.pane.typed(), 0, "typed with no record of it");
}

#[cfg(unix)]
#[test]
fn the_table_is_readable_by_its_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let rig = Rig::new(ToolKind::Claude);
    rig.ask("r1", "status?", true);
    let mode = std::fs::metadata(&rig.path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");
}

#[test]
fn another_peer_can_neither_read_nor_redirect_a_request() {
    let rig = Rig::new(ToolKind::Claude);
    let other = Phone::new("SHA256:phone-b");
    assert_eq!(rig.ask("r1", "status?", true)["outcome"], "accepted");
    rig.host.pane.push(true, "status?");
    rig.host.pane.push(false, "All good.");
    rig.desk.tick();
    let as_other = |ask: ProjectAsk| {
        rig.desk
            .answer("p", Some("claude-1"), &ask, &other)
            .unwrap()
    };
    // Its id means nothing to another peer: no state, no retry, no cancel.
    assert_eq!(
        as_other(ProjectAsk::SiriResult {
            request_id: "r1".into()
        })["state"],
        "unknown"
    );
    let typed = rig.host.pane.typed();
    assert_eq!(ask_as(&rig, &other, "r1", "status?")["reason"], "invalid");
    assert_eq!(rig.host.pane.typed(), typed);
    as_other(ProjectAsk::SiriDelivered {
        request_id: "r1".into(),
    });
    // Knowing the old token is not enough to take the notices.
    let mut theirs = destination();
    theirs.device_token = "cd34".into();
    theirs.key = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=".into();
    as_other(ProjectAsk::PushRegister {
        old_device_token: Some("ab12".into()),
        to: theirs,
    });
    as_other(ProjectAsk::ActivityToken {
        request_id: "r1".into(),
        token: "ee".into(),
    });
    rig.later(GRACE_MS);
    rig.desk.tick();
    let alerts = rig.gateway.alerts();
    assert_eq!(alerts.len(), 1, "cancelled by another peer");
    assert_eq!(alerts[0].device_token, "ab12");
    let sealed = alerts[0].payload["c"].as_str().unwrap();
    assert_eq!(open_sealed(KEY, "r1", sealed).as_deref(), Some("All good."));
    assert_eq!(
        rig.gateway.sent.lock().unwrap().len(),
        1,
        "a Live Activity push went to another peer's token"
    );
    assert_eq!(rig.result("r1")["state"], "answered");
}

#[test]
fn a_rotation_told_late_still_moves_the_owners_notices_once() {
    let rig = Rig::new(ToolKind::Claude);
    rig.ask("r1", "status?", true);
    let mut moved = destination();
    moved.device_token = "ee55".into();
    let rotate = || {
        rig.desk
            .answer(
                "p",
                None,
                &ProjectAsk::PushRegister {
                    old_device_token: Some("ab12".into()),
                    to: moved.clone(),
                },
                &rig.phone,
            )
            .unwrap()
    };
    // Told again (the phone retries until a host has it): no change.
    rotate();
    rotate();
    rig.host.pane.push(true, "status?");
    rig.host.pane.push(false, "Done.");
    rig.later(GRACE_MS);
    rig.desk.tick();
    let alerts = rig.gateway.alerts();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].device_token, "ee55");
}

#[test]
fn two_answers_to_the_same_question_are_typed_once() {
    let rig = Rig::new(ToolKind::Codex);
    rig.host.set(TermActivity::NeedsUser);
    *rig.host.pane.screen.lock().unwrap() = CODEX_QUESTION.into();
    let q = connecto_core::question::active(ToolKind::Codex, CODEX_QUESTION)
        .unwrap()
        .text;
    assert_eq!(
        rig.ask_answering("r1", "postgres", Some(&q), false)["outcome"],
        "accepted"
    );
    let typed = rig.host.pane.typed();
    // The question still shows, the agent has not taken the answer yet.
    rig.desk.tick();
    let second = rig.ask_answering("r2", "sqlite", Some(&q), false);
    assert_eq!(second["outcome"], "refused", "{second}");
    assert_eq!(second["reason"], "busy");
    assert_eq!(rig.host.pane.typed(), typed, "a second answer was typed");
    // Its turn moves on (the question is gone): the session is free again.
    *rig.host.pane.screen.lock().unwrap() = "working".into();
    rig.host.set(TermActivity::Idle);
    rig.desk.tick();
    assert_eq!(rig.ask("r3", "status?", false)["outcome"], "accepted");
}

#[test]
fn pause_between_the_paste_and_the_enter_leaves_it_uncertain_and_untyped() {
    let rig = Rig::new(ToolKind::Claude);
    // The paste goes; then Pause: the Enter is refused.
    rig.phone.allowed.store(1, Ordering::SeqCst);
    let answer = rig.ask("r1", "deploy it", true);
    assert_eq!(answer["outcome"], "duplicate", "{answer}");
    assert_eq!(answer["state"]["state"], "uncertain");
    let inputs = rig.host.pane.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 1, "{inputs:?}");
    assert_ne!(inputs[0], vec![13], "Enter written in Pause");
    rig.phone.allowed.store(u64::MAX, Ordering::SeqCst);
    assert_eq!(
        rig.ask("r1", "deploy it", true)["state"]["state"],
        "uncertain"
    );
    assert_eq!(
        rig.host.pane.typed(),
        1,
        "an uncertain request was typed again"
    );
}

#[test]
fn a_full_table_refuses_new_work_and_keeps_every_promised_notice() {
    let mut rig = Rig::new(ToolKind::Claude);
    rig.desk = Rig::desk(&rig.path, &rig.host, &rig.clock, &rig.gateway).with_max_rows(3);
    rig.gateway.down.store(true, Ordering::SeqCst);
    for i in 0..3 {
        let id = format!("r{i}");
        let text = format!("question {i}");
        assert_eq!(rig.ask(&id, &text, true)["outcome"], "accepted");
        rig.host.pane.push(true, &text);
        rig.host.pane.push(false, &format!("answer {i}"));
        rig.desk.tick();
        assert_eq!(rig.result(&id)["state"], "answered");
    }
    let full = rig.ask("r3", "one more", true);
    assert_eq!(full["reason"], "full", "{full}");
    assert_eq!(rig.host.pane.typed(), 6);
    // The outage goes on across a restart; then the gateway comes back.
    rig.later(GRACE_MS);
    rig.desk.tick();
    rig.desk = Rig::desk(&rig.path, &rig.host, &rig.clock, &rig.gateway).with_max_rows(3);
    rig.gateway.down.store(false, Ordering::SeqCst);
    for _ in 0..3 {
        rig.later(LAST_RETRY_MS);
        rig.desk.tick();
    }
    let mut sent: Vec<String> = rig
        .gateway
        .alerts()
        .iter()
        .map(|p| p.payload["r"].as_str().unwrap().to_owned())
        .collect();
    sent.sort();
    assert_eq!(
        sent,
        ["r0", "r1", "r2"],
        "a promised notice was lost or doubled"
    );
    // Kept a day; then there is room again.
    rig.later(KEEP_MS);
    rig.desk.tick();
    assert_eq!(rig.ask("r3", "one more", true)["outcome"], "accepted");
}

#[test]
fn an_unreadable_table_is_kept_as_it_is_and_nothing_is_typed_again() {
    let mut rig = Rig::new(ToolKind::Claude);
    assert_eq!(rig.ask("r1", "deploy it", true)["outcome"], "accepted");
    assert_eq!(rig.host.pane.typed(), 2);
    // The table is cut short (a disk full, a crash of another writer).
    let whole = std::fs::read(&rig.path).unwrap();
    let cut = whole[..whole.len() / 2].to_vec();
    std::fs::write(&rig.path, &cut).unwrap();
    rig.restart();
    let again = rig.ask("r1", "deploy it", true);
    assert_eq!(
        (again["outcome"].as_str(), again["reason"].as_str()),
        (Some("refused"), Some("storage")),
        "{again}"
    );
    assert_eq!(
        rig.host.pane.typed(),
        2,
        "a request it may have typed was typed again"
    );
    // Nothing else is answered for or written either, and the file stays
    // exactly as it was, for whoever repairs it.
    assert!(
        rig.desk
            .answer(
                "p",
                None,
                &ProjectAsk::SiriResult {
                    request_id: "r1".into()
                },
                &rig.phone
            )
            .is_err(),
        "a result while the table is unreadable"
    );
    rig.later(GRACE_MS);
    rig.desk.tick();
    assert_eq!(std::fs::read(&rig.path).unwrap(), cut);
    assert!(rig.gateway.sent.lock().unwrap().is_empty());
    // Valid JSON with a request that does not decode is no better.
    let bad_row = br#"{"requests":[{"id":"r1","phase":"submitted"}],"notices":[]}"#;
    std::fs::write(&rig.path, bad_row).unwrap();
    rig.restart();
    assert_eq!(rig.ask("r2", "status?", false)["reason"], "storage");
    rig.desk.tick();
    assert_eq!(rig.host.pane.typed(), 2);
    assert_eq!(std::fs::read(&rig.path).unwrap(), bad_row);
    // Moved away by the person: a new table, and requests again.
    std::fs::remove_file(&rig.path).unwrap();
    rig.desk.tick();
    assert_eq!(rig.ask("r3", "status?", false)["outcome"], "accepted");
}

#[test]
fn an_answer_holds_its_question_through_unrelated_messages_and_its_echo() {
    let rig = Rig::new(ToolKind::Codex);
    rig.host.set(TermActivity::NeedsUser);
    *rig.host.pane.screen.lock().unwrap() = CODEX_QUESTION.into();
    let q = connecto_core::question::active(ToolKind::Codex, CODEX_QUESTION)
        .unwrap()
        .text;
    assert_eq!(
        rig.ask_answering("r1", "postgres", Some(&q), false)["outcome"],
        "accepted"
    );
    let typed = rig.host.pane.typed();
    // The same question still shows while the chat moves: a board report,
    // the echo of the answer itself, then a word from the agent.
    rig.host.pane.push(false, "Task complete [t-1a2b]. Merged.");
    rig.host.pane.push(true, "postgres");
    rig.host.pane.push(false, "One moment.");
    rig.desk.tick();
    let second = rig.ask_answering("r2", "sqlite", Some(&q), false);
    assert_eq!(second["reason"], "busy", "{second}");
    assert_eq!(rig.host.pane.typed(), typed, "a second answer was typed");
    assert_eq!(
        rig.result("r1")["state"],
        "pending",
        "a message while the question showed was taken for its answer"
    );
    // The question leaves the screen: the session is free, and the reply
    // after it is the answer's.
    *rig.host.pane.screen.lock().unwrap() = "working".into();
    rig.host.set(TermActivity::Idle);
    rig.host.pane.push(false, "Using postgres.");
    rig.desk.tick();
    assert_eq!(rig.ask("r3", "status?", false)["outcome"], "accepted");
}

#[test]
fn a_notice_refused_for_an_old_token_goes_to_the_new_one_after_a_restart() {
    let mut rig = Rig::new(ToolKind::Claude);
    assert_eq!(rig.ask("r1", "status?", true)["outcome"], "accepted");
    rig.host.pane.push(true, "status?");
    rig.host.pane.push(false, "Done.");
    // The phone's token changed before it could tell this Mac: APNs refuses
    // the old one.
    rig.gateway.gone.lock().unwrap().insert("ab12".into());
    rig.later(GRACE_MS);
    rig.desk.tick();
    let tried = rig.gateway.tries.load(Ordering::SeqCst);
    assert!(tried >= 1);
    // Kept, and not sent again to the token APNs refused.
    rig.later(LAST_RETRY_MS);
    rig.desk.tick();
    assert_eq!(rig.gateway.tries.load(Ordering::SeqCst), tried);
    // The phone tells its rotation; the table cannot be written at first:
    // not acknowledged, so the phone tells it again.
    let mut moved = destination();
    moved.device_token = "ee55".into();
    let rotate = |desk: &SiriDesk| {
        desk.answer(
            "p",
            None,
            &ProjectAsk::PushRegister {
                old_device_token: Some("ab12".into()),
                to: moved.clone(),
            },
            &rig.phone,
        )
    };
    let blocker = rig.path.with_extension("json.tmp");
    std::fs::create_dir(&blocker).unwrap();
    assert!(rotate(&rig.desk).is_err(), "acknowledged without saving it");
    std::fs::remove_dir(&blocker).unwrap();
    assert_eq!(rotate(&rig.desk).unwrap()["done"], true);
    // A restart before it goes: it still goes, once, to the new token.
    rig.restart();
    rig.later(SECOND);
    rig.desk.tick();
    rig.later(LAST_RETRY_MS);
    rig.desk.tick();
    let alerts = rig.gateway.alerts();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0].device_token, "ee55");
    assert_eq!(alerts[0].payload["r"], "r1");
}

#[test]
fn a_still_working_notice_parked_on_a_refused_token_never_follows_the_reply() {
    let rig = Rig::new(ToolKind::Claude);
    assert_eq!(rig.ask("r1", "big job", true)["outcome"], "accepted");
    rig.host.pane.push(true, "big job");
    rig.host.set(TermActivity::Working);
    // The old token is refused: the 30-minute "still working" is parked.
    rig.gateway.gone.lock().unwrap().insert("ab12".into());
    rig.desk.tick();
    rig.later(STILL_WORKING_MS);
    rig.desk.tick();
    assert!(
        rig.gateway.tries.load(Ordering::SeqCst) >= 1,
        "the progress notice was tried"
    );
    assert!(rig.gateway.alerts().is_empty());
    // The turn ends while it is parked.
    rig.host.pane.push(false, "All done.");
    rig.host.set(TermActivity::Idle);
    rig.desk.tick();
    // Then the phone tells its new token.
    let mut moved = destination();
    moved.device_token = "ee55".into();
    rig.desk
        .answer(
            "p",
            None,
            &ProjectAsk::PushRegister {
                old_device_token: Some("ab12".into()),
                to: moved,
            },
            &rig.phone,
        )
        .unwrap();
    for _ in 0..3 {
        rig.later(LAST_RETRY_MS);
        rig.desk.tick();
    }
    let alerts = rig.gateway.alerts();
    let kinds: Vec<&Value> = alerts.iter().map(|p| &p.payload["k"]).collect();
    assert_eq!(kinds, [&json!("reply")], "only the final alert: {alerts:?}");
    assert_eq!(alerts[0].device_token, "ee55");
}
