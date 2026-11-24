//! The bridge through the real hook helper, socket, `connecto` command, Gate and
//! audit log, with a stand-in for each tool (section 7: AC4 to AC13).
//!
//! The agent is a script that runs a shell command when a prompt says `RUN`, so
//! `connecto` really runs inside the terminal's process tree, as it would for a
//! model's shell tool. No model, no login, no network.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use common::{hook_binary, no_wake};
use connecto_audit::Audit;
use connecto_core::{InputModes, PaneControl, RelayControl, TerminalSession, encode_paste};
use connecto_session::launch::{LaunchExtras, Layout};
use connecto_session::{ManagedPane, Relay, RelayConfig};
use connecto_store::{Signature, Store, Tool};
use serde_json::Value;

/// The limit for each wait; `CONNECTO_TEST_TIME_SCALE` stretches it on a slow machine.
const WAIT: Duration = Duration::from_secs(60);

struct AnySignature;

impl Signature for AnySignature {
    fn verify(&self, _file: &Path, _team: Option<&str>) -> Result<(), String> {
        Ok(())
    }
}

/// Each scene runs several Python tools and a real PTY each, so scenes take a
/// permit: past a few at once the timing of the tests, not the bridge, fails.
/// What the audit log does with an agent's allowed `type`.
#[derive(Clone, Copy)]
enum SinkMode {
    Plain,
    /// Says when it has started, then holds the write until the test has
    /// acted inside the wait ([`Scene::release`]), at most 120 ms (under the
    /// Gate's 200 ms wait). A fixed 120 ms left too little margin on a busy
    /// CI runner, where the Gate then gave up on the audit first.
    Slow,
    /// Cannot write it.
    Fail,
}

struct TestSink {
    file: fs::File,
    mode: SinkMode,
    entered: std::sync::Arc<std::sync::atomic::AtomicBool>,
    released: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// While not zero, each cross-project record takes this many
    /// milliseconds: past the Gate's 200 ms wait the log is behind, as under
    /// backpressure (ADR 0035).
    slow_cross: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Set when a slowed cross-project record starts.
    cross_entered: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl connecto_audit::Sink for TestSink {
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        use std::io::Write;
        let slow = self.slow_cross.load(std::sync::atomic::Ordering::SeqCst);
        if slow > 0 && line.contains(r#""action":"cross_project_"#) {
            self.cross_entered
                .store(true, std::sync::atomic::Ordering::SeqCst);
            thread::sleep(Duration::from_millis(slow));
        }
        if line.contains(r#""action":"type""#) && line.contains(r#""decision":"allow""#) {
            match self.mode {
                SinkMode::Plain => {}
                SinkMode::Slow => {
                    self.entered
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    let until = Instant::now() + Duration::from_millis(120);
                    while !self.released.load(std::sync::atomic::Ordering::SeqCst)
                        && Instant::now() < until
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                SinkMode::Fail => return Err(std::io::Error::other("disk full")),
            }
        }
        writeln!(self.file, "{line}")
    }
}

struct Scene {
    slow_cross: std::sync::Arc<std::sync::atomic::AtomicU64>,
    cross_entered: std::sync::Arc<std::sync::atomic::AtomicBool>,
    entered: std::sync::Arc<std::sync::atomic::AtomicBool>,
    released: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _permit: common::Permit,
    root: PathBuf,
    project: PathBuf,
    /// Where the stand-in tools run now: the project, or another after a switch.
    current: std::sync::Mutex<PathBuf>,
    audit_file: PathBuf,
    relay: Relay,
}

fn unlock(path: &Path) {
    if path.is_dir() && !path.is_symlink() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                unlock(&entry.path());
            }
        }
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        self.relay.begin_shutdown();
        self.relay.stop_all();
        for pane in self.relay.panes() {
            pane.close();
            let _ = pane.wait_exit(Duration::from_secs(7));
        }
        self.relay.shut_down(Duration::from_secs(1));
        for pane in self.relay.panes() {
            pane.release();
        }
        unlock(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Save settings that say Isolated for a project, as an existing project has.
fn pin_isolated(data_root: &Path, project: &Path) {
    use connecto_core::{ConfigMode, ProjectSettings};
    let id = connecto_session::launch::project_id(&fs::canonicalize(project).unwrap());
    connecto_session::project_settings::save(
        data_root,
        &id,
        &ProjectSettings {
            claude_config: ConfigMode::Isolated,
            codex_config: ConfigMode::Isolated,
            ..ProjectSettings::default()
        },
    )
    .unwrap();
}

fn layout_data_projects(root: &Path) -> PathBuf {
    root.join("data/projects")
}

fn wrapper(version_line: &str) -> String {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_tool.py");
    format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{version_line}\"; exit 0; fi\n\
         if [ \"$1\" = \"auth\" ] && [ \"$2\" = \"login\" ]; then sleep 60; exit 0; fi\n\
         if [ \"$1\" = \"auth\" ] || [ \"$1\" = \"login\" ]; then echo '{{\"loggedIn\":true,\"authMethod\":\"fake\"}}'; exit 0; fi\n\
         printf '%s\\n' \"$@\" >> \"args-$CONNECTO_TERMINAL.txt\"\n\
         if [ -e resume-fails ]; then for arg in \"$@\"; do if [ \"$arg\" = --resume ]; then echo 'No conversation found'; exit 1; fi; done; fi\n\
         exec /usr/bin/python3 '{}'\n",
        fixture.display()
    )
}

/// A Codex release folder in the shape the store copies, whose `bin/codex`
/// is a stand-in: `app-server` answers `hooks/list`, anything else is the tool.
fn provision_codex(root: &Path, store: &Store) {
    let release = root.join("source/codex-release");
    for dir in ["bin", "codex-path", "codex-resources"] {
        fs::create_dir_all(release.join(dir)).unwrap();
    }
    fs::write(release.join("codex-path/rg"), "x").unwrap();
    fs::write(release.join("codex-resources/data"), "x").unwrap();
    fs::write(release.join("codex-package.json"), "{}").unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli 0.166.0'; exit 0; fi\n\
         if [ \"$1\" = \"login\" ]; then echo 'Logged in using ChatGPT'; exit 0; fi\n\
         for arg in \"$@\"; do if [ \"$arg\" = app-server ]; then [ -e '{}' ] && exit 1; exec /usr/bin/python3 '{}'; fi; done\n\
         printf '%s\\n' \"$@\" >> \"args-$CONNECTO_TERMINAL.txt\"\n\
         FAKE_TOOL=codex exec /usr/bin/python3 '{}'\n",
        root.join("source/no-trust").display(),
        fixtures.join("fake_app_server.py").display(),
        fixtures.join("fake_tool.py").display(),
    );
    let entry = release.join("bin/codex");
    fs::write(&entry, script).unwrap();
    fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
    store.provision(Tool::Codex, &entry, &AnySignature).unwrap();
}

impl Scene {
    /// Two Claude terminals, `claude` and `claude-2`, running.
    fn new(name: &str) -> Self {
        Self::with_tools(name, vec![Tool::Claude, Tool::Claude], "2.1.318")
    }

    /// One Claude and one Codex terminal.
    ///
    /// Codex's hooks are trusted by a 5 s read of `hooks/list`, and a failed read
    /// is cached for good, so a scene starved of CPU by the other tests would
    /// come up with Codex's bridge off. That is the right answer for the app and
    /// the wrong one for this test, so it tries again with a fresh data folder.
    fn mixed(name: &str) -> Self {
        for attempt in 0..3 {
            let scene = Self::with_tools(
                &format!("{name}{attempt}"),
                vec![Tool::Claude, Tool::Codex],
                "2.1.318",
            );
            if scene
                .pane("codex")
                .is_some_and(|pane| pane.bridge_off_reason().is_none())
            {
                return scene;
            }
        }
        panic!("Codex's bridge stayed off in three tries");
    }

    fn with_tools(name: &str, tools: Vec<Tool>, version: &str) -> Self {
        Self::build(name, tools, version, false)
    }

    /// Codex is on a verified version but its hook hashes cannot be read.
    fn untrusted(name: &str) -> Self {
        Self::build(name, vec![Tool::Claude, Tool::Codex], "2.1.318", true)
    }

    fn build(name: &str, tools: Vec<Tool>, version: &str, untrusted: bool) -> Self {
        Self::build_with(name, tools, version, untrusted, SinkMode::Plain)
    }

    /// A scene whose audit log is slow or broken for an agent's `type`.
    fn with_audit(name: &str, mode: SinkMode) -> Self {
        Self::build_with(name, vec![Tool::Claude; 2], "2.1.318", false, mode)
    }

    fn build_with(
        name: &str,
        tools: Vec<Tool>,
        version: &str,
        untrusted: bool,
        mode: SinkMode,
    ) -> Self {
        Self::build_full(name, tools, version, untrusted, mode, &[], true, None)
    }

    /// Terminals told how to boot by files in the project folder (see
    /// `fake_tool.py`). Not waited for: a slow or dying one is the point.
    fn booting(name: &str, files: &[(&str, &str)]) -> Self {
        Self::build_full(
            name,
            vec![Tool::Claude; 2],
            "2.1.318",
            false,
            SinkMode::Plain,
            files,
            false,
            None,
        )
    }

    /// A project that has a saved terminal list from an earlier run, and boot
    /// files. Not waited for.
    fn seeded(name: &str, list: &str, files: &[(&str, &str)]) -> Self {
        Self::build_full(
            name,
            vec![Tool::Claude; 2],
            "2.1.318",
            false,
            SinkMode::Plain,
            files,
            false,
            Some(list),
        )
    }

