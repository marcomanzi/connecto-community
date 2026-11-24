//! A real Pi under connecto's own launch path, against a local fake model.
//!
//! Evidence class REAL-CLI/FAKE-MODEL, as `real_cli.rs`: the installed,
//! unmodified `pi` is started the way a terminal starts it (its profile, the
//! extension Connecto writes, the hook helper), and talks to a fake
//! OpenAI-compatible server that stands where LM Studio would. No provider
//! account, no real model and no spend. It proves what Pi reports and when;
//! nothing about a real provider.
//!
//! Run with `CONNECTO_TEST_PI=/path/to/pi cargo test -p connecto-session --test
//! real_pi -- --ignored --test-threads=1`. Ignored by default because it needs
//! Pi (and so Node.js) installed; `node` is taken from this process's `PATH`.
//! Pi fetches its `fd` helper into the scratch profile on first start, so the
//! first run needs the network. `CONNECTO_TEST_DUMP` prints the screens.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::{hook_binary, no_wake, screen_text};
use connecto_adapter::{ComposerState, LogMark, Screen, pi_composer_state};
use connecto_core::{
    GridSize, InputModes, Reply, SessionStatus, TermSnapshot, TerminalSession, ToolEvent,
    encode_paste,
};
use connecto_session::launch::{
    BridgeInfo, LaunchExtras, Layout, adapter, set_pi_providers, start_resuming,
};
use connecto_session::{LiveSession, PiProviders};
use connecto_store::{Codesign, Installed, Store, Tool};

const SIZE: GridSize = GridSize {
    cols: 120,
    rows: 40,
};
const TURN_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the fake model takes over a prompt that says SLOW.
const SLOW_REPLY: Duration = Duration::from_secs(20);

/// An OpenAI-compatible server with one model. It answers a chat completion
/// with `REPLY <the last user text>`, after [`SLOW_REPLY`] when that text says
/// SLOW.
struct FakeModel {
    url: String,
    stop: Arc<AtomicBool>,
}

fn respond(mut stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut length = 0_usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0_u8; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    if request_line.starts_with("GET") {
        let models = if request_line.contains("/api/v0/models") {
            r#"{"data":[{"id":"fake-one","type":"llm","max_context_length":65536},{"id":"embedder","type":"embeddings"}]}"#
        } else {
            r#"{"object":"list","data":[{"id":"fake-one","object":"model"}]}"#
        };
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{models}",
            models.len()
        );
        return;
    }
    let request: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let last_user = request["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message["role"] == "user")
        })
        .map(|message| match &message["content"] {
            serde_json::Value::String(text) => text.clone(),
            other => other
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        })
        .unwrap_or_default();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n"
    );
    let _ = stream.flush();
    if last_user.contains("SLOW") {
        thread::sleep(SLOW_REPLY);
    }
    let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
        serde_json::json!({
            "id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "fake-one",
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    };
    let text = format!("REPLY {last_user}");
    let first = chunk(
        serde_json::json!({ "role": "assistant", "content": text }),
        serde_json::Value::Null,
    );
    let last = chunk(serde_json::json!({}), serde_json::json!("stop"));
    let _ = write!(stream, "data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
    let _ = stream.flush();
}

impl FakeModel {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = stream {
                    thread::spawn(move || respond(stream));
                }
            }
        });
        Self { url, stop }
    }
}

impl Drop for FakeModel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
    }
}

