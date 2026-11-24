//! M3 on Windows: the real Claude Code and Codex through Connecto's own code
//! path. Each test finds the user's install (discovery), copies it into a
//! temporary store with the real Authenticode check, and starts it in a
//! ConPTY from a temporary data folder and home, the user's own
//! `%USERPROFILE%\.claude`, `.claude.json` and `.codex` are never used (only
//! the installed programs are read). No login and no account: the turn tests
//! talk to a local fake model (`tests/fixtures/fake_model.py`, needs Python 3
//! as `py`), the start-up tests stop at the first screen.
//!
//! Run by hand, one at a time, with both agents installed:
//!
//! ```text
//! cargo test -p connecto-session --features test-endpoints --test windows_agents -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! `CONNECTO_TEST_DUMP=1` prints the screens (fake-model text and the CLIs'
//! own start-up screens only).

#![cfg(all(windows, feature = "test-endpoints"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::{hook_binary, no_wake, process_exists, screen_text};
use connecto_adapter::{Readiness, ToolAdapter};
use connecto_audit::Audit;
use connecto_core::{
    AuthState, ConfigMode, GridSize, InputModes, PaneControl, PaneStage, ProjectSettings,
    SessionStatus, TermActivity, TermSnapshot, TerminalSession, encode_paste,
};
use connecto_session::launch::{
    BridgeInfo, LaunchExtras, Layout, adapter, check_auth, project_id, start_verified, validated,
};
use connecto_session::{LiveSession, ManagedPane, Relay, RelayConfig, project_settings};
use connecto_store::{Codesign, Installed, Store, Tool};

const SIZE: GridSize = GridSize {
    cols: 120,
    rows: 40,
};
const WAIT: Duration = Duration::from_secs(120);