    /// As [`Scene::seeded`], for a project whose saved Tools setting is
    /// `allowed` before anything starts.
    fn seeded_with_tools(name: &str, list: &str, allowed: connecto_core::ProjectTools) -> Self {
        Self::build_tools(
            name,
            vec![Tool::Claude; 2],
            "2.1.318",
            false,
            SinkMode::Plain,
            &[],
            false,
            Some(list),
            Some(allowed),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_full(
        name: &str,
        tools: Vec<Tool>,
        version: &str,
        untrusted: bool,
        mode: SinkMode,
        boot: &[(&str, &str)],
        wait_ready: bool,
        seed: Option<&str>,
    ) -> Self {
        Self::build_tools(
            name, tools, version, untrusted, mode, boot, wait_ready, seed, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_tools(
        name: &str,
        tools: Vec<Tool>,
        version: &str,
        untrusted: bool,
        mode: SinkMode,
        boot: &[(&str, &str)],
        wait_ready: bool,
        seed: Option<&str>,
        allowed: Option<connecto_core::ProjectTools>,
    ) -> Self {
        let permit = common::permit();
        let root = std::env::temp_dir().join(format!("mg-bridge-{name}-{}", std::process::id()));
        unlock(&root);
        let _ = fs::remove_dir_all(&root);
        for dir in ["home/project", "data", "source"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let entry = root.join("source").join(version);
        common::link_fake(&wrapper(&format!("{version} (fake)")), &entry);
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let store = Store::new(&layout.data_root);
        store
            .provision(Tool::Claude, &entry, &AnySignature)
            .unwrap();
        provision_codex(&root, &store);
        if untrusted {
            fs::write(root.join("source/no-trust"), "").unwrap();
        }
        store.refresh_hook(&hook_binary()).unwrap();
        let project = root.join("home/project");
        for (file, text) in boot {
            let path = project.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        // These tests are about turns read from the tool's own profile folder, so
        // the project is isolated (a new project is on the system configuration).
        pin_isolated(&layout.data_root, &project);
        if let Some(allowed) = allowed {
            let id = connecto_session::launch::project_id(&fs::canonicalize(&project).unwrap());
            let mut settings = connecto_session::project_settings::load(&layout.data_root, &id);
            settings.tools = allowed;
            connecto_session::project_settings::save(&layout.data_root, &id, &settings).unwrap();
        }
        if let Some(list) = seed {
            let id = connecto_session::launch::project_id(&fs::canonicalize(&project).unwrap());
            let dir = layout_data_projects(&root).join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("terminals.json"), list).unwrap();
        }
        let audit_file = root.join("audit.jsonl");
        let entered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let slow_cross = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let cross_entered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let relay = Relay::start(RelayConfig {
            layout,
            project: Some(project.clone()),
            extras: LaunchExtras::default(),
            audit: Audit::with_sink(TestSink {
                file: fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&audit_file)
                    .unwrap(),
                mode,
                entered: std::sync::Arc::clone(&entered),
                released: std::sync::Arc::clone(&released),
                slow_cross: std::sync::Arc::clone(&slow_cross),
                cross_entered: std::sync::Arc::clone(&cross_entered),
            })
            .unwrap(),
            wake: no_wake(),
            default_tools: tools,
            pause: None,
            utility_settings: None,
            auto_title: false,
            after_update: false,
        })
        .unwrap();
        let scene = Self {
            slow_cross,
            cross_entered,
            entered,
            released,
            _permit: permit,
            root,
            current: std::sync::Mutex::new(project.clone()),
            project,
            audit_file,
            relay,
        };
        if wait_ready {
            for id in scene.ids() {
                scene.wait_for(&format!("{id} to be ready"), |s| {
                    s.screen(&id).contains("FAKE-READY")
                });
            }
        }
        scene
    }

    fn screen(&self, id: &str) -> String {
        let Some(session) = self.pane(id).and_then(|pane| pane.session()) else {
            return String::new();
        };
        let mut snapshot = connecto_core::TermSnapshot::default();
        session.poll_snapshot(&mut snapshot);
        common::screen_text(&snapshot)
    }

    fn ids(&self) -> Vec<String> {
        self.relay
            .terminals()
            .view()
            .terminals
            .iter()
            .map(|entry| entry.id.clone())
            .collect()
    }

    fn pane(&self, id: &str) -> Option<std::sync::Arc<ManagedPane>> {
        self.relay.pane(id)
    }

    /// Let a [`SinkMode::Slow`] audit write finish now.
    fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn wait_for(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + common::scaled(WAIT);
        while !done(self) {
            if Instant::now() >= deadline {
                for id in self.ids() {
                    eprintln!(
                        "--- stage of {id}: {:?} ---",
                        self.pane(&id).map(|pane| pane.stage())
                    );
                    if let Some(session) = self.pane(&id).and_then(|pane| pane.session()) {
                        let mut snapshot = connecto_core::TermSnapshot::default();
                        session.poll_snapshot(&mut snapshot);
                        eprintln!("--- screen of {id} ---\n{}", common::screen_text(&snapshot));
                    }
                }
                for id in self.ids() {
                    eprintln!(
                        "--- typed-{id}.log ---\n{}",
                        self.log(&format!("typed-{id}.log"))
                    );
                    eprintln!(
                        "--- keys-{id}.log ---\n{}",
                        self.log(&format!("keys-{id}.log"))
                    );
                }
            }
            assert!(Instant::now() < deadline, "waiting for {what}");
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Where a terminal's fake tool keeps its logs: its working directory.
    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.current.lock().unwrap().join(name)).unwrap_or_default()
    }

    /// The user types `text` and Enter into a terminal.
    fn user_types(&self, id: &str, text: &str) {
        let session = self.pane(id).unwrap().session().unwrap();
        let modes = InputModes {
            bracketed_paste: true,
            ..InputModes::default()
        };
        session.send_input(encode_paste(text, modes)).unwrap();
        thread::sleep(Duration::from_millis(500));
        session.send_input(b"\r".to_vec()).unwrap();
    }

    /// An agent in `id` runs `command` in its shell, as a model's shell tool
    /// would. Returns what the command printed and its exit code.
    fn agent_runs(&self, id: &str, command: &str) -> Run {
        let before = self.runs(id).len();
        self.user_types(id, &format!("RUN {command}"));
        self.wait_for(&format!("{id} to run {command}"), |s| {
            s.runs(id).len() > before
        });
        self.runs(id).pop().unwrap()
    }

    fn runs(&self, id: &str) -> Vec<Run> {
        let text = self.log(&format!("runs-{id}.log"));
        text.lines()
            .map(|line| {
                let value: Value = serde_json::from_str(line).unwrap();
                Run {
                    code: i32::try_from(value["code"].as_i64().unwrap()).unwrap(),
                    out: value["out"].as_str().unwrap().to_owned(),
                    err: value["err"].as_str().unwrap().to_owned(),
                }
            })
            .collect()
    }

    fn audit(&self) -> String {
        assert!(self.relay.shut_down(Duration::from_secs(2)));
        fs::read_to_string(&self.audit_file).unwrap()
    }
}

#[derive(Debug)]
struct Run {
    code: i32,
    out: String,
    err: String,
}

#[test]
fn a_session_gets_the_bridge_environment_and_the_connecto_command() {
    let scene = Scene::new("env");
    let run = scene.agent_runs(
        "claude",
        "echo sock=$CONNECTO_BRIDGE_SOCK term=$CONNECTO_TERMINAL; command -v connecto",
    );
    assert!(run.out.contains("bridge-"), "{run:?}");
    assert!(run.out.contains("/cli/bin/connecto"), "{run:?}");
    let _ = Command::new("true");
}

impl Scene {
    /// What a terminal's tool was given as prompts, oldest first.
    fn prompts(&self, id: &str) -> Vec<String> {
        self.log(&format!("typed-{id}.log"))
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["prompt"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    /// The keys a terminal received, as one string.
    fn keys_seen(&self, id: &str) -> String {
        self.log(&format!("keys-{id}.log"))
            .lines()
            .map(|line| serde_json::from_str::<String>(line).unwrap())
            .collect()
    }

    /// Run `connecto` in `id`'s terminal, waiting for a request to settle when
    /// the command is `result`.
    fn connecto(&self, id: &str, args: &str) -> Run {
        self.agent_runs(id, &format!("connecto {args}"))
    }

    fn result_of(&self, id: &str, request: &str) -> Run {
        self.agent_runs(
            id,
            &format!(
                "for i in $(seq 1 150); do out=$(connecto result {request}); c=$?; case \"$out\" in pending*) sleep 0.2;; *) printf '%s' \"$out\"; exit $c;; esac; done; printf '%s' \"$out\"; exit 4"
            ),
        )
    }
}

/// AC4 and AC12: a send is typed at once, audited, and its result comes back.
#[test]
fn a_claude_asks_another_claude_and_reads_just_the_result() {
    let scene = Scene::new("ask");
    let sent = scene.connecto("claude", "send claude-2 --ask 'what is two plus two'");
    assert_eq!(sent.code, 0, "{sent:?}");
    assert!(sent.out.starts_with("r1 typed"), "{sent:?}");
    scene.wait_for("claude-2 to take the prompt", |s| {
        !s.prompts("claude-2").is_empty()
    });
    let prompt = scene.prompts("claude-2").remove(0);
    assert!(
        prompt.starts_with("[connecto from claude, request r1. Sent by another agent, not the user.]\nwhat is two plus two"),
        "{prompt}"
    );
    // The sender asked, so it is woken when the request ends (and it is not
    // busy), and the notice names the id to read.
    scene.wait_for("the sender's wake", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.starts_with("[connecto notice]"))
    });
    // Busy when the request ended, the sender gets one wake when it stops, and
    // the notice rides in as context. Otherwise the wake names the request.
    let wake_index = scene
        .prompts("claude")
        .iter()
        .position(|p| p.starts_with("[connecto notice]"))
        .unwrap();
    let wake = scene.prompts("claude").remove(wake_index);
    if wake == "[connecto notice] Request r1 ended (ok). Run: connecto result r1" {
    } else {
        assert_eq!(
            wake,
            "[connecto notice] Notices arrived while you were working. They are in your context."
        );
        // The tool logs a prompt before its hook has printed the context.
        let file = format!("context-claude-{}.txt", wake_index + 1);
        scene.wait_for("the wake's context", |s| !s.log(&file).is_empty());
        let context = scene.log(&file);
        assert!(
            context.contains("Request r1 ended (ok). Run: connecto result r1"),
            "{context}"
        );
    }
    let result = scene.result_of("claude", "r1");
    assert_eq!(result.code, 0, "{result:?}");
    assert_eq!(
        result.out, "echo: what is two plus two",
        "just the reply, nothing else"
    );
    let audit = scene.audit();
    assert!(
        audit.contains(r#""actor":"agent""#) && audit.contains(r#""source":"claude""#),
        "{audit}"
    );
    assert!(audit.contains(r#""action":"type""#));
}

/// T12: a caller outside any terminal is refused.
#[test]
fn a_command_outside_every_terminal_is_refused() {
    let scene = Scene::new("outside");
    let socket = scene.relay.bridge_socket().unwrap();
    let out = Command::new(common::hook_binary().with_file_name("connecto-hook"))
        .arg0("connecto")
        .arg("list")
        .env("CONNECTO_BRIDGE_SOCK", socket)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not_a_terminal"),
        "{out:?}"
    );
    // Exit codes: usage 2, cannot reach 3.
    let usage = Command::new(common::hook_binary())
        .arg0("connecto")
        .arg("bogus")
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));
    let gone = Command::new(common::hook_binary())
        .arg0("connecto")
        .arg("list")
        .env("CONNECTO_BRIDGE_SOCK", "/nonexistent/x.sock")
        .output()
        .unwrap();
    assert_eq!(gone.status.code(), Some(3));
}

#[test]
fn list_shows_every_terminal_with_state_and_marks() {
    let scene = Scene::new("list");
    let run = scene.connecto("claude", "list");
    assert_eq!(run.code, 0, "{run:?}");
    assert!(run.out.lines().next().unwrap().starts_with("ID"), "{run:?}");
    assert!(
        run.out.contains("claude-2") && run.out.contains("claude  "),
        "{run:?}"
    );
    assert!(run.out.contains("self"), "{run:?}");
    // The phase is published by the actor, so it can trail the prompt that ran
    // this command by a tick: either word is right.
    let row = run.out.lines().find(|l| l.starts_with("claude ")).unwrap();
    assert!(row.contains("working") || row.contains("idle"), "{run:?}");
}

/// The CONTEXT cell of `id` in `connecto list` output.
fn context_cell(list: &str, id: &str) -> String {
    let row = list
        .lines()
        .find(|line| line.starts_with(&format!("{id} ")))
        .unwrap_or_else(|| panic!("no row for {id}: {list}"));
    row.split_whitespace().nth(3).unwrap().to_owned()
}

/// `list` shows the context use an agent's footer prints at the bottom of its
/// screen, and "-" for one that prints none. A footer the user typed, echoed
/// in the history, is not one.
#[test]
fn list_shows_context_use_from_the_footer() {
    let scene = Scene::new("list-context");
    scene.user_types(
        "claude-2",
        "Opus 5.5 · high  |  [████░░░░░░░░░░░░░░░░] 45% (450k/1M)  |  $1",
    );
    scene.wait_for("the echoed prompt", |s| {
        s.screen("claude-2").contains("45% (450k/1M)")
    });
    let run = scene.connecto("claude", "list");
    assert_eq!(run.code, 0, "{run:?}");
    assert!(
        run.out.lines().next().unwrap().contains("CONTEXT"),
        "{run:?}"
    );
    assert_eq!(context_cell(&run.out, "claude-2"), "-", "{run:?}");
    // Now the fake prints a footer, below a rule, as Claude does.
    scene.user_types("claude-2", "FOOTER");
    scene.wait_for("the footer", |s| {
        s.screen("claude-2").contains("23% (230k/1M)")
    });
    let run = scene.connecto("claude", "list");
    assert_eq!(context_cell(&run.out, "claude-2"), "23%", "{run:?}");
    assert_eq!(context_cell(&run.out, "claude"), "-", "{run:?}");
}

/// AC10: `read` returns the screen and scrollback as text, audits who read
/// what and never the contents.
#[test]
fn read_returns_scrollback_as_text_and_the_audit_holds_no_contents() {
    let scene = Scene::new("read");
    // Fill claude-2's history well past one screen.
    scene.user_types("claude-2", "RUN seq 1 120");
    scene.wait_for("claude-2 to run", |s| !s.runs("claude-2").is_empty());
    scene.user_types("claude-2", "an earlier line SECRET-MARKER-123");
    scene.wait_for("the second prompt", |s| s.prompts("claude-2").len() == 2);
    let read = scene.connecto("claude", "read claude-2");
    assert_eq!(read.code, 0, "{read:?}");
    assert!(
        read.out.contains("FAKE-READY"),
        "the start of the history is there: {read:?}"
    );
    assert!(read.out.contains("SECRET-MARKER-123"));
    let audit = scene.audit();
    assert!(audit.contains(r#""action":"read""#) && audit.contains(r#""source":"claude""#));
    assert!(
        !audit.contains("SECRET-MARKER-123"),
        "contents are never audited"
    );
}

#[test]
fn keys_press_the_named_keys_and_an_unknown_key_sends_nothing() {
    let scene = Scene::new("keys");
    let run = scene.connecto("claude", "keys claude-2 esc down y");
    assert_eq!(run.code, 0, "{run:?}");
    assert!(run.out.starts_with("k1 typed"), "{run:?}");
    scene.wait_for("the keys", |s| s.keys_seen("claude-2").ends_with('y'));
    assert_eq!(scene.keys_seen("claude-2"), "\u{1b}\u{1b}[By");
}

/// AC4 and R14: under Pause an agent's action is refused and nothing reaches
/// the target; `list`, `read` and `result` still work.
#[test]
fn pause_refuses_agent_actions_and_nothing_reaches_the_pty() {
    let scene = Scene::new("pause");
    scene.relay.pause_switch().pause();
    for args in [
        "send claude-2 hello",
        "keys claude-2 esc",
        "add claude",
        "close claude-2",
        "start claude-2",
    ] {
        let run = scene.connecto("claude", args);
        assert_eq!(run.code, 1, "{args}: {run:?}");
        assert!(run.err.contains("paused"), "{args}: {run:?}");
    }
    assert!(scene.prompts("claude-2").is_empty());
    assert!(scene.keys_seen("claude-2").is_empty());
    assert_eq!(scene.connecto("claude", "list").code, 0);
    assert_eq!(scene.connecto("claude", "read claude-2").code, 0);
    assert_eq!(scene.ids().len(), 2);
}

/// AC13 and T11: add starts a terminal at once, close stops it and removes it
/// (the tab goes, and the saved list), start gives a stopped one a new
/// conversation, and the last terminal is only stopped.
#[test]
fn an_agent_adds_starts_and_closes_terminals_and_close_removes_them() {
    let scene = Scene::new("lifecycle");
    let added = scene.connecto("claude", "add claude");
    assert_eq!(added.code, 0, "{added:?}");
    assert!(added.out.starts_with("claude-3 starting"), "{added:?}");
    scene.wait_for("claude-3 to be ready", |s| {
        s.screen("claude-3").contains("FAKE-READY")
    });
    assert_eq!(scene.ids(), ["claude", "claude-2", "claude-3"]);

    // Start on a running terminal is refused; on a stopped one it makes a new
    // conversation.
    let again = scene.connecto("claude", "start claude-2");
    assert_eq!(again.code, 1);
    assert!(again.err.contains("already_running"), "{again:?}");
    let pane = scene.pane("claude-3").unwrap();
    pane.request_stop();
    scene.wait_for("claude-3 to stop", |s| {
        !s.pane("claude-3").unwrap().is_live()
    });
    let before = pane.attempt();
    let started = scene.connecto("claude", "start claude-3");
    assert_eq!(started.code, 0, "{started:?}");
    scene.wait_for("a new conversation", |s| {
        let pane = s.pane("claude-3").unwrap();
        pane.is_live() && pane.attempt() > before
    });

    // Close stops it and removes it, as the remove button does: it leaves the
    // live view, and the saved list.
    let closed = scene.connecto("claude", "close claude-3");
    assert_eq!(closed.code, 0, "{closed:?}");
    assert!(closed.out.contains("removed"), "{closed:?}");
    scene.wait_for("claude-3 to go", |s| s.ids() == ["claude", "claude-2"]);
    assert!(scene.pane("claude-3").is_none());
    let saved = fs::read_to_string(
        scene
            .root
            .join("data/projects")
            .read_dir()
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path()
            .join("terminals.json"),
    )
    .unwrap();
    assert!(
        !saved.contains("claude-3") || saved.contains(r#""claude": 4"#),
        "{saved}"
    );
    assert!(!saved.contains(r#""id": "claude-3""#), "{saved}");
    let gone = scene.connecto("claude", "close claude-3");
    assert!(gone.err.contains("unknown_terminal"), "{gone:?}");
    // There is no operation to remove other than close.
    assert_eq!(scene.connecto("claude", "remove claude-2").code, 2);
}

/// An agent cannot add a CLI the project's Tools setting leaves out: it is
/// told which ones the project uses, and nothing is started. Read at each
/// add, so a change in the settings counts at once.
#[test]
fn an_agent_cannot_add_a_cli_the_project_leaves_out() {
    use connecto_core::{ProjectTools, ToolKind};
    let scene = Scene::new("toolset");
    let data_root = scene.root.join("data");
    let id = connecto_session::launch::project_id(&fs::canonicalize(&scene.project).unwrap());
    let mut settings = connecto_session::project_settings::load(&data_root, &id);
    settings.tools = ProjectTools::from_tools([ToolKind::Claude, ToolKind::Pi]).unwrap();
    connecto_session::project_settings::save(&data_root, &id, &settings).unwrap();
    let refused = scene.connecto("claude", "add codex");
    assert_eq!(refused.code, 1, "{refused:?}");
    assert!(refused.err.contains("not_allowed"), "{refused:?}");
    assert!(
        refused.err.contains("allows only claude, pi"),
        "{refused:?}"
    );
    assert_eq!(scene.ids(), ["claude", "claude-2"], "nothing was added");
    let added = scene.connecto("claude", "add claude");
    assert_eq!(added.code, 0, "{added:?}");
}

/// A stopped terminal of a CLI the project has since left out is not
/// started again: its owner is told which CLIs the project uses.
#[test]
fn a_stopped_terminal_of_a_cli_since_left_out_does_not_start_again() {
    use connecto_core::{ProjectTools, ToolKind};
    let scene = Scene::new("toolset-start");
    let added = scene.connecto("claude", "add claude");
    assert_eq!(added.code, 0, "{added:?}");
    scene.wait_for("claude-3 to be ready", |s| {
        s.screen("claude-3").contains("FAKE-READY")
    });
    let pane = scene.pane("claude-3").unwrap();
    pane.request_stop();
    scene.wait_for("claude-3 to stop", |s| {
        !s.pane("claude-3").unwrap().is_live()
    });
    let before = pane.attempt();
    let data_root = scene.root.join("data");
    let id = connecto_session::launch::project_id(&fs::canonicalize(&scene.project).unwrap());
    let mut settings = connecto_session::project_settings::load(&data_root, &id);
    settings.tools = ProjectTools::from_tools([ToolKind::Codex, ToolKind::Pi]).unwrap();
    connecto_session::project_settings::save(&data_root, &id, &settings).unwrap();
    let refused = scene.connecto("claude", "start claude-3");
    assert_eq!(refused.code, 1, "{refused:?}");
    assert!(refused.err.contains("not_allowed"), "{refused:?}");
    assert!(refused.err.contains("allows only codex, pi"), "{refused:?}");
    std::thread::sleep(Duration::from_millis(300));
    let pane = scene.pane("claude-3").unwrap();
    assert!(!pane.is_live() && pane.attempt() == before, "not started");
}

/// The project's Tools setting is checked where every terminal is added or
/// started, whoever asks: the window's add is refused and makes no entry; a
/// restart, the window's start and a saved terminal brought back at launch
/// end failed with the reason, nothing launched; a shell and an allowed CLI
/// still go. A terminal that was running keeps running.
#[test]
fn the_tools_setting_is_checked_wherever_a_terminal_starts() {
    use connecto_core::{PaneControl, PaneStage, ProjectTools, ToolKind};
    let codex_only = ProjectTools::Codex;
    // Brought back at launch: the working one was to resume.
    let scene = Scene::seeded_with_tools("tools-everywhere", SEEDED, codex_only);
    let failed = |s: &Scene, id: &str| {
        matches!(
            s.pane(id).unwrap().stage(),
            PaneStage::Failed { ref reason } if reason.contains("allows only codex")
        )
    };
    scene.wait_for("the restored claude to be refused", |s| failed(s, "claude"));
    assert!(scene.launch_args("claude").is_empty(), "nothing launched");
    // The window's add: refused, no entry.
    let control = scene.relay.terminals();
    let before = scene.ids();
    control.add(ToolKind::Claude, None).unwrap();
    scene.wait_for("the window's add to be refused", |s| {
        s.relay
            .terminals()
            .view()
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("allows only codex"))
    });
    assert_eq!(scene.ids(), before, "no entry was made");
    // The add every app helper uses (Operations, Agentic Works, packages,
    // automations): refused, no entry.
    let refused = control
        .add_with(ToolKind::Claude, None, Some("fix it".into()), None, None)
        .unwrap_err();
    assert!(refused.contains("allows only codex"), "{refused}");
    assert_eq!(scene.ids(), before, "no entry was made");
    // The window's start (restart) of a stopped one: refused the same way.
    control.restart("claude-2").unwrap();
    scene.wait_for("claude-2 to be refused", |s| failed(s, "claude-2"));
    assert!(scene.launch_args("claude-2").is_empty());
    scene.pane("claude-3").unwrap().request_start();
    scene.wait_for("claude-3 to be refused", |s| failed(s, "claude-3"));
    // A shell, and the CLI the project uses, still go.
    control.add(ToolKind::Shell, None).unwrap();
    control.add(ToolKind::Codex, None).unwrap();
    let want = before.len() + 2;
    scene.wait_for("the shell and codex to be added", |s| s.ids().len() == want);
}

/// `connecto agents --roles` offers only the CLIs the project's Tools allow,
/// and, until the app has said which are ready, starts none of them for a
/// role (ADR 0045).
#[test]
fn agents_lists_what_the_tools_setting_allows_and_picks_only_ready_ones() {
    use connecto_core::{ProjectTools, ToolKind};
    let allowed = ProjectTools::from_tools([ToolKind::Claude, ToolKind::Pi]).unwrap();
    let scene = Scene::build_tools(
        "agents-roster",
        vec![Tool::Claude; 2],
        "2.1.318",
        false,
        SinkMode::Plain,
        &[],
        true,
        None,
        Some(allowed),
    );
    let run = scene.connecto("claude", "agents --roles");
    assert_eq!(run.code, 0, "{run:?}");
    let row = |tool: &str| {
        run.out
            .lines()
            .find(|line| line.split_whitespace().next() == Some(tool))
            .unwrap_or_default()
            .to_owned()
    };
    assert!(row("codex").contains("not allowed"), "{}", run.out);
    assert!(row("antigravity").contains("not allowed"), "{}", run.out);
    assert!(row("claude").contains("checking"), "{}", run.out);
    assert!(row("pi").contains("checking"), "{}", run.out);
    assert!(row("reviewer").contains("none"), "{}", run.out);
    assert!(!run.out.contains("connecto add"), "{}", run.out);
    let bad = scene.connecto("claude", "agents --json");
    assert_ne!(bad.code, 0, "{bad:?}");
}

/// A terminal refused by the project's Tools setting is not brought back by
/// itself: its saved "running" and "interrupted" are cleared (its
/// conversation stays), so the next launch leaves it stopped instead of
/// refusing it again.
#[test]
fn a_refused_terminal_is_not_brought_back_at_the_next_launch() {
    use connecto_core::{PaneControl, PaneStage, ProjectTools};
    let saved = {
        let scene = Scene::seeded_with_tools("tools-norestore", SEEDED, ProjectTools::Codex);
        scene.wait_for("the restored claude to be refused", |s| {
            s.pane("claude").is_some_and(|pane| pane.tools_refused())
        });
        scene.wait_for("its intent to start to be cleared", |s| {
            s.entry("claude").is_some_and(|e| !e.desired_running)
                && !s.saved_list().contains(r#""interrupted": true"#)
                && !s.saved_list().contains(r#""working": true"#)
        });
        let saved = scene.saved_list();
        assert!(
            saved.contains("aaaa-1111"),
            "its conversation stays: {saved}"
        );
        saved
    };
    // The next launch: nothing is started, nothing refused again.
    let scene = Scene::seeded_with_tools("tools-norestore2", &saved, ProjectTools::Codex);
    scene.wait_for("the list", |s| s.pane("claude").is_some());
    std::thread::sleep(Duration::from_millis(600));
    let pane = scene.pane("claude").unwrap();
    assert_eq!(pane.stage(), PaneStage::Stopped, "left stopped");
    assert!(!pane.tools_refused());
    assert!(scene.launch_args("claude").is_empty());
}

/// A start that waits for the project's package install checks the Tools
/// setting again once it may go on: a CLI left out meanwhile does not start.
/// What that start took (the saved session to resume) is put back, so a
/// start once the CLI is allowed again resumes it as this one would have.
#[test]
fn a_cli_left_out_while_its_start_waits_does_not_start() {
    use connecto_core::{PaneControl, PaneStage, ProjectTools};
    use connecto_session::install_gate;
    // claude-3 waits, stopped, with a saved session to resume.
    let scene = Scene::seeded("tools-permit", SEEDED, &[]);
    scene.wait_for("the list", |s| s.pane("claude-3").is_some());
    let pane = scene.pane("claude-3").unwrap();
    let before = pane.attempt();
    let data_root = scene.root.join("data");
    let id = connecto_session::launch::project_id(&fs::canonicalize(&scene.project).unwrap());
    let set_tools = |tools| {
        let mut settings = connecto_session::project_settings::load(&data_root, &id);
        settings.tools = tools;
        connecto_session::project_settings::save(&data_root, &id, &settings).unwrap();
    };
    // The install holds the project: a start waits for it.
    install_gate::enable(&data_root);
    let lease = install_gate::lease(&data_root, &id, Duration::from_secs(5)).unwrap();
    let (reached, at) = std::sync::mpsc::channel();
    let reached = std::sync::Mutex::new(reached);
    pane.set_probe(move |point| {
        if point == "before_spawn" {
            let _ = reached.lock().unwrap().send(());
        }
    });
    let control = scene.relay.terminals();
    control.restart("claude-3").unwrap();
    at.recv_timeout(Duration::from_secs(20))
        .expect("the start reached its spawn");
    // Left out while the start waits for the install.
    set_tools(ProjectTools::Codex);
    drop(lease);
    install_gate::finish(&data_root, &id);
    scene.wait_for("claude-3 to be refused", |s| {
        matches!(
            s.pane("claude-3").unwrap().stage(),
            PaneStage::Failed { ref reason } if reason.contains("allows only codex")
        )
    });
    assert_eq!(pane.attempt(), before, "no process started");
    assert!(scene.launch_args("claude-3").is_empty());
    install_gate::disable(&data_root);
    // Allowed again: the next start resumes the saved session.
    set_tools(ProjectTools::All);
    control.restart("claude-3").unwrap();
    scene.wait_for("claude-3 to be ready", |s| {
        s.screen("claude-3").contains("FAKE-READY")
    });
    let args = scene.launch_args("claude-3");
    let at = args.iter().position(|a| a == "--resume");
    assert_eq!(
        at.map(|at| args[at + 1].as_str()),
        Some("bbbb-2222"),
        "{args:?}"
    );
}

/// A login runs the CLI too: refused before it runs, and again once the
/// CLI's copy is verified, so one left out meanwhile does not log in.
#[test]
fn a_cli_left_out_while_its_login_is_prepared_does_not_log_in() {
    use connecto_core::{PaneControl, PaneStage, ProjectTools};
    let scene = Scene::new("tools-login");
    // No Claude session runs, so nothing else would stop the login.
    for id in ["claude", "claude-2"] {
        scene.pane(id).unwrap().request_stop();
        scene.wait_for(&format!("{id} to stop"), |s| !s.pane(id).unwrap().is_live());
    }
    let pane = scene.pane("claude-2").unwrap();
    let data_root = scene.root.join("data");
    let id = connecto_session::launch::project_id(&fs::canonicalize(&scene.project).unwrap());
    // Left out while the login's copy is verified.
    pane.set_probe(move |point| {
        if point == "login_verified" {
            let mut settings = connecto_session::project_settings::load(&data_root, &id);
            settings.tools = ProjectTools::Codex;
            connecto_session::project_settings::save(&data_root, &id, &settings).unwrap();
        }
    });
    pane.request_login();
    let until = Instant::now() + Duration::from_secs(2);
    let mut logged_in = false;
    while Instant::now() < until {
        logged_in |= matches!(pane.stage(), PaneStage::LoggingIn);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!logged_in, "the login ran: {:?}", pane.stage());
    assert!(pane.tools_refused(), "{:?}", pane.stage());
}

/// R8: the last terminal goes like any other. The project ends with none.
#[test]
fn closing_the_last_terminal_removes_it_too() {
    let scene = Scene::with_tools("lastclose", vec![Tool::Claude], "2.1.318");
    let run = scene.connecto("claude", "close claude");
    assert_eq!(run.code, 0, "{run:?}");
    assert!(!run.out.contains("last terminal"), "{run:?}");
    scene.wait_for("claude to go", |s| s.ids().is_empty());
}

/// AC3 from the agent side: a 17th terminal is refused with `cap_reached`.
#[test]
fn the_seventeenth_terminal_is_refused_with_cap_reached() {
    let scene = Scene::new("cap");
    for _ in 0..14 {
        assert_eq!(scene.connecto("claude", "add claude").code, 0);
    }
    let over = scene.connecto("claude", "add codex");
    assert_eq!(over.code, 1, "{over:?}");
    assert!(over.err.contains("cap_reached"), "{over:?}");
}

/// T11: close on your own terminal replies first and stops it afterwards.
#[test]
fn closing_your_own_terminal_replies_before_the_stop() {
    let scene = Scene::new("selfclose");
    let run = scene.connecto("claude", "close claude");
    assert_eq!(run.code, 0, "{run:?}");
    assert!(run.out.contains("will be removed"), "{run:?}");
    scene.wait_for("claude to go", |s| s.ids() == ["claude-2"]);
}

/// AC5, AC11: a watcher is woken within 5 s of `began` and of `finished`, and an
/// unwatch ends it.
#[test]
fn a_watcher_is_woken_on_began_and_finished_and_unwatch_stops_it() {
    let scene = Scene::new("watch");
    let run = scene.connecto("claude", "watch claude-2");
    assert_eq!(run.code, 0, "{run:?}");
    assert!(run.out.contains("watching claude-2"), "{run:?}");
    // Wait for claude's own turn to end so it is not busy.
    scene.wait_for("claude to be idle", |s| {
        s.screen("claude").contains("PROMPT-SEEN") && !s.relay.panes().is_empty()
    });
    thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    scene.user_types("claude-2", "SLEEP 1 hello there");
    scene.wait_for("the began wake", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p == "[connecto notice] claude-2 began working. You are watching it.")
    });
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // `began` woke claude; its turn is the wake. `finished` comes a second later.
    // If claude is still in the turn the first wake started, `finished` is an
    // unread notice and one wake covers it, with the notice in its context.
    scene.wait_for("the finished wake", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.contains("claude-2 finished. You are watching it."))
            || (1..=9).any(|n| {
                s.log(&format!("context-claude-{n}.txt"))
                    .contains("claude-2 finished. You are watching it.")
            })
    });
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    let count = scene.prompts("claude").len();
    assert_eq!(scene.connecto("claude", "unwatch claude-2").code, 0);
    thread::sleep(Duration::from_millis(800));
    scene.user_types("claude-2", "one more");
    thread::sleep(Duration::from_millis(2500));
    let after: Vec<String> = scene
        .prompts("claude")
        .into_iter()
        .skip(count + 1)
        .collect();
    assert!(
        after.iter().all(|p| !p.starts_with("[connecto notice]")),
        "an unwatched terminal wakes nobody: {after:?}"
    );
}

/// AC9: events reach a non-watcher as context at its next prompt, never as a
/// wake, and the roster comes with the first drain.
#[test]
fn a_non_watcher_gets_notices_as_context_and_is_never_woken() {
    let scene = Scene::new("context");
    scene.user_types("claude-2", "do something");
    scene.wait_for("claude-2 to finish", |s| s.prompts("claude-2").len() == 1);
    thread::sleep(Duration::from_millis(1500));
    assert!(
        scene.prompts("claude").is_empty(),
        "no wake for a non-watcher"
    );
    scene.user_types("claude", "what happened");
    scene.wait_for("claude's prompt", |s| s.prompts("claude").len() == 1);
    thread::sleep(Duration::from_millis(300));
    let context = scene.log("context-claude-1.txt");
    let value: Value = serde_json::from_str(&context).unwrap();
    let text = value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        text.starts_with("[connecto] You are terminal claude. Terminals: claude-2 (claude, "),
        "{text}"
    );
    // An unwatched terminal's older events fold into its newest one.
    assert!(
        text.contains("- claude-2 finished. (+1 earlier change)"),
        "{text}"
    );
    assert!(!text.contains("claude-2 began working."), "{text}");
    assert!(!text.contains("You are watching it"), "{text}");
}

/// T19: a Stop with background work gives no `finished`; the request stays
/// pending, and ends with the later reply.
#[test]
fn background_work_holds_the_result_until_it_is_done() {
    let scene = Scene::new("background");
    let sent = scene.connecto("claude", "send claude-2 'BACKGROUND please'");
    assert_eq!(sent.code, 0, "{sent:?}");
    scene.wait_for("claude-2's first stop", |s| {
        s.prompts("claude-2").len() == 1
    });
    thread::sleep(Duration::from_millis(400));
    let pending = scene.connecto("claude", "result r1");
    assert_eq!(pending.code, 4, "{pending:?}");
    assert!(pending.out.starts_with("pending"), "{pending:?}");
    assert!(pending.out.contains("background"), "{pending:?}");
    // What runs is named, with its type and age.
    assert!(
        pending.out.contains("Still running: sleep 10 (shell, "),
        "{pending:?}"
    );
    assert!(
        pending.out.contains("connecto tasks claude-2"),
        "{pending:?}"
    );
    let list = scene.connecto("claude", "list");
    let row = list
        .out
        .lines()
        .find(|line| line.starts_with("claude-2"))
        .unwrap_or_default();
    assert!(row.contains("background"), "{list:?}");
    assert!(row.contains("1 task, oldest "), "{list:?}");
    let tasks = scene.connecto("claude", "tasks claude-2");
    assert_eq!(tasks.code, 0, "{tasks:?}");
    assert!(tasks.out.starts_with("claude-2 background\n"), "{tasks:?}");
    let task = tasks
        .out
        .lines()
        .find(|line| line.starts_with("b1"))
        .unwrap_or_default();
    assert!(
        task.contains("shell") && task.ends_with("sleep 10"),
        "{tasks:?}"
    );
    // The fake tool runs no real background shell: nothing is found to stop.
    let stop = scene.connecto("claude", "tasks claude-2 --stop");
    assert_eq!(stop.code, 1, "{stop:?}");
    assert!(stop.err.contains("not_stopped"), "{stop:?}");
    let other = scene.connecto("claude", "tasks claude-2 --stop a9");
    assert!(other.err.contains("nothing_to_stop"), "{other:?}");
    let done = scene.result_of("claude", "r1");
    assert_eq!(done.code, 0, "{done:?}");
    assert_eq!(
        done.out.lines().last(),
        Some("background finished"),
        "{done:?}"
    );
}

/// The desktop's stop of one of two background shells that run the same
/// command: it cannot tell them apart, so neither is signalled, and the
/// reason, found on the stop's thread after the click was answered, reaches
/// the tab strip. Stopping all then takes both.
#[test]
fn a_desktop_stop_of_one_of_two_identical_shells_is_refused_on_the_tab_strip() {
    let scene = Scene::new("twins");
    let sent = scene.connecto("claude", "send claude-2 'TWINS please'");
    assert_eq!(sent.code, 0, "{sent:?}");
    let terminals = scene.relay.terminals();
    let tasks = |s: &Scene| {
        s.relay
            .terminals()
            .view()
            .terminals
            .iter()
            .find(|entry| entry.id == "claude-2")
            .map_or(0, |entry| entry.background.len())
    };
    scene.wait_for("claude-2's two background shells", |s| tasks(s) == 2);
    let twins = |s: &Scene| {
        let out = std::process::Command::new("/bin/ps")
            .args(["-axww", "-o", "command="])
            .output()
            .unwrap();
        let root = s.root.display().to_string();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.contains("eval 'sleep 30 ; true'") && line.contains(&root))
            .count()
    };
    scene.wait_for("both shells to run", |s| twins(s) == 2);
    // What a peer's request calls: it waits, and answers with the refusal.
    let now = terminals.stop_background_now("claude-2", Some("b1"));
    assert!(
        now.as_ref()
            .is_err_and(|why| why.contains("cannot be told apart")),
        "{now:?}"
    );
    assert_eq!(twins(&scene), 2, "neither shell was signalled");
    let started = terminals.stop_background("claude-2", Some("b1"));
    assert!(started.is_ok(), "{started:?}");
    scene.wait_for("the refusal on the tab strip", |s| {
        s.relay
            .terminals()
            .view()
            .last_error
            .as_deref()
            .is_some_and(|why| why.starts_with("claude-2:") && why.contains("cannot be told apart"))
    });
    thread::sleep(Duration::from_millis(300));
    assert_eq!(twins(&scene), 2, "neither shell was signalled");
    let all = terminals.stop_background("claude-2", None);
    assert!(all.is_ok(), "{all:?}");
    scene.wait_for("both shells to end", |s| twins(s) == 0);
}

#[test]
fn a_result_of_an_unknown_request_is_refused() {
    let scene = Scene::new("unknown");
    let run = scene.connecto("claude", "result r999");
    assert_eq!(run.code, 1);
    assert!(run.err.contains("unknown_request"), "{run:?}");
}

// ---- guards on paid model turns, run with the network denied by the OS
//
// A wake is a model turn on the user's plan, so Pause, unwatch and "nothing is
// queued for Resume" are guards on paid calls. They are proved here with the
// network denied at the OS level (Seatbelt), so that a broken guard can show its
// failure without any chance of reaching a real model. The child process is this
// same test binary under `sandbox-exec`; the stand-in tools never touch the
// network either way. (Mutation check: break a guard, run `cargo test -p
// connecto-session --test bridge guards_hold`, and this test fails.)

const OFFLINE_ENV: &str = "CONNECTO_OFFLINE_CHILD";
const DENY_NETWORK: &str = "(version 1)(allow default)(deny network-outbound (remote ip))(deny network-bind (local ip))(deny network-inbound (local ip))";
const GUARDS: [&str; 11] = [
    "guard_an_attempt_that_goes_stale_during_the_audit_wait_types_nothing",
    "guard_a_refused_turn_end_frame_ends_the_request_unavailable",
    "guard_pause_during_the_audit_wait_stops_the_paste",
    "guard_a_failing_audit_log_types_nothing",
    "guard_a_stale_attempt_types_nothing",
    "guard_an_agent_never_types_into_a_login",
    "guard_a_pause_during_a_start_cancels_it",
    "guard_pause_between_paste_and_enter_keeps_a_request_that_a_later_enter_binds",
    "guard_the_os_denies_the_network",
    "guard_watchers_stop_waking_each_other_at_pause_and_nothing_waits_for_resume",
    "guard_no_wake_when_an_agent_stops_during_pause",
];

fn offline() -> bool {
    std::env::var_os(OFFLINE_ENV).is_some()
}

#[test]
fn guards_hold_with_the_network_denied() {
    if offline() {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .args(["-p", DENY_NETWORK])
        .arg(exe)
        .args(["--exact", "--test-threads=1"]);
    command.args(GUARDS).env(OFFLINE_ENV, "1");
    let out = command
        .output()
        .expect("sandbox-exec must exist to prove the guards offline");
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a guard failed under the network deny:\n{text}"
    );
    assert!(
        text.contains(&format!("test result: ok. {} passed", GUARDS.len())),
        "{text}"
    );
}

/// The guards that live in bridge state and the registry are unit tests of the
/// library. They run the same way: this test finds the library's test binary
/// and runs them under the same network deny.
const STATE_GUARDS: [&str; 9] = [
    "a_reply_notice_folded_by_overflow_is_wanted_again_when_its_wake_fails",
    "a_reply_notice_waits_in_the_inbox_for_its_wake_and_is_wanted_again_if_it_fails",
    "unwatch_clears_the_wanted_flag_of_a_reserved_notice_but_keeps_ask_notices",
    "unwatch_cancels_the_recovery_of_a_wake_in_flight",
    "a_full_state_queue_cannot_lose_a_drain_completion",
    "a_report_from_an_earlier_attempt_does_not_touch_the_new_one",
    "a_wake_that_was_never_typed_gives_back_every_wanted_flag_merged_ones_too",
    "a_released_reservation_restores_the_wanted_flag_and_wakes_nobody",
    "a_queued_close_is_decided_again_when_it_runs",
];

fn library_test_binary() -> PathBuf {
    // Next to this binary in deps/; in cargo's newer layout each binary has
    // its own build/<package>/<hash>/out/ folder, so every sibling's is read.
    let here = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let mut folders = vec![here.clone()];
    if here.file_name().is_some_and(|name| name == "out")
        && let Some(package) = here.parent().and_then(Path::parent)
    {
        folders.extend(
            fs::read_dir(package)
                .unwrap()
                .flatten()
                .map(|entry| entry.path().join("out")),
        );
    }
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = folders
        .iter()
        .filter_map(|folder| fs::read_dir(folder).ok())
        .flatten()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("connecto_session-") && !name.contains('.')
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    found.sort();
    found
        .into_iter()
        .rev()
        .map(|(_, path)| path)
        .find(|path| {
            Command::new(path).arg("--list").output().is_ok_and(|out| {
                String::from_utf8_lossy(&out.stdout).contains("bridge::state::tests::")
            })
        })
        .expect("the library's test binary")
}

#[test]
fn state_guards_hold_with_the_network_denied() {
    if offline() {
        return;
    }
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .args(["-p", DENY_NETWORK])
        .arg(library_test_binary());
    command
        .args(["--exact", "--test-threads=1"])
        .args(STATE_GUARDS.iter().map(|name| {
            if *name == "a_queued_close_is_decided_again_when_it_runs" {
                format!("terminals::tests::{name}")
            } else {
                format!("bridge::state::tests::{name}")
            }
        }));
    let out = command.output().expect("sandbox-exec");
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a state guard failed under the network deny:\n{text}"
    );
    assert!(
        text.contains(&format!("test result: ok. {} passed", STATE_GUARDS.len())),
        "{text}"
    );
}

#[test]
fn guard_the_os_denies_the_network() {
    if !offline() {
        return;
    }
    // Without this, every guard below would prove nothing about the network.
    assert!(
        std::net::TcpStream::connect_timeout(
            &"1.1.1.1:53".parse().unwrap(),
            Duration::from_secs(1)
        )
        .is_err()
    );
    assert!(
        std::net::TcpStream::connect_timeout(
            &"127.0.0.1:9".parse().unwrap(),
            Duration::from_secs(1)
        )
        .is_err()
    );
    assert!(std::net::TcpListener::bind("127.0.0.1:0").is_err());
}

fn notice_prompts(scene: &Scene, id: &str) -> usize {
    scene
        .prompts(id)
        .iter()
        .filter(|p| p.starts_with("[connecto notice]"))
        .count()
}

/// T18 and R14: two terminals watching each other keep waking each other for
/// several rounds (R7 puts no limit on it), and Pause stops it. Nothing is
/// queued for Resume.
#[test]
fn guard_watchers_stop_waking_each_other_at_pause_and_nothing_waits_for_resume() {
    if !offline() {
        return;
    }
    let scene = Scene::new("loop");
    assert_eq!(scene.connecto("claude", "watch claude-2").code, 0);
    assert_eq!(scene.connecto("claude-2", "watch claude").code, 0);
    scene.wait_for("several rounds of wakes", |s| {
        notice_prompts(s, "claude") + notice_prompts(s, "claude-2") >= 8
    });
    scene.relay.pause_switch().pause();
    thread::sleep(Duration::from_millis(2500));
    let settled = (
        notice_prompts(&scene, "claude"),
        notice_prompts(&scene, "claude-2"),
    );
    thread::sleep(Duration::from_millis(3000));
    assert_eq!(
        settled,
        (
            notice_prompts(&scene, "claude"),
            notice_prompts(&scene, "claude-2")
        ),
        "the loop went on under Pause"
    );
    scene.relay.pause_switch().resume();
    thread::sleep(Duration::from_millis(3000));
    assert_eq!(
        settled,
        (
            notice_prompts(&scene, "claude"),
            notice_prompts(&scene, "claude-2")
        ),
        "something was queued for Resume"
    );
}

/// T17: an agent that stops being busy during Pause gets no wake, and nothing is
/// typed on Resume. The same scene wakes it when Pause is not on.
#[test]
fn guard_no_wake_when_an_agent_stops_during_pause() {
    if !offline() {
        return;
    }
    let scene = Scene::new("pausewake");
    assert_eq!(scene.connecto("claude", "watch claude-2").code, 0);
    thread::sleep(Duration::from_millis(800));
    let base = scene.prompts("claude").len();
    scene.user_types("claude", "SLEEP 4 busy");
    scene.wait_for("claude to be busy", |s| {
        s.prompts("claude").len() == base + 1
    });
    scene.user_types("claude-2", "a quick turn");
    thread::sleep(Duration::from_millis(1200));
    scene.relay.pause_switch().pause();
    thread::sleep(Duration::from_millis(5500));
    assert_eq!(
        scene.prompts("claude").len(),
        base + 1,
        "a wake was typed under Pause"
    );
    scene.relay.pause_switch().resume();
    thread::sleep(Duration::from_millis(3000));
    assert_eq!(
        scene.prompts("claude").len(),
        base + 1,
        "a wake was typed on Resume"
    );
    // Control: with Pause off, the same watcher is woken.
    scene.user_types("claude-2", "another turn");
    scene.wait_for("the control wake", |s| notice_prompts(s, "claude") >= 1);
}

/// R6 and AC6's shape without a real model: Claude asks Codex and reads the
/// result, then the reverse. Codex's hooks are trusted through `hooks/list`,
/// its turn end comes from `notify`, and its turn id pairs the reply.
#[test]
fn claude_and_codex_ask_each_other_and_read_just_the_result() {
    let scene = Scene::mixed("mixed");
    let list = scene.connecto("claude", "list");
    assert!(
        list.out.contains("codex") && !list.out.contains("bridge off"),
        "{list:?}"
    );

    let sent = scene.connecto("claude", "send codex --ask 'say hi to codex'");
    assert_eq!(sent.code, 0, "{sent:?}");
    scene.wait_for("codex to take it", |s| !s.prompts("codex").is_empty());
    assert!(scene.prompts("codex")[0].starts_with("[connecto from claude, request r1."));
    scene.wait_for("the sender's notice", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.starts_with("[connecto notice]"))
    });
    let result = scene.result_of("claude", "r1");
    assert_eq!(result.code, 0, "{result:?}");
    assert_eq!(result.out, "echo: say hi to codex");

    // And the reverse.
    let back = scene.connecto("codex", "send claude 'and back'");
    assert_eq!(back.code, 0, "{back:?}");
    let id = back.out.split_whitespace().next().unwrap().to_owned();
    scene.wait_for("claude to take it", |s| {
        s.prompts("claude").iter().any(|p| p.contains("and back"))
    });
    let result = scene.result_of("codex", &id);
    assert_eq!(result.code, 0, "{result:?}");
    assert_eq!(result.out, "echo: and back");
}

/// T49: a watch needs an attempt running.
#[test]
fn watching_a_terminal_with_no_attempt_running_is_not_running() {
    let scene = Scene::new("watchstopped");
    scene.pane("claude-2").unwrap().request_stop();
    scene.wait_for("claude-2 to stop", |s| {
        !s.pane("claude-2").unwrap().is_live()
    });
    let watch = scene.connecto("claude", "watch claude-2");
    assert_eq!(watch.code, 1, "{watch:?}");
    assert!(watch.err.contains("not_running"), "{watch:?}");
    let myself = scene.connecto("claude", "watch claude");
    assert!(
        myself.err.contains("bad_request"),
        "watching yourself only wakes yourself: {myself:?}"
    );
}

/// T14: `--ask` wakes on the end of a request only, never on `began`.
#[test]
fn ask_wakes_the_sender_on_the_end_of_the_request_and_never_on_began() {
    let scene = Scene::new("askwake");
    let sent = scene.connecto("claude", "send claude-2 --ask 'SLEEP 3 slowly'");
    assert_eq!(sent.code, 0, "{sent:?}");
    scene.wait_for("claude-2 to begin", |s| !s.prompts("claude-2").is_empty());
    thread::sleep(Duration::from_millis(1200));
    assert!(
        scene
            .prompts("claude")
            .iter()
            .all(|p| !p.contains("began working")),
        "an --ask sender is not a watcher: {:?}",
        scene.prompts("claude")
    );
    scene.wait_for("the end of the request", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.starts_with("[connecto notice]"))
    });
}