struct Scene {
    root: PathBuf,
    layout: Layout,
    project: PathBuf,
    /// Kept for its lifetime: the server stops when the scene goes.
    _model: FakeModel,
    installed: Installed,
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Scene {
    fn new(name: &str) -> Self {
        let pi = PathBuf::from(
            std::env::var_os("CONNECTO_TEST_PI")
                .expect("set CONNECTO_TEST_PI to the installed pi; see the file header"),
        );
        let root = std::env::temp_dir().join(format!("mg-pi-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home/project", "data", "bin"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        // The scratch home has no search path of its own, so `node` is given
        // to Pi by a launcher, as a user's shell would.
        let entry = root.join("bin/pi");
        fs::write(
            &entry,
            format!(
                "#!/bin/sh\nPATH='{}':\"$PATH\"\nexport PATH\nexec '{}' \"$@\"\n",
                std::env::var("PATH").unwrap(),
                pi.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        let store = Store::new(&layout.data_root);
        let installed = store.provision(Tool::Pi, &entry, &Codesign).unwrap();
        // Run where it is: nothing was copied.
        assert_eq!(installed.entry, entry);
        store.install_hook(&hook_binary()).unwrap();
        let model = FakeModel::start();
        set_pi_providers(PiProviders {
            env: Vec::new(),
            lmstudio_url: Some(model.url.clone()),
            default_model: None,
        });
        Self {
            project: root.join("home/project"),
            root,
            layout,
            _model: model,
            installed,
        }
    }

    fn start(&self, resume: Option<&str>) -> LiveSession {
        start_resuming(
            &self.layout,
            &self.project,
            Tool::Pi,
            &self.installed,
            SIZE,
            no_wake(),
            &LaunchExtras {
                model: Some("lmstudio/fake-one".to_owned()),
                ..LaunchExtras::default()
            },
            &BridgeInfo::absent(),
            resume,
        )
        .unwrap()
    }
}

fn screen(session: &LiveSession) -> String {
    let mut snapshot = TermSnapshot::default();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        session.poll_snapshot(&mut snapshot);
        if !snapshot.cells.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    screen_text(&snapshot)
}

fn dump(label: &str, text: &str) {
    if std::env::var_os("CONNECTO_TEST_DUMP").is_some() {
        eprintln!("---- {label}\n{text}\n----");
    }
}

fn wait_for_state(session: &LiveSession, wanted: ComposerState, limit: Duration) -> String {
    let deadline = Instant::now() + limit;
    loop {
        let text = screen(session);
        if pi_composer_state(Screen::plain(&text)) == wanted {
            dump(&format!("{wanted:?}"), &text);
            return text;
        }
        assert_eq!(session.status(), SessionStatus::Running, "screen:\n{text}");
        assert!(
            Instant::now() < deadline,
            "the editor never read as {wanted:?}; it reads as {:?}:\n{text}",
            pi_composer_state(Screen::plain(&text))
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn next_event(session: &LiveSession, limit: Duration) -> Option<ToolEvent> {
    let hook = session.hook().unwrap();
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(event) = hook.events().recv_timeout(Duration::from_millis(200)) {
            return Some(event);
        }
    }
    None
}

fn expect_event(session: &LiveSession, what: &str) -> ToolEvent {
    next_event(session, TURN_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "no {what} event. rejected={} screen:\n{}",
            session.hook().unwrap().rejected(),
            screen(session)
        )
    })
}

/// The session's id and its session file, from the start event.
fn started(session: &LiveSession) -> (String, PathBuf) {
    match expect_event(session, "start") {
        ToolEvent::Started {
            tool_session,
            turn_log,
        } => (
            tool_session.expose().clone(),
            PathBuf::from(turn_log.expect("a session file").expose()),
        ),
        other => panic!("the first event was {other:?}"),
    }
}

fn send_message(session: &LiveSession, text: &str) {
    let mut snapshot = TermSnapshot::default();
    session.poll_snapshot(&mut snapshot);
    let modes: InputModes = snapshot.modes;
    session.send_input(encode_paste(text, modes)).unwrap();
    thread::sleep(Duration::from_millis(400));
    session.send_input(b"\r".to_vec()).unwrap();
}

fn marks(log: &std::path::Path) -> Vec<LogMark> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| adapter(Tool::Pi).read_log(line, true))
        .filter(|mark| *mark != LogMark::Other)
        .collect()
}

fn stop(session: &LiveSession) {
    session.terminate();
    assert!(session.wait_exit(Duration::from_secs(8)));
}

#[test]
#[ignore = "needs Pi installed; see the file header"]
fn pi_reports_a_turn_offers_the_local_models_and_resumes_its_session() {
    let scene = Scene::new("turn");
    let session = scene.start(None);
    let (id, log) = started(&session);
    // The session file is in Pi's own folder under the scratch home, which is
    // the only place a turn log is read from.
    let profile = scene.layout.home.join(".pi/agent");
    assert!(log.starts_with(&profile), "{}", log.display());
    assert!(
        adapter(Tool::Pi)
            .turn_log(&log, session.hook_ctx().unwrap())
            .is_some()
    );
    let fresh = wait_for_state(&session, ComposerState::Empty, TURN_TIMEOUT);
    // The model came from the fake server, by way of the extension.
    assert!(
        fresh.contains("fake-one"),
        "no local model in the footer:\n{fresh}"
    );

    // What Pi reports for a prompt is the prompt in its ack form.
    let sent = "  say hello to connecto  ";
    send_message(&session, sent);
    let prompt = expect_event(&session, "prompt");
    assert_eq!(
        prompt,
        ToolEvent::PromptSubmitted {
            prompt: connecto_core::EventText::new(adapter(Tool::Pi).ack_form(sent)),
            turn: None,
        }
    );
    match expect_event(&session, "turn end") {
        ToolEvent::TurnEnded {
            reply: Reply::Text(reply),
            lifecycle_known: true,
            background,
            ..
        } => {
            assert_eq!(reply.expose(), "REPLY say hello to connecto");
            assert!(background.is_empty());
        }
        other => panic!("the turn ended as {other:?}"),
    }
    wait_for_state(&session, ComposerState::Empty, TURN_TIMEOUT);
    assert_eq!(
        marks(&log),
        [
            LogMark::Prompt {
                text: "say hello to connecto".to_owned()
            },
            LogMark::FinalReply,
        ]
    );
    stop(&session);

    // The same conversation again: the id Pi reported is the one it resumes.
    let resumed = scene.start(Some(&id));
    let (again, again_log) = started(&resumed);
    assert_eq!(again, id);
    assert_eq!(again_log, log);
    wait_for_state(&resumed, ComposerState::Empty, TURN_TIMEOUT);
    // The earlier turn is drawn again, a moment after the editor.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !screen(&resumed).contains("REPLY say hello to connecto") {
        assert!(
            Instant::now() < deadline,
            "the resumed conversation is not shown:\n{}",
            screen(&resumed)
        );
        thread::sleep(Duration::from_millis(100));
    }
    stop(&resumed);
}

#[test]
#[ignore = "needs Pi installed; see the file header"]
fn pi_reports_no_end_for_an_interrupted_turn_and_the_session_file_marks_it() {
    let scene = Scene::new("interrupt");
    let session = scene.start(None);
    let (_, log) = started(&session);
    wait_for_state(&session, ComposerState::Empty, TURN_TIMEOUT);

    send_message(&session, "a SLOW one");
    assert!(matches!(
        expect_event(&session, "prompt"),
        ToolEvent::PromptSubmitted { .. }
    ));
    wait_for_state(&session, ComposerState::Busy, TURN_TIMEOUT);
    // A draft typed while it works reads as a draft, not as busy.
    session.send_input(b"next".to_vec()).unwrap();
    wait_for_state(&session, ComposerState::Occupied, Duration::from_secs(10));
    session.send_input(b"\x7f\x7f\x7f\x7f".to_vec()).unwrap();
    wait_for_state(&session, ComposerState::Busy, Duration::from_secs(10));

    session.send_input(b"\x1b".to_vec()).unwrap();
    wait_for_state(&session, ComposerState::Empty, Duration::from_secs(15));
    let after: Vec<ToolEvent> =
        std::iter::from_fn(|| next_event(&session, Duration::from_secs(3))).collect();
    assert!(
        !after.iter().any(ToolEvent::is_terminal),
        "an end was reported for the interrupted turn: {after:?}"
    );
    assert_eq!(
        marks(&log),
        [
            LogMark::Prompt {
                text: "a SLOW one".to_owned()
            },
            LogMark::Interrupt { turn: None },
        ]
    );

    // The next turn is reported as any other.
    send_message(&session, "and a quick one");
    assert!(matches!(
        expect_event(&session, "prompt"),
        ToolEvent::PromptSubmitted { .. }
    ));
    assert!(expect_event(&session, "turn end").is_terminal());
    stop(&session);
}