/// Where the user installed `tool`, as the app finds it.
fn real_install(tool: Tool) -> PathBuf {
    let home = connecto_store::home_dir().expect("a home folder");
    Store::find_installed(tool, &home).unwrap_or_else(|| panic!("{} is not installed", tool.name()))
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn dump(label: &str, text: &str) {
    if std::env::var_os("CONNECTO_TEST_DUMP").is_some() {
        println!("---- {label}\n{text}\n----");
    }
}

/// A temporary data folder, home and project (with a space and a dot in its
/// name), and a store holding a verified copy of the real `tool`.
struct Scene {
    root: PathBuf,
    layout: Layout,
    project: PathBuf,
    installed: Installed,
    model: Option<Child>,
    url: Option<String>,
}

impl Drop for Scene {
    fn drop(&mut self) {
        if let Some(model) = &mut self.model {
            let _ = model.kill();
            let _ = model.wait();
        }
        // Files of a process that just ended can stay locked for a moment.
        for _ in 0..20 {
            if fs::remove_dir_all(&self.root).is_ok() || !self.root.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
}

impl Scene {
    fn new(tool: Tool, with_model: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "connecto-wa-{}-{}-{}",
            tool.name(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let _ = fs::remove_dir_all(&root);
        let project = root.join("work").join("proj dir.x_y");
        for dir in [&project, &root.join("home"), &root.join("data")] {
            fs::create_dir_all(dir).unwrap();
        }
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        // Per-project profiles (`CLAUDE_CONFIG_DIR`, `CODEX_HOME` in the data
        // folder), not the default "system" mode: on Windows the CLIs find
        // the user's own folders by `USERPROFILE`, not by this test's home.
        let id = project_id(&validated(&layout, &project).unwrap());
        let settings = project_settings::path(&layout.data_root, &id);
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            project_settings::to_json(&ProjectSettings {
                claude_config: ConfigMode::Isolated,
                codex_config: ConfigMode::Isolated,
                ..ProjectSettings::default()
            }),
        )
        .unwrap();
        // Discovery, then the real signature check and copy.
        let entry = real_install(tool);
        let store = Store::new(&layout.data_root);
        let started = Instant::now();
        let installed = store
            .provision(tool, &entry, &Codesign)
            .unwrap_or_else(|err| panic!("{} was refused: {err}", tool.name()));
        println!(
            "{}: found {} -> stored {} (version {}) in {:.1} s",
            tool.name(),
            entry.display(),
            installed.entry.display(),
            installed.version,
            started.elapsed().as_secs_f64()
        );
        store.install_hook(&hook_binary()).unwrap();
        let (model, url) = if with_model {
            let (model, url) = start_fake_model(&root);
            (Some(model), Some(url))
        } else {
            (None, None)
        };
        Self {
            root,
            layout,
            project,
            installed,
            model,
            url,
        }
    }

    fn extras(&self) -> LaunchExtras {
        LaunchExtras {
            fake_model: self.url.clone(),
            ..LaunchExtras::default()
        }
    }

    /// The relay the app makes for a window, with one terminal of `tool`.
    fn relay(&self, tool: Tool) -> (Relay, Arc<ManagedPane>) {
        let relay = Relay::start(RelayConfig {
            layout: self.layout.clone(),
            project: Some(self.project.clone()),
            extras: self.extras(),
            audit: Audit::open(&self.root.join("audit.jsonl")).unwrap(),
            wake: no_wake(),
            default_tools: vec![tool],
            pause: None,
            utility_settings: None,
            auto_title: false,
            after_update: false,
        })
        .unwrap();
        let pane = relay.pane(tool.name()).unwrap();
        (relay, pane)
    }
}

/// The local fake model on a free port, and its address.
fn start_fake_model(root: &Path) -> (Child, String) {
    let port = free_port();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fake_model.py");
    let command_file = root.join("tool-command.txt");
    fs::write(&command_file, "echo fake").unwrap();
    let model = Command::new("py")
        .args(["-3", "-I"])
        .arg(fixture)
        .arg(port.to_string())
        .arg(root.join("model-metadata.jsonl"))
        .arg(&command_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Python 3 (`py`) runs the fake model");
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the fake model did not start");
        thread::sleep(Duration::from_millis(50));
    }
    (model, format!("http://127.0.0.1:{port}"))
}

fn screen(session: &LiveSession) -> String {
    let mut snapshot = TermSnapshot::default();
    session.poll_snapshot(&mut snapshot);
    screen_text(&snapshot)
}

fn modes(session: &LiveSession) -> InputModes {
    let mut snapshot = TermSnapshot::default();
    session.poll_snapshot(&mut snapshot);
    snapshot.modes
}

/// Answer first-run screens by what they say until `is_ready`. In the test
/// only: the app never answers a screen for the user.
fn clear_first_run(
    session: &LiveSession,
    is_ready: impl Fn(&str) -> bool,
    steps: &[(&str, &[u8])],
) -> String {
    let deadline = Instant::now() + WAIT;
    let mut last = String::new();
    let mut answered_at = Instant::now();
    loop {
        let text = screen(session);
        assert_eq!(
            session.status(),
            SessionStatus::Running,
            "it ended; screen:\n{text}"
        );
        if text != last {
            dump("screen", &text);
            last.clone_from(&text);
        }
        if is_ready(&text) {
            return text;
        }
        assert!(Instant::now() < deadline, "stuck on:\n{text}");
        if answered_at.elapsed() > Duration::from_millis(1500)
            && let Some((marker, keys)) = steps.iter().find(|(marker, _)| text.contains(marker))
        {
            println!("answering {marker:?}");
            session.send_input(keys.to_vec()).unwrap();
            answered_at = Instant::now();
        }
        thread::sleep(Duration::from_millis(100));
    }
}

const ENTER: &[u8] = b"\r";
const DOWN_ENTER: &[u8] = b"\x1b[B\r";

fn clear_claude_first_run(session: &LiveSession) -> String {
    clear_first_run(
        session,
        |text| text.contains("bypass permissions on") || text.contains("? for shortcuts"),
        &[
            ("Yes, I accept", DOWN_ENTER),
            ("Yes, I trust", DOWN_ENTER),
            ("Yes, proceed", ENTER),
            ("Choose the text style", ENTER),
            ("Security notes", ENTER),
            ("Press Enter to continue", ENTER),
            ("Use this API key", DOWN_ENTER),
        ],
    )
}

/// Codex is at its composer with its footer (the model line). Connecto's own
/// readiness also wants `? for shortcuts`, which 0.167.2 on Windows does not
/// show (see the report it prints).
fn clear_codex_first_run(session: &LiveSession) -> String {
    let codex: &dyn ToolAdapter = adapter(Tool::Codex);
    clear_first_run(
        session,
        |text| {
            codex.readiness(text) == Readiness::Ready
                || (text.lines().any(|line| line.trim_start().starts_with('›'))
                    && text.contains("gpt-fake"))
        },
        &[
            ("Do you trust", ENTER),
            ("Yes, continue", ENTER),
            ("Press enter to continue", ENTER),
        ],
    )
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool, show: impl Fn() -> String) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "waiting for {what}:\n{}", show());
        thread::sleep(Duration::from_millis(100));
    }
}