/// AC11: `needs-user` and `exited` reach a watcher.
#[test]
fn a_watcher_hears_needs_user_and_exited() {
    let scene = Scene::new("events");
    assert_eq!(scene.connecto("claude", "watch claude-2").code, 0);
    thread::sleep(Duration::from_millis(800));
    scene.user_types("claude-2", "NEEDS the user");
    scene.wait_for("the needs-user wake", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.contains("claude-2 needs the user. You are watching it."))
            || s.log("context-claude-2.txt")
                .contains("claude-2 needs the user")
    });
    thread::sleep(Duration::from_millis(1500));
    scene.pane("claude-2").unwrap().request_stop();
    scene.wait_for("the exited wake", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.contains("claude-2 exited."))
            || (1..=9).any(|n| {
                s.log(&format!("context-claude-{n}.txt"))
                    .contains("claude-2 exited.")
            })
    });
}

/// CLI versions are not pinned: a terminal on any version has the bridge on.
#[test]
fn a_terminal_on_any_cli_version_has_the_bridge_on() {
    let scene = Scene::with_tools("anyversion", vec![Tool::Claude], "2.1.319");
    let run = scene.connecto("claude", "list");
    assert_eq!(run.code, 0, "{run:?}");
    assert!(!run.err.contains("bridge_off"), "{run:?}");
    assert!(scene.pane("claude").unwrap().is_live());
    assert!(scene.pane("claude").unwrap().bridge_off_reason().is_none());
}

// ---- the rest of the acceptance tests

fn notices(scene: &Scene, id: &str) -> Vec<String> {
    scene
        .prompts(id)
        .into_iter()
        .filter(|p| p.starts_with("[connecto notice]"))
        .collect()
}

fn context_of(scene: &Scene, id: &str, n: usize) -> String {
    let raw = scene.log(&format!("context-{id}-{n}.txt"));
    serde_json::from_str::<Value>(&raw)
        .map(|v| {
            v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        })
        .unwrap_or_default()
}

/// T10: two senders typing into one target at once never share a paste or an
/// Enter: each prompt arrives whole.
#[test]
fn two_senders_to_one_target_arrive_whole_and_in_order() {
    let scene = Scene::with_tools("twosenders", vec![Tool::Claude; 3], "2.1.318");
    let big = |c: char| c.to_string().repeat(3000);
    let a = scene.connecto("claude", &format!("send claude-3 {}", big('a')));
    assert_eq!(a.code, 0, "{a:?}");
    // Both at once, from two terminals.
    scene.user_types(
        "claude",
        &format!("RUN connecto send claude-3 {}", big('b')),
    );
    scene.user_types(
        "claude-2",
        &format!("RUN connecto send claude-3 {}", big('c')),
    );
    scene.wait_for("three prompts", |s| s.prompts("claude-3").len() == 3);
    for prompt in scene.prompts("claude-3") {
        let body = prompt.split_once('\n').unwrap().1;
        assert!(prompt.starts_with("[connecto from "), "{prompt:.80}");
        assert!(
            body.chars().all(|c| c == body.chars().next().unwrap()),
            "interleaved: {prompt:.80}"
        );
        assert_eq!(body.len(), 3000);
    }
}

/// T13, T16, T44: a busy watcher is not woken per event. It gets notices as
/// context, and one wake when it stops covers all of them.
#[test]
fn a_busy_watcher_gets_one_wake_for_several_unread_notices() {
    let scene = Scene::new("unread");
    assert_eq!(scene.connecto("claude", "watch claude-2").code, 0);
    thread::sleep(Duration::from_millis(800));
    let base = scene.prompts("claude").len();
    scene.user_types("claude", "SLEEP 6 busy");
    scene.wait_for("claude to be busy", |s| {
        s.prompts("claude").len() == base + 1
    });
    for word in ["one", "two"] {
        scene.user_types("claude-2", &format!("turn {word}"));
        scene.wait_for(word, |s| {
            s.prompts("claude-2").len() >= if word == "one" { 1 } else { 2 }
        });
        thread::sleep(Duration::from_millis(700));
    }
    assert!(
        notices(&scene, "claude").is_empty(),
        "no wake while busy: {:?}",
        notices(&scene, "claude")
    );
    scene.wait_for("the wake", |s| !notices(s, "claude").is_empty());
    thread::sleep(Duration::from_millis(3000));
    let wakes = notices(&scene, "claude");
    assert_eq!(
        wakes,
        ["[connecto notice] Notices arrived while you were working. They are in your context."]
    );
    let context = context_of(&scene, "claude", base + 2);
    assert_eq!(
        context
            .matches("claude-2 finished. You are watching it.")
            .count(),
        2,
        "{context}"
    );
}

/// T30: typed text that starts no turn ends `unmatched` after 15 s, and the
/// `--ask` sender is told.
#[test]
fn text_that_starts_no_turn_ends_unmatched_and_the_ask_sender_is_told() {
    let scene = Scene::new("swallow");
    let sent = scene.connecto("claude", "send claude-2 --ask \"SWAL\"\"LOW this\"");
    assert_eq!(sent.code, 0, "{sent:?}");
    let began = Instant::now();
    let result = scene.result_of("claude", "r1");
    assert_eq!(result.code, 4, "{result:?}");
    let last = result.out.lines().last().unwrap();
    assert!(last.starts_with("unmatched"), "{result:?}");
    assert!(
        began.elapsed() >= Duration::from_secs(12),
        "{:?}",
        began.elapsed()
    );
    scene.wait_for("the sender's notice", |s| {
        notices(s, "claude")
            .iter()
            .any(|n| n.contains("Request r1 ended (unmatched)"))
            || (1..=9).any(|n| context_of(s, "claude", n).contains("Request r1 ended (unmatched)"))
    });
}

/// A project that turns hooks off or the bypass mode off cannot be overridden
/// from the command line, so the launch finds it and says so: hooks off means
/// no bridge for that terminal, bypass off means a visible warning.
#[test]
fn project_settings_that_defeat_the_launch_are_found_and_reported() {
    let scene = Scene::booting(
        "conflicts",
        &[(
            ".claude/settings.json",
            r#"{"disableAllHooks": true, "permissions": {"disableBypassPermissionsMode": "disable"}}"#,
        )],
    );
    scene.wait_for("a terminal to be up", |s| {
        s.pane("claude")
            .is_some_and(|pane| pane.bridge_off_reason().is_some())
            && s.pane("claude-2")
                .is_some_and(|pane| pane.power_warning().is_some())
    });
    let reason = scene.pane("claude").unwrap().bridge_off_reason().unwrap();
    assert!(reason.contains("disableAllHooks"), "{reason}");
    let pane = scene.pane("claude-2").unwrap();
    assert!(
        pane.power_warning()
            .unwrap()
            .contains("disableBypassPermissionsMode")
    );
    assert!(
        pane.warnings()
            .iter()
            .any(|w| w.contains("disableBypassPermissionsMode")),
        "{:?}",
        pane.warnings()
    );
}

/// A project `env` cannot take the bridge socket: the launch settings carry the
/// real one and outrank it.
#[test]
fn a_project_env_cannot_replace_the_bridge_socket() {
    let scene = Scene::booting(
        "envpin",
        &[(
            ".claude/settings.json",
            r#"{"env": {"CONNECTO_BRIDGE_SOCK": "/nonexistent/x.sock"}}"#,
        )],
    );
    scene.wait_for("claude to be ready", |s| {
        s.screen("claude").contains("FAKE-READY")
    });
    assert!(scene.pane("claude").unwrap().bridge_off_reason().is_none());
    let args = scene.log("args-claude.txt");
    let file = args
        .lines()
        .skip_while(|l| *l != "--settings")
        .nth(1)
        .unwrap()
        .to_owned();
    let settings = fs::read_to_string(file).unwrap();
    assert!(settings.contains("CONNECTO_BRIDGE_SOCK"), "{settings}");
}

