//! An OpenCode terminal under Connecto's own launch path, with the REAL
//! `opencode` program (`~/.opencode/bin/opencode`, as OpenCode's installer
//! puts it) and a stand-in model server (`fixtures/mock_openai.py`), so no
//! account and no network model is needed.
//!
//! Evidence class REAL-CLI: it proves that the plugin Connecto names in
//! `OPENCODE_CONFIG_CONTENT` is loaded, that its reports become Connecto's
//! events (start, prompt, end with the reply, a permission prompt), that the
//! bridge instructions reach the model, that an isolated project keeps
//! OpenCode's database in its own profile, and that a resume names the
//! session again. Ignored by default: it needs OpenCode installed. Run with
//! `cargo test -p connecto-session --test real_opencode -- --ignored`.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{hook_binary, no_wake, scaled, screen_text, wait_for_screen};
use connecto_adapter::{ComposerState, Screen, opencode_composer_state};
use connecto_audit::Audit;
use connecto_core::{
    AgentMode, ConfigMode, DeliveryState, GridSize, ProjectSettings, Readiness, RelayControl,
    Reply, SendRequest, TerminalSession, ToolEvent,
};
use connecto_session::launch::{
    BridgeInfo, LaunchExtras, Layout, launch_modes, project_id, set_launch_modes, start_resuming,
};
use connecto_session::{LiveSession, Relay, RelayConfig};
use connecto_store::{Codesign, Installed, Store, Tool};

const SIZE: GridSize = GridSize {
    cols: 120,
    rows: 40,
};

/// The user's own OpenCode, where its installer puts it, or the program
/// `CONNECTO_TEST_OPENCODE` names (another version).
fn real_opencode() -> PathBuf {
    let entry = std::env::var_os("CONNECTO_TEST_OPENCODE")
        .filter(|v| !v.is_empty())
        .map_or_else(
            || Path::new(&std::env::var_os("HOME").expect("HOME")).join(".opencode/bin/opencode"),
            PathBuf::from,
        );
    assert!(
        entry.is_file(),
        "OpenCode is not installed at {}",
        entry.display()
    );
    entry
}

struct Mock {
    child: Child,
    port: u16,
    log: PathBuf,
}

impl Mock {
    fn start(log: PathBuf) -> Self {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_openai.py");
        let mut child = Command::new("/usr/bin/python3")
            .arg("-I")
            .arg(&fixture)
            .arg(&log)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        Self {
            child,
            port: line.trim().parse().unwrap(),
            log,
        }
    }

