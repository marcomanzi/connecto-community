//! Launch through the real PTY and store, with a scripted CLI.
//!
//! Evidence class FAKE-CLI: the PTY and store are real; the program is
//! a script. It runs `session.sh` from its project, so each test chooses what
//! the "agent" attempts.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::{hook_binary, no_wake, wait_for_exit, wait_for_text};
use connecto_core::{
    AuthState, EventText, GridSize, InputModes, PaneControl, PaneStage, TerminalSession, ToolEvent,
    encode_paste,
};
use connecto_session::launch::{LaunchError, LaunchExtras, Layout, start_verified_copy};
use connecto_session::project_folder::PathError;
use connecto_session::{LiveSession, ManagedPane, PaneSetup};
use connecto_store::{Signature, Store, StoreError, Tool};

const SIZE: GridSize = GridSize {
    cols: 100,
    rows: 30,
};
const VERSION: &str = "9.9.9";

const FAKE_CLI: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "9.9.9 (fake)"; exit 0; fi
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  if [ -f "$HOME/slow-status" ]; then /bin/sleep 2; fi
  (echo x > "$HOME/written-by-status") 2>/dev/null
  pwd > "$CLAUDE_CONFIG_DIR/status-cwd"
  if [ -f "$HOME/broken-status" ]; then echo "boom"; exit 3; fi
  if [ -f "$CLAUDE_CONFIG_DIR/logged-in" ] || [ -f "$HOME/always-logged-in" ]; then
    echo '{"loggedIn":true,"authMethod":"fake"}'
  else
    echo '{"loggedIn":false,"authMethod":"none"}'
  fi
  exit 0
fi
if [ "$1" = "auth" ] && [ "$2" = "login" ]; then
  if [ -f "$HOME/sync-query.py" ]; then
    exec /usr/bin/python3 "$HOME/sync-query.py" "$HOME/sync-result"
  fi
  echo "cwd=$(pwd)"
  echo "hookvars=$(env | grep -c CONNECTO_)"
  echo "entries=$(ls -A "$CLAUDE_CONFIG_DIR" | wc -l | tr -d ' ')"
  (echo x > ./cwd-writable) 2>/dev/null && echo "cwd accepts writes"
  echo "outside" > "$HOME/written-by-login" && echo "wrote outside"
  echo "paste your code"
  IFS= read -r line
  echo "$line" > "$CLAUDE_CONFIG_DIR/pasted"
  touch "$CLAUDE_CONFIG_DIR/logged-in"
  exit 0
fi
exec /bin/sh ./session.sh "$@"
"#;

struct AnySignature;

impl Signature for AnySignature {
    fn verify(&self, _file: &Path, _team: Option<&str>) -> Result<(), String> {
        Ok(())
    }
}

struct Fixture {
    _permit: common::Permit,
    root: PathBuf,
    layout: Layout,
}

fn unlock(path: &Path) {
    if path.exists() {
        let _ = std::process::Command::new("/usr/bin/chflags")
            .args(["-R", "nouchg"])
            .arg(path)
            .status();
    }
    if path.is_dir() && !path.is_symlink() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                unlock(&entry.path());
            }
        }
    }
}

impl Fixture {
    fn new(name: &str) -> Self {
        let permit = common::permit();
        let root = std::env::temp_dir().join(format!("mg-launch-{name}-{}", std::process::id()));
        unlock(&root);
        let _ = fs::remove_dir_all(&root);
        for dir in ["home", "data", "source"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let entry = root.join("source/9.9.9");
        fs::write(&entry, FAKE_CLI).unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let store = Store::new(&layout.data_root);
        store
            .provision(Tool::Claude, &entry, &AnySignature)
            .unwrap();
        store.install_hook(&hook_binary()).unwrap();
        Self {
            _permit: permit,
            root,
            layout,
        }
    }

    fn project(&self, name: &str, script: &str) -> PathBuf {
        let project = self.root.join("home").join(name);
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("session.sh"), script).unwrap();
        // These tests are about the isolated profile (login, run and profile
        // folders); a new project would be on the system configuration.
        let id = connecto_session::launch::project_id(&fs::canonicalize(&project).unwrap());
        connecto_session::project_settings::save(
            &self.layout.data_root,
            &id,
            &connecto_core::ProjectSettings {
                claude_config: connecto_core::ConfigMode::Isolated,
                codex_config: connecto_core::ConfigMode::Isolated,
                ..connecto_core::ProjectSettings::default()
            },
        )
        .unwrap();
        project
    }