fn entry(relay: &Relay, id: &str) -> (TermActivity, u64) {
    let view = relay.terminals().view();
    let row = view.terminals.iter().find(|row| row.id == id).unwrap();
    (row.state, row.finished_turns)
}

fn wait_running(pane: &ManagedPane) -> Arc<LiveSession> {
    wait_for(
        "the terminal to run",
        || match pane.stage() {
            PaneStage::Running => true,
            PaneStage::Failed { reason } => panic!("the start failed: {reason}"),
            PaneStage::NeedsLogin { auth } => panic!("it asks for a login: {auth:?}"),
            _ => false,
        },
        || format!("{:?}", pane.stage()),
    );
    pane.session().unwrap()
}

/// Close the terminal as the app does, and check its process is gone.
fn close(relay: Relay, pane: &ManagedPane, pid: i32) {
    pane.close();
    assert!(pane.wait_exit(Duration::from_secs(15)), "it did not exit");
    relay.shut_down(Duration::from_secs(2));
    pane.release();
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_exists(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!process_exists(pid), "process {pid} is still there");
}

/// One turn through the TUI: the prompt is pasted as the app pastes, then
/// Enter. The hook events (over the named pipe) move the terminal's state.
fn one_turn(relay: &Relay, id: &str, session: &LiveSession, reply_marker: &str) {
    let (state, before) = entry(relay, id);
    println!("{id} before the turn: {state:?}, {before} turns");
    session
        .send_input(encode_paste("say hello please", modes(session)))
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    session.send_input(ENTER.to_vec()).unwrap();
    let mut seen = Vec::new();
    wait_for(
        "the turn to end",
        || {
            let (state, turns) = entry(relay, id);
            if seen.last() != Some(&state) {
                println!("{id}: {state:?} ({turns} turns)");
                seen.push(state);
            }
            turns > before
        },
        || screen(session),
    );
    let text = screen(session);
    dump("after the turn", &text);
    assert!(text.contains(reply_marker), "{text}");
    let hook = session.hook().unwrap();
    assert_eq!(hook.rejected(), 0, "a hook caller was refused");
    println!("{id}: states seen {seen:?}");
}