    fn requests(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Scene {
    root: PathBuf,
    layout: Layout,
    project: PathBuf,
    installed: Installed,
    mock: Mock,
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Scene {
    fn new(name: &str) -> Self {
        // A `#`, a `%` and a space in every path (the run folder holding the
        // plugin too): they must survive the plugin's `file:` URL.
        let root =
            std::env::temp_dir().join(format!("mg-oc #1 100%-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home/project", "data"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let project = root.join("home/project");
        let mock = Mock::start(root.join("requests.log"));
        // The project names the stand-in model and asks before bash runs.
        fs::write(
            project.join("opencode.json"),
            serde_json::json!({
                "$schema": "https://opencode.ai/config.json",
                "model": "mock/m1",
                "small_model": "mock/m1",
                "autoupdate": false,
                "share": "disabled",
                "permission": { "bash": "ask" },
                "provider": { "mock": {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "Mock",
                    "options": {
                        "baseURL": format!("http://127.0.0.1:{}/v1", mock.port),
                        "apiKey": "x"
                    },
                    "models": { "m1": { "name": "Mock 1" } }
                }}
            })
            .to_string(),
        )
        .unwrap();
        // OpenCode in this project keeps its own folders.
        let settings = ProjectSettings {
            opencode_config: ConfigMode::Isolated,
            ..ProjectSettings::default()
        };
        connecto_session::project_settings::save(
            &layout.data_root,
            &project_id(&project),
            &settings,
        )
        .unwrap();
        let store = Store::new(&layout.data_root);
        let entry = real_opencode();
        let installed = store.provision(Tool::OpenCode, &entry, &Codesign).unwrap();
        // Run where it is: nothing was copied.
        assert_eq!(installed.entry, fs::canonicalize(&entry).unwrap());
        store.install_hook(&hook_binary()).unwrap();
        Self {
            root,
            layout,
            project,
            installed,
            mock,
        }
    }

    fn start(&self, resume: Option<&str>) -> LiveSession {
        let bridge = BridgeInfo {
            terminal: "opencode".to_owned(),
            text: Some("LAUNCH TEXT for opencode".to_owned()),
            hooks: true,
            ..BridgeInfo::absent()
        };
        start_resuming(
            &self.layout,
            &self.project,
            Tool::OpenCode,
            &self.installed,
            SIZE,
            no_wake(),
            &LaunchExtras::default(),
            &bridge,
            resume,
        )
        .unwrap()
    }

    /// The OpenCode databases under the project's Connecto profile.
    fn profile_databases(&self) -> Vec<PathBuf> {
        let profiles = self
            .layout
            .data_root
            .join("projects")
            .join(project_id(&self.project))
            .join("profiles/opencode");
        fs::read_dir(profiles)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path().join("data/opencode/opencode.db"))
                    .filter(|db| db.is_file())
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn next_event(session: &LiveSession, what: &str) -> ToolEvent {
    let hook = session.hook().unwrap();
    let deadline = Instant::now() + scaled(Duration::from_secs(60));
    while Instant::now() < deadline {
        if let Some(event) = hook.events().recv_timeout(Duration::from_millis(200)) {
            return event;
        }
    }
    panic!("no {what} event; rejected={}", hook.rejected());
}

/// Type a prompt the way a person does: the text, then Enter.
fn type_prompt(session: &LiveSession, text: &str) {
    session.send_input(text.as_bytes().to_vec()).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    session.send_input(b"\r".to_vec()).unwrap();
}

/// Until OpenCode's prompt is drawn empty and waiting.
fn wait_for_prompt(session: &LiveSession) -> String {
    let snapshot = wait_for_screen(session, "OpenCode's empty prompt", |snapshot| {
        opencode_composer_state(Screen::plain(&screen_text(snapshot))) == ComposerState::Empty
    });
    screen_text(&snapshot)
}

#[test]
#[ignore = "needs OpenCode installed in ~/.opencode/bin; run with --ignored"]
fn a_real_opencode_terminal_reports_its_turns_asks_and_resumes_in_its_own_profile() {
    // The user has XDG folders of their own: an isolated OpenCode keeps its
    // folders in the profile, and the agent's shell gets the user's. Connecto
    // reads them from its own environment, so the test runs again as a child
    // that has them.
    const XDG: [&str; 3] = ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"];
    if std::env::var_os("CONNECTO_TEST_OWN_XDG").is_none() {
        let base = std::env::temp_dir().join(format!("mg-oc-xdg-{}", std::process::id()));
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "a_real_opencode_terminal_reports_its_turns_asks_and_resumes_in_its_own_profile",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env("CONNECTO_TEST_OWN_XDG", "1");
        for (name, folder) in XDG.iter().zip(["config", "data", "state"]) {
            let dir = base.join(folder);
            fs::create_dir_all(&dir).unwrap();
            child.env(name, fs::canonicalize(&dir).unwrap());
        }
        let status = child.status().unwrap();
        let _ = fs::remove_dir_all(&base);
        assert!(
            status.success(),
            "the run with the user's XDG folders failed"
        );
        return;
    }
    let own: Vec<PathBuf> = XDG
        .iter()
        .map(|name| PathBuf::from(std::env::var_os(name).unwrap()))
        .collect();
    let scene = Scene::new("turns");
    let before = launch_modes();
    // Its own prompts, so the bash call asks.
    set_launch_modes(before.with(connecto_core::ToolKind::OpenCode, AgentMode::Prompt));
    let session = scene.start(None);

    // Up before any session exists: ready, with nothing to resume yet.
    match next_event(&session, "start") {
        ToolEvent::Started { tool_session, .. } => assert_eq!(tool_session.expose(), ""),
        other => panic!("the first event is {other:?}"),
    }
    wait_for_prompt(&session);

    type_prompt(&session, "say hi");
    // The session OpenCode made for the prompt is named with it: no start.
    let session_id = match next_event(&session, "session id") {
        ToolEvent::SessionNamed { tool_session } => tool_session.expose().clone(),
        other => panic!("expected the session's id, got {other:?}"),
    };
    assert!(session_id.starts_with("ses_"), "{session_id}");
    assert_eq!(
        next_event(&session, "prompt"),
        ToolEvent::PromptSubmitted {
            prompt: connecto_core::EventText::new("say hi".to_owned()),
            turn: None,
        }
    );
    match next_event(&session, "end") {
        ToolEvent::TurnEnded {
            reply: Reply::Text(text),
            failure: None,
            lifecycle_known: true,
            ..
        } => assert_eq!(text.expose(), "Hello from the mock model."),
        other => panic!("the turn ended as {other:?}"),
    }
    // Idle again, and the screen reads as an empty prompt.
    let idle = wait_for_prompt(&session);
    assert!(!idle.contains("esc interrupt"), "{idle}");

    // The bridge instructions reached the model, in OpenCode's system prompt.
    let requests = scene.mock.requests();
    assert!(
        requests.iter().any(|request| request["system"]
            .as_str()
            .is_some_and(|system| system.contains("LAUNCH TEXT for opencode"))),
        "no request carried the launch text"
    );
    // Isolated: OpenCode's database is in the project's own profile.
    assert_eq!(scene.profile_databases().len(), 1);

    // A slow turn reads as at work on the screen, then ends as usual. Its
    // session is known already: no second start.
    type_prompt(&session, "please SLOW");
    assert!(matches!(
        next_event(&session, "slow prompt"),
        ToolEvent::PromptSubmitted { .. }
    ));
    wait_for_screen(&session, "OpenCode at work", |snapshot| {
        opencode_composer_state(Screen::plain(&screen_text(snapshot))) == ComposerState::Busy
    });
    assert!(matches!(
        next_event(&session, "slow end"),
        ToolEvent::TurnEnded {
            reply: Reply::Text(_),
            ..
        }
    ));
    wait_for_prompt(&session);

    // A tool call that needs permission: OpenCode asks the user.
    type_prompt(&session, "please USE_BASH now");
    assert!(matches!(
        next_event(&session, "prompt"),
        ToolEvent::PromptSubmitted { .. }
    ));
    assert_eq!(next_event(&session, "ask"), ToolEvent::NeedsUser);
    // Allowed once (the first choice): the command runs with the user's own
    // XDG folders, not the profile's.
    std::thread::sleep(Duration::from_millis(500));
    session.send_input(b"\r".to_vec()).unwrap();
    let written = scene.project.join("xdg.txt");
    let deadline = Instant::now() + scaled(Duration::from_secs(30));
    while !written.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    let mine: Vec<String> = own.iter().map(|d| d.display().to_string()).collect();
    assert_eq!(fs::read_to_string(&written).unwrap(), mine.join("|"));
    // OpenCode itself wrote nothing into the user's own folders.
    assert!(
        own.iter()
            .all(|dir| fs::read_dir(dir).unwrap().next().is_none()),
        "the isolated terminal used the user's XDG folders"
    );
    session.terminate();

    // Started again on the same session: its id is known at once.
    let resumed = scene.start(Some(&session_id));
    match next_event(&resumed, "resumed start") {
        ToolEvent::Started { tool_session, .. } => {
            assert_eq!(tool_session.expose(), &session_id);
        }
        other => panic!("the resumed start is {other:?}"),
    }
    // The old conversation comes back on the screen.
    common::wait_for_text(&resumed, "Hello from the mock model.");
    wait_for_prompt(&resumed);
    resumed.terminate();
    set_launch_modes(before);
}

/// The project's saved terminal list, as the app writes it.
fn saved_list(scene: &Scene) -> String {
    let id = project_id(&scene.project);
    fs::read_to_string(
        scene
            .layout
            .data_root
            .join("projects")
            .join(id)
            .join("terminals.json"),
    )
    .unwrap_or_default()
}

/// The app's relay for the scene's project; `run` names this start's audit log.
fn relay(scene: &Scene, run: &str) -> Relay {
    Relay::start(RelayConfig {
        layout: scene.layout.clone(),
        project: Some(scene.project.clone()),
        extras: LaunchExtras::default(),
        audit: Audit::open(&scene.root.join(format!("audit-{run}.jsonl"))).unwrap(),
        wake: no_wake(),
        default_tools: vec![Tool::OpenCode],
        pause: None,
        utility_settings: None,
        auto_title: false,
        after_update: false,
    })
    .unwrap()
}

fn wait(what: &str, limit: Duration, done: impl Fn() -> bool) {
    let deadline = Instant::now() + scaled(limit);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Through the relay: the first message sent to a new OpenCode terminal is
/// acknowledged, and the session OpenCode makes for it is saved while the
/// turn still runs. So an app that quits in that turn starts the terminal
/// again on the same conversation.
#[test]
#[ignore = "needs OpenCode installed in ~/.opencode/bin; run with --ignored"]
fn a_first_message_names_the_session_so_a_quit_in_its_turn_resumes_it() {
    let scene = Scene::new("relay");
    let first = relay(&scene, "first");
    let pane = first.pane("opencode").expect("the OpenCode terminal");
    wait("the terminal to be ready", Duration::from_secs(60), || {
        first.view().sessions[0].readiness == Readiness::Ready
    });
    // OpenCode draws its prompt a moment after its plugin reports.
    wait_for_prompt(&pane.session().unwrap());
    first
        .send(SendRequest {
            target: 0,
            attempt: pane.attempt(),
            body: "please SLOW, the first message".to_owned(),
            confirmed: false,
            source: None,
            origin: None,
        })
        .unwrap();
    wait(
        "the message to be acknowledged",
        Duration::from_secs(60),
        || {
            first
                .view()
                .ledger
                .first()
                .is_some_and(|row| row.state == DeliveryState::Acknowledged)
        },
    );
    // Saved while the turn runs (the stand-in answers after six seconds):
    // the session, and that it is at work (written a moment apart).
    wait("the session to be saved", Duration::from_secs(5), || {
        let list = saved_list(&scene);
        list.contains(r#""session": "ses_"#) && list.contains(r#""working": true"#)
    });
    let list = saved_list(&scene);
    let session = list
        .split(r#""session": ""#)
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    first.begin_shutdown();
    assert!(first.shut_down(Duration::from_secs(10)));
    drop(first);

    // The next start resumes that conversation.
    let again = relay(&scene, "again");
    let pane = again.pane("opencode").expect("the OpenCode terminal again");
    wait("the resumed terminal", Duration::from_secs(60), || {
        pane.session().is_some()
    });
    common::wait_for_text(&*pane.session().unwrap(), "please SLOW, the first message");
    assert!(
        saved_list(&scene).contains(&session),
        "{}",
        saved_list(&scene)
    );
    again.begin_shutdown();
    assert!(again.shut_down(Duration::from_secs(10)));
}