    fn start(&self, project: &Path) -> Result<LiveSession, LaunchError> {
        start_verified_copy(
            &self.layout,
            project,
            Tool::Claude,
            VERSION,
            SIZE,
            no_wake(),
        )
    }

    fn store_binary(&self) -> PathBuf {
        self.root.join("data/cli/claude/9.9.9/claude")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unlock(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn the_store_binary_runs_directly_in_its_project_with_its_own_tty() {
    let fixture = Fixture::new("runs");
    let project = fixture.project("p", "echo cwd=$(pwd); tty; echo started\n");
    let session = fixture.start(&project).unwrap();
    wait_for_text(&session, "started");
    wait_for_text(&session, "/dev/ttys");
    wait_for_exit(&session);
}

/// AC2: no session runs under `sandbox-exec`. The script's parent is the store
/// entry itself, and nothing above it is the sandbox tool.
#[test]
fn the_session_is_spawned_directly_with_no_sandbox_above_it() {
    let fixture = Fixture::new("direct");
    let project = fixture.project(
        "p",
        "echo parent=$(/bin/ps -o command= -p $PPID)\n\
         echo grand=$(/bin/ps -o command= -p $(/bin/ps -o ppid= -p $PPID))\n\
         echo done\n",
    );
    let session = fixture.start(&project).unwrap();
    wait_for_text(&session, "done");
    let mut snapshot = connecto_core::TermSnapshot::default();
    session.poll_snapshot(&mut snapshot);
    let text = common::screen_text(&snapshot);
    // The store entry was exec'd by Connecto's PTY spawn itself: the parent is
    // this test process, with no wrapper between.
    assert!(
        text.contains("parent=") && text.contains("launch-"),
        "{text}"
    );
    assert!(!text.contains("sandbox-exec"), "{text}");
}

#[test]
fn a_tampered_store_binary_blocks_the_launch() {
    let fixture = Fixture::new("tamper");
    let project = fixture.project("p", "echo started\n");
    let binary = fixture.store_binary();
    fs::set_permissions(binary.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(&binary, "#!/bin/sh\necho evil\n").unwrap();
    assert!(matches!(
        fixture.start(&project),
        Err(LaunchError::Store(StoreError::Tampered(_)))
    ));
}

#[test]
fn an_unprovisioned_version_blocks_the_launch() {
    let fixture = Fixture::new("version");
    let project = fixture.project("p", "echo started\n");
    let result = start_verified_copy(
        &fixture.layout,
        &project,
        Tool::Claude,
        "1.0.0",
        SIZE,
        no_wake(),
    );
    assert!(matches!(
        result,
        Err(LaunchError::Store(StoreError::NotProvisioned { .. }))
    ));
}

#[test]
fn forbidden_project_folders_block_the_launch() {
    let fixture = Fixture::new("forbidden");
    for project in [
        PathBuf::from("/"),
        fixture.layout.home.clone(),
        fixture.layout.data_root.clone(),
    ] {
        assert!(
            matches!(
                fixture.start(&project),
                Err(LaunchError::Path(PathError::ForbiddenProject(_)))
            ),
            "{}",
            project.display()
        );
    }
}

// Hook channel, V-23. Every refusal has a positive control in the same session.

// Only ever waited on for something that should arrive, so a long one costs
// nothing when a test passes. At 5 s these timed out under parallel load.
const WAIT: Duration = Duration::from_secs(60);

impl Fixture {
    fn hook_path(&self) -> String {
        self.root
            .join("data/cli/bin/connecto-hook")
            .display()
            .to_string()
    }

    /// A shell line sending one prompt through the real helper.
    fn send_prompt(&self, prompt: &str) -> String {
        format!(
            r#"printf '{{"hook_event_name":"UserPromptSubmit","prompt":"{prompt}"}}' | '{}' claude"#,
            self.hook_path()
        )
    }

    fn pane(&self, project: &Path) -> ManagedPane {
        ManagedPane::spawn(PaneSetup {
            layout: self.layout.clone(),
            project: Some(project.to_owned()),
            tool: Tool::Claude,
            size: SIZE,
            wake: no_wake(),
            extras: LaunchExtras::default(),
            bridge: None,
        })
        .unwrap()
    }
}

fn prompt_event(text: &str) -> ToolEvent {
    ToolEvent::PromptSubmitted {
        prompt: EventText::new(text.to_owned()),
        turn: None,
    }
}

fn wait_for_rejected(session: &LiveSession, count: u64) {
    let hook = session.hook().unwrap();
    let deadline = Instant::now() + WAIT;
    while hook.rejected() < count && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(hook.rejected(), count);
}

#[test]
fn a_hook_inside_the_session_reaches_only_that_sessions_queue() {
    let fixture = Fixture::new("hookok");
    let script = format!("{}\necho sent\n", fixture.send_prompt("hello"));
    let project = fixture.project("p", &script);
    let session = fixture.start(&project).unwrap();
    let hook = session.hook().unwrap();
    assert_eq!(
        hook.events().recv_timeout(WAIT),
        Some(prompt_event("hello"))
    );
    assert_eq!(hook.rejected(), 0);
    wait_for_text(&session, "sent");
}

#[test]
fn a_wrong_token_from_a_real_descendant_is_refused() {
    let fixture = Fixture::new("hooktoken");
    let good = fixture.send_prompt("good");
    let bad = fixture.send_prompt("forged");
    let script = format!(
        "REAL=$CONNECTO_HOOK_TOKEN\nCONNECTO_HOOK_TOKEN=wrong; export CONNECTO_HOOK_TOKEN\n\
         {bad}\nCONNECTO_HOOK_TOKEN=$REAL\n{good}\necho sent\n"
    );
    let project = fixture.project("p", &script);
    let session = fixture.start(&project).unwrap();
    let hook = session.hook().unwrap();
    assert_eq!(hook.events().recv_timeout(WAIT), Some(prompt_event("good")));
    assert_eq!(hook.rejected(), 1);
    assert_eq!(hook.events().recv_timeout(Duration::ZERO), None);
}

#[test]
fn a_stolen_token_from_outside_the_session_tree_is_refused() {
    let fixture = Fixture::new("hooksteal");
    let script = format!(
        "printf '%s' \"$CONNECTO_HOOK_TOKEN\" > token.txt\n\
         while [ ! -f go ]; do /bin/sleep 0.05; done\n{}\necho sent\n",
        fixture.send_prompt("from-inside")
    );
    let project = fixture.project("p", &script);
    let session = fixture.start(&project).unwrap();
    let token_file = project.join("token.txt");
    let deadline = Instant::now() + WAIT;
    while !token_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(50));
    let token = fs::read(&token_file).unwrap();
    assert_eq!(token.len(), 64);

    // This test process is not a descendant of the session.
    let hook = session.hook().unwrap();
    let frame = connecto_hook::encode(
        &token,
        br#"{"hook_event_name":"UserPromptSubmit","prompt":"stolen"}"#,
    )
    .unwrap();
    let mut forger = std::os::unix::net::UnixStream::connect(hook.socket()).unwrap();
    let _ = forger.write_all(&frame);
    wait_for_rejected(&session, 1);

    // Control: the same token from inside is accepted.
    fs::write(project.join("go"), "").unwrap();
    assert_eq!(
        hook.events().recv_timeout(WAIT),
        Some(prompt_event("from-inside"))
    );
    assert_eq!(hook.rejected(), 1);
}

#[test]
fn an_unparsable_payload_from_a_descendant_is_counted_and_ignored() {
    let fixture = Fixture::new("hookjunk");
    let script = format!(
        "printf 'not json' | '{}' claude\n{}\necho sent\n",
        fixture.hook_path(),
        fixture.send_prompt("after")
    );
    let project = fixture.project("p", &script);
    let session = fixture.start(&project).unwrap();
    let hook = session.hook().unwrap();
    assert_eq!(
        hook.events().recv_timeout(WAIT),
        Some(prompt_event("after"))
    );
    assert_eq!(hook.rejected(), 1);
}

#[test]
fn a_dropped_session_leaves_no_socket_and_no_run_folder() {
    let fixture = Fixture::new("cleanup");
    let project = fixture.project("p", "echo started\n");
    let session = fixture.start(&project).unwrap();
    let socket = session.hook().unwrap().socket().to_owned();
    wait_for_text(&session, "started");
    wait_for_exit(&session);
    assert!(socket.exists());
    drop(session);
    assert!(!socket.exists());
    let left = fs::read_dir(fixture.root.join("data/run")).unwrap().count();
    assert_eq!(left, 0);
}

#[test]
fn two_attempts_in_one_process_do_not_share_folders_or_sockets() {
    let fixture = Fixture::new("attempts");
    let project = fixture.project("p", "/bin/sleep 3\n");
    let first = fixture.start(&project).unwrap();
    let second = fixture.start(&project).unwrap();
    assert_ne!(
        first.hook().unwrap().socket(),
        second.hook().unwrap().socket()
    );
    assert_eq!(
        fs::read_dir(fixture.root.join("data/run")).unwrap().count(),
        2
    );
}

// Preflight and login through the managed pane.

fn wait_for_stage(
    pane: &ManagedPane,
    what: &str,
    matches: impl Fn(&PaneStage) -> bool,
) -> PaneStage {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let stage = pane.stage();
        if matches(&stage) {
            return stage;
        }
        assert!(Instant::now() < deadline, "waiting for {what}: {stage:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// R4: a logged-out tool goes straight into its login. There is no confirm to
/// click, and the project's own command never runs.
#[test]
fn a_logged_out_tool_goes_straight_into_its_login() {
    let fixture = Fixture::new("loggedout");
    let project = fixture.project("p", "touch started-file\n");
    let pane = fixture.pane(&project);
    let stage = wait_for_stage(&pane, "login", |stage| {
        !matches!(stage, PaneStage::Preparing { .. })
    });
    assert_eq!(stage, PaneStage::LoggingIn);
    let login = pane.session().unwrap();
    assert!(login.hook().is_none(), "a login has no hook channel");
    wait_for_text(login.as_ref(), "paste your code");
    assert!(!project.join("started-file").exists());
}

#[test]
fn a_failed_status_check_is_unknown_and_starts_nothing() {
    let fixture = Fixture::new("statusbad");
    fs::write(fixture.root.join("home/broken-status"), "").unwrap();
    let project = fixture.project("p", "touch started-file\n");
    let pane = fixture.pane(&project);
    let stage = wait_for_stage(&pane, "a result", |stage| {
        !matches!(stage, PaneStage::Preparing { .. })
    });
    assert_eq!(
        stage,
        PaneStage::NeedsLogin {
            auth: AuthState::Unknown
        }
    );
    assert!(pane.session().is_none());
}

#[test]
fn a_slow_preflight_never_holds_up_the_caller_and_shows_its_step() {
    let fixture = Fixture::new("slow");
    fs::write(fixture.root.join("home/slow-status"), "").unwrap();
    let project = fixture.project("p", "echo x\n");
    let before = Instant::now();
    let pane = fixture.pane(&project);
    assert!(before.elapsed() < Duration::from_millis(100));
    let stage = wait_for_stage(&pane, "login check", |stage| {
        *stage
            == PaneStage::Preparing {
                step: "Checking login".to_owned(),
            }
    });
    // Everything the UI calls answers while preflight runs.
    let timed = Instant::now();
    let _ = pane.stage();
    let _ = pane.status();
    let _ = pane.send_input(vec![b'x']);
    let mut snapshot = connecto_core::TermSnapshot::default();
    let _ = pane.poll_snapshot(&mut snapshot);
    assert!(timed.elapsed() < Duration::from_millis(50), "{stage:?}");
}

#[test]
fn a_tampered_copy_fails_in_the_pane() {
    let fixture = Fixture::new("panetamper");
    let binary = fixture.store_binary();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&binary)
        .unwrap()
        .write_all(b"# changed\n")
        .unwrap();
    let project = fixture.project("p", "echo x\n");
    let pane = fixture.pane(&project);
    let stage = wait_for_stage(&pane, "failure", |stage| {
        matches!(stage, PaneStage::Failed { .. })
    });
    let PaneStage::Failed { reason } = stage else {
        panic!("not failed");
    };
    assert!(reason.contains("failed verification"), "{reason}");
}

#[test]
fn a_forbidden_project_fails_in_the_pane() {
    let fixture = Fixture::new("paneproject");
    fs::write(
        fixture.root.join("data/projects-marker"),
        "the data root is forbidden",
    )
    .unwrap();
    let pane = fixture.pane(&fixture.root.join("home"));
    wait_for_stage(&pane, "failure", |stage| {
        matches!(stage, PaneStage::Failed { .. })
    });
}

// Later steps validate too, so only the reason shows the pane checked first.
#[test]
fn a_forbidden_project_is_refused_before_the_store_is_looked_at() {
    let fixture = Fixture::new("paneorder");
    let binary = fixture.store_binary();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&binary)
        .unwrap()
        .write_all(b"# changed\n")
        .unwrap();
    let pane = fixture.pane(&fixture.root.join("home"));
    let stage = wait_for_stage(&pane, "failure", |stage| {
        matches!(stage, PaneStage::Failed { .. })
    });
    let PaneStage::Failed { reason } = stage else {
        panic!("not failed");
    };
    assert!(reason.contains("cannot be a project"), "{reason}");
}

#[test]
fn a_query_in_a_stalled_update_is_answered_in_a_login_with_no_ui_running() {
    let fixture = Fixture::new("login-sync");
    let project = fixture.project("p", "/bin/sleep 3\n");
    let home = fixture.root.join("home");
    fs::write(home.join("sync-query.py"), common::SYNC_QUERY_CHILD).unwrap();
    let pane = Arc::new(fixture.pane(&project));
    wait_for_stage(&pane, "login", |stage| *stage == PaneStage::LoggingIn);
    let (ms, reply) = common::sync_query_result(&home.join("sync-result"));
    assert!(reply.contains('R'), "no cursor report: {reply}");
    assert!(ms <= common::SYNC_REPLY_LIMIT_MS, "took {ms} ms");
}

#[test]
fn login_runs_outside_the_sandbox_then_takes_a_paste_then_the_session_starts() {
    let fixture = Fixture::new("login");
    let project = fixture.project("p", "echo session-up; /bin/sleep 3\n");
    let pane = Arc::new(fixture.pane(&project));
    wait_for_stage(&pane, "login", |stage| *stage == PaneStage::LoggingIn);
    let login = pane.session().unwrap();
    assert!(login.hook().is_none(), "login has no hook channel");
    let screen = wait_for_text(login.as_ref(), "paste your code");
    let text = common::screen_text(&screen);
    assert!(text.contains("/data/run/"), "cwd is the run folder: {text}");
    assert!(!text.contains("/profiles/"), "{text}");
    assert!(text.contains("hookvars=0"), "{text}");
    assert!(text.contains("wrote outside"), "{text}");
    assert!(fixture.root.join("home/written-by-login").exists());

    // What Cmd+V sends, including the bracketed form a real TUI turns on.
    let mut bytes = encode_paste("code-123", InputModes::default());
    bytes.push(b'\n');
    pane.send_input(bytes).unwrap();

    wait_for_stage(&pane, "session", |stage| *stage == PaneStage::Running);
    let session = pane.session().unwrap();
    assert!(session.hook().is_some());
    wait_for_text(session.as_ref(), "session-up");
    let pasted: Vec<_> = walk_files(&fixture.root.join("data/projects"))
        .into_iter()
        .filter(|path| path.ends_with("pasted"))
        .collect();
    assert_eq!(pasted.len(), 1);
    assert_eq!(fs::read_to_string(&pasted[0]).unwrap().trim(), "code-123");
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

#[test]
fn a_command_is_ignored_while_preflight_is_running() {
    let fixture = Fixture::new("busy");
    fs::write(fixture.root.join("home/slow-status"), "").unwrap();
    let project = fixture.project("p", "echo x\n");
    let pane = fixture.pane(&project);
    wait_for_stage(&pane, "login check", |stage| {
        matches!(stage, PaneStage::Preparing { .. })
    });
    // A click on a login meanwhile does nothing; the start's own login runs.
    pane.request_login();
    let stage = wait_for_stage(&pane, "result", |stage| {
        !matches!(stage, PaneStage::Preparing { .. })
    });
    assert_eq!(stage, PaneStage::LoggingIn);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(pane.stage(), PaneStage::LoggingIn);
}

/// A Stop that lands during the login check cancels the login that would have
/// followed: the pane ends Stopped and never shows a login.
#[test]
fn a_stop_during_the_login_check_starts_no_login() {
    let fixture = Fixture::new("stopcheck");
    fs::write(fixture.root.join("home/slow-status"), "").unwrap();
    let project = fixture.project("p", "echo x\n");
    let pane = fixture.pane(&project);
    wait_for_stage(&pane, "login check", |stage| {
        matches!(stage, PaneStage::Preparing { .. })
    });
    pane.request_stop();
    // The check takes 2 s; watch well past it.
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        assert_ne!(
            pane.stage(),
            PaneStage::LoggingIn,
            "a login started after Stop"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(pane.stage(), PaneStage::Stopped);
    assert!(pane.session().is_none() || !pane.is_live());
}

// ADR 0009. V-29: login never sees what a session left behind.

fn tool_root_of(fixture: &Fixture) -> PathBuf {
    let projects = fixture.root.join("data/projects");
    let project = fs::read_dir(projects).unwrap().next().unwrap().unwrap();
    project.path().join("profiles/claude")
}

/// The current generation, by the app's own record.
fn profile_of(fixture: &Fixture) -> PathBuf {
    let root = tool_root_of(fixture);
    let current = fs::read_to_string(root.join("current")).unwrap_or_else(|_| "1".to_owned());
    root.join(format!("g{}", current.trim()))
}

fn installed(fixture: &Fixture) -> connecto_store::Installed {
    Store::new(&fixture.layout.data_root)
        .verify(Tool::Claude, VERSION)
        .unwrap()
}

/// Every file and folder under `dir`.
fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(tree(&path));
        }
        found.push(path);
    }
    found.sort();
    found
}

#[test]
fn a_refused_launch_leaves_no_folders_behind() {
    let fixture = Fixture::new("linksclean");
    let data = fixture.root.join("data");
    let before = tree(&data);

    // A refusal leaves nothing behind. A forbidden folder is the refusal left.
    assert!(fixture.start(&fixture.layout.home).is_err());
    assert_eq!(tree(&data), before);

    let pane = fixture.pane(&fixture.layout.home);
    wait_for_stage(&pane, "failure", |stage| {
        matches!(stage, PaneStage::Failed { .. })
    });
    assert_eq!(tree(&data), before);
}

#[test]
fn login_is_refused_while_that_tools_session_is_live_and_changes_nothing() {
    let fixture = Fixture::new("v29live");
    let project = fixture.project("p", "/bin/sleep 5\n");
    let session = fixture.start(&project).unwrap();
    let profile = profile_of(&fixture);
    fs::write(profile.join("history"), "keep").unwrap();

    let result = connecto_session::launch::start_login(
        &fixture.layout,
        &project,
        Tool::Claude,
        &installed(&fixture),
        SIZE,
        no_wake(),
    );
    assert!(matches!(result, Err(LaunchError::SessionLive)));
    assert_eq!(fs::read_to_string(profile.join("history")).unwrap(), "keep");

    // Control: with the session gone, the same call goes ahead.
    drop(session);
    let login = connecto_session::launch::start_login(
        &fixture.layout,
        &project,
        Tool::Claude,
        &installed(&fixture),
        SIZE,
        no_wake(),
    )
    .unwrap();
    // A14: a kept generation is normal now. Login makes a new one and leaves
    // the old one alone.
    assert_eq!(fs::read_to_string(profile.join("history")).unwrap(), "keep");
    login.terminate();
}

// V-30: nothing automatic runs a CLI outside the sandbox.

#[test]
fn a_status_check_leaves_no_run_folder_and_no_socket() {
    let fixture = Fixture::new("v30clean");
    let project = fixture.project("p", "echo x\n");
    let state = connecto_session::launch::check_auth(
        &fixture.layout,
        &project,
        Tool::Claude,
        &installed(&fixture),
    )
    .unwrap();
    assert_eq!(state, AuthState::LoggedOut);
    assert_eq!(
        fs::read_dir(fixture.root.join("data/run")).unwrap().count(),
        0
    );
}

#[test]
fn stop_ends_a_login_and_nothing_starts_after_it() {
    let fixture = Fixture::new("stoplogin");
    let project = fixture.project("p", "touch started-file\n");
    let pane = fixture.pane(&project);
    wait_for_stage(&pane, "login", |stage| *stage == PaneStage::LoggingIn);
    let login = pane.session().unwrap();
    wait_for_text(login.as_ref(), "paste your code");
    drop(login);

    pane.request_stop();
    wait_for_stage(&pane, "stopped", |stage| *stage == PaneStage::Stopped);
    assert!(pane.session().is_none(), "the login screen is gone");
    thread::sleep(Duration::from_millis(200));
    assert_eq!(pane.stage(), PaneStage::Stopped);
    assert!(!project.join("started-file").exists());
}

// Stop and start, T5. A start carries a ticket from the moment it is asked for,
// and a Stop cancels it wherever it has got to.

/// Holds the worker at `point` until released.
fn hold_at(
    pane: &ManagedPane,
    point: &'static str,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached) = crossbeam_channel::bounded::<()>(1);
    let (release, release_rx) = crossbeam_channel::bounded::<()>(1);
    pane.set_probe(move |at| {
        if at == point {
            let _ = reached_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(20));
        }
    });
    (reached, release)
}

fn logged_in_pane(name: &str) -> (Fixture, PathBuf, ManagedPane) {
    let fixture = Fixture::new(name);
    let project = fixture.project("p", "touch started-file; /bin/sleep 20\n");
    fs::write(fixture.root.join("home/always-logged-in"), "").unwrap();
    let pane = fixture.pane(&project);
    (fixture, project, pane)
}

#[test]
fn a_stop_during_preflight_is_kept_and_nothing_starts() {
    let (_fixture, project, pane) = logged_in_pane("t5pre");
    let (reached, release) = hold_at(&pane, "before_spawn");
    reached.recv_timeout(Duration::from_secs(20)).unwrap();
    pane.request_stop();
    release.send(()).unwrap();
    wait_for_stage(&pane, "stopped", |stage| *stage == PaneStage::Stopped);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(pane.stage(), PaneStage::Stopped);
    assert!(pane.session().is_none());
    assert!(!project.join("started-file").exists());
}

#[test]
fn a_stop_between_spawn_and_publication_leaves_no_process() {
    let (_fixture, _project, pane) = logged_in_pane("t5spawn");
    let (reached, release) = hold_at(&pane, "spawned");
    reached.recv_timeout(Duration::from_secs(20)).unwrap();
    // The process exists and is not yet published. The Stop's bump comes first.
    pane.request_stop();
    release.send(()).unwrap();
    wait_for_stage(&pane, "stopped", |stage| *stage == PaneStage::Stopped);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(pane.stage(), PaneStage::Stopped);
    assert!(
        pane.wait_exit(Duration::from_secs(10)),
        "a process survived"
    );
    assert!(!pane.is_live());
}

#[test]
fn a_stop_just_after_publication_stops_the_published_session() {
    let (_fixture, _project, pane) = logged_in_pane("t5pub");
    let (reached, release) = hold_at(&pane, "published");
    reached.recv_timeout(Duration::from_secs(20)).unwrap();
    // `live` is stored and the stage is Running: the Stop finds both.
    assert_eq!(pane.stage(), PaneStage::Running);
    pane.request_stop();
    release.send(()).unwrap();
    wait_for_stage(&pane, "stopped", |stage| *stage == PaneStage::Stopped);
    assert!(
        pane.wait_exit(Duration::from_secs(10)),
        "a process survived"
    );
    assert_eq!(pane.stage(), PaneStage::Stopped);
}

// Profile reservation, T7 and T8.

fn login(fixture: &Fixture, project: &Path) -> Result<LiveSession, LaunchError> {
    connecto_session::launch::start_login(
        &fixture.layout,
        project,
        Tool::Claude,
        &installed(fixture),
        SIZE,
        no_wake(),
    )
}

#[test]
fn stopped_terminals_do_not_block_login_and_keep_their_screens() {
    let fixture = Fixture::new("t7");
    let project = fixture.project("p", "echo screen-$$; /bin/sleep 20\n");
    let first = fixture.start(&project).unwrap();
    let second = fixture.start(&project).unwrap();
    wait_for_text(&first, "screen-");
    wait_for_text(&second, "screen-");
    // A running one still blocks.
    assert!(matches!(
        login(&fixture, &project),
        Err(LaunchError::SessionLive)
    ));
    first.terminate();
    wait_for_exit(&first);
    assert!(matches!(
        login(&fixture, &project),
        Err(LaunchError::SessionLive)
    ));
    second.terminate();
    wait_for_exit(&second);
    // Both stopped, both kept for reading: login goes ahead.
    let started = login(&fixture, &project).unwrap();
    for kept in [&first, &second] {
        let mut snapshot = connecto_core::TermSnapshot::default();
        kept.poll_snapshot(&mut snapshot);
        assert!(common::screen_text(&snapshot).contains("screen-"));
    }
    started.terminate();
}

#[test]
fn two_logins_at_once_are_refused_and_so_is_a_start_during_one() {
    let fixture = Fixture::new("t8a");
    let project = fixture.project("p", "echo up; /bin/sleep 20\n");
    let first = login(&fixture, &project).unwrap();
    assert!(matches!(
        login(&fixture, &project),
        Err(LaunchError::LoginRunning)
    ));
    assert!(matches!(
        fixture.start(&project),
        Err(LaunchError::LoginRunning)
    ));
    // Once the login's process ends the claim stops counting, even while
    // something still holds its screen.
    first.terminate();
    wait_for_exit(&first);
    let session = fixture.start(&project).unwrap();
    session.terminate();
}

#[test]
fn a_start_and_a_login_racing_never_both_win_and_the_session_keeps_its_generation() {
    let fixture = Arc::new(Fixture::new("t8b"));
    let project = fixture.project(
        "p",
        "printf '%s' \"$CLAUDE_CONFIG_DIR\" > gen.txt; echo gen-written; /bin/sleep 20\n",
    );
    for round in 0..12 {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let (a, b) = (Arc::clone(&fixture), Arc::clone(&fixture));
        let (pa, pb) = (project.clone(), project.clone());
        let (ba, bb) = (Arc::clone(&barrier), barrier);
        let start = thread::spawn(move || {
            ba.wait();
            a.start(&pa)
        });
        let login_thread = thread::spawn(move || {
            bb.wait();
            login(&b, &pb)
        });
        let started = start.join().unwrap();
        let logged = login_thread.join().unwrap();
        match (&started, &logged) {
            (Ok(_), Ok(_)) => panic!("round {round}: a session and a login both started"),
            (Ok(session), Err(why)) => {
                assert!(matches!(why, LaunchError::SessionLive), "{why}");
                // The session launched on a generation that exists, and it is
                // the one `current` still names.
                wait_for_text(session, "gen-written");
                let used = fs::read_to_string(project.join("gen.txt")).unwrap();
                assert_eq!(used, profile_of(&fixture).to_str().unwrap());
            }
            (Err(why), Ok(_)) => assert!(matches!(why, LaunchError::LoginRunning), "{why}"),
            (Err(a), Err(b)) => panic!("round {round}: neither started: {a} / {b}"),
        }
        for session in [started.ok(), logged.ok()].into_iter().flatten() {
            session.terminate();
            wait_for_exit(&session);
        }
    }
}

#[test]
fn a_refused_login_keeps_the_panes_own_stopped_screen() {
    let (fixture, project, pane) = logged_in_pane("t8c");
    wait_for_stage(&pane, "running", |stage| *stage == PaneStage::Running);
    let screen = wait_for_text(pane.session().unwrap().as_ref(), "");
    drop(screen);
    pane.request_stop();
    wait_for_stage(&pane, "stopped", |stage| *stage == PaneStage::Stopped);
    assert!(pane.wait_exit(Duration::from_secs(10)));
    // Another terminal of the same tool is running, so login is refused.
    let other = fixture.start(&project).unwrap();
    pane.request_login();
    wait_for_stage(&pane, "refused", |stage| {
        matches!(stage, PaneStage::Failed { .. })
    });
    assert!(pane.session().is_some(), "the stopped screen was dropped");
    other.terminate();
}

/// The setup choice reaches the command line of every launch that goes through
/// the launcher, which is every way an agent starts (new terminal, restart,
/// relaunch, automations, a remote peer's add and restart).
#[test]
fn the_launch_mode_set_in_setup_reaches_the_tools_command_line() {
    use connecto_core::{AgentMode, LaunchModes};
    use connecto_session::launch::{launch_modes, set_launch_modes};

    let fixture = Fixture::new("mode");
    let project = fixture.project("p", "echo \"ARGS: $*\"; sleep 1\n");
    let before = launch_modes();
    assert_eq!(
        before,
        LaunchModes::default(),
        "the default is today's behaviour"
    );
    let argv = |mode: AgentMode| {
        set_launch_modes(LaunchModes {
            claude: mode,
            ..LaunchModes::default()
        });
        let session = fixture.start(&project).unwrap();
        let screen = common::wait_for_text(&session, "ARGS:");
        session.terminate();
        common::screen_text(&screen)
    };
    let bypass = argv(AgentMode::Bypass);
    let auto = argv(AgentMode::Auto);
    let prompt = argv(AgentMode::Prompt);
    set_launch_modes(before);
    assert!(
        bypass.contains("--dangerously-skip-permissions"),
        "{bypass}"
    );
    assert!(!bypass.contains("--permission-mode"), "{bypass}");
    assert!(auto.contains("--permission-mode auto"), "{auto}");
    assert!(!auto.contains("skip-permissions"), "{auto}");
    assert!(
        !prompt.contains("skip-permissions") && !prompt.contains("--permission-mode"),
        "{prompt}"
    );
    // Whatever the choice, the hooks and the bridge are still on the line.
    for line in [&bypass, &auto, &prompt] {
        assert!(line.contains("--settings"), "{line}");
    }
}