/// Claude Code, interactive, in a ConPTY, through the app's relay: discovered,
/// Authenticode-checked, copied, started with the generated settings (hooks in
/// exec form), and its hooks reach the app over the named pipe: the
/// terminal's state follows `SessionStart`, `UserPromptSubmit` and `Stop`.
#[test]
#[ignore = "needs the installed Claude Code and Python; see the file header"]
fn claude_runs_a_turn_in_a_conpty_and_its_hooks_reach_the_app() {
    let scene = Scene::new(Tool::Claude, true);
    let (relay, pane) = scene.relay(Tool::Claude);
    let session = wait_running(&pane);
    let pid = session.pid().unwrap();
    let ready = clear_claude_first_run(&session);
    dump("ready", &ready);
    // The generated settings: exec-form hooks, no `\\?\` anywhere.
    let profile = session.hook_ctx().unwrap().profile.clone();
    assert!(
        !profile.to_string_lossy().starts_with(r"\\?\"),
        "{profile:?}"
    );
    wait_for(
        "SessionStart to make it idle",
        || entry(&relay, "claude").0 == TermActivity::Idle,
        || format!("{:?}\n{}", entry(&relay, "claude"), screen(&session)),
    );
    one_turn(&relay, "claude", &session, "FAKE-CLAUDE-REPLY");
    // The transcript is where Claude keeps it for this working folder.
    let projects = profile.join("projects");
    let folders: Vec<String> = fs::read_dir(&projects)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    println!("claude transcript folders: {folders:?}");
    assert!(
        folders
            .iter()
            .any(|name| name.ends_with("-work-proj-dir-x-y")),
        "{folders:?}"
    );
    close(relay, &pane, pid);
}

/// Codex, interactive, in a ConPTY, through the app's relay: discovered (npm's
/// `codex.cmd` mapped to the real package), Authenticode-checked, copied; the
/// hook trust read with `codex app-server`; started with the PowerShell hook
/// commands and that trust; `SessionStart` and `UserPromptSubmit` reach the
/// app over the named pipe, and the turn end arrives by `notify`.
#[test]
#[ignore = "needs the installed Codex and Python; see the file header"]
fn codex_runs_a_turn_in_a_conpty_and_its_hooks_and_notify_reach_the_app() {
    let scene = Scene::new(Tool::Codex, true);
    let (relay, pane) = scene.relay(Tool::Codex);
    let session = wait_running(&pane);
    let pid = session.pid().unwrap();
    println!("bridge: {:?}", pane.bridge_off_reason());
    let ready = clear_codex_first_run(&session);
    dump("ready", &ready);
    println!(
        "codex readiness at its composer: {:?}",
        adapter(Tool::Codex).readiness(&ready)
    );
    // Codex's TUI runs `SessionStart` with the first turn, not at start-up
    // (as seen here on 0.167.2): the terminal says Starting until then.
    thread::sleep(Duration::from_secs(3));
    println!(
        "codex at its composer: {:?}, hook callers refused: {}",
        entry(&relay, "codex"),
        session.hook().unwrap().rejected()
    );
    one_turn(&relay, "codex", &session, "FAKE-CODEX-REPLY");
    close(relay, &pane, pid);
}

/// The status commands of the real programs, in a fresh profile: logged out,
/// read from their JSON / text (no console window, no login).
#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn real_status_commands_say_logged_out_in_a_fresh_profile() {
    for tool in [Tool::Claude, Tool::Codex] {
        let scene = Scene::new(tool, false);
        let state = check_auth(&scene.layout, &scene.project, tool, &scene.installed).unwrap();
        println!("{} status: {state:?}", tool.name());
        assert_eq!(state, AuthState::LoggedOut);
    }
}

/// The interactive start without a login: the TUI starts in the ConPTY, draws
/// its first screen (a sign-in or set-up screen), and ends on terminate.
#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn logged_out_tuis_draw_their_first_screen_and_end_on_terminate() {
    for tool in [Tool::Claude, Tool::Codex] {
        let scene = Scene::new(tool, false);
        let session = start_verified(
            &scene.layout,
            &scene.project,
            tool,
            &scene.installed,
            SIZE,
            no_wake(),
            &LaunchExtras::default(),
            &BridgeInfo::absent(),
        )
        .unwrap();
        let pid = session.pid().unwrap();
        let mut text = String::new();
        wait_for(
            "a first screen",
            || {
                text = screen(&session);
                text.lines().filter(|line| !line.trim().is_empty()).count() >= 3
            },
            || screen(&session),
        );
        thread::sleep(Duration::from_secs(3));
        let text = screen(&session);
        println!("---- {} first screen\n{text}\n----", tool.name());
        assert_eq!(session.status(), SessionStatus::Running, "{text}");
        // A resize reaches the TUI through ConPTY: it draws again, at the new
        // width, and keeps running.
        let narrow = GridSize { cols: 70, rows: 30 };
        println!("{} resize: {:?}", tool.name(), session.resize(narrow));
        thread::sleep(Duration::from_secs(3));
        let mut snapshot = TermSnapshot::default();
        session.poll_snapshot(&mut snapshot);
        let resized = screen_text(&snapshot);
        println!("---- {} after resize\n{resized}\n----", tool.name());
        assert_eq!(snapshot.cols, narrow.cols);
        assert!(
            resized
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
                >= 3,
            "{resized}"
        );
        assert_eq!(session.status(), SessionStatus::Running, "{resized}");
        session.terminate();
        assert!(session.wait_exit(Duration::from_secs(10)));
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_exists(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_exists(pid));
    }
}
