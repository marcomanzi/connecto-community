//! Sessions on Windows: PowerShell in a ConPTY, as the Unix tests do with zsh.
//! The emulator answers ConPTY's opening cursor-position query.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::{
    TIMEOUT, hook_binary, no_wake, process_exists, screen_text, shell, wait_for_exit, wait_for_text,
};
use connecto_audit::Audit;
use connecto_core::{GridSize, RelayControl, SessionStatus, TerminalSession};
use connecto_pty::PtyError;
use connecto_session::launch::{
    BridgeInfo, LaunchExtras, Layout, set_shell_program, start_shell, start_verified_copy,
};
use connecto_session::{LiveSession, Relay, RelayConfig, shell_spec};
use connecto_store::{Store, Tool};

#[test]
fn child_output_reaches_the_screen() {
    let session = shell("Write-Output pty-hello");
    wait_for_text(&session, "pty-hello");
}

#[test]
fn keystrokes_reach_the_child() {
    let session = shell("$line = Read-Host; Write-Output \"got:$line\"");
    session.send_input(b"typed\r".to_vec()).unwrap();
    wait_for_text(&session, "got:typed");
}

#[test]
fn a_normal_exit_code_is_reported() {
    let session = shell("exit 7");
    assert_eq!(
        wait_for_exit(&session),
        SessionStatus::Exited {
            code: 7,
            signal: None
        }
    );
}

#[test]
fn the_child_environment_is_only_the_allowlist() {
    // Set by cargo for this test process, so it would leak if inherited.
    assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
    let session = shell(
        "Write-Output \"[$env:CARGO_PKG_NAME]\"; Write-Output \"term=$env:TERM\"; Write-Output done",
    );
    let snapshot = wait_for_text(&session, "done");
    let text = screen_text(&snapshot);
    assert!(text.contains("[]"), "{text}");
    assert!(text.contains("term=xterm-256color"), "{text}");
}

#[test]
fn terminate_ends_the_child() {
    let session = shell("Write-Output armed; Start-Sleep -Seconds 30");
    wait_for_text(&session, "armed");
    let pid = session.pid().unwrap();
    session.terminate();
    wait_for_exit(&session);
    let deadline = Instant::now() + TIMEOUT;
    while process_exists(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!process_exists(pid));
}

#[test]
fn dropping_a_session_terminates_its_child() {
    let session = shell("Start-Sleep -Seconds 30");
    let pid = session.pid().unwrap();
    drop(session);
    let deadline = Instant::now() + TIMEOUT;
    while process_exists(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!process_exists(pid));
}

#[test]
fn a_missing_working_directory_is_rejected() {
    let spec = shell_spec(None, Path::new(r"C:\nonexistent\connecto"));
    assert!(matches!(
        LiveSession::start(spec, no_wake()),
        Err(PtyError::MissingCwd(_))
    ));
}

/// A data folder, a home folder and a project, apart from each other.
struct Scene {
    root: PathBuf,
    layout: Layout,
    project: PathBuf,
}