/// An `--ask` to a terminal that is still starting is not `unmatched` just
/// because starting is slow: the clock runs from when the tool is ready, and the
/// reply it gives after a boot longer than the 15 s window is delivered.
#[test]
fn an_ask_to_a_slow_booting_terminal_still_gets_its_reply() {
    // The send now waits for the booting tool's input box, so the time is
    // counted from the start of the scene, when the 18 s boot began.
    let began = Instant::now();
    let scene = Scene::booting("slowboot", &[("boot-delay-claude-2", "18")]);
    scene.wait_for("claude to be ready", |s| {
        s.screen("claude").contains("FAKE-READY")
    });
    let sent = scene.connecto("claude", "send claude-2 --ask 'slow hello'");
    assert_eq!(sent.code, 0, "{sent:?}");
    // The send returned once the boot was over and the text was typed; the
    // reply follows at once, and its notice settles before the result is read.
    scene.wait_for("the sender's notice", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.starts_with("[connecto notice]"))
    });
    thread::sleep(Duration::from_secs(1));
    let result = scene.result_of("claude", "r1");
    assert_eq!(result.code, 0, "{result:?}");
    assert!(result.out.contains("echo: slow hello"), "{result:?}");
    assert!(
        began.elapsed() >= Duration::from_secs(15),
        "the target booted for longer than the unmatched window: {:?}",
        began.elapsed()
    );
}

/// A terminal that dies says why: how it ended and the last line it printed, in
/// `list` and in the `exited` notice.
#[test]
fn an_exited_terminal_says_how_and_what_it_last_printed() {
    let scene = Scene::booting(
        "bootexit",
        &[
            ("boot-delay-claude-2", "3"),
            (
                "boot-exit-claude-2",
                "account/read failed: workspace routing discovery timed out",
            ),
        ],
    );
    scene.wait_for("claude to be ready", |s| {
        s.screen("claude").contains("FAKE-READY")
    });
    let watched = scene.connecto("claude", "watch claude-2");
    assert_eq!(watched.code, 0, "{watched:?}");
    // A send to a terminal that is still booting waits for its input box. This
    // one ends first, so nothing is pasted and the sender is told at once.
    let sent = scene.connecto("claude", "send claude-2 --ask 'anyone there'");
    assert_ne!(sent.code, 0, "{sent:?}");
    assert!(
        sent.err.contains("no process") || sent.out.contains("no process"),
        "{sent:?}"
    );
    assert!(scene.prompts("claude-2").is_empty());
    scene.wait_for("the exit notice", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.contains("[connecto notice]"))
    });
    thread::sleep(Duration::from_secs(2));
    let list = scene.connecto("claude", "list");
    assert!(
        list.out.contains("exited with code 3")
            && list.out.contains("workspace routing discovery timed out"),
        "{list:?}"
    );
    scene.wait_for("the exited notice with its reason", |s| {
        let mut all = s.prompts("claude");
        all.extend((1..=9).map(|n| context_of(s, "claude", n)));
        all.iter().any(|p| {
            p.contains("claude-2 exited.") && p.contains("workspace routing discovery timed out")
        })
    });
}

/// T32 (target side): a verified Codex whose hook hash cannot be read is bridge
/// off, and the table of what that refuses holds.
#[test]
fn a_bridge_off_target_allows_only_what_needs_no_tool_behaviour() {
    let scene = Scene::untrusted("targetoff");
    let list = scene.connecto("claude", "list");
    assert!(
        list.out
            .lines()
            .any(|l| l.starts_with("codex") && l.contains("bridge off")),
        "{list:?}"
    );
    for args in ["send codex hi", "keys codex esc", "watch codex"] {
        let run = scene.connecto("claude", args);
        assert_eq!(
            (run.code, run.err.contains("bridge_off")),
            (1, true),
            "{args}: {run:?}"
        );
    }
    for args in ["read codex", "unwatch codex"] {
        let run = scene.connecto("claude", args);
        assert_eq!(run.code, 0, "{args}: {run:?}");
    }
    // Not manual mode: the version is verified, so the user can still send to it.
    assert!(
        scene
            .pane("codex")
            .unwrap()
            .bridge_off_reason()
            .unwrap()
            .contains("trusted")
    );
    // Stop and start are process control and pass; so does close, last.
    scene.pane("codex").unwrap().request_stop();
    scene.wait_for("codex to stop", |s| !s.pane("codex").unwrap().is_live());
    for args in ["start codex", "close codex"] {
        let run = scene.connecto("claude", args);
        assert_eq!(run.code, 0, "{args}: {run:?}");
    }
}

/// T34, T39: a drain whose reply cannot be written puts its notices back, so
/// the next drain still carries them and the roster; two drains at once never
/// return the same notice.
#[test]
fn a_drain_that_cannot_be_written_loses_nothing_and_drains_never_overlap() {
    let scene = Scene::new("drain");
    scene.user_types("claude-2", "make a notice");
    scene.wait_for("claude-2 to finish", |s| s.prompts("claude-2").len() == 1);
    thread::sleep(Duration::from_millis(1500));
    scene.user_types("claude", "DRAINDROP and DRAINTWICE");
    scene.wait_for("the prompt", |s| s.prompts("claude").len() == 1);
    thread::sleep(Duration::from_millis(1500));
    let outputs: Vec<String> = ["0", "1"]
        .iter()
        .map(|tag| {
            let bytes = fs::read(
                scene
                    .current
                    .lock()
                    .unwrap()
                    .join(format!("drain-claude-{tag}.txt")),
            )
            .unwrap();
            String::from_utf8_lossy(&bytes).into_owned()
        })
        .chain([context_of(&scene, "claude", 1)])
        .collect();
    let all = outputs.join("\n");
    // The dropped drain restored everything, then the notice went out once.
    assert_eq!(all.matches("claude-2 finished.").count(), 1, "{all}");
    assert_eq!(all.matches("Since your last prompt").count(), 1, "{all}");
    assert_eq!(
        all.matches("You are terminal claude").count(),
        1,
        "the roster went out once: {all}"
    );
}

/// T35: a Pause between paste and Enter gives no Enter and a refusal carrying the
/// request id; a submission within the clock binds; a refusal before any paste
/// leaves no record.
#[test]
fn guard_pause_between_paste_and_enter_keeps_a_request_that_a_later_enter_binds() {
    if !offline() {
        return;
    }
    // The Pause has to land between the paste and the Enter, a window of about
    // 0.8 s for a body this size. A machine too busy to catch it in time gives a
    // send that was entered, which says nothing about the guard, so try again.
    let big = "z".repeat(200_000);
    let mut caught = None;
    for attempt in 0..3 {
        let scene = Scene::new(&format!("pastepause{attempt}"));
        scene.user_types(
            "claude",
            &format!("RUN printf %s {big} | connecto send claude-2"),
        );
        scene.wait_for("the paste", |s| !s.log("pasted-claude-2.log").is_empty());
        scene.relay.pause_switch().pause();
        scene.wait_for("the refusal", |s| !s.runs("claude").is_empty());
        let run = scene.runs("claude").remove(0);
        if run.err.contains("paused_before_enter") {
            caught = Some((scene, run));
            break;
        }
    }
    let (scene, run) =
        caught.expect("the Pause never landed between paste and Enter in three tries");
    assert_eq!(run.code, 1, "{run:?}");
    assert!(
        run.err.contains("paused_before_enter") && run.err.contains("request r1"),
        "{run:?}"
    );
    assert!(scene.prompts("claude-2").is_empty(), "no Enter was pressed");
    // A send refused before any paste leaves no record: its id is unknown.
    let refused = scene.connecto("claude", "send claude-2 hi");
    assert!(refused.err.contains("paused"), "{refused:?}");
    scene.relay.pause_switch().resume();
    assert!(
        scene
            .connecto("claude", "result r2")
            .err
            .contains("unknown_request")
    );
    // The user presses Enter on the leftover text, within the clock: it binds.
    scene
        .pane("claude-2")
        .unwrap()
        .session()
        .unwrap()
        .send_input(b"\r".to_vec())
        .unwrap();
    let result = scene.result_of("claude", "r1");
    assert_eq!(result.code, 0, "{result:?}");
}

/// T36: an `--ask` sender that restarts gets no wake or notice, its watches go,
/// and the result stays readable.
#[test]
fn an_ask_sender_that_restarts_gets_nothing_and_the_result_stays_readable() {
    let scene = Scene::new("restart");
    assert_eq!(scene.connecto("claude", "watch claude-2").code, 0);
    assert_eq!(
        scene
            .connecto("claude", "send claude-2 --ask 'SLEEP 4 slow'")
            .code,
        0
    );
    let before = scene.pane("claude").unwrap().attempt();
    scene.pane("claude").unwrap().request_restart();
    scene.wait_for("the restart", |s| {
        s.pane("claude").unwrap().attempt() > before && s.screen("claude").contains("FAKE-READY")
    });
    let count = scene.prompts("claude").len();
    thread::sleep(Duration::from_millis(7000));
    assert_eq!(
        scene.prompts("claude").len(),
        count,
        "the new attempt was woken for its predecessor"
    );
    let list = scene.connecto("claude", "list");
    assert!(
        !list.out.contains("watched"),
        "watches ended with the attempt: {list:?}"
    );
    let result = scene.result_of("claude", "r1");
    assert_eq!(result.code, 0, "{result:?}");
}

/// T37: after a project change another project's request is unknown, ids do not
/// recur, and returning finds the results.
#[test]
fn a_project_change_partitions_results_and_ids_do_not_recur() {
    let scene = Scene::new("projects");
    assert_eq!(scene.connecto("claude", "send claude-2 first").code, 0);
    assert_eq!(scene.result_of("claude", "r1").code, 0);
    let other = scene.root.join("home/other");
    fs::create_dir_all(&other).unwrap();
    let switch = |to: &Path| {
        scene.relay.stop_all();
        scene.wait_for("everything to stop", |s| {
            s.relay.panes().iter().all(|p| !p.is_live())
        });
        scene.relay.pause_switch().resume();
        let before: Vec<(String, u64)> = scene
            .ids()
            .into_iter()
            .map(|id| {
                let attempt = scene.pane(&id).unwrap().attempt();
                (id, attempt)
            })
            .collect();
        *scene.current.lock().unwrap() = to.to_owned();
        scene.relay.choose_project(to).unwrap();
        // `choose_project` is asynchronous, and a stopped session keeps its last
        // screen, so `FAKE-READY` alone can be the old one. Wait for the project
        // shown, a new attempt, and then the new screen.
        scene.wait_for("the project to show", |s| {
            s.relay.view().project.as_deref() == to.to_str()
        });
        // Terminals the user stopped stay stopped when a project is chosen again
        // (the saved list says so), so ask for those to start, as the user would.
        scene.wait_for("the project's list", |s| {
            s.relay.terminals().view().terminals.len() == before.len()
        });
        for (id, _) in &before {
            let wanted = scene
                .relay
                .terminals()
                .view()
                .terminals
                .iter()
                .any(|entry| entry.id == *id && entry.desired_running);
            if !wanted {
                scene.wait_for("the pane to settle", |s| {
                    !s.pane(id).unwrap().is_preparing()
                });
                scene.pane(id).unwrap().request_start();
            }
        }
        for (id, was) in &before {
            scene.wait_for("a new attempt", |s| {
                s.pane(id)
                    .is_some_and(|p| p.attempt() > *was && p.is_live())
            });
            scene.wait_for("ready", |s| s.screen(id).contains("FAKE-READY"));
        }
    };
    switch(&other);
    let gone = scene.connecto("claude", "result r1");
    assert!(gone.err.contains("unknown_request"), "{gone:?}");
    let sent = scene.connecto("claude", "send claude-2 second");
    assert!(!sent.out.starts_with("r1 "), "ids never recur: {sent:?}");
    switch(&scene.project.clone());
    let found = scene.result_of("claude", "r1");
    assert_eq!(
        (found.code, found.out.as_str()),
        (0, "echo: first"),
        "{found:?}"
    );
}

/// AC11 `interrupted` on Claude: the transcript says so and no hook fires. A
/// watcher is told, and the request ends `interrupted`.
#[test]
fn an_interrupted_claude_turn_is_reported_and_ends_its_request() {
    let scene = Scene::new("interrupt");
    assert_eq!(scene.connecto("claude", "watch claude-2").code, 0);
    assert_eq!(
        scene
            .connecto("claude", "send claude-2 --ask 'INTERRUPT now'")
            .code,
        0
    );
    scene.wait_for("the interrupted wake", |s| {
        notices(s, "claude")
            .iter()
            .any(|n| n.contains("claude-2 was interrupted.") || n.contains("Notices arrived"))
            || (1..=9).any(|n| context_of(s, "claude", n).contains("claude-2 was interrupted."))
    });
    let result = scene.result_of("claude", "r1");
    assert_eq!(result.code, 4, "{result:?}");
    assert!(
        result
            .out
            .lines()
            .last()
            .unwrap()
            .starts_with("interrupted"),
        "{result:?}"
    );
    let list = scene.connecto("claude", "list");
    assert!(
        list.out
            .lines()
            .any(|l| l.starts_with("claude-2") && l.contains("idle") && l.contains("interrupted")),
        "{list:?}"
    );
}

// ---- review round 1: the actor and the registry decide again

fn nothing_typed(scene: &Scene, id: &str) {
    assert!(
        !scene.screen(id).contains("PASTED"),
        "something was pasted: {}",
        scene.screen(id)
    );
    assert!(scene.prompts(id).is_empty());
}

/// Finding 1: a Pause that comes while the Gate waits on the durable audit log
/// must stop the paste, not only the Enter.
#[test]
fn guard_pause_during_the_audit_wait_stops_the_paste() {
    if !offline() {
        return;
    }
    let scene = Scene::with_audit("auditpause", SinkMode::Slow);
    scene.user_types("claude", "RUN connecto send claude-2 hello");
    scene.wait_for("the audit write to start", |s| {
        s.entered.load(std::sync::atomic::Ordering::SeqCst)
    });
    scene.relay.pause_switch().pause();
    scene.release();
    scene.wait_for("the send to answer", |s| !s.runs("claude").is_empty());
    let run = scene.runs("claude").remove(0);
    assert_eq!(run.code, 1, "{run:?}");
    thread::sleep(Duration::from_millis(1500));
    nothing_typed(&scene, "claude-2");
}

/// Audit failure: nothing is typed (`audit_failed`).
#[test]
fn guard_a_failing_audit_log_types_nothing() {
    if !offline() {
        return;
    }
    let scene = Scene::with_audit("auditfail", SinkMode::Fail);
    let run = scene.connecto("claude", "send claude-2 hello");
    assert_eq!(run.code, 1, "{run:?}");
    assert!(run.err.contains("audit_failed"), "{run:?}");
    thread::sleep(Duration::from_millis(1000));
    nothing_typed(&scene, "claude-2");
}

/// A write admitted for an earlier attempt types nothing in the new one.
#[test]
fn guard_a_stale_attempt_types_nothing() {
    if !offline() {
        return;
    }
    let scene = Scene::new("stale");
    let attempt = scene.pane("claude-2").unwrap().attempt();
    let outcome = scene
        .relay
        .agent_type_raw("claude-2", "late text", Some(attempt + 1));
    assert_eq!(
        outcome,
        Ok(connecto_session::WriteOutcome::Refused(
            connecto_gate::Denial::StaleTarget
        ))
    );
    thread::sleep(Duration::from_millis(1000));
    nothing_typed(&scene, "claude-2");
    // The stale attempt is refused before the audit wait: no "allow" intent was
    // ever written for it. (The check after the wait would catch it too, but
    // only this one keeps the intent out of the log.)
    let audit = scene.audit();
    assert!(
        !audit
            .lines()
            .any(|l| l.contains(r#""action":"type""#) && l.contains(r#""decision":"allow""#)),
        "an allow intent was recorded for a stale write:\n{audit}"
    );
    assert!(
        audit
            .lines()
            .any(|l| l.contains(r#""action":"type""#) && l.contains(r#""decision":"deny""#))
    );
}

/// An attempt that becomes stale only while the Gate waits on the audit log: the
/// check before the wait passed, so only the check after it can stop the paste.
#[test]
fn guard_an_attempt_that_goes_stale_during_the_audit_wait_types_nothing() {
    if !offline() {
        return;
    }
    let scene = Scene::with_audit("auditstale", SinkMode::Slow);
    let pane = scene.pane("claude-2").unwrap();
    let attempt = pane.attempt();
    let outcome = thread::scope(|threads| {
        let typing = threads.spawn(|| {
            scene
                .relay
                .agent_type_raw("claude-2", "late text", Some(attempt))
        });
        scene.wait_for("the audit write to start", |s| {
            s.entered.load(std::sync::atomic::Ordering::SeqCst)
        });
        // A restart completes inside the wait.
        pane.force_attempt(attempt + 1);
        scene.release();
        typing.join().unwrap()
    });
    assert_eq!(
        outcome,
        Ok(connecto_session::WriteOutcome::Refused(
            connecto_gate::Denial::StaleTarget
        ))
    );
    thread::sleep(Duration::from_millis(1000));
    nothing_typed(&scene, "claude-2");
}

/// Finding 3: a terminal that is running a login has no hook channel, and an
/// agent's write is never typed into it.
#[test]
fn guard_an_agent_never_types_into_a_login() {
    if !offline() {
        return;
    }
    let scene = Scene::with_tools("logintarget", vec![Tool::Claude], "2.1.318");
    scene.pane("claude").unwrap().request_stop();
    scene.wait_for("claude-2 to stop", |s| !s.pane("claude").unwrap().is_live());
    scene.pane("claude").unwrap().request_login();
    scene.wait_for("the login to run", |s| {
        let pane = s.pane("claude").unwrap();
        pane.is_live() && pane.session().is_some_and(|x| x.hook().is_none())
    });
    let outcome = scene.relay.agent_type_raw("claude", "LOGIN-LEAK", None);
    assert!(
        matches!(
            outcome,
            Ok(connecto_session::WriteOutcome::Refused(_)
                | connecto_session::WriteOutcome::NotRunning { pasted: false })
        ),
        "{outcome:?}"
    );
    thread::sleep(Duration::from_millis(1000));
    assert!(
        !scene.screen("claude").contains("LOGIN-LEAK"),
        "{}",
        scene.screen("claude")
    );
}

/// A start whose Pause comes during preflight never launches a process.
#[test]
fn guard_a_pause_during_a_start_cancels_it() {
    if !offline() {
        return;
    }
    let scene = Scene::new("startpause");
    let pane = scene.pane("claude-2").unwrap();
    pane.request_stop();
    scene.wait_for("claude-2 to stop", |s| {
        !s.pane("claude-2").unwrap().is_live()
    });
    thread::sleep(Duration::from_millis(500));
    let (reached, release) = {
        let (reached_tx, reached) = crossbeam_channel::bounded::<()>(1);
        let (release, release_rx) = crossbeam_channel::bounded::<()>(1);
        pane.set_probe(move |at| {
            if at == "before_spawn" {
                let _ = reached_tx.send(());
                let _ = release_rx.recv_timeout(Duration::from_secs(20));
            }
        });
        (reached, release)
    };
    scene.relay.agent_start("claude-2").unwrap();
    reached.recv_timeout(Duration::from_secs(20)).unwrap();
    scene.relay.pause_switch().pause();
    release.send(()).unwrap();
    thread::sleep(Duration::from_millis(1500));
    assert!(
        !pane.is_live(),
        "a start cancelled by Pause launched anyway"
    );
    assert_eq!(pane.stage(), connecto_core::PaneStage::Stopped);
}

/// Finding 5: a refused turn-end frame ends the requests that rode on it as
/// `unavailable`, and does not leave them pending.
#[test]
fn guard_a_refused_turn_end_frame_ends_the_request_unavailable() {
    if !offline() {
        return;
    }
    let scene = Scene::new("rejected");
    let sent = scene.connecto("claude", "send claude-2 --ask 'LOSE the end'");
    assert_eq!(sent.code, 0, "{sent:?}");
    // The request ends at once, and the sender is woken for it. Let that wake
    // land first: typed into the same input box as the next command, it would
    // join it (L2).
    scene.wait_for("the sender's wake", |s| !notices(s, "claude").is_empty());
    thread::sleep(Duration::from_millis(800));
    let result = scene.result_of("claude", "r1");
    assert_eq!(result.code, 4, "{result:?}");
    assert!(
        result
            .out
            .lines()
            .last()
            .unwrap()
            .starts_with("unavailable"),
        "{result:?}"
    );
}

// ---- review round 3

/// An agent's text and keys that were typed show in the ledger as final `Typed`
/// rows, with who typed what; a wake shows as Connecto's. Nothing offers to end
/// them.
#[test]
fn what_an_agent_typed_is_in_the_ledger_as_a_final_typed_row() {
    let scene = Scene::new("ledger");
    assert_eq!(scene.connecto("claude", "send claude-2 hello").code, 0);
    assert_eq!(scene.connecto("claude", "keys claude-2 esc").code, 0);
    assert_eq!(scene.connecto("claude", "watch claude-2").code, 0);
    scene.wait_for("the typed rows", |s| {
        s.relay
            .view()
            .ledger
            .iter()
            .filter(|r| r.state == connecto_core::DeliveryState::Typed)
            .count()
            >= 2
    });
    let ledger = scene.relay.view().ledger.clone();
    let row = |id: &str| ledger.iter().find(|r| r.id == id).cloned();
    let text = row("r1").expect("the send has a row");
    assert_eq!(
        (text.from.as_str(), text.to.as_str()),
        ("claude", "claude-2")
    );
    assert_eq!(text.state, connecto_core::DeliveryState::Typed);
    assert!(text.state.is_final(), "no End delivery is offered");
    assert_eq!(text.state.label(), "typed");
    let keys = row("k1").expect("the keys have a row");
    assert_eq!(
        (keys.from.as_str(), keys.to.as_str()),
        ("claude", "claude-2")
    );
    // A user's Send still uses its own states, alongside.
    scene.user_types("claude-2", "SLEEP 1 go");
    scene.wait_for("a wake row from Connecto", |s| {
        s.relay.view().ledger.iter().any(|r| {
            r.from == "connecto"
                && r.to == "claude"
                && r.state == connecto_core::DeliveryState::Typed
        })
    });
}

/// A paste that Pause stopped before Enter is in the ledger as uncertain, not
/// as typed.
#[test]
fn a_paste_left_without_enter_is_an_uncertain_row() {
    let scene = Scene::new("ledgerpause");
    let big = "z".repeat(200_000);
    scene.user_types(
        "claude",
        &format!("RUN printf %s {big} | connecto send claude-2"),
    );
    scene.wait_for("the paste", |s| !s.log("pasted-claude-2.log").is_empty());
    scene.relay.pause_switch().pause();
    scene.wait_for("the row", |s| {
        s.relay
            .view()
            .ledger
            .iter()
            .any(|r| r.id == "r1" && r.state == connecto_core::DeliveryState::Uncertain)
    });
    assert!(
        scene
            .relay
            .view()
            .ledger
            .iter()
            .all(|r| r.id != "r1" || r.state != connecto_core::DeliveryState::Typed)
    );
}

/// Every operation the spec says gives `logging_in` does, for a terminal that is
/// running a login (a live process with no hook channel); `close` still works.
#[test]
fn a_terminal_running_a_login_refuses_send_keys_read_watch_and_start_but_not_close() {
    let scene = Scene::mixed("logincodes");
    let claude = scene.pane("claude").unwrap();
    claude.request_stop();
    scene.wait_for("claude to stop", |s| !s.pane("claude").unwrap().is_live());
    claude.request_login();
    scene.wait_for("the login to run", |s| {
        let pane = s.pane("claude").unwrap();
        pane.is_live() && pane.stage() == connecto_core::PaneStage::LoggingIn
    });
    for args in [
        "send claude hi",
        "keys claude esc",
        "read claude",
        "watch claude",
        "start claude",
    ] {
        let run = scene.connecto("codex", args);
        assert_eq!(run.code, 1, "{args}: {run:?}");
        assert!(run.err.contains("logging_in"), "{args}: {run:?}");
    }
    assert!(
        !scene.screen("claude").contains("PROMPT"),
        "nothing reached the login"
    );
    // Close works: it stops the login and takes the terminal out of the list.
    let login = claude.session().unwrap();
    let close = scene.connecto("codex", "close claude");
    assert_eq!(close.code, 0, "{close:?}");
    scene.wait_for("the login to stop", |s| s.pane("claude").is_none());
    assert!(login.wait_exit(Duration::from_secs(10)), "the login ended");
}

// ---- the sender of an --ask ends its turn and is woken with the reply

/// The agent is told plainly not to poll, `result` says the same while the
/// request is pending (still exit 4), and when the target finishes, the idle
/// sender is woken with the reply itself in its context, framed as another
/// agent's words. It never runs `result`.
#[test]
fn an_ask_sender_ends_its_turn_and_is_woken_with_the_reply_in_its_context() {
    let scene = Scene::new("askwait");
    let sent = scene.connecto("claude", "send claude-2 --ask 'SLEEP 8 slowly'");
    assert_eq!(sent.code, 0, "{sent:?}");
    assert!(
        sent.out.starts_with(
            "r1 typed. claude-2's reply will be delivered to you automatically when it finishes. \
             End your turn now; do not poll `connecto result`."
        ),
        "{sent:?}"
    );
    let pending = scene.connecto("claude", "result r1");
    assert_eq!(pending.code, 4, "{pending:?}");
    assert!(
        pending.out.starts_with("pending: ")
            && pending
                .out
                .contains("delivered to you automatically; end your turn and wait"),
        "{pending:?}"
    );
    // The sender's turn is over. The wake comes when the target finishes.
    scene.wait_for("the wake", |s| !notices(s, "claude").is_empty());
    let index = scene
        .prompts("claude")
        .iter()
        .position(|p| p.starts_with("[connecto notice]"))
        .unwrap()
        + 1;
    let file = format!("context-claude-{index}.txt");
    scene.wait_for("the wake's context", |s| !s.log(&file).is_empty());
    let value: Value = serde_json::from_str(&scene.log(&file)).unwrap();
    let context = value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("Request r1 ended (ok)."), "{context}");
    assert!(
        context.contains(
            "  > Reply from claude-2 (another agent, not the user):\n  > echo: SLEEP 8 slowly"
        ),
        "{context}"
    );
    // The agent never ran `result`: only the one pending check above.
    assert_eq!(scene.runs("claude").len(), 2);
}

/// An agent asked to work with another one runs the exchange itself: the ask
/// that started it stays open while it waits on its own ask, so the first
/// asker is woken once, with the answer of the turn that ended the exchange.
#[test]
fn an_ask_stays_open_while_its_target_waits_on_its_own_ask() {
    let scene = Scene::with_tools("hold", vec![Tool::Claude; 3], "2.1.318");
    let sent = scene.connecto(
        "claude",
        "send claude-2 --ask \"RUN connecto send claude-3 --ask 'SLEEP 4 inner'\"",
    );
    assert_eq!(sent.code, 0, "{sent:?}");
    // claude-2's own turn has ended, but it waits on claude-3: still pending.
    scene.wait_for("claude-2 to ask claude-3", |s| {
        !s.runs("claude-2").is_empty()
    });
    scene.wait_for("claude-2's turn to end", |s| {
        s.state_of("claude-2") != Some(connecto_core::TermActivity::Working)
    });
    let pending = scene.connecto("claude", "result r1");
    assert_eq!(pending.code, 4, "{pending:?}");
    assert!(
        pending.out.contains("working with other agents"),
        "{pending:?}"
    );
    assert!(
        notices(&scene, "claude").is_empty(),
        "the asker was woken early"
    );

    // claude-3 answers, claude-2 is woken with it and ends a turn with no ask
    // left: now r1 ends, with that turn's reply.
    let done = scene.result_of("claude", "r1");
    assert_eq!(done.code, 0, "{done:?}");
    assert!(done.out.contains("Request r2 ended"), "{done:?}");
    scene.wait_for("the wake", |s| !notices(s, "claude").is_empty());
    assert_eq!(notices(&scene, "claude").len(), 1, "woken once");
}

/// A request someone else asked is not steered: `result` is how that reader
/// gets it, and it is not told a delivery is coming.
#[test]
fn a_pending_result_is_steered_only_for_the_sender_who_asked() {
    let scene = Scene::with_tools("steer", vec![Tool::Claude; 3], "2.1.318");
    assert_eq!(
        scene
            .connecto("claude", "send claude-2 --ask 'SLEEP 8 slowly'")
            .code,
        0
    );
    let other = scene.connecto("claude-3", "result r1");
    assert_eq!(other.code, 4, "{other:?}");
    assert!(!other.out.contains("delivered to you"), "{other:?}");
}

// ---- close when the list cannot be saved

/// Make the project's saved list unwritable, as a full or read-only disk would.
fn lock_list(scene: &Scene) {
    let projects = scene.root.join("data/projects");
    let dir = projects
        .read_dir()
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).unwrap();
}

/// A self-close writes its reply before the stop, so the reply says what the
/// removal depends on; and when the save then fails, the terminal is stopped and
/// kept, and the tab strip says why.
#[test]
fn a_self_close_with_an_unwritable_list_stops_keeps_and_says_why() {
    let scene = Scene::new("selfclosekeeps");
    lock_list(&scene);
    let run = scene.connecto("claude", "close claude");
    assert_eq!(run.code, 0, "{run:?}");
    assert!(
        run.out.contains("will be removed") && run.out.contains("needs the list to be saved"),
        "the reply does not promise more than it knows: {run:?}"
    );
    scene.wait_for("claude to stop", |s| !s.pane("claude").unwrap().is_live());
    scene.wait_for("the reason in the tab strip", |s| {
        s.relay
            .terminals()
            .view()
            .last_error
            .as_deref()
            .is_some_and(|why| why.contains("claude was stopped but not removed"))
    });
    assert_eq!(scene.ids(), ["claude", "claude-2"], "it stays listed");
    // The tab strip's reason and the list's "unsaved" mark come from two steps of
    // the registry, so the second is waited for rather than read the instant the
    // first shows.
    scene.wait_for("the list to be marked unsaved", |s| {
        s.relay.terminals().view().save == connecto_core::SaveState::Unsaved
    });
}

/// Another terminal's close learns it in the reply.
#[test]
fn a_close_with_an_unwritable_list_says_it_kept_the_terminal() {
    let scene = Scene::new("closekeeps");
    lock_list(&scene);
    let run = scene.connecto("claude", "close claude-2");
    assert_eq!(run.code, 0, "{run:?}");
    assert!(run.out.contains("stopped, but not removed"), "{run:?}");
    assert_eq!(scene.ids(), ["claude", "claude-2"]);
}

// ---- full power, on every launch path

/// Claude and Codex run with their bypass flags when Connecto starts them, a
/// terminal added by an agent, one started again, and one restarted included.
#[test]
fn every_launch_path_runs_with_the_bypass_flags() {
    let scene = Scene::mixed("flags");
    let launches = |id: &str| -> Vec<Vec<String>> {
        // The stand-in logs each launch's arguments, one per line, in order.
        let text = scene.log(&format!("args-{id}.txt"));
        let flag = if id.starts_with("codex") {
            "--dangerously-bypass-approvals-and-sandbox"
        } else {
            "--dangerously-skip-permissions"
        };
        text.split_inclusive('\n')
            .fold((Vec::<Vec<String>>::new(), false), |(mut all, _), line| {
                let line = line.trim_end().to_owned();
                if line == flag {
                    all.push(vec![line]);
                } else if let Some(last) = all.last_mut() {
                    last.push(line);
                }
                (all, true)
            })
            .0
    };
    let seen = |id: &str, n: usize| launches(id).len() >= n;
    // The original terminals.
    scene.wait_for("the original launches", |_| {
        seen("claude", 1) && seen("codex", 1)
    });
    // Added by an agent.
    assert_eq!(scene.connecto("claude", "add codex").code, 0);
    scene.wait_for("the added terminal", |s| {
        s.screen("codex-2").contains("FAKE-READY")
    });
    scene.wait_for("its launch", |_| seen("codex-2", 1));
    // Started again after a stop.
    scene.pane("codex-2").unwrap().request_stop();
    scene.wait_for("it to stop", |s| !s.pane("codex-2").unwrap().is_live());
    assert_eq!(scene.connecto("claude", "start codex-2").code, 0);
    scene.wait_for("its second launch", |_| seen("codex-2", 2));
    // Restarted.
    scene.pane("claude").unwrap().request_restart();
    scene.wait_for("claude's second launch", |_| seen("claude", 2));
    for id in ["claude", "codex", "codex-2"] {
        for launch in launches(id) {
            assert!(
                !launch
                    .iter()
                    .any(|w| w.contains("approval_policy") || w.contains("sandbox_mode")),
                "{id}: {launch:?}"
            );
        }
    }
}

// ---- project tree: names, parents, history, resume, states

impl Scene {
    fn entry(&self, id: &str) -> Option<connecto_core::TerminalEntry> {
        self.relay
            .terminals()
            .view()
            .terminals
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
    }

    /// Wait until `id` is at its prompt (idle, or done) and stays so.
    fn wait_settled(&self, id: &str) {
        use connecto_core::TermActivity::{Done, Idle};
        let calm = std::cell::Cell::new(0);
        self.wait_for(&format!("{id} to settle"), |s| {
            if matches!(s.state_of(id), Some(Idle | Done)) {
                calm.set(calm.get() + 1);
            } else {
                calm.set(0);
            }
            thread::sleep(Duration::from_millis(100));
            calm.get() >= 8
        });
    }

    fn state_of(&self, id: &str) -> Option<connecto_core::TermActivity> {
        self.entry(id).map(|entry| entry.state)
    }

    /// The project's data folder (`projects/<id>`).
    fn project_data(&self) -> PathBuf {
        self.root
            .join("data/projects")
            .read_dir()
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path()
    }

    fn saved_list(&self) -> String {
        fs::read_to_string(self.project_data().join("terminals.json")).unwrap_or_default()
    }

    /// The arguments of each launch of a terminal, one list per launch.
    fn launch_args(&self, id: &str) -> Vec<String> {
        self.log(&format!("args-{id}.txt"))
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// R6: an agent's add records the caller as parent, flattened to one level,
/// keeps the real caller as `asked_by`, and takes `--name`. A long name is
/// refused, not cut.
#[test]
fn an_agents_add_records_parent_name_and_flattens_grandchildren() {
    let scene = Scene::new("lineage");
    let added = scene.connecto("claude", "add claude --name 'helper one'");
    assert_eq!(added.code, 0, "{added:?}");
    let child = scene.entry("claude-3").unwrap();
    assert_eq!(child.parent.as_deref(), Some("claude"));
    assert_eq!(child.asked_by, None);
    assert_eq!(child.name.as_deref(), Some("helper one"));
    scene.wait_for("claude-3 to be ready", |s| {
        s.screen("claude-3").contains("FAKE-READY")
    });

    // A child's own add sits under the top-level ancestor, and says who asked.
    let grand = scene.connecto("claude-3", "add claude");
    assert_eq!(grand.code, 0, "{grand:?}");
    let grand = scene.entry("claude-4").unwrap();
    assert_eq!(grand.parent.as_deref(), Some("claude"), "one nesting level");
    assert_eq!(grand.asked_by.as_deref(), Some("claude-3"));
    assert_eq!(grand.name, None);

    let long = format!("add claude --name '{}'", "x".repeat(65));
    let refused = scene.connecto("claude", &long);
    assert_eq!(refused.code, 1, "{refused:?}");
    assert!(refused.err.contains("bad_request"), "{refused:?}");
    assert!(scene.entry("claude-5").is_none());

    // And it all survives in the saved list.
    let saved = scene.saved_list();
    assert!(saved.contains("helper one"), "{saved}");
    assert!(saved.contains(r#""asked_by": "claude-3""#), "{saved}");
    // The user's own add has no parent.
    scene
        .relay
        .terminals()
        .add(connecto_core::ToolKind::Claude, Some("mine".to_owned()))
        .unwrap();
    scene.wait_for("the user's terminal", |s| s.entry("claude-5").is_some());
    assert_eq!(scene.entry("claude-5").unwrap().parent, None);
}

/// A terminal an agent started belongs to that agent: another agent can read
/// it but not type into it, press keys there, start it or close it. A terminal
/// the user opened stays open to all, and a child can write to its starter.
#[test]
fn a_terminal_an_agent_started_is_used_only_by_that_agent() {
    let scene = Scene::new("ownership");
    let added = scene.connecto("claude", "add claude");
    assert_eq!(added.code, 0, "{added:?}");
    scene.wait_for("claude-3 to be ready", |s| {
        s.screen("claude-3").contains("FAKE-READY")
    });

    for command in [
        "send claude-3 'take this over'",
        "send claude-3 --ask 'take this over'",
        "keys claude-3 enter",
        "close claude-3",
        "start claude-3",
    ] {
        let refused = scene.connecto("claude-2", command);
        assert_eq!(refused.code, 1, "{command}: {refused:?}");
        assert!(refused.err.contains("not_yours"), "{command}: {refused:?}");
        assert!(
            refused.err.contains("connecto add"),
            "{command}: {refused:?}"
        );
    }
    assert!(scene.entry("claude-3").is_some(), "it was not closed");
    assert!(scene.prompts("claude-3").is_empty(), "nothing was typed");

    // Looking is open to all, and the list says whose it is.
    assert_eq!(scene.connecto("claude-2", "read claude-3").code, 0);
    let list = scene.connecto("claude-2", "list");
    assert!(list.out.contains("belongs to claude"), "{list:?}");
    let list = scene.connecto("claude", "list");
    assert!(list.out.contains("yours"), "{list:?}");

    // The starter uses it, and it can write back to its starter.
    let sent = scene.connecto("claude", "send claude-3 'your task'");
    assert_eq!(sent.code, 0, "{sent:?}");
    let back = scene.connecto("claude-3", "send claude 'a question'");
    assert_eq!(back.code, 0, "{back:?}");
    // A terminal the user opened belongs to nobody.
    let open = scene.connecto("claude-3", "send claude-2 'hello'");
    assert_eq!(open.code, 0, "{open:?}");

    // Two terminals the same agent started work with each other directly.
    assert_eq!(scene.connecto("claude", "add claude").code, 0);
    scene.wait_for("claude-4 to be ready", |s| {
        s.screen("claude-4").contains("FAKE-READY")
    });
    let sibling = scene.connecto("claude-3", "send claude-4 'your move'");
    assert_eq!(sibling.code, 0, "{sibling:?}");
    let sibling = scene.connecto("claude-4", "send claude-3 'your move'");
    assert_eq!(sibling.code, 0, "{sibling:?}");
    assert_eq!(scene.connecto("claude", "close claude-4").code, 0);
    scene.wait_for("claude-4 to close", |s| s.entry("claude-4").is_none());

    // A grandchild belongs to the agent that asked for it, not to the tree's root.
    assert_eq!(scene.connecto("claude-3", "add claude").code, 0);
    scene.wait_for("claude-5 to be ready", |s| {
        s.screen("claude-5").contains("FAKE-READY")
    });
    let refused = scene.connecto("claude", "send claude-5 'mine?'");
    assert!(refused.err.contains("not_yours"), "{refused:?}");
    assert_eq!(scene.connecto("claude-3", "close claude-5").code, 0);
    assert_eq!(scene.connecto("claude", "close claude-3").code, 0);
}

/// R6: without `--name`, the first `--ask` body names the child: its first
/// words, at most 40 characters. A later ask does not rename it, and a named
/// terminal keeps its name.
#[test]
fn the_first_ask_names_a_child_that_has_no_name() {
    let scene = Scene::new("autoname");
    assert_eq!(scene.connecto("claude", "add claude").code, 0);
    assert_eq!(
        scene.connecto("claude", "add claude --name kept").code,
        0,
        "claude-4 has a name"
    );
    for id in ["claude-3", "claude-4"] {
        scene.wait_for(&format!("{id} to be ready"), |s| {
            s.screen(id).contains("FAKE-READY")
        });
    }
    let ask = "review the diff of the parser and report back when it is done";
    assert_eq!(
        scene
            .connecto("claude", &format!("send claude-3 --ask '{ask}'"))
            .code,
        0
    );
    scene.wait_for("the name", |s| {
        s.entry("claude-3").is_some_and(|e| e.name.is_some())
    });
    assert_eq!(
        scene.entry("claude-3").unwrap().name.as_deref(),
        Some("review the diff of the parser and report")
    );
    scene.wait_settled("claude");
    let other = scene.connecto("claude", "send claude-4 --ask 'other words'");
    assert_eq!(other.code, 0, "{other:?}");
    thread::sleep(Duration::from_millis(600));
    assert_eq!(
        scene.entry("claude-4").unwrap().name.as_deref(),
        Some("kept")
    );
    // A second ask to claude-3 does not rename it.
    scene.wait_settled("claude");
    scene.wait_settled("claude-3");
    let second = scene.connecto("claude", "send claude-3 --ask 'something else'");
    assert_eq!(second.code, 0, "{second:?}");
    thread::sleep(Duration::from_millis(600));
    assert_eq!(
        scene.entry("claude-3").unwrap().name.as_deref(),
        Some("review the diff of the parser and report")
    );
}

/// R16: a rename never changes the id, and `send` still reaches the terminal by
/// it. A name that is too long is refused at once.
#[test]
fn a_rename_keeps_the_id_and_send_still_works() {
    let scene = Scene::new("rename");
    let control = scene.relay.terminals();
    control
        .rename("claude-2", Some("the reviewer".to_owned()))
        .unwrap();
    scene.wait_for("the new name", |s| {
        s.entry("claude-2")
            .is_some_and(|e| e.name.as_deref() == Some("the reviewer"))
    });
    assert!(control.rename("claude-2", Some("x".repeat(65))).is_err());
    assert!(control.rename("nope", Some("x".to_owned())).is_err());
    let sent = scene.connecto("claude", "send claude-2 hello after rename");
    assert_eq!(sent.code, 0, "{sent:?}");
    scene.wait_for("claude-2 to take it", |s| {
        s.prompts("claude-2")
            .iter()
            .any(|p| p.contains("hello after rename"))
    });
    assert_eq!(scene.ids(), ["claude", "claude-2"]);
    // An empty name clears it.
    control.rename("claude-2", Some("  ".to_owned())).unwrap();
    scene.wait_for("the name to clear", |s| {
        s.entry("claude-2").is_some_and(|e| e.name.is_none())
    });
}

/// R8 and R9: a close saves a redacted transcript and an index entry, closes
/// its children with it, and the entry says whether it can be resumed.
#[test]
fn a_close_writes_a_redacted_transcript_and_closes_its_children() {
    let scene = Scene::new("closehist");
    let secret = format!("sk-{}", "a1B2c3D4e5".repeat(3));
    scene.user_types("claude", &format!("please keep {secret} safe"));
    scene.wait_for("the prompt on the screen", |s| {
        s.screen("claude").contains("PROMPT-SEEN")
    });
    scene.wait_settled("claude");
    assert_eq!(
        scene.connecto("claude", "add claude --name 'a child'").code,
        0
    );
    scene.wait_for("claude-3 to be ready", |s| {
        s.screen("claude-3").contains("FAKE-READY")
    });
    scene.wait_for("the session id to be saved", |s| {
        s.saved_list().contains(r#""session""#)
    });
    let control = scene.relay.terminals();
    control.close("claude").unwrap();
    scene.wait_for("claude to go", |s| s.entry("claude").is_none());
    assert!(scene.entry("claude-3").is_none(), "the child goes with it");
    assert!(scene.pane("claude-3").is_none());

    let view = control.view();
    assert_eq!(view.history.len(), 2, "the child is in the history too");
    let entry = view
        .history
        .iter()
        .find(|entry| entry.terminal_id == "claude")
        .unwrap();
    assert_eq!(entry.terminal_id, "claude");
    assert!(entry.resumable);
    let task = entry.task.as_deref().unwrap();
    assert!(
        task.contains("please keep") && !task.contains(&secret[..12]),
        "{task}"
    );
    let dir = scene.project_data().join("history");
    let index = fs::read_to_string(dir.join("index.json")).unwrap();
    assert!(
        index.contains(&entry.id) && !index.contains(&secret[..12]),
        "{index}"
    );
    let text = fs::read_to_string(dir.join(format!("{}.txt", entry.id))).unwrap();
    assert!(text.contains("PROMPT-SEEN"), "{text}");
    assert!(
        !text.contains(&secret[..12]),
        "the secret is redacted: {text}"
    );
    assert!(text.contains("[REDACTED]"), "{text}");

    // The transcript is read off the UI thread and shows in the view.
    control.request_transcript(&entry.id).unwrap();
    scene.wait_for("the transcript", |s| {
        s.relay
            .terminals()
            .view()
            .transcript
            .as_ref()
            .is_some_and(|t| t.history_id == entry.id && t.text.contains("PROMPT-SEEN"))
    });
    assert!(control.request_transcript("h-nope").is_err());
}

/// R10: Continue opens a new top-level terminal of the same tool and name that
/// resumes the saved session: Claude `--resume <id>` in place of `--session-id`,
/// Codex `resume <id>`.
#[test]
fn continue_builds_the_resume_arguments_for_both_tools() {
    let scene = Scene::mixed("continue");
    scene.wait_for("both session ids to be saved", |s| {
        s.saved_list().matches(r#""session""#).count() == 2
    });
    let control = scene.relay.terminals();
    control
        .rename("codex", Some("the coder".to_owned()))
        .unwrap();
    scene.wait_for("the name", |s| {
        s.entry("codex").is_some_and(|e| e.name.is_some())
    });
    for id in ["claude", "codex"] {
        control.close(id).unwrap();
        scene.wait_for(&format!("{id} to go"), |s| s.entry(id).is_none());
    }
    let history = control.view().history.clone();
    assert_eq!(history.len(), 2);
    for entry in history.iter() {
        assert!(entry.resumable);
        control.continue_from(&entry.id).unwrap();
    }
    scene.wait_for("both continued", |s| {
        s.entry("claude-2").is_some() && s.entry("codex-2").is_some()
    });
    let again = scene.entry("codex-2").unwrap();
    assert_eq!(again.name.as_deref(), Some("the coder"));
    assert_eq!(again.parent, None);
    scene.wait_for("their launches", |s| {
        !s.launch_args("claude-2").is_empty() && !s.launch_args("codex-2").is_empty()
    });
    let claude = scene.launch_args("claude-2");
    let at = claude.iter().position(|a| a == "--resume").unwrap();
    assert_eq!(claude[at + 1], "11111111-aaaa");
    assert!(!claude.iter().any(|a| a == "--session-id"), "{claude:?}");
    let codex = scene.launch_args("codex-2");
    assert_eq!(codex[..2], ["resume", "11111111-aaaa"], "{codex:?}");
    assert!(codex.iter().any(|a| a == "--no-daemon"));
    // The entries stay in the history.
    assert_eq!(control.view().history.len(), 2);
    assert!(control.continue_from("h-nope").is_err());
}

const SEEDED: &str = r#"{"version":1,"next":{"claude":4},"terminals":[
  {"id":"claude","tool":"claude","desired":"running","name":"old one","session":"aaaa-1111","working":true},
  {"id":"claude-2","tool":"claude","desired":"running"},
  {"id":"claude-3","tool":"claude","desired":"running","session":"bbbb-2222"}]}"#;

/// R11: a relaunch starts only the terminals that were working when the app
/// closed. They resume their saved session and are sent nothing: the
/// conversation comes back idle. The others wait, shown as not started, and
/// start when asked for, with their saved session if they have one.
///
/// The one that was working shows as interrupted, to the user and to other
/// agents, and that is saved, until it starts a turn of its own.
#[test]
fn a_relaunch_starts_what_was_working_and_the_rest_wait() {
    use connecto_core::TermActivity::{Idle, Interrupted, NotStarted};
    let scene = Scene::seeded("relaunch", SEEDED, &[]);
    scene.wait_for("claude to be ready", |s| {
        s.screen("claude").contains("FAKE-READY")
    });
    let first = scene.launch_args("claude");
    let at = first.iter().position(|a| a == "--resume").unwrap();
    assert_eq!(first[at + 1], "aaaa-1111");
    assert!(!first.iter().any(|a| a == "--session-id"));
    assert!(!first.iter().any(|a| a == "--"), "a note: {first:?}");
    assert_eq!(
        scene.entry("claude").unwrap().name.as_deref(),
        Some("old one")
    );
    assert!(scene.pane("claude").unwrap().warnings().is_empty());
    // The two that were idle were not launched, whatever `desired` said.
    for id in ["claude-2", "claude-3"] {
        assert_eq!(scene.state_of(id), Some(NotStarted), "{id}");
        assert!(!scene.entry(id).unwrap().desired_running, "{id}");
        assert!(scene.launch_args(id).is_empty(), "{id} was launched");
    }
    scene.wait_for("claude to show as interrupted", |s| {
        s.state_of("claude") == Some(Interrupted)
    });
    assert!(
        scene.saved_list().contains(r#""interrupted": true"#),
        "{}",
        scene.saved_list()
    );
    // Asked for, they start.
    let control = scene.relay.terminals();
    for id in ["claude-2", "claude-3"] {
        control.restart(id).unwrap();
        scene.wait_for(&format!("{id} to be ready"), |s| {
            s.screen(id).contains("FAKE-READY")
        });
    }
    assert!(
        scene
            .launch_args("claude-2")
            .iter()
            .any(|a| a == "--session-id")
    );
    let third = scene.launch_args("claude-3");
    let at = third.iter().position(|a| a == "--resume").unwrap();
    assert_eq!(third[at + 1], "bbbb-2222");
    // Started by hand, they are at rest, not interrupted.
    scene.wait_for("claude-3 idle", |s| s.state_of("claude-3") == Some(Idle));
    let listed = scene.connecto("claude-3", "list");
    assert!(listed.out.contains("interrupted"), "{listed:?}");
    // A turn of its own ends it, on screen and on disk.
    scene.user_types("claude", "hello again");
    scene.wait_for("claude at rest", |s| {
        s.state_of("claude") == Some(Idle) && !s.saved_list().contains("interrupted")
    });
}

/// A terminal in the middle of a turn is saved as working, an idle one is
/// not, and a quit keeps what was saved however the turn ends afterwards.
#[test]
fn a_turn_is_saved_as_working_and_a_quit_keeps_it() {
    use connecto_core::TermActivity::{Idle, Working};
    let working = |s: &Scene| s.saved_list().contains(r#""working": true"#);
    let scene = Scene::new("savedworking");
    scene.user_types("claude-2", "SLEEP 1 first");
    scene.wait_for("the turn to be saved", working);
    scene.wait_for("the end of the turn to be saved", |s| {
        s.state_of("claude-2") == Some(Idle) && !working(s)
    });
    scene.user_types("claude-2", "SLEEP 3 second");
    scene.wait_for("the second turn to be saved", |s| {
        s.state_of("claude-2") == Some(Working) && working(s)
    });
    scene.relay.begin_shutdown();
    scene.wait_for("the turn to end", |s| s.state_of("claude-2") == Some(Idle));
    thread::sleep(Duration::from_millis(800));
    assert!(working(&scene), "{}", scene.saved_list());
}

/// R11: when the resume fails at launch, the terminal starts a new
/// conversation once, and says so on the pane.
#[test]
fn a_failed_resume_starts_fresh_once_and_says_so() {
    let scene = Scene::seeded("resumefail", SEEDED, &[("resume-fails", "")]);
    scene.wait_for("claude to be ready", |s| {
        s.screen("claude").contains("FAKE-READY")
    });
    let args = scene.launch_args("claude");
    assert_eq!(
        args.iter().filter(|a| *a == "--resume").count(),
        1,
        "resumed once: {args:?}"
    );
    assert_eq!(
        args.iter().filter(|a| *a == "--session-id").count(),
        1,
        "then started new, once: {args:?}"
    );
    let warnings = scene.pane("claude").unwrap().warnings();
    assert!(
        warnings.iter().any(|w| w.contains("could not be resumed")),
        "{warnings:?}"
    );
}

/// R7a: `NeedsUser` is set by the tool asking, and the next prompt or a
/// keystroke from the user clears it.
#[test]
fn needs_user_is_set_by_the_tool_and_cleared_by_typing() {
    use connecto_core::TermActivity::{Idle, NeedsUser};
    let scene = Scene::new("needsuser");
    scene.user_types("claude-2", "NEEDS");
    scene.wait_for("the dot to ask for the user", |s| {
        s.state_of("claude-2") == Some(NeedsUser)
    });
    assert_eq!(scene.state_of("claude"), Some(Idle));
    // The user answers in the pane.
    scene
        .pane("claude-2")
        .unwrap()
        .send_input(b"y".to_vec())
        .unwrap();
    scene.wait_for("the dot to clear", |s| s.state_of("claude-2") == Some(Idle));
    // And the next prompt clears it for good.
    scene.user_types("claude-2", "NEEDS");
    scene.wait_for("asking again", |s| {
        s.state_of("claude-2") == Some(NeedsUser)
    });
    scene.user_types("claude-2", "hello");
    scene.wait_for("the next prompt to clear it", |s| {
        s.state_of("claude-2") == Some(Idle)
    });
    assert!(scene.entry("claude-2").unwrap().finished_turns >= 2);
}

/// R7b and R7c: `Done` is for a child whose last `--ask` ended and which is
/// idle; a top-level terminal asked the same is just idle. Each turn end counts.
#[test]
fn done_applies_only_to_a_child_whose_ask_ended() {
    use connecto_core::TermActivity::{Done, Idle, Working};
    let scene = Scene::new("done");
    assert_eq!(scene.connecto("claude", "add claude").code, 0);
    scene.wait_for("claude-3 to be ready", |s| {
        s.screen("claude-3").contains("FAKE-READY")
    });
    scene.wait_for("claude-3 idle", |s| s.state_of("claude-3") == Some(Idle));
    let before = scene.entry("claude-3").unwrap().finished_turns;
    assert_eq!(
        scene
            .connecto("claude", "send claude-3 --ask 'SLEEP 2 slowly'")
            .code,
        0
    );
    scene.wait_for("claude-3 working", |s| {
        s.state_of("claude-3") == Some(Working)
    });
    scene.wait_for("claude-3 done", |s| s.state_of("claude-3") == Some(Done));
    assert!(scene.entry("claude-3").unwrap().finished_turns > before);

    scene.wait_settled("claude");
    assert_eq!(scene.connecto("claude", "send claude-2 --ask 'hi'").code, 0);
    scene.wait_for("claude-2 to take it", |s| !s.prompts("claude-2").is_empty());
    scene.wait_for("claude-2 to finish", |s| {
        s.entry("claude-2").is_some_and(|e| e.finished_turns >= 1)
    });
    thread::sleep(Duration::from_millis(600));
    assert_eq!(
        scene.state_of("claude-2"),
        Some(Idle),
        "a top-level terminal is never Done"
    );
}

/// An exited terminal restarts in place: same id, same name, a new attempt.
#[test]
fn a_restart_keeps_the_id_and_the_name() {
    let scene = Scene::new("restart");
    let control = scene.relay.terminals();
    control
        .rename("claude-2", Some("again".to_owned()))
        .unwrap();
    let pane = scene.pane("claude-2").unwrap();
    pane.request_stop();
    scene.wait_for("it to stop", |s| !s.pane("claude-2").unwrap().is_live());
    scene.wait_for("exited", |s| {
        s.state_of("claude-2") == Some(connecto_core::TermActivity::Exited)
    });
    let before = pane.attempt();
    control.restart("claude-2").unwrap();
    scene.wait_for("a new attempt", |s| {
        let pane = s.pane("claude-2").unwrap();
        pane.is_live() && pane.attempt() > before
    });
    let entry = scene.entry("claude-2").unwrap();
    assert_eq!(entry.name.as_deref(), Some("again"));
    assert!(control.restart("nope").is_err());
}

/// R7a: a permission prompt arrives inside a turn. The dot asks for the user
/// then, while the turn is still open, not when it is over.
#[test]
fn needs_user_shows_while_the_turn_is_still_working() {
    use connecto_core::TermActivity::NeedsUser;
    let scene = Scene::new("needsduring");
    let before = scene.entry("claude-2").unwrap().finished_turns;
    scene.user_types("claude-2", "NEEDSLONG");
    scene.wait_for("the dot to ask for the user", |s| {
        s.state_of("claude-2") == Some(NeedsUser)
    });
    assert_eq!(
        scene.entry("claude-2").unwrap().finished_turns,
        before,
        "the turn had not ended yet"
    );
}

/// Real-app bug: a Claude with background work running was given a prompt, and
/// the background task's notification was queued into that same turn. Its hook
/// fired, but the turn had one `Stop`. The notification's prompt was never
/// closed, so Connecto saw the Claude as working for ever and never woke it with
/// the reply to the `--ask` it had sent in that turn.
#[test]
fn a_notification_queued_into_a_turn_does_not_leave_the_terminal_working() {
    use connecto_core::TermActivity::{Idle, Working};
    let scene = Scene::new("mergednote");
    // Work in the background: the turn ends, the terminal is still busy.
    scene.user_types("claude-2", "BG1");
    scene.wait_for("the first turn to end", |s| {
        s.entry("claude-2").is_some_and(|e| e.finished_turns == 0)
            && s.prompts("claude-2").len() == 1
            && s.state_of("claude-2") == Some(Working)
    });
    thread::sleep(Duration::from_millis(800));
    // The next turn takes the finished task's notification along, and asks
    // claude something.
    scene.user_types(
        "claude-2",
        "MERGE RUN connecto send claude --ask 'hello there'",
    );
    scene.wait_for("the ask to be sent", |s| !s.runs("claude-2").is_empty());
    scene.wait_for("the turn to end and the dot to go idle", |s| {
        s.state_of("claude-2") == Some(Idle)
    });
    // And the reply reaches the asker, which is woken for it.
    scene.wait_for("the wake", |s| {
        s.prompts("claude-2")
            .iter()
            .any(|p| p.starts_with("[connecto notice]"))
    });
}

/// A send right after a terminal was added waits until the tool can take input
/// and only then types: the paste is never lost to a half-started TUI.
#[cfg(feature = "test-endpoints")]
#[test]
fn a_send_to_a_booting_terminal_waits_for_it_and_then_types() {
    let scene = Scene::booting("holdboot", &[("boot-delay-claude-2", "4")]);
    scene.wait_for("claude to be ready", |s| {
        s.screen("claude").contains("FAKE-READY")
    });
    let sent = scene.connecto("claude", "send claude-2 'hello after boot'");
    assert_eq!(sent.code, 0, "{sent:?}");
    // It was held for the input box, then typed.
    assert_eq!(scene.relay.held_sends("claude-2"), 1);
    scene.wait_for("the prompt", |s| !s.prompts("claude-2").is_empty());
    assert!(scene.prompts("claude-2")[0].contains("hello after boot"));
}

/// Already-started targets are typed into at once.
#[cfg(feature = "test-endpoints")]
#[test]
fn a_send_to_a_started_terminal_is_not_held() {
    let scene = Scene::new("noholdready");
    scene.wait_for("both ready", |s| {
        s.screen("claude").contains("FAKE-READY") && s.screen("claude-2").contains("FAKE-READY")
    });
    thread::sleep(Duration::from_millis(1500));
    let sent = scene.connecto("claude", "send claude-2 'quick'");
    assert_eq!(sent.code, 0, "{sent:?}");
    // Not a wall-clock threshold: the send was simply never held.
    assert_eq!(scene.relay.held_sends("claude-2"), 0);
    scene.wait_for("the prompt", |s| !s.prompts("claude-2").is_empty());
}

/// A screen that waits for a person (the folder trust prompt) is never typed
/// into: the sender is told what it shows, and nothing reaches the terminal.
#[cfg(feature = "test-endpoints")]
#[test]
fn a_blocking_screen_is_not_typed_into_and_the_sender_is_told() {
    let scene = Scene::booting(
        "holdblock",
        &[
            ("boot-delay-claude-2", "40"),
            (
                "boot-screen-claude-2",
                "Is this a project you created or one you trust?\n 1. Yes, I trust this folder",
            ),
        ],
    );
    scene.wait_for("claude to be ready", |s| {
        s.screen("claude").contains("FAKE-READY")
    });
    scene.wait_for("the prompt on screen", |s| {
        s.screen("claude-2").contains("one you trust")
    });
    let sent = scene.connecto("claude", "send claude-2 'do not paste'");
    assert_ne!(sent.code, 0, "{sent:?}");
    let text = format!("{}{}", sent.out, sent.err);
    assert!(
        text.contains("folder trust") && text.contains("Nothing was typed"),
        "{sent:?}"
    );
    assert!(
        text.contains("one you trust"),
        "the screen is quoted: {sent:?}"
    );
    assert_eq!(scene.relay.held_sends("claude-2"), 1);
    thread::sleep(Duration::from_secs(1));
    assert!(scene.prompts("claude-2").is_empty());
}

/// When the input box never shows, the sender is told exactly why.
#[cfg(feature = "test-endpoints")]
#[test]
fn a_send_that_never_gets_an_input_box_says_what_the_screen_shows() {
    let scene = Scene::booting(
        "holdtimeout",
        &[
            ("boot-delay-claude-2", "60"),
            ("boot-screen-claude-2", "loading servers..."),
        ],
    );
    scene.wait_for("claude to be ready", |s| {
        s.screen("claude").contains("FAKE-READY")
    });
    scene.wait_for("the boot screen", |s| {
        s.screen("claude-2").contains("loading servers")
    });
    // A short limit keeps the test quick; the rule is the same.
    scene.relay.set_hold_limit(Duration::from_secs(4));
    // Two sends at once: each has its own limit from when it arrived, so both
    // are refused with the reason, neither runs into the bridge's 30 s
    // `timeout`, and neither is typed later.
    let both = scene.agent_runs(
        "claude",
        "connecto send claude-2 'too early one' > first.txt 2>&1 & sleep 1; \
         connecto send claude-2 'too early two' 2>&1; wait; cat first.txt",
    );
    let text = format!("{}{}", both.out, both.err);
    assert_eq!(
        text.matches("Claude did not show its input box within")
            .count(),
        2,
        "{both:?}"
    );
    assert!(text.contains("loading servers"), "{both:?}");
    assert!(!text.contains("No outcome in 30 s"), "{both:?}");
    thread::sleep(Duration::from_secs(6));
    assert!(scene.prompts("claude-2").is_empty());
}

/// A send past its deadline is never typed, even when the target is ready by
/// the time it would go: its handler has given up. (A limit of zero makes every
/// send past its deadline at once.)
#[cfg(feature = "test-endpoints")]
#[test]
fn an_expired_send_is_not_typed_even_to_a_ready_terminal() {
    let scene = Scene::new("expired");
    scene.wait_for("both ready", |s| {
        s.screen("claude").contains("FAKE-READY") && s.screen("claude-2").contains("FAKE-READY")
    });
    thread::sleep(Duration::from_millis(1500));
    scene.relay.set_hold_limit(Duration::ZERO);
    let sent = scene.connecto("claude", "send claude-2 'too late'");
    assert_ne!(sent.code, 0, "{sent:?}");
    let text = format!("{}{}", sent.out, sent.err);
    assert!(text.contains("did not show its input box"), "{sent:?}");
    thread::sleep(Duration::from_secs(1));
    assert!(scene.prompts("claude-2").is_empty());
    // With the normal limit the same send goes through.
    scene.relay.set_hold_limit(Duration::from_secs(25));
    let sent = scene.connecto("claude", "send claude-2 'in time'");
    assert_eq!(sent.code, 0, "{sent:?}");
    scene.wait_for("the prompt", |s| !s.prompts("claude-2").is_empty());
}

/// The deadline runs from when the bridge made the command, not from when the
/// actor picks it up: time spent in the channel and the backlog counts. A send
/// that is already older than the limit is refused and not typed, a fresh one
/// goes.
#[cfg(feature = "test-endpoints")]
#[test]
fn a_send_that_waited_in_the_queue_past_its_deadline_is_not_typed() {
    use connecto_session::WriteOutcome;
    let scene = Scene::new("aged");
    scene.wait_for("both ready", |s| {
        s.screen("claude").contains("FAKE-READY") && s.screen("claude-2").contains("FAKE-READY")
    });
    thread::sleep(Duration::from_millis(1500));
    let late = scene
        .relay
        .agent_type_aged("claude-2", "stale text", Duration::from_secs(26))
        .unwrap();
    assert!(
        matches!(&late, WriteOutcome::NotReady { why } if why.contains("did not show its input box")),
        "{late:?}"
    );
    thread::sleep(Duration::from_secs(1));
    assert!(scene.prompts("claude-2").is_empty());
    let fresh = scene
        .relay
        .agent_type_aged("claude-2", "fresh text", Duration::from_secs(1))
        .unwrap();
    assert_eq!(fresh, WriteOutcome::Typed);
    scene.wait_for("the prompt", |s| !s.prompts("claude-2").is_empty());
}

// ---- a plain shell terminal

/// A scene with a plain shell added by an agent (`connecto add shell`).
fn scene_with_shell(name: &str) -> Scene {
    // Deterministic: no user dotfiles, no prompt theme.
    connecto_session::launch::set_shell_program(Some("/bin/sh".into()));
    let scene = Scene::new(name);
    let added = scene.connecto("claude", "add shell");
    assert_eq!(added.code, 0, "{added:?}");
    assert!(added.out.contains("shell"), "{added:?}");
    scene.wait_for("the shell to run", |s| {
        s.pane("shell").is_some_and(|pane| pane.is_live()) && !s.screen("shell").trim().is_empty()
    });
    scene
}

/// A plain terminal is listed with its own kind, and its dot says only that it runs.
#[test]
fn a_shell_added_with_the_cli_is_a_plain_terminal() {
    let scene = scene_with_shell("shell-add");
    let view = scene.relay.terminals().view();
    let entry = view.terminals.iter().find(|e| e.id == "shell").unwrap();
    assert_eq!(entry.tool, connecto_core::ToolKind::Shell);
    scene.wait_for("the plain state", |s| {
        s.relay
            .terminals()
            .view()
            .terminals
            .iter()
            .any(|e| e.id == "shell" && e.state == connecto_core::TermActivity::Plain)
    });
    let listed = scene.connecto("claude", "list");
    assert!(listed.out.contains("shell"), "{listed:?}");
}

/// `send` types the line as a command and presses Enter; `read` returns what
/// it printed. No agent framing is typed into a shell.
#[test]
fn a_shell_is_driven_with_send_and_read() {
    let scene = scene_with_shell("shell-send");
    let sent = scene.connecto("claude", "send shell 'echo ran-$((20+22))'");
    assert_eq!(sent.code, 0, "{sent:?}");
    assert!(sent.out.contains("typed"), "{sent:?}");
    scene.wait_for("the command's output", |s| {
        s.screen("shell").contains("ran-42")
    });
    let read = scene.connecto("claude", "read shell");
    assert_eq!(read.code, 0, "{read:?}");
    assert!(read.out.contains("ran-42"), "{read:?}");
    assert!(
        !scene.screen("shell").contains("[connecto from"),
        "a shell is not given the agent framing:\n{}",
        scene.screen("shell")
    );
    // Gate and audit: the write is an agent write to the shell, on the record.
    let audit = scene.audit();
    assert!(
        audit.contains(r#""action":"type""#) && audit.contains(r#""source":"claude""#),
        "{audit}"
    );
}

/// A shell starts in the project with the small environment agents get, its own
/// identity on the bridge, and the `connecto` command on `PATH`; it is given no
/// hook channel and no secrets from the app's environment.
#[test]
fn a_shell_starts_in_the_project_with_the_connecto_command_and_no_hooks() {
    let scene = scene_with_shell("shell-env");
    let sent = scene.connecto(
        "claude",
        "send shell 'echo dir=$PWD term=$CONNECTO_TERMINAL sock=${CONNECTO_BRIDGE_SOCK:+set} hook=${CONNECTO_HOOK_TOKEN-none} key=${ANTHROPIC_API_KEY-none} cmd=$(command -v connecto | sed s,.*/cli/,cli/,)'",
    );
    assert_eq!(sent.code, 0, "{sent:?}");
    let project = scene.project.display().to_string();
    // The terminal wraps long lines; joined again they read as printed.
    let flat = |s: &Scene| s.screen("shell").replace('\n', "");
    scene.wait_for("the environment line", |s| {
        flat(s).contains(&format!("dir={project} term="))
    });
    let screen = flat(&scene);
    for expected in [
        "term=shell",
        "sock=set",
        "hook=none",
        "key=none",
        "cmd=cli/bin/connecto",
    ] {
        assert!(screen.contains(expected), "{expected} missing:\n{screen}");
    }
}

/// A shell has no turns: `--ask` and `watch` are refused and say what to do
/// instead, and nothing is typed.
#[test]
fn ask_and_watch_to_a_shell_are_refused_and_name_send_and_read() {
    let scene = scene_with_shell("shell-noask");
    let asked = scene.connecto("claude", "send shell --ask 'echo SHOULD-NOT-RUN-77'");
    assert_ne!(asked.code, 0, "{asked:?}");
    assert!(
        asked.err.contains("no turns") && asked.err.contains("connecto read shell"),
        "{asked:?}"
    );
    let watched = scene.connecto("claude", "watch shell");
    assert_ne!(watched.code, 0, "{watched:?}");
    assert!(watched.err.contains("no turns"), "{watched:?}");
    thread::sleep(Duration::from_millis(800));
    assert!(!scene.screen("shell").contains("SHOULD-NOT-RUN-77"));
}

/// One command line per send, no control characters: the refusal says how to
/// send the rest, and nothing is typed.
#[test]
fn a_shell_takes_one_plain_command_line_per_send() {
    let scene = scene_with_shell("shell-lines");
    let many = scene.connecto(
        "claude",
        "send shell \"$(printf 'echo LINE-ONE-91\\necho LINE-TWO-91')\"",
    );
    assert_ne!(many.code, 0, "{many:?}");
    assert!(many.err.contains("one command line"), "{many:?}");
    let control = scene.connecto("claude", "send shell \"$(printf 'echo CTRL-92\\003')\"");
    assert_ne!(control.code, 0, "{control:?}");
    assert!(control.err.contains("control characters"), "{control:?}");
    thread::sleep(Duration::from_millis(800));
    let screen = scene.screen("shell");
    assert!(
        !screen.contains("LINE-ONE-91") && !screen.contains("CTRL-92"),
        "{screen}"
    );
    // A trailing newline is only the line's end.
    let one = scene.connecto("claude", "send shell \"$(printf 'echo ONE-93\\n')\"");
    assert_eq!(one.code, 0, "{one:?}");
    scene.wait_for("the line", |s| s.screen("shell").contains("ONE-93"));
}

/// Pause blocks every write to a shell, as it does to an agent.
#[test]
fn pause_blocks_writes_to_a_shell() {
    let scene = scene_with_shell("shell-pause");
    scene.relay.pause_switch().pause();
    for args in ["send shell 'echo PAUSED-94'", "keys shell enter"] {
        let run = scene.connecto("claude", args);
        assert_ne!(run.code, 0, "{args}: {run:?}");
        assert!(run.err.contains("paused"), "{args}: {run:?}");
    }
    thread::sleep(Duration::from_millis(500));
    assert!(!scene.screen("shell").contains("PAUSED-94"));
}

/// Keys go through to a shell (Ctrl-C ends a running command), and a restart
/// brings a fresh shell that takes commands again.
#[test]
fn keys_and_a_restart_work_on_a_shell() {
    let scene = scene_with_shell("shell-keys");
    assert_eq!(scene.connecto("claude", "send shell 'sleep 60'").code, 0);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(scene.connecto("claude", "keys shell ctrl-c").code, 0);
    assert_eq!(
        scene
            .connecto("claude", "send shell 'echo AFTER-CTRL-C-95'")
            .code,
        0
    );
    scene.wait_for("the shell to answer after Ctrl-C", |s| {
        s.screen("shell").contains("AFTER-CTRL-C-95")
    });
    // A restart is for a shell that has ended (the user typed `exit`).
    let before = scene.pane("shell").unwrap().attempt();
    assert_eq!(scene.connecto("claude", "send shell 'exit'").code, 0);
    scene.wait_for("the shell to end", |s| {
        s.pane("shell").is_some_and(|pane| !pane.is_live())
    });
    scene.relay.terminals().restart("shell").unwrap();
    scene.wait_for("a new shell", |s| {
        s.pane("shell")
            .is_some_and(|pane| pane.attempt() > before && pane.is_live())
    });
    scene.wait_for("the new shell's prompt", |s| {
        !s.screen("shell").trim().is_empty()
    });
    assert_eq!(
        scene
            .connecto("claude", "send shell 'echo AFTER-RESTART-96'")
            .code,
        0
    );
    scene.wait_for("the restarted shell to answer", |s| {
        s.screen("shell").contains("AFTER-RESTART-96")
    });
}

// ---- the terminal's name is also the CLI's session name

/// A name given to a new terminal reaches Claude as its session name at launch.
#[test]
fn a_new_terminal_launches_claude_with_its_name() {
    let scene = Scene::new("name-launch");
    let added = scene.connecto("claude", "add claude --name 'review the diff'");
    assert_eq!(added.code, 0, "{added:?}");
    scene.wait_for("claude-3 to start with its arguments", |s| {
        s.log("args-claude-3.txt").contains("--name")
    });
    let args = scene.log("args-claude-3.txt");
    let lines: Vec<&str> = args.lines().collect();
    let at = lines.iter().position(|l| *l == "--name").unwrap();
    assert_eq!(lines[at + 1], "review the diff", "{args}");
}

/// A rename of a running Claude is told to it with `/rename`, once it sits at a
/// quiet prompt; the last name wins; the id never changes.
#[test]
fn a_rename_is_told_to_an_idle_claude_and_the_last_name_wins() {
    let scene = Scene::new("name-idle");
    let terminals = scene.relay.terminals();
    terminals
        .rename("claude-2", Some("first name".into()))
        .unwrap();
    terminals
        .rename("claude-2", Some("second name".into()))
        .unwrap();
    scene.wait_for("the /rename", |s| {
        s.prompts("claude-2")
            .iter()
            .any(|p| p.starts_with("/rename"))
    });
    let renames: Vec<String> = scene
        .prompts("claude-2")
        .into_iter()
        .filter(|p| p.starts_with("/rename"))
        .collect();
    assert_eq!(renames, ["/rename second name"]);
    assert!(scene.ids().contains(&"claude-2".to_owned()));
    // It was a write of Connecto's own, through the Gate and into the audit log.
    let audit = scene.audit();
    assert!(
        audit.contains(r#""action":"wake""#) && audit.contains(r#""source":"connecto""#),
        "{audit}"
    );
}

/// Codex has `/rename <name>` as well: told the same way, the last name wins.
#[test]
fn a_rename_is_told_to_an_idle_codex_and_the_last_name_wins() {
    let scene = Scene::mixed("name-codex");
    scene.wait_for("a running Codex", |s| {
        s.screen("codex").contains("FAKE-READY")
    });
    let terminals = scene.relay.terminals();
    terminals
        .rename("codex", Some("first name".into()))
        .unwrap();
    terminals
        .rename("codex", Some("second name".into()))
        .unwrap();
    scene.wait_for("the /rename", |s| {
        s.prompts("codex").iter().any(|p| p.starts_with("/rename"))
    });
    let renames: Vec<String> = scene
        .prompts("codex")
        .into_iter()
        .filter(|p| p.starts_with("/rename"))
        .collect();
    assert_eq!(renames, ["/rename second name"]);
    let audit = scene.audit();
    assert!(
        audit.contains(r#""action":"wake""#) && audit.contains(r#""source":"connecto""#),
        "{audit}"
    );
}

/// In a turn, nothing is typed; the name goes in when the turn is over.
#[test]
fn a_rename_waits_for_the_end_of_a_turn() {
    let scene = Scene::new("name-busy");
    let sent = scene.connecto("claude", "send claude-2 --ask 'SLEEP 6 slowly'");
    assert_eq!(sent.code, 0, "{sent:?}");
    scene.wait_for("claude-2 to take the prompt", |s| {
        !s.prompts("claude-2").is_empty()
    });
    scene
        .relay
        .terminals()
        .rename("claude-2", Some("busy name".into()))
        .unwrap();
    thread::sleep(Duration::from_secs(3));
    assert!(
        !scene
            .prompts("claude-2")
            .iter()
            .any(|p| p.starts_with("/rename")),
        "nothing is typed into a turn: {:?}",
        scene.prompts("claude-2")
    );
    scene.wait_for("the /rename after the turn", |s| {
        s.prompts("claude-2")
            .iter()
            .any(|p| p == "/rename busy name")
    });
}

/// A plain shell is never told a name.
#[test]
fn a_shell_is_never_typed_a_rename() {
    let scene = scene_with_shell("name-shell");
    scene
        .relay
        .terminals()
        .rename("shell", Some("my shell".into()))
        .unwrap();
    thread::sleep(Duration::from_secs(4));
    assert!(
        !scene.screen("shell").contains("/rename"),
        "{}",
        scene.screen("shell")
    );
    let view = scene.relay.terminals().view();
    let entry = view.terminals.iter().find(|e| e.id == "shell").unwrap();
    assert_eq!(entry.name.as_deref(), Some("my shell"));
}

// ---- project memory (spec §6, §6.1)

/// A memory engine stand-in, on for the projects listed, set once for this
/// test binary. Every other scene's project stays off, so their launch texts
/// and drains are as before.
#[derive(Default)]
struct FakeMemory {
    on: std::sync::Mutex<std::collections::HashSet<String>>,
    versions: std::sync::Mutex<std::collections::HashMap<(String, String), u64>>,
    #[allow(clippy::type_complexity)]
    asked: std::sync::Mutex<
        Vec<(
            String,
            connecto_core::memory::MemoryCaller,
            String,
            Option<String>,
        )>,
    >,
}

impl connecto_core::memory::MemoryBridge for FakeMemory {
    fn command(
        &self,
        project_id: &str,
        caller: &connecto_core::memory::MemoryCaller,
        command: &connecto_core::memory::MemoryCommand,
        text: Option<&str>,
    ) -> String {
        self.asked.lock().unwrap().push((
            project_id.to_owned(),
            caller.clone(),
            command.verb().word().to_owned(),
            text.map(str::to_owned),
        ));
        format!("FAKE PAGE for {}", command.verb().word())
    }

    fn enabled(&self, project_id: &str) -> bool {
        self.on.lock().unwrap().contains(project_id)
    }

    fn take_version_notice(&self, project_id: &str, terminal: &str) -> Option<u64> {
        self.versions
            .lock()
            .unwrap()
            .remove(&(project_id.to_owned(), terminal.to_owned()))
    }
}

fn fake_memory() -> std::sync::Arc<FakeMemory> {
    static FAKE: std::sync::OnceLock<std::sync::Arc<FakeMemory>> = std::sync::OnceLock::new();
    std::sync::Arc::clone(FAKE.get_or_init(|| {
        let fake = std::sync::Arc::new(FakeMemory::default());
        connecto_session::set_memory_bridge(std::sync::Arc::clone(&fake) as _);
        fake
    }))
}

/// The id a scene's project will have, before the scene is built.
fn scene_project_id(name: &str) -> String {
    let tmp = fs::canonicalize(std::env::temp_dir()).unwrap();
    let project = tmp
        .join(format!("mg-bridge-{name}-{}", std::process::id()))
        .join("home/project");
    connecto_session::launch::project_id(&project)
}

#[test]
fn memory_commands_reach_the_engine_as_the_bridge_names_the_caller() {
    let fake = fake_memory();
    let project_id = scene_project_id("memory");
    fake.on.lock().unwrap().insert(project_id.clone());
    let scene = Scene::new("memory");
    assert_eq!(
        connecto_session::launch::project_id(&fs::canonicalize(&scene.project).unwrap()),
        project_id
    );
    fs::create_dir_all(scene.project.join(".git")).unwrap();
    fs::write(
        scene.project.join(".git/HEAD"),
        "ref: refs/heads/feature-x\n",
    )
    .unwrap();
    // The launch text carries the fixed memory section, and nothing else of it.
    let args = scene.log("args-claude.txt");
    assert!(
        args.contains("BEFORE YOU START ANY WORK, run `connecto memory brief`"),
        "{args}"
    );
    assert!(!args.contains("FAKE PAGE"));

    let run = scene.connecto("claude", "memory brief --budget 300");
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(run.out, "FAKE PAGE for brief\n");
    // The terminal named in the environment is not believed.
    let run = scene.agent_runs(
        "claude",
        "CONNECTO_TERMINAL=claude-2 connecto memory status",
    );
    assert_eq!(run.code, 0, "{run:?}");
    let run = scene.agent_runs(
        "claude-2",
        "printf 'Next: ship it.' | connecto memory propose --type task",
    );
    assert_eq!(run.out, "FAKE PAGE for propose\n", "{run:?}");
    // A usage mistake is refused by the bridge, before the engine.
    let run = scene.connecto("claude", "memory brief --budget 1");
    assert_eq!(run.code, 1, "{run:?}");
    assert!(run.err.contains("--budget must be a number"), "{run:?}");
    let asked = fake.asked.lock().unwrap().clone();
    let verbs: Vec<&str> = asked.iter().map(|(_, _, verb, _)| verb.as_str()).collect();
    assert_eq!(verbs, ["brief", "status", "propose"]);
    for (project, caller, _, _) in &asked {
        assert_eq!(project, &project_id);
        assert_eq!(caller.branch.as_deref(), Some("feature-x"));
        assert!(caller.tool_session.is_some(), "{caller:?}");
    }
    assert_eq!(asked[0].1.terminal, "claude");
    assert_eq!(asked[1].1.terminal, "claude", "from the process tree");
    assert_eq!(asked[2].1.terminal, "claude-2");
    assert_eq!(asked[2].3.as_deref(), Some("Next: ship it."));

    // A new version waits for claude's next prompt: nothing is typed into it.
    let before = scene.prompts("claude").len();
    fake.versions
        .lock()
        .unwrap()
        .insert((project_id.clone(), "claude".to_owned()), 7);
    thread::sleep(Duration::from_secs(1));
    assert_eq!(scene.prompts("claude").len(), before, "nobody was woken");
    scene.user_types("claude", "hello");
    scene.wait_for("the next prompt", |s| s.prompts("claude").len() > before);
    let n = scene.prompts("claude").len();
    scene.wait_for("its context", |s| !context_of(s, "claude", n).is_empty());
    let context = context_of(&scene, "claude", n);
    assert_eq!(
        context
            .matches("[connecto notice] project memory updated to version 7")
            .count(),
        1,
        "{context}"
    );
    assert!(notices(&scene, "claude").is_empty());
}

// ---- the user's draft: Connecto types into a terminal the user is typing in

impl Scene {
    /// Two Claude terminals whose stand-ins edit their input line as a TUI
    /// does: typed characters, Backspace, Left and Right.
    fn composing(name: &str) -> Self {
        Self::build_full(
            name,
            vec![Tool::Claude; 2],
            "2.1.318",
            false,
            SinkMode::Plain,
            &[("composer-claude", ""), ("composer-claude-2", "")],
            true,
            None,
        )
    }

    /// The user presses keys in a terminal, through its pane, as the window
    /// sends them: one write per key.
    fn user_keys(&self, id: &str, bytes: &[u8]) {
        self.pane(id).unwrap().send_input(bytes.to_vec()).unwrap();
    }

    fn user_text(&self, id: &str, text: &str) {
        for ch in text.chars() {
            self.user_keys(id, ch.to_string().as_bytes());
        }
    }

    /// Connecto types `text` into `id`, as a `connecto send` would.
    fn inject(&self, id: &str, text: &str) -> connecto_session::WriteOutcome {
        self.relay.agent_type_raw(id, text, None).unwrap()
    }

    fn input_given_back(&self, id: &str) {
        self.wait_for("the user's input given back", |s| {
            !s.pane(id).unwrap().injecting()
        });
    }
}

const TYPED: connecto_session::WriteOutcome = connecto_session::WriteOutcome::Typed;

/// The states a terminal shows over `secs`, each change once, sampled as the
/// app's list sees them.
fn states_over(scene: &Scene, id: &str, secs: u64) -> Vec<connecto_core::TermActivity> {
    let mut seen: Vec<connecto_core::TermActivity> = Vec::new();
    let until = Instant::now() + common::scaled(Duration::from_secs(secs));
    while Instant::now() < until {
        if let Some(state) = scene.state_of(id)
            && seen.last() != Some(&state)
        {
            seen.push(state);
        }
        thread::sleep(Duration::from_millis(20));
    }
    seen
}

/// Review round 7, finding 1, through the relay's real log polling: after a
/// turn's `Stop`, another `Stop` hook keeps it going. With nothing open the
/// log is still read after a `Stop`, so the work it shows a second later is
/// seen and the terminal reads `working` again until the turn's next `Stop`.
#[test]
fn a_turn_kept_going_after_its_stop_reads_working_through_the_relay() {
    use connecto_core::TermActivity;
    let scene = Scene::new("keptgoing");
    // The turn itself runs a second, so its own `working` is seen too.
    scene.user_types("claude-2", "SLEEP 1 CONTINUE x");
    let states = states_over(&scene, "claude-2", 10);
    let working = |s: &TermActivity| *s == TermActivity::Working;
    let first = states.iter().position(working);
    let resumed = first.and_then(|at| {
        let idle = at + states[at..].iter().position(|s| !working(s))?;
        Some(idle + states[idle..].iter().position(working)?)
    });
    assert!(
        resumed.is_some(),
        "never working again after its Stop: {states:?}"
    );
    assert!(
        !working(states.last().unwrap()),
        "settled at the end: {states:?}"
    );
}

/// The real API-error lifecycle through the relay: an error record and
/// `StopFailure`, never a `Stop`. The ask ends `unavailable`, not with the
/// error text as a reply, and the terminal settles.
#[test]
fn an_api_error_turn_ends_its_ask_unavailable_through_the_relay() {
    let scene = Scene::new("apifail");
    let sent = scene.connecto("claude", "send claude-2 --ask 'APIFAIL now'");
    assert_eq!(sent.code, 0, "{sent:?}");
    // The asker is woken when the ask ends; then the result is read once.
    scene.wait_for("the ask to end", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.contains("Request r1 ended"))
            || (1..=9).any(|n| {
                s.log(&format!("context-claude-{n}.txt"))
                    .contains("Request r1 ended")
            })
    });
    let result = scene.connecto("claude", "result r1");
    assert!(result.out.contains("unavailable"), "{result:?}");
    assert!(!result.out.contains("API Error"), "{result:?}");
    scene.wait_for("claude-2 to settle", |s| {
        s.state_of("claude-2") != Some(connecto_core::TermActivity::Working)
    });
}

/// `StopFailure` with its error record written 3 s later and a message
/// queued into the failed turn: the ask ends `unavailable` at once, the late
/// record, read by the relay's polling, settles the queued one, and nothing
/// stays `working`.
#[test]
fn a_failed_turn_with_a_late_error_record_settles_through_the_relay() {
    let scene = Scene::new("faillate");
    let sent = scene.connecto("claude", "send claude-2 --ask 'APIFAIL LATEEND MERGE now'");
    assert_eq!(sent.code, 0, "{sent:?}");
    // The asker is woken when the ask ends; then the result is read once.
    scene.wait_for("the ask to end", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.contains("Request r1 ended"))
            || (1..=9).any(|n| {
                s.log(&format!("context-claude-{n}.txt"))
                    .contains("Request r1 ended")
            })
    });
    let result = scene.connecto("claude", "result r1");
    assert!(result.out.contains("unavailable"), "{result:?}");
    let states = states_over(&scene, "claude-2", 6);
    assert_ne!(
        states.last(),
        Some(&connecto_core::TermActivity::Working),
        "{states:?}"
    );
}

/// An end record written 3 s after the `Stop` (past the wait), with a
/// message queued into the turn: the `Stop` closes the ask by count, the
/// late end record, read by the relay's polling, settles the queued one,
/// and nothing stays `working`.
#[test]
fn a_late_end_record_settles_the_turn_through_the_relay() {
    let scene = Scene::new("lateend");
    let sent = scene.connecto("claude", "send claude-2 --ask 'LATEEND MERGE now'");
    assert_eq!(sent.code, 0, "{sent:?}");
    // The asker is woken when the ask ends; then the result is read once.
    scene.wait_for("the ask to end", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.contains("Request r1 ended"))
            || (1..=9).any(|n| {
                s.log(&format!("context-claude-{n}.txt"))
                    .contains("Request r1 ended")
            })
    });
    // The ask is closed by count: its reply is the turn's.
    let result = scene.connecto("claude", "result r1");
    assert!(result.out.contains("echo: LATEEND MERGE now"), "{result:?}");
    let states = states_over(&scene, "claude-2", 6);
    assert_ne!(
        states.last(),
        Some(&connecto_core::TermActivity::Working),
        "{states:?}"
    );
}

/// The stuck-wake bug, its typing half. A turn that starts by itself between
/// a wake's paste and its Enter (a background task's notification) would take
/// the notice as a message queued into that turn. The wake is taken back off
/// the input line instead, and its notice wakes the terminal once that turn
/// has ended.
#[test]
fn a_wake_is_taken_back_when_a_turn_starts_before_its_enter() {
    let scene = Scene::build_full(
        "wakerace",
        vec![Tool::Claude; 2],
        "2.1.318",
        false,
        SinkMode::Plain,
        &[
            ("composer-claude", ""),
            ("composer-claude-2", ""),
            ("selfstart-claude", ""),
        ],
        true,
        None,
    );
    assert_eq!(scene.connecto("claude", "watch claude-2").code, 0);
    thread::sleep(Duration::from_millis(800));
    scene.user_types("claude-2", "SLEEP 4 work");
    scene.wait_for("the turn that started by itself", |s| {
        s.prompts("claude")
            .iter()
            .any(|p| p.starts_with("<task-notification>"))
    });
    scene.wait_for("a wake after that turn ended", |s| {
        s.prompts("claude")
            .iter()
            .skip_while(|p| !p.starts_with("<task-notification>"))
            .any(|p| p.starts_with("[connecto notice]"))
    });
    assert_eq!(
        scene.log("midturn-claude.log"),
        "",
        "nothing was entered into the running turn"
    );
    for prompt in scene.prompts("claude") {
        assert_eq!(
            prompt.matches("[connecto notice]").count(),
            prompt.starts_with("[connecto notice]") as usize,
            "no leftover of the withdrawn text: {prompt:?}"
        );
    }
}

/// An empty input line is typed into as always. A draft is taken out, the
/// message is entered alone, and the draft goes back unsent with the cursor
/// where it was.
#[test]
fn a_message_moves_the_users_draft_aside_and_puts_it_back() {
    let scene = Scene::composing("draftaside");
    assert_eq!(scene.inject("claude-2", "MSG empty"), TYPED);
    scene.wait_for("the first message", |s| {
        s.prompts("claude-2") == ["MSG empty"]
    });
    scene.input_given_back("claude-2");
    scene.user_text("claude-2", "hello world");
    for _ in 0..5 {
        scene.user_keys("claude-2", b"\x1b[D");
    }
    assert_eq!(scene.inject("claude-2", "MSG draft"), TYPED);
    scene.wait_for("the message", |s| s.prompts("claude-2").len() == 2);
    assert_eq!(scene.prompts("claude-2")[1], "MSG draft");
    scene.input_given_back("claude-2");
    // Back, unsent, with the cursor before "world".
    scene.user_keys("claude-2", b"X");
    scene.user_keys("claude-2", b"\r");
    scene.wait_for("the draft", |s| s.prompts("claude-2").len() == 3);
    assert_eq!(scene.prompts("claude-2")[2], "hello Xworld");
}

/// A draft of several lines comes back whole after several messages that
/// arrive together, and each message is entered alone.
#[test]
fn a_multi_line_draft_survives_several_messages_in_a_row() {
    let scene = Scene::composing("draftrow");
    let modes = InputModes {
        bracketed_paste: true,
        ..InputModes::default()
    };
    scene.user_keys("claude-2", &encode_paste("line one\nline two", modes));
    scene.user_keys("claude-2", b"\x1b\r");
    scene.user_text("claude-2", "line three");
    thread::scope(|scope| {
        let sends: Vec<_> = (0..3)
            .map(|n| {
                let scene = &scene;
                scope.spawn(move || scene.inject("claude-2", &format!("MSG {n}")))
            })
            .collect();
        for send in sends {
            assert_eq!(send.join().unwrap(), TYPED);
        }
    });
    scene.wait_for("three messages", |s| s.prompts("claude-2").len() == 3);
    let mut prompts = scene.prompts("claude-2");
    prompts.sort();
    assert_eq!(prompts, ["MSG 0", "MSG 1", "MSG 2"]);
    scene.input_given_back("claude-2");
    scene.user_keys("claude-2", b"\r");
    scene.wait_for("the draft", |s| s.prompts("claude-2").len() == 4);
    assert_eq!(
        scene.prompts("claude-2")[3],
        "line one\nline two\nline three"
    );
}

/// Keys the user types while Connecto types are held and go in after, at the
/// end of the draft put back. A busy terminal gets the message at once, as
/// before, and the draft typed during the turn is kept the same way.
#[test]
fn keys_typed_meanwhile_join_the_draft_and_a_busy_terminal_is_typed_into_at_once() {
    let scene = Scene::composing("draftkeys");
    scene.user_text("claude-2", "draft");
    thread::scope(|scope| {
        let send = scope.spawn(|| scene.inject("claude-2", "MSG during"));
        scene.wait_for("Connecto to type", |s| {
            s.pane("claude-2").unwrap().injecting()
        });
        scene.user_text("claude-2", " more");
        assert_eq!(send.join().unwrap(), TYPED);
    });
    scene.wait_for("the message", |s| !s.prompts("claude-2").is_empty());
    assert_eq!(scene.prompts("claude-2"), ["MSG during"]);
    scene.input_given_back("claude-2");
    scene.user_keys("claude-2", b"\r");
    scene.wait_for("the draft", |s| s.prompts("claude-2").len() == 2);
    assert_eq!(scene.prompts("claude-2")[1], "draft more");

    // In a turn: the tool reads its input only when the turn ends.
    scene.user_types("claude-2", "SLEEP 3");
    scene.wait_for("the turn", |s| s.prompts("claude-2").len() == 3);
    scene.user_text("claude-2", "later");
    let sent = Instant::now();
    assert_eq!(scene.inject("claude-2", "MSG busy"), TYPED);
    assert!(
        sent.elapsed() < Duration::from_secs(3),
        "the send waited for the turn"
    );
    scene.input_given_back("claude-2");
    scene.wait_for("the message", |s| s.prompts("claude-2").len() == 4);
    assert_eq!(scene.prompts("claude-2")[3], "MSG busy");
    scene.user_keys("claude-2", b"\r");
    scene.wait_for("the draft", |s| s.prompts("claude-2").len() == 5);
    assert_eq!(scene.prompts("claude-2")[4], "later");
}

/// A draft Connecto cannot follow (here, after a Tab, which may complete it) is
/// never typed into. The sender is told at once that the message waits; it
/// waits past the usual 25 s limit, for the draft limit, and goes in once the
/// user sends the line, or is refused at that limit, never typed.
#[cfg(feature = "test-endpoints")]
#[test]
fn a_draft_that_cannot_be_followed_holds_the_message_until_the_user_sends_it() {
    let scene = Scene::composing("draftheld");
    // The two limits are separate: the short one must not end the wait.
    scene.relay.set_hold_limit(Duration::from_secs(1));
    scene.relay.set_draft_limit(Duration::from_secs(30));
    scene.user_text("claude-2", "mine");
    scene.user_keys("claude-2", b"\t");
    let began = Instant::now();
    match scene.inject("claude-2", "MSG held") {
        connecto_session::WriteOutcome::Waiting { note } => {
            assert!(note.contains("send or clear their line"), "{note}");
            assert!(note.contains("30 s"), "{note}");
        }
        other => panic!("not told it waits: {other:?}"),
    }
    assert!(began.elapsed() < Duration::from_secs(5));
    scene.wait_for("the pane to say a message waits", |s| {
        s.pane("claude-2").unwrap().message_waiting()
    });
    thread::sleep(Duration::from_secs(3));
    assert!(scene.prompts("claude-2").is_empty());
    scene.user_keys("claude-2", b"\r");
    scene.wait_for("both", |s| s.prompts("claude-2").len() == 2);
    assert_eq!(scene.prompts("claude-2"), ["mine", "MSG held"]);
    scene.wait_for("the hint to go", |s| {
        !s.pane("claude-2").unwrap().message_waiting()
    });

    scene.input_given_back("claude-2");
    scene.relay.set_draft_limit(Duration::from_secs(2));
    scene.user_text("claude-2", "again");
    scene.user_keys("claude-2", b"\t");
    assert!(matches!(
        scene.inject("claude-2", "MSG late"),
        connecto_session::WriteOutcome::Waiting { .. }
    ));
    scene.wait_for("the hint", |s| {
        s.pane("claude-2").unwrap().message_waiting()
    });
    scene.wait_for("the wait to end", |s| {
        !s.pane("claude-2").unwrap().message_waiting()
    });
    scene.user_keys("claude-2", b"\r");
    scene.wait_for("the draft", |s| s.prompts("claude-2").len() == 3);
    thread::sleep(Duration::from_secs(1));
    assert_eq!(scene.prompts("claude-2"), ["mine", "MSG held", "again"]);
}

/// What an agent's `connecto send --ask` sees while the target's user is
/// typing: an answer at once that the message waits and why, no timeout, no
/// "did not start a turn" while it waits, and the usual end notice after.
#[cfg(feature = "test-endpoints")]
#[test]
fn an_ask_to_a_terminal_the_user_is_typing_in_is_told_it_waits() {
    let scene = Scene::composing("draftask");
    scene.relay.set_hold_limit(Duration::from_secs(1));
    scene.relay.set_draft_limit(Duration::from_secs(60));
    scene.user_text("claude-2", "mine");
    scene.user_keys("claude-2", b"\t");
    let began = Instant::now();
    let sent = scene.connecto("claude", "send claude-2 --ask 'MSG asked'");
    assert_eq!(sent.code, 0, "{sent:?}");
    assert!(sent.out.contains("waiting"), "{sent:?}");
    assert!(sent.out.contains("send or clear their line"), "{sent:?}");
    assert!(began.elapsed() < Duration::from_secs(20), "{sent:?}");
    let id = sent
        .out
        .lines()
        .find(|line| line.contains(" waiting"))
        .and_then(|line| line.split_whitespace().next())
        .unwrap()
        .to_owned();
    // Past the 15 s in which a typed text must start a turn: still pending.
    thread::sleep(Duration::from_secs(17));
    let result = scene.connecto("claude", &format!("result {id}"));
    assert!(result.out.contains("pending"), "{result:?}");
    assert!(notices(&scene, "claude").is_empty());
    scene.user_keys("claude-2", b"\r");
    scene.wait_for("the message", |s| {
        s.prompts("claude-2")
            .iter()
            .any(|p| p.ends_with("MSG asked"))
    });
    scene.wait_for("the end notice", |s| !notices(s, "claude").is_empty());
    let notice = notices(&scene, "claude").remove(0);
    assert!(notice.contains(&id), "{notice}");
    assert!(!notice.contains("not typed"), "{notice}");
}

/// A request's end notice is typed into the sender the same way: the draft the
/// user started meanwhile is not sent with it, and comes back after.
#[test]
fn a_request_notice_leaves_the_senders_draft_unsent() {
    let scene = Scene::composing("draftnotice");
    let sent = scene.connecto("claude", "send claude-2 --ask 'SLEEP 3 slowly'");
    assert_eq!(sent.code, 0, "{sent:?}");
    scene.wait_for("claude-2 to begin", |s| !s.prompts("claude-2").is_empty());
    scene.user_text("claude", "half a thought");
    scene.wait_for("the end of the request", |s| {
        !notices(s, "claude").is_empty()
    });
    let notice = notices(&scene, "claude").remove(0);
    assert!(!notice.contains("half a thought"), "{notice}");
    scene.input_given_back("claude");
    let before = scene.prompts("claude").len();
    scene.user_keys("claude", b"\r");
    scene.wait_for("the draft", |s| s.prompts("claude").len() == before + 1);
    assert_eq!(scene.prompts("claude").pop().unwrap(), "half a thought");
}

/// ADR 0035: two projects in one Connecto. Without `--project` a command
/// stays in the caller's own project; with it, an agent lists, reads and asks
/// in the other one, and the reply comes back to it as for an ask at home.
/// Everything but talking and reading is refused across projects, so are
/// terminals another agent started, and a Pause in either project refuses;
/// both projects' audits record what crossed. One test, so it never holds two
/// of the scene permits while another test waits for its second.
#[test]
fn an_agent_talks_to_another_project_only_when_it_names_it() {
    use connecto_session::{ProjectDirectory, ProjectEntry};
    let alpha = Scene::new("cross-alpha");
    let beta = Scene::new("cross-beta");
    // A third project whose terminals have the same ids, slots and attempts
    // as alpha's: an answer must still reach the project that asked.
    let gamma = Scene::new("cross-gamma");
    let entry = |scene: &Scene, name: &str| ProjectEntry {
        id: connecto_session::launch::project_id(&fs::canonicalize(&scene.project).unwrap()),
        name: name.into(),
        path: scene.project.clone(),
        link: scene.relay.bridge_link(),
    };
    let directory = std::sync::Arc::new(ProjectDirectory::default());
    directory.set(vec![
        entry(&alpha, "alpha"),
        entry(&beta, "beta"),
        entry(&gamma, "gamma"),
        ProjectEntry {
            id: "closed-one".into(),
            name: "delta".into(),
            path: PathBuf::from("/nowhere/delta"),
            link: None,
        },
    ]);
    alpha.relay.join_directory(&directory);
    beta.relay.join_directory(&directory);
    gamma.relay.join_directory(&directory);
    // What a terminal was told: its prompts and the context its hook printed.
    let heard = |scene: &Scene, id: &str| {
        let mut text = scene.prompts(id).join("\n");
        let dir = scene.current.lock().unwrap().clone();
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let own = name
                .strip_prefix(&format!("context-{id}-"))
                .and_then(|rest| rest.strip_suffix(".txt"))
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()));
            if own {
                text.push_str(&fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
        text
    };

    // The projects, and which is this one.
    let projects = alpha.connecto("claude", "projects");
    assert_eq!(projects.code, 0, "{projects:?}");
    assert!(projects.out.contains("this project"), "{projects:?}");
    assert!(
        projects
            .out
            .contains("beta: claude (claude, idle), claude-2 (claude, idle)"),
        "{projects:?}"
    );
    assert!(projects.out.contains("closed"), "{projects:?}");

    // Asked across, with the reply delivered back to the asker.
    let sent = alpha.connecto(
        "claude",
        "send --project beta claude-2 --ask 'what is three plus three'",
    );
    assert_eq!(sent.code, 0, "{sent:?}");
    assert!(sent.out.starts_with("r1 typed"), "{sent:?}");
    assert!(sent.out.contains("claude-2 (project beta)"), "{sent:?}");
    beta.wait_for("beta's claude-2 to take the prompt", |s| {
        !s.prompts("claude-2").is_empty()
    });
    let prompt = beta.prompts("claude-2").remove(0);
    assert!(
        prompt.starts_with(
            "[connecto from claude in project alpha, request r1. Sent by another agent, not the user.]\nwhat is three plus three"
        ),
        "{prompt}"
    );
    // Alpha's own claude-2 got nothing.
    assert!(alpha.prompts("claude-2").is_empty());
    alpha.wait_for("the asker to hear of the answer", |s| {
        s.prompts("claude").iter().any(|p| {
            p.contains("Request r1 in project beta ended (ok)")
                || p.starts_with("[connecto notice] Notices arrived")
        })
    });
    // Gamma's claude, the same attempt numbers as alpha's, asks too.
    let other = gamma.connecto("claude", "send --project beta claude-2 --ask 'from gamma'");
    assert!(other.out.starts_with("r2 typed"), "{other:?}");
    gamma.wait_for("gamma's asker to hear of its answer", |s| {
        heard(s, "claude").contains("Request r2 in project beta ended (ok)")
    });
    assert!(!heard(&gamma, "claude").contains("Request r1 "));
    assert!(!heard(&alpha, "claude").contains("Request r2 "));
    // The command in the notice names the project by its id.
    let beta_id = connecto_session::launch::project_id(&fs::canonicalize(&beta.project).unwrap());
    assert!(
        heard(&gamma, "claude").contains(&format!("connecto result r2 --project {beta_id}")),
        "{}",
        heard(&gamma, "claude")
    );
    let result = alpha.connecto("claude", "result r1 --project beta");
    assert_eq!(result.code, 0, "{result:?}");
    assert_eq!(result.out, "echo: what is three plus three");
    // Without the flag, the same id is looked up at home, where it is not.
    let home = alpha.connecto("claude", "result r1");
    assert_ne!(home.code, 0, "{home:?}");
    assert!(home.err.contains("unknown_request"), "{home:?}");

    // Read and list across; list without the flag stays at home.
    let listed = alpha.connecto("claude", "list --project beta");
    assert_eq!(listed.code, 0, "{listed:?}");
    assert!(
        listed.out.starts_with("Terminals of project beta:"),
        "{listed:?}"
    );
    assert!(!listed.out.contains("self"), "{listed:?}");
    let own = alpha.connecto("claude", "list");
    assert!(own.out.contains("self"), "{own:?}");
    let read = alpha.connecto("claude", "read claude-2 --project beta");
    assert_eq!(read.code, 0, "{read:?}");
    assert!(read.out.contains("three plus three"), "{read:?}");
    // A relative path is read from the folder the command runs in (alpha's
    // project), not from the app's: it names beta.
    let beta_folder = beta.project.strip_prefix(std::env::temp_dir()).unwrap();
    let relative = format!("../../../{}", beta_folder.display());
    let by_path = alpha.connecto("claude", &format!("read claude-2 --project {relative}"));
    assert_eq!(by_path.code, 0, "{by_path:?}");
    assert!(by_path.out.contains("three plus three"), "{by_path:?}");

    // Not across projects: the CLI refuses, and so does the app.
    let keys = alpha.connecto("claude", "keys --project beta claude-2 esc");
    assert_eq!(keys.code, 2, "{keys:?}");
    assert!(keys.err.contains("works only in this project"), "{keys:?}");
    for (args, code) in [
        ("send --project delta claude 'hi'", "project_closed"),
        ("send --project nowhere claude 'hi'", "unknown_project"),
    ] {
        let run = alpha.connecto("claude", args);
        assert_eq!(run.code, 1, "{args}: {run:?}");
        assert!(run.err.contains(code), "{args}: {run:?}");
    }

    // A terminal an agent of beta started is not open to everyone.
    let added = beta.connecto("claude", "add shell");
    assert_eq!(added.code, 0, "{added:?}");
    let shell = added.out.split_whitespace().next().unwrap().to_owned();
    for args in [
        format!("read --project beta {shell}"),
        format!("send --project beta {shell} 'ls'"),
    ] {
        let run = alpha.connecto("claude", &args);
        assert_eq!(run.code, 1, "{args}: {run:?}");
        assert!(run.err.contains("not_yours"), "{args}: {run:?}");
    }
    // Context use across projects comes only from a terminal open to
    // everyone: one an agent of beta started shows "-", as it can't be read.
    let owned = beta.connecto("claude", "add claude");
    assert_eq!(owned.code, 0, "{owned:?}");
    let owned = owned.out.split_whitespace().next().unwrap().to_owned();
    beta.wait_for("the started terminal to be ready", |s| {
        s.pane(&owned).is_some() && s.screen(&owned).contains("FAKE-READY")
    });
    beta.user_types("claude-2", "FOOTER");
    beta.user_types(&owned, "FOOTER");
    beta.wait_for("both footers", |s| {
        s.screen("claude-2").contains("23% (230k/1M)") && s.screen(&owned).contains("23% (230k/1M)")
    });
    let home = beta.connecto("claude", "list");
    assert_eq!(context_cell(&home.out, &owned), "23%", "{home:?}");
    let across = alpha.connecto("claude", "list --project beta");
    assert_eq!(context_cell(&across.out, "claude-2"), "23%", "{across:?}");
    assert_eq!(context_cell(&across.out, &owned), "-", "{across:?}");

    // A helper an agent started at home, as an app session starts one for a
    // check, reads and asks in another project itself: only the target must
    // be open to everyone, never the caller.
    let helper = alpha.connecto("claude", "add claude --name checker");
    assert_eq!(helper.code, 0, "{helper:?}");
    let helper = helper.out.split_whitespace().next().unwrap().to_owned();
    alpha.wait_for("the helper to be ready", |s| {
        s.pane(&helper).is_some() && s.screen(&helper).contains("FAKE-READY")
    });
    let read = alpha.connecto(&helper, "read claude-2 --project beta");
    assert_eq!(read.code, 0, "{read:?}");
    assert!(read.out.contains("three plus three"), "{read:?}");
    let asked = alpha.connecto(
        &helper,
        "send --project beta claude-2 --ask 'from the helper'",
    );
    assert_eq!(asked.code, 0, "{asked:?}");
    let request = asked.out.split_whitespace().next().unwrap().to_owned();
    alpha.wait_for("the helper to hear of its answer", |s| {
        heard(s, &helper).contains(&format!("Request {request} in project beta ended (ok)"))
    });
    // Still nothing but talking and reading for it across projects.
    let close = alpha.connecto(&helper, "close --project beta claude-2");
    assert_eq!(close.code, 2, "{close:?}");

    // A watch across projects wakes the watcher at home, named with its
    // project; an unwatch ends it.
    let watch = alpha.connecto("claude", "watch --project beta claude");
    assert_eq!(watch.code, 0, "{watch:?}");
    assert_eq!(watch.out.trim(), "watching claude");
    // An unwatch under Pause is refused and leaves the watch as it was.
    beta.relay.pause_switch().pause();
    let refused = alpha.connecto("claude", "unwatch --project beta claude");
    assert_eq!(refused.code, 1, "{refused:?}");
    assert!(refused.err.contains("paused"), "{refused:?}");
    beta.relay.pause_switch().resume();
    let listed = alpha.connecto("claude", "list --project beta");
    let row = listed
        .out
        .lines()
        .find(|line| line.starts_with("claude "))
        .unwrap_or_default()
        .to_owned();
    assert!(row.contains("watched"), "{listed:?}");
    beta.user_types("claude", "hello from the user");
    alpha.wait_for("the watched event across projects", |s| {
        s.prompts("claude").iter().any(|p| {
            p.starts_with("[connecto notice] claude in project beta")
                || p.starts_with("[connecto notice] Notices arrived")
        })
    });
    // Each wake is a turn of the watcher (a later event may also ride in as
    // the context of one): let them settle before typing, so a late wake
    // does not run into the next command.
    let mut seen = alpha.prompts("claude").len();
    loop {
        thread::sleep(Duration::from_millis(1500));
        let now = alpha.prompts("claude").len();
        if now == seen {
            break;
        }
        seen = now;
    }
    let unwatch = alpha.connecto("claude", "unwatch --project beta claude");
    assert_eq!(unwatch.out.trim(), "not watching claude", "{unwatch:?}");

    // A Pause in the target project refuses, and so does one at home.
    beta.relay.pause_switch().pause();
    let paused = alpha.connecto("claude", "send --project beta claude-2 'hi'");
    assert_eq!(paused.code, 1, "{paused:?}");
    assert!(paused.err.contains("paused"), "{paused:?}");
    beta.relay.pause_switch().resume();
    alpha.relay.pause_switch().pause();
    let paused = alpha.connecto("claude", "list --project beta");
    assert_eq!(paused.code, 1, "{paused:?}");
    assert!(paused.err.contains("paused"), "{paused:?}");
    alpha.relay.pause_switch().resume();

    // A forbidden op sent straight to the socket, as an older or forged
    // command would: refused, and recorded in both projects.
    fs::write(
        alpha.current.lock().unwrap().join("raw_keys.py"),
        r#"import json, os, socket, struct
p = json.dumps({"v": 1, "op": "in_project", "project": "beta",
                "inner": {"v": 1, "op": "keys", "target": "claude-2", "keys": ["esc"]}}).encode()
s = socket.socket(socket.AF_UNIX)
s.connect(os.environ["CONNECTO_BRIDGE_SOCK"])
s.sendall(struct.pack(">I", len(p) + 1) + b"\0" + p)
out = b""
while True:
    chunk = s.recv(65536)
    if not chunk:
        break
    out += chunk
print(out)
"#,
    )
    .unwrap();
    let raw = alpha.agent_runs("claude", "python3 raw_keys.py");
    assert!(raw.out.contains("not_across_projects"), "{raw:?}");
    assert!(beta.keys_seen("claude-2").is_empty());

    // An audit log that cannot take the intent in time: nothing is done.
    use std::sync::atomic::Ordering::SeqCst;
    beta.slow_cross.store(400, SeqCst);
    let late = alpha.connecto("claude", "read claude-2 --project beta");
    assert_eq!(late.code, 1, "{late:?}");
    assert!(late.err.contains("audit_failed"), "{late:?}");
    assert!(late.out.is_empty(), "{late:?}");
    // Nor a refusal: one the log cannot take in time says so.
    beta.relay.pause_switch().pause();
    let refused = alpha.connecto("claude", "send --project beta claude-2 'hi'");
    beta.relay.pause_switch().resume();
    assert_eq!(refused.code, 1, "{refused:?}");
    assert!(refused.err.contains("audit_failed"), "{refused:?}");
    assert!(refused.err.contains("paused"), "{refused:?}");

    // A Pause while the intent is being written, in time but slowly: the
    // read is refused when it would act, and nothing is read.
    beta.slow_cross.store(150, SeqCst);
    beta.cross_entered.store(false, SeqCst);
    let pauser = {
        let entered = std::sync::Arc::clone(&beta.cross_entered);
        let pause = beta.relay.pause_switch();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !entered.load(SeqCst) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            pause.pause();
        })
    };
    let during = alpha.connecto("claude", "read claude-2 --project beta");
    pauser.join().unwrap();
    beta.relay.pause_switch().resume();
    beta.slow_cross.store(0, SeqCst);
    assert_eq!(during.code, 1, "{during:?}");
    assert!(during.err.contains("paused"), "{during:?}");
    assert!(during.out.is_empty(), "{during:?}");

    // Both audits name what crossed.
    let alpha_audit = alpha.audit();
    let beta_audit = beta.audit();
    assert!(
        alpha_audit.contains(r#""action":"cross_project_send""#)
            && alpha_audit.contains(r#""source":"claude""#),
        "{alpha_audit}"
    );
    assert!(
        beta_audit.contains(r#""action":"cross_project_send""#)
            && beta_audit.contains(r#""action":"cross_project_read""#)
            && beta_audit.contains(r#""decision":"deny""#),
        "{beta_audit}"
    );
    // A closed project has no gate, but its record is in the one log: the
    // refusal is kept under its id as well as under the caller's.
    let closed: Vec<&str> = alpha_audit
        .lines()
        .filter(|line| {
            line.contains(r#""action":"cross_project_send""#)
                && line.contains(r#""decision":"deny""#)
                && line.contains(r#""reason":"project_closed""#)
        })
        .collect();
    assert!(
        closed
            .iter()
            .any(|line| line.contains(r#""project":"closed-one""#)
                && line.contains(r#""target":"claude""#)),
        "{alpha_audit}"
    );
    assert!(
        closed
            .iter()
            .any(|line| !line.contains(r#""project":"closed-one""#)
                && line.contains(r#""target":"closed-one/claude""#)),
        "{alpha_audit}"
    );
    for audit in [&alpha_audit, &beta_audit] {
        assert!(
            audit
                .lines()
                .any(|line| line.contains(r#""action":"cross_project_keys""#)
                    && line.contains(r#""decision":"deny""#)),
            "{audit}"
        );
    }
}