impl Scene {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "connecto-win-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let project = root.join("work").join(format!("project-{name}"));
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        Self {
            layout: Layout {
                data_root: root.join("data"),
                home: root.join("home"),
            },
            project,
            root,
        }
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// M1: a plain terminal runs PowerShell in the project folder, with a hook
/// channel (a shell sends nothing on it) and, here, no bridge.
#[test]
fn a_shell_terminal_runs_powershell_in_the_project_folder() {
    let scene = Scene::new("shell");
    let session = start_shell(
        &scene.layout,
        &scene.project,
        // Wide, so the long temporary path stays on one row.
        GridSize {
            cols: 300,
            rows: 30,
        },
        no_wake(),
        &BridgeInfo::absent(),
    )
    .unwrap();
    assert!(session.hook().is_some(), "a terminal has a hook channel");
    // Split, so the echoed command line is not what matches.
    session
        .send_input(
            b"Write-Output ('wh' + 'ere:' + (Get-Location).Path + '|' + $PSVersionTable.PSEdition)\r"
                .to_vec(),
        )
        .unwrap();
    let snapshot = wait_for_text(&session, "where:");
    let text = screen_text(&snapshot);
    // A plain drive path, not the verbatim `\\?\` one canonicalizing gave.
    let shown = text.split("where:").nth(1).unwrap_or_default().as_bytes();
    assert!(
        shown.len() > 3 && shown[0].is_ascii_alphabetic() && shown[1..3] == *b":\\",
        "{text}"
    );
    assert!(!text.contains(r"\\?\"), "{text}");
    assert!(text.contains("project-shell|"), "{text}");
    // `Desktop` is Windows PowerShell, `Core` is PowerShell 7.
    assert!(
        text.contains("|Desktop") || text.contains("|Core"),
        "{text}"
    );
    session.send_input(b"exit 3\r".to_vec()).unwrap();
    assert_eq!(
        wait_for_exit(&session),
        SessionStatus::Exited {
            code: 3,
            signal: None
        }
    );
    drop(session);
}

/// M3: agents are supported on Windows. Without a store copy the launch fails
/// with the store's reason (no copy), not with a refusal, and without a panic.
#[test]
fn an_agent_terminal_without_a_store_copy_says_so() {
    let scene = Scene::new("agent");
    for tool in [Tool::Claude, Tool::Codex] {
        let refused = start_verified_copy(
            &scene.layout,
            &scene.project,
            tool,
            "1.0.0",
            GridSize { cols: 80, rows: 24 },
            no_wake(),
        );
        let err = refused.err().unwrap();
        assert!(
            matches!(err, connecto_session::launch::LaunchError::Store(_)),
            "{err}"
        );
        assert!(!err.to_string().contains("not supported"), "{err}");
    }
}

// ---- M2: the `connecto` command reaches the app over a named pipe

/// A relay on a project with one PowerShell terminal, and the store's
/// `connecto.exe` (a copy of the real hook helper).
struct Bridged {
    relay: Relay,
    scene: Scene,
}

impl Bridged {
    fn new(name: &str) -> Self {
        let scene = Scene::new(name);
        // The same PowerShell every run (no profile prompt themes matter here).
        set_shell_program(Some(shell_spec(None, &scene.project).program));
        Store::new(&scene.layout.data_root)
            .refresh_hook(&hook_binary())
            .unwrap();
        let relay = Relay::start(RelayConfig {
            layout: scene.layout.clone(),
            project: Some(scene.project.clone()),
            extras: LaunchExtras::default(),
            audit: Audit::open(&scene.root.join("audit.jsonl")).unwrap(),
            wake: no_wake(),
            default_tools: vec![Tool::Shell],
            pause: None,
            utility_settings: None,
            auto_title: false,
            after_update: false,
        })
        .unwrap();
        let bridged = Self { relay, scene };
        bridged.wait_for("the shell's prompt", |b| b.screen().contains("PS "));
        bridged
    }

    fn screen(&self) -> String {
        let Some(session) = self.relay.pane("shell").and_then(|pane| pane.session()) else {
            return String::new();
        };
        let mut snapshot = connecto_core::TermSnapshot::default();
        session.poll_snapshot(&mut snapshot);
        screen_text(&snapshot)
    }

    fn wait_for(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "waiting for {what}; the screen:\n{}",
                self.screen()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The user types a line into the PowerShell terminal.
    fn types(&self, line: &str) {
        let session = self.relay.pane("shell").unwrap().session().unwrap();
        session
            .send_input(format!("{line}\r").into_bytes())
            .unwrap();
    }

    fn connecto_exe(&self) -> PathBuf {
        Store::new(&self.scene.layout.data_root)
            .bin_dir()
            .join("connecto.exe")
    }
}

impl Drop for Bridged {
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
    }
}

/// M2 acceptance: `connecto` typed in the PowerShell terminal is the store's
/// `connecto.exe`, it reaches the app over a named pipe, and the app knows it
/// as that terminal (the `self` mark). The same command run by a process
/// outside the terminal's tree, with the same environment, is refused.
#[test]
fn connecto_in_a_powershell_terminal_reaches_the_app_and_outsiders_are_refused() {
    let bridged = Bridged::new("bridge");
    let socket = bridged.relay.bridge_socket().expect("the bridge is bound");
    let pipe = socket.to_str().unwrap().to_owned();
    assert!(pipe.starts_with(r"\\.\pipe\connecto-bridge-"), "{pipe}");

    // Split, so the echoed command line is not what matches.
    bridged.types(
        "Write-Output ('wh' + 'ich=' + (Get-Command connecto).Source + '|sock=' + $env:CONNECTO_BRIDGE_SOCK + '|term=' + $env:CONNECTO_TERMINAL + '|')",
    );
    bridged.wait_for("the command's path", |b| {
        b.screen()
            .replace('\n', "")
            .split("which=")
            .nth(1)
            .is_some_and(|line| line.contains("|term="))
    });
    let flat = bridged.screen().replace('\n', "");
    let expected = bridged.connecto_exe();
    assert!(
        flat.to_lowercase()
            .contains(&format!("which={}|", expected.display()).to_lowercase()),
        "expected {}:\n{flat}",
        expected.display()
    );
    assert!(flat.contains(&format!("|sock={pipe}|")), "{flat}");
    assert!(flat.contains("|term=shell|"), "{flat}");

    bridged.types("connecto list; Write-Output ('ex' + 'it=' + $LASTEXITCODE)");
    bridged.wait_for("the list", |b| b.screen().contains("exit="));
    let screen = bridged.screen();
    assert!(screen.contains("exit=0"), "{screen}");
    let row = screen
        .lines()
        .find(|line| line.starts_with("shell ") && line.contains("self"))
        .unwrap_or_else(|| panic!("no `shell ... self` row:\n{screen}"));
    assert!(!row.contains("bridge off"), "{row}");

    // The same command and environment, from a process outside the tree.
    let outside = Command::new(bridged.connecto_exe())
        .arg("list")
        .env("CONNECTO_BRIDGE_SOCK", &socket)
        .env("CONNECTO_TERMINAL", "shell")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&outside.stderr);
    assert_eq!(outside.status.code(), Some(1), "{err}");
    assert!(
        err.contains("works only inside a Connecto terminal"),
        "{err}"
    );
    assert!(outside.stdout.is_empty());
}

/// `connecto send` from the terminal types into a terminal (here, itself), and
/// `connecto read` returns what it printed, as on macOS.
#[test]
fn a_powershell_terminal_drives_a_terminal_with_send_and_read() {
    let bridged = Bridged::new("send");
    bridged.types("connecto send shell \"Write-Output ('se' + 'nt-' + (6*7))\"");
    bridged.wait_for("the typed command's output", |b| {
        b.screen().contains("sent-42")
    });
    bridged.types(
        "connecto read shell | Out-File -Encoding utf8 read.txt; Write-Output ('re' + 'ad=' + $LASTEXITCODE)",
    );
    bridged.wait_for("the read", |b| b.screen().contains("read="));
    assert!(bridged.screen().contains("read=0"), "{}", bridged.screen());
    let read = fs::read_to_string(bridged.scene.project.join("read.txt")).unwrap_or_default();
    assert!(read.contains("sent-42"), "{read}");
}
